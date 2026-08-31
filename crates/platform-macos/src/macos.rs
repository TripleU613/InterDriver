use block2::{DynBlock, RcBlock};
use interdriver_usbip_protocol::{Direction, IsoPacketDescriptor};
use interdriver_usbip_session::{
    Error as SessionError, Session, TransferCompletion, TransferError, TransferRequest,
};
use objc2::rc::{Allocated, Retained, autoreleasepool};
use objc2::runtime::{AnyObject, NSObject};
use objc2::{AnyThread, Encode, Encoding, RefEncode, extern_class, extern_methods};
use objc2_foundation::{NSData, NSError};
use std::collections::HashMap;
use std::ffi::c_void;
use std::net::ToSocketAddrs;
use std::ptr::NonNull;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

const MESSAGE_TYPE_CONTROLLER_CAPABILITIES: u32 = 0x00;
const MESSAGE_TYPE_PORT_CAPABILITIES: u32 = 0x01;
const MESSAGE_CONTROL_NO_RESPONSE: u32 = 1 << 14;
const MESSAGE_CONTROL_VALID: u32 = 1 << 15;
const CAPABILITIES_PORT_COUNT_PHASE: u32 = 16;
const CAPABILITIES_TIMEOUT_PHASE: u32 = 0;
const CAPABILITIES_LATENCY_PHASE: u32 = 4;
const PORT_CAPABILITIES_PORT_NUMBER_PHASE: u32 = 16;
const PORT_CAPABILITIES_MAX_POWER_PHASE: u32 = 0;
const MESSAGE_TYPE_MASK: u32 = 0x3f;
const MESSAGE_TYPE_CONTROLLER_POWER_ON: u32 = 0x10;
const MESSAGE_TYPE_CONTROLLER_POWER_OFF: u32 = 0x11;
const MESSAGE_TYPE_CONTROLLER_START: u32 = 0x12;
const MESSAGE_TYPE_CONTROLLER_PAUSE: u32 = 0x13;
const MESSAGE_TYPE_CONTROLLER_FRAME_NUMBER: u32 = 0x14;
const MESSAGE_TYPE_PORT_POWER_ON: u32 = 0x18;
const MESSAGE_TYPE_PORT_POWER_OFF: u32 = 0x19;
const MESSAGE_TYPE_PORT_RESUME: u32 = 0x1a;
const MESSAGE_TYPE_PORT_SUSPEND: u32 = 0x1b;
const MESSAGE_TYPE_PORT_RESET: u32 = 0x1c;
const MESSAGE_TYPE_PORT_DISABLE: u32 = 0x1d;
const MESSAGE_TYPE_PORT_STATUS: u32 = 0x1e;
const MESSAGE_TYPE_DEVICE_CREATE: u32 = 0x20;
const MESSAGE_TYPE_DEVICE_DESTROY: u32 = 0x21;
const MESSAGE_TYPE_DEVICE_START: u32 = 0x22;
const MESSAGE_TYPE_DEVICE_PAUSE: u32 = 0x23;
const MESSAGE_TYPE_DEVICE_UPDATE: u32 = 0x24;
const MESSAGE_TYPE_ENDPOINT_CREATE: u32 = 0x28;
const MESSAGE_TYPE_ENDPOINT_DESTROY: u32 = 0x29;
const MESSAGE_TYPE_ENDPOINT_PAUSE: u32 = 0x2b;
const MESSAGE_TYPE_ENDPOINT_UPDATE: u32 = 0x2c;
const MESSAGE_TYPE_ENDPOINT_RESET: u32 = 0x2d;
const MESSAGE_TYPE_ENDPOINT_SET_NEXT: u32 = 0x2e;
const MESSAGE_TYPE_SETUP_TRANSFER: u32 = 0x38;
const MESSAGE_TYPE_NORMAL_TRANSFER: u32 = 0x39;
const MESSAGE_TYPE_STATUS_TRANSFER: u32 = 0x3a;
const MESSAGE_TYPE_ISOCHRONOUS_TRANSFER: u32 = 0x3b;
const MESSAGE_STATUS_SUCCESS: i32 = 1;
const MESSAGE_STATUS_OFFLINE: i32 = 2;
const MESSAGE_STATUS_NOT_PERMITTED: i32 = 3;
const MESSAGE_STATUS_BAD_ARGUMENT: i32 = 4;
const MESSAGE_STATUS_TIMEOUT: i32 = 5;
const MESSAGE_STATUS_NO_RESOURCES: i32 = 6;
const MESSAGE_STATUS_ENDPOINT_STOPPED: i32 = 7;
const MESSAGE_STATUS_OVERRUN: i32 = 10;
const MESSAGE_STATUS_STALL: i32 = 11;
const MESSAGE_STATUS_ERROR: i32 = 13;
const ENDPOINT_STATE_ACTIVE: i32 = 3;
const TRANSFER_COMPLETION_NOT_SENT: isize = -536870174;
const DEVICE_SPEED_NONE: u32 = 0;
const DEVICE_SPEED_HIGH: u32 = 3;
const LINK_STATE_U0: u32 = 0;
const LINK_STATE_U3: u32 = 3;
const LINK_STATE_DISABLED: u32 = 4;
const LINK_STATE_RX_DETECT: u32 = 5;
const LINK_STATE_RESET: u32 = 9;
const MAX_DOORBELLS: usize = 4096;
const TRANSFER_LENGTH_MASK: u32 = (1 << 28) - 1;
const TRANSFER_FLAG_ISO_ASAP: u32 = 2;
const ISO_FRAME_PHASE: u32 = 16;
const ISO_ASAP: u32 = 1 << 24;

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct IOUSBHostCIMessage {
    control: u32,
    data0: u32,
    data1: u64,
}

unsafe impl Encode for IOUSBHostCIMessage {
    const ENCODING: Encoding = Encoding::Struct(
        "IOUSBHostCIMessage",
        &[u32::ENCODING, u32::ENCODING, u64::ENCODING],
    );
}

unsafe impl RefEncode for IOUSBHostCIMessage {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Self::ENCODING);
}

extern_class!(
    #[unsafe(super(NSObject))]
    #[name = "IOUSBHostControllerInterface"]
    struct IOUSBHostControllerInterface;
);

extern_class!(
    #[unsafe(super(NSObject))]
    #[name = "IOUSBHostCIControllerStateMachine"]
    struct IOUSBHostCIControllerStateMachine;
);

extern_class!(
    #[unsafe(super(NSObject))]
    #[name = "IOUSBHostCIPortStateMachine"]
    struct IOUSBHostCIPortStateMachine;
);

extern_class!(
    #[unsafe(super(NSObject))]
    #[name = "IOUSBHostCIDeviceStateMachine"]
    struct IOUSBHostCIDeviceStateMachine;
);

extern_class!(
    #[unsafe(super(NSObject))]
    #[name = "IOUSBHostCIEndpointStateMachine"]
    struct IOUSBHostCIEndpointStateMachine;
);

type InterestHandler = Option<
    unsafe extern "C-unwind" fn(
        reference: *mut c_void,
        service: u32,
        message_type: u32,
        message_argument: *mut c_void,
    ),
>;

type CommandBlock = DynBlock<dyn Fn(NonNull<IOUSBHostControllerInterface>, IOUSBHostCIMessage)>;
type DoorbellBlock = DynBlock<dyn Fn(NonNull<IOUSBHostControllerInterface>, *mut u32, u32)>;

impl IOUSBHostControllerInterface {
    extern_methods!(
        #[unsafe(method(initWithCapabilities:queue:interruptRateHz:error:commandHandler:doorbellHandler:interestHandler:))]
        #[unsafe(method_family = init)]
        unsafe fn init_with_capabilities(
            this: Allocated<Self>,
            capabilities: &NSData,
            queue: Option<&AnyObject>,
            interrupt_rate_hz: usize,
            error: *mut *mut NSError,
            command_handler: &CommandBlock,
            doorbell_handler: &DoorbellBlock,
            interest_handler: InterestHandler,
        ) -> Option<Retained<Self>>;

        #[unsafe(method(destroy))]
        fn destroy(&self);

        #[unsafe(method(controllerStateMachine))]
        fn controller_state_machine(&self) -> Retained<IOUSBHostCIControllerStateMachine>;

        #[unsafe(method(getPortStateMachineForCommand:error:))]
        unsafe fn port_state_machine_for_command(
            &self,
            command: *const IOUSBHostCIMessage,
            error: *mut *mut NSError,
        ) -> Option<Retained<IOUSBHostCIPortStateMachine>>;
    );
}

impl IOUSBHostCIControllerStateMachine {
    extern_methods!(
        #[unsafe(method(inspectCommand:error:))]
        unsafe fn inspect_command(
            &self,
            command: *const IOUSBHostCIMessage,
            error: *mut *mut NSError,
        ) -> bool;

        #[unsafe(method(respondToCommand:status:error:))]
        unsafe fn respond_to_command(
            &self,
            command: *const IOUSBHostCIMessage,
            status: i32,
            error: *mut *mut NSError,
        ) -> bool;

        #[unsafe(method(respondToCommand:status:frame:timestamp:error:))]
        unsafe fn respond_to_frame_command(
            &self,
            command: *const IOUSBHostCIMessage,
            status: i32,
            frame: u64,
            timestamp: u64,
            error: *mut *mut NSError,
        ) -> bool;
    );
}

impl IOUSBHostCIPortStateMachine {
    extern_methods!(
        #[unsafe(method(inspectCommand:error:))]
        unsafe fn inspect_command(
            &self,
            command: *const IOUSBHostCIMessage,
            error: *mut *mut NSError,
        ) -> bool;

        #[unsafe(method(respondToCommand:status:error:))]
        unsafe fn respond_to_command(
            &self,
            command: *const IOUSBHostCIMessage,
            status: i32,
            error: *mut *mut NSError,
        ) -> bool;

        #[unsafe(method(setPowered:))]
        fn set_powered(&self, powered: bool);

        #[unsafe(method(setConnected:))]
        fn set_connected(&self, connected: bool);

        #[unsafe(method(updateLinkState:speed:inhibitLinkStateChange:error:))]
        unsafe fn update_link_state(
            &self,
            link_state: i32,
            speed: i32,
            inhibit_link_state_change: bool,
            error: *mut *mut NSError,
        ) -> bool;
    );
}

impl IOUSBHostCIDeviceStateMachine {
    extern_methods!(
        #[unsafe(method(initWithInterface:command:error:))]
        #[unsafe(method_family = init)]
        unsafe fn init_with_interface(
            this: Allocated<Self>,
            controller: &IOUSBHostControllerInterface,
            command: *const IOUSBHostCIMessage,
            error: *mut *mut NSError,
        ) -> Option<Retained<Self>>;

        #[unsafe(method(inspectCommand:error:))]
        unsafe fn inspect_command(
            &self,
            command: *const IOUSBHostCIMessage,
            error: *mut *mut NSError,
        ) -> bool;

        #[unsafe(method(respondToCommand:status:error:))]
        unsafe fn respond_to_command(
            &self,
            command: *const IOUSBHostCIMessage,
            status: i32,
            error: *mut *mut NSError,
        ) -> bool;

        #[unsafe(method(respondToCommand:status:deviceAddress:error:))]
        unsafe fn respond_to_create_command(
            &self,
            command: *const IOUSBHostCIMessage,
            status: i32,
            device_address: usize,
            error: *mut *mut NSError,
        ) -> bool;
    );
}

impl IOUSBHostCIEndpointStateMachine {
    extern_methods!(
        #[unsafe(method(initWithInterface:command:error:))]
        #[unsafe(method_family = init)]
        unsafe fn init_with_interface(
            this: Allocated<Self>,
            controller: &IOUSBHostControllerInterface,
            command: *const IOUSBHostCIMessage,
            error: *mut *mut NSError,
        ) -> Option<Retained<Self>>;

        #[unsafe(method(inspectCommand:error:))]
        unsafe fn inspect_command(
            &self,
            command: *const IOUSBHostCIMessage,
            error: *mut *mut NSError,
        ) -> bool;

        #[unsafe(method(respondToCommand:status:error:))]
        unsafe fn respond_to_command(
            &self,
            command: *const IOUSBHostCIMessage,
            status: i32,
            error: *mut *mut NSError,
        ) -> bool;

        #[unsafe(method(processDoorbell:error:))]
        unsafe fn process_doorbell(&self, doorbell: u32, error: *mut *mut NSError) -> bool;

        #[unsafe(method(enqueueTransferCompletionForMessage:status:transferLength:error:))]
        unsafe fn complete_transfer(
            &self,
            message: *const IOUSBHostCIMessage,
            status: i32,
            transfer_length: usize,
            error: *mut *mut NSError,
        ) -> bool;

        #[unsafe(method(currentTransferMessage))]
        fn current_transfer_message(&self) -> *const IOUSBHostCIMessage;

        #[unsafe(method(endpointState))]
        fn endpoint_state(&self) -> i32;
    );
}

enum ControllerEvent {
    Command(IOUSBHostCIMessage),
    Doorbells(Vec<u32>),
    TransferFinished {
        key: EndpointKey,
        message: usize,
        buffer: usize,
        requested_length: usize,
        direction: Direction,
        no_response: bool,
        result: Result<TransferCompletion, TransferError>,
    },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct EndpointKey {
    device: u8,
    endpoint: u8,
}

struct Endpoint {
    state_machine: Retained<IOUSBHostCIEndpointStateMachine>,
    setup: Option<[u8; 8]>,
    pending_sequence: Option<u32>,
    deferred_command: Option<IOUSBHostCIMessage>,
    previous_no_response: Option<usize>,
}

struct MacController {
    controller: Retained<IOUSBHostControllerInterface>,
    event_sender: mpsc::Sender<ControllerEvent>,
    events: Receiver<ControllerEvent>,
    _command_handler: RcBlock<dyn Fn(NonNull<IOUSBHostControllerInterface>, IOUSBHostCIMessage)>,
    _doorbell_handler: RcBlock<dyn Fn(NonNull<IOUSBHostControllerInterface>, *mut u32, u32)>,
    started_at: Instant,
    remote_connected: bool,
    remote_speed: u32,
    session: Option<Session>,
    next_device_address: u8,
    devices: HashMap<u8, Retained<IOUSBHostCIDeviceStateMachine>>,
    endpoints: HashMap<EndpointKey, Endpoint>,
}

impl MacController {
    fn create(session: Option<Session>, remote_speed: u32) -> Result<Self, String> {
        let capabilities = capabilities();
        let capabilities = unsafe {
            std::slice::from_raw_parts(
                capabilities.as_ptr().cast::<u8>(),
                std::mem::size_of_val(&capabilities),
            )
        };
        let capabilities = NSData::with_bytes(capabilities);
        let (event_sender, events) = mpsc::channel();
        let command_sender = event_sender.clone();
        let command_handler = RcBlock::new(
            move |_controller: NonNull<IOUSBHostControllerInterface>,
                  command: IOUSBHostCIMessage| {
                let _ = command_sender.send(ControllerEvent::Command(command));
            },
        );
        let doorbell_sender = event_sender.clone();
        let doorbell_handler = RcBlock::new(
            move |_controller: NonNull<IOUSBHostControllerInterface>,
                  doorbells: *mut u32,
                  doorbell_count: u32| {
                let count = usize::try_from(doorbell_count)
                    .unwrap_or(MAX_DOORBELLS)
                    .min(MAX_DOORBELLS);
                if count == 0 || doorbells.is_null() {
                    return;
                }
                let copied = unsafe { std::slice::from_raw_parts(doorbells, count) }.to_vec();
                let _ = doorbell_sender.send(ControllerEvent::Doorbells(copied));
            },
        );
        let mut error: *mut NSError = std::ptr::null_mut();
        let controller = unsafe {
            IOUSBHostControllerInterface::init_with_capabilities(
                IOUSBHostControllerInterface::alloc(),
                &capabilities,
                None,
                1000,
                &mut error,
                &command_handler,
                &doorbell_handler,
                None,
            )
        };
        let Some(controller) = controller else {
            return Err(format_error(error));
        };

        Ok(Self {
            controller,
            event_sender,
            events,
            _command_handler: command_handler,
            _doorbell_handler: doorbell_handler,
            started_at: Instant::now(),
            remote_connected: session.is_some(),
            remote_speed,
            session,
            next_device_address: 1,
            devices: HashMap::new(),
            endpoints: HashMap::new(),
        })
    }

    fn service_for(&mut self, duration: Duration) -> Result<(), String> {
        let deadline = Instant::now() + duration;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(());
            }
            match self.events.recv_timeout(deadline - now) {
                Ok(ControllerEvent::Command(command)) => self.handle_command(command)?,
                Ok(ControllerEvent::Doorbells(doorbells)) => {
                    for doorbell in doorbells {
                        self.handle_doorbell(doorbell)?;
                    }
                }
                Ok(ControllerEvent::TransferFinished {
                    key,
                    message,
                    buffer,
                    requested_length,
                    direction,
                    no_response,
                    result,
                }) => self.handle_transfer_finished(
                    key,
                    message,
                    buffer,
                    requested_length,
                    direction,
                    no_response,
                    result,
                )?,
                Err(RecvTimeoutError::Timeout) => return Ok(()),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("IOUSBHost controller event channel disconnected".into());
                }
            }
        }
    }

    fn remote_closed(&self) -> bool {
        self.session.as_ref().is_none_or(Session::is_closed)
    }

    fn handle_command(&mut self, command: IOUSBHostCIMessage) -> Result<(), String> {
        let message_type = command.control & MESSAGE_TYPE_MASK;
        match message_type {
            MESSAGE_TYPE_CONTROLLER_POWER_ON
            | MESSAGE_TYPE_CONTROLLER_POWER_OFF
            | MESSAGE_TYPE_CONTROLLER_START
            | MESSAGE_TYPE_CONTROLLER_PAUSE
            | MESSAGE_TYPE_CONTROLLER_FRAME_NUMBER => self.handle_controller_command(&command),
            MESSAGE_TYPE_PORT_POWER_ON
            | MESSAGE_TYPE_PORT_POWER_OFF
            | MESSAGE_TYPE_PORT_RESUME
            | MESSAGE_TYPE_PORT_SUSPEND
            | MESSAGE_TYPE_PORT_RESET
            | MESSAGE_TYPE_PORT_DISABLE
            | MESSAGE_TYPE_PORT_STATUS => self.handle_port_command(&command),
            MESSAGE_TYPE_DEVICE_CREATE => self.handle_device_create(&command),
            MESSAGE_TYPE_DEVICE_DESTROY
            | MESSAGE_TYPE_DEVICE_START
            | MESSAGE_TYPE_DEVICE_PAUSE
            | MESSAGE_TYPE_DEVICE_UPDATE => self.handle_device_command(&command),
            MESSAGE_TYPE_ENDPOINT_CREATE => self.handle_endpoint_create(&command),
            MESSAGE_TYPE_ENDPOINT_DESTROY
            | MESSAGE_TYPE_ENDPOINT_PAUSE
            | MESSAGE_TYPE_ENDPOINT_UPDATE
            | MESSAGE_TYPE_ENDPOINT_RESET
            | MESSAGE_TYPE_ENDPOINT_SET_NEXT => self.handle_endpoint_command(command),
            _ => Err(format!(
                "unsupported IOUSBHost command type 0x{message_type:02x}"
            )),
        }
    }

    fn handle_controller_command(&self, command: &IOUSBHostCIMessage) -> Result<(), String> {
        let state_machine = self.controller.controller_state_machine();
        let mut error = std::ptr::null_mut();
        let accepted = unsafe { state_machine.inspect_command(command, &mut error) };
        ensure_accepted(accepted, error, "controller command inspection")?;

        error = std::ptr::null_mut();
        let message_type = command.control & MESSAGE_TYPE_MASK;
        let responded = if message_type == MESSAGE_TYPE_CONTROLLER_FRAME_NUMBER {
            let frame = u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
            unsafe {
                state_machine.respond_to_frame_command(
                    command,
                    MESSAGE_STATUS_SUCCESS,
                    frame,
                    mach_absolute_time(),
                    &mut error,
                )
            }
        } else {
            unsafe { state_machine.respond_to_command(command, MESSAGE_STATUS_SUCCESS, &mut error) }
        };
        ensure_accepted(responded, error, "controller command response")
    }

    fn handle_port_command(&self, command: &IOUSBHostCIMessage) -> Result<(), String> {
        let mut error = std::ptr::null_mut();
        let state_machine = unsafe {
            self.controller
                .port_state_machine_for_command(command, &mut error)
        }
        .ok_or_else(|| operation_error(error, "root-port state machine lookup"))?;

        error = std::ptr::null_mut();
        let accepted = unsafe { state_machine.inspect_command(command, &mut error) };
        ensure_accepted(accepted, error, "root-port command inspection")?;

        let message_type = command.control & MESSAGE_TYPE_MASK;
        match message_type {
            MESSAGE_TYPE_PORT_POWER_ON => {
                state_machine.set_powered(true);
                state_machine.set_connected(self.remote_connected);
                let link_state = if self.remote_connected {
                    LINK_STATE_U0
                } else {
                    LINK_STATE_RX_DETECT
                };
                self.update_port_link(&state_machine, link_state)?;
            }
            MESSAGE_TYPE_PORT_POWER_OFF => {
                state_machine.set_connected(false);
                state_machine.set_powered(false);
            }
            MESSAGE_TYPE_PORT_RESUME | MESSAGE_TYPE_PORT_STATUS => {
                self.update_port_link(&state_machine, LINK_STATE_U0)?;
            }
            MESSAGE_TYPE_PORT_SUSPEND => {
                self.update_port_link(&state_machine, LINK_STATE_U3)?;
            }
            MESSAGE_TYPE_PORT_RESET => {
                self.update_port_link(&state_machine, LINK_STATE_RESET)?;
                self.update_port_link(&state_machine, LINK_STATE_U0)?;
            }
            MESSAGE_TYPE_PORT_DISABLE => {
                self.update_port_link(&state_machine, LINK_STATE_DISABLED)?;
            }
            _ => unreachable!(),
        }

        error = std::ptr::null_mut();
        let responded = unsafe {
            state_machine.respond_to_command(command, MESSAGE_STATUS_SUCCESS, &mut error)
        };
        ensure_accepted(responded, error, "root-port command response")
    }

    fn update_port_link(
        &self,
        state_machine: &IOUSBHostCIPortStateMachine,
        link_state: u32,
    ) -> Result<(), String> {
        let mut error = std::ptr::null_mut();
        let speed = if self.remote_connected {
            self.remote_speed
        } else {
            DEVICE_SPEED_NONE
        };
        let updated =
            unsafe { state_machine.update_link_state(link_state as i32, speed as i32, false, &mut error) };
        ensure_accepted(updated, error, "root-port link update")
    }

    fn handle_device_create(&mut self, command: &IOUSBHostCIMessage) -> Result<(), String> {
        let mut error = std::ptr::null_mut();
        let state_machine = unsafe {
            IOUSBHostCIDeviceStateMachine::init_with_interface(
                IOUSBHostCIDeviceStateMachine::alloc(),
                &self.controller,
                command,
                &mut error,
            )
        }
        .ok_or_else(|| operation_error(error, "device state machine creation"))?;

        let Some(address) = self.allocate_device_address() else {
            error = std::ptr::null_mut();
            let responded = unsafe {
                state_machine.respond_to_create_command(
                    command,
                    MESSAGE_STATUS_NO_RESOURCES,
                    0,
                    &mut error,
                )
            };
            return ensure_accepted(responded, error, "device create rejection");
        };

        error = std::ptr::null_mut();
        let responded = unsafe {
            state_machine.respond_to_create_command(
                command,
                MESSAGE_STATUS_SUCCESS,
                usize::from(address),
                &mut error,
            )
        };
        ensure_accepted(responded, error, "device create response")?;
        self.devices.insert(address, state_machine);
        Ok(())
    }

    fn allocate_device_address(&mut self) -> Option<u8> {
        for _ in 0..127 {
            let address = self.next_device_address;
            self.next_device_address = if address >= 127 { 1 } else { address + 1 };
            if !self.devices.contains_key(&address) {
                return Some(address);
            }
        }
        None
    }

    fn handle_device_command(&mut self, command: &IOUSBHostCIMessage) -> Result<(), String> {
        let address = device_address(command);
        let Some(state_machine) = self.devices.get(&address) else {
            return Ok(());
        };
        if command.control & MESSAGE_TYPE_MASK == MESSAGE_TYPE_DEVICE_DESTROY
            && self.endpoints.keys().any(|key| key.device == address)
        {
            return Err(format!(
                "device {address} destroy arrived before its endpoints were destroyed"
            ));
        }
        let mut error = std::ptr::null_mut();
        let accepted = unsafe { state_machine.inspect_command(command, &mut error) };
        ensure_accepted(accepted, error, "device command inspection")?;
        error = std::ptr::null_mut();
        let responded = unsafe {
            state_machine.respond_to_command(command, MESSAGE_STATUS_SUCCESS, &mut error)
        };
        ensure_accepted(responded, error, "device command response")?;
        if command.control & MESSAGE_TYPE_MASK == MESSAGE_TYPE_DEVICE_DESTROY {
            self.devices.remove(&address);
        }
        Ok(())
    }

    fn handle_endpoint_create(&mut self, command: &IOUSBHostCIMessage) -> Result<(), String> {
        let key = endpoint_key(command);
        if self.endpoints.contains_key(&key) {
            return Err(format!(
                "duplicate endpoint create for {}.{:#04x}",
                key.device, key.endpoint
            ));
        }
        let mut error = std::ptr::null_mut();
        let state_machine = unsafe {
            IOUSBHostCIEndpointStateMachine::init_with_interface(
                IOUSBHostCIEndpointStateMachine::alloc(),
                &self.controller,
                command,
                &mut error,
            )
        }
        .ok_or_else(|| operation_error(error, "endpoint state machine creation"))?;
        error = std::ptr::null_mut();
        let responded = unsafe {
            state_machine.respond_to_command(command, MESSAGE_STATUS_SUCCESS, &mut error)
        };
        ensure_accepted(responded, error, "endpoint create response")?;
        self.endpoints.insert(
            key,
            Endpoint {
                state_machine,
                setup: None,
                pending_sequence: None,
                deferred_command: None,
                previous_no_response: None,
            },
        );
        Ok(())
    }

    fn handle_endpoint_command(&mut self, command: IOUSBHostCIMessage) -> Result<(), String> {
        let key = endpoint_key(&command);
        let Some(endpoint) = self.endpoints.get_mut(&key) else {
            return Ok(());
        };
        let mut error = std::ptr::null_mut();
        let accepted = unsafe { endpoint.state_machine.inspect_command(&command, &mut error) };
        ensure_accepted(accepted, error, "endpoint command inspection")?;
        if let Some(sequence) = endpoint.pending_sequence {
            if endpoint.deferred_command.is_some() {
                return Err(format!(
                    "multiple endpoint commands queued while {}.{:#04x} is active",
                    key.device, key.endpoint
                ));
            }
            endpoint.deferred_command = Some(command);
            if let Some(session) = &self.session {
                match session.unlink(sequence) {
                    Ok(unlink) => {
                        thread::Builder::new()
                            .name("interdriver-usbip-unlink".into())
                            .spawn(move || {
                                let _ = unlink.wait_timeout(Duration::from_secs(5));
                            })
                            .map_err(|error| error.to_string())?;
                    }
                    Err(SessionError::InvalidRequest("target transfer is not pending")) => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
            return Ok(());
        }
        self.respond_endpoint_command(key, command)?;
        if self.endpoints.contains_key(&key) {
            self.drive_endpoint(key)?;
        }
        Ok(())
    }

    fn respond_endpoint_command(
        &mut self,
        key: EndpointKey,
        command: IOUSBHostCIMessage,
    ) -> Result<(), String> {
        let destroy = command.control & MESSAGE_TYPE_MASK == MESSAGE_TYPE_ENDPOINT_DESTROY;
        let Some(endpoint) = self.endpoints.get(&key) else {
            return Ok(());
        };
        let mut error = std::ptr::null_mut();
        let responded = unsafe {
            endpoint
                .state_machine
                .respond_to_command(&command, MESSAGE_STATUS_SUCCESS, &mut error)
        };
        ensure_accepted(responded, error, "endpoint command response")?;
        if destroy {
            self.endpoints.remove(&key);
        }
        Ok(())
    }

    fn handle_doorbell(&mut self, doorbell: u32) -> Result<(), String> {
        let key = EndpointKey {
            device: (doorbell & 0xff) as u8,
            endpoint: ((doorbell >> 8) & 0xff) as u8,
        };
        let Some(endpoint) = self.endpoints.get(&key) else {
            return Ok(());
        };
        let mut error = std::ptr::null_mut();
        let accepted = unsafe {
            endpoint
                .state_machine
                .process_doorbell(doorbell, &mut error)
        };
        ensure_accepted(accepted, error, "endpoint doorbell")?;
        self.drive_endpoint(key)
    }

    fn drive_endpoint(&mut self, key: EndpointKey) -> Result<(), String> {
        let Some(mut endpoint) = self.endpoints.remove(&key) else {
            return Ok(());
        };
        let result = self.drive_endpoint_inner(key, &mut endpoint);
        self.endpoints.insert(key, endpoint);
        result
    }

    fn drive_endpoint_inner(
        &mut self,
        key: EndpointKey,
        endpoint: &mut Endpoint,
    ) -> Result<(), String> {
        while endpoint.pending_sequence.is_none() {
            if endpoint.state_machine.endpoint_state() != ENDPOINT_STATE_ACTIVE {
                return Ok(());
            }
            let pointer = endpoint.state_machine.current_transfer_message();
            if pointer.is_null() {
                return Ok(());
            }
            let message = unsafe { pointer.read_unaligned() };
            if message.control & MESSAGE_CONTROL_VALID == 0 {
                return Ok(());
            }
            let pointer_value = pointer as usize;
            let no_response = message.control & MESSAGE_CONTROL_NO_RESPONSE != 0;
            if no_response && endpoint.previous_no_response == Some(pointer_value) {
                return Ok(());
            }
            endpoint.previous_no_response = no_response.then_some(pointer_value);

            match message.control & MESSAGE_TYPE_MASK {
                MESSAGE_TYPE_SETUP_TRANSFER => {
                    endpoint.setup = Some(message.data1.to_le_bytes());
                    if !no_response {
                        complete_endpoint_transfer(
                            &endpoint.state_machine,
                            pointer,
                            MESSAGE_STATUS_SUCCESS,
                            0,
                        )?;
                    }
                }
                MESSAGE_TYPE_STATUS_TRANSFER => {
                    let Some(setup) = endpoint.setup else {
                        if !no_response {
                            complete_endpoint_transfer(
                                &endpoint.state_machine,
                                pointer,
                                MESSAGE_STATUS_SUCCESS,
                                0,
                            )?;
                        }
                        continue;
                    };
                    let data_length = u16::from_le_bytes([setup[6], setup[7]]);
                    if data_length == 0 {
                        let mut request = TransferRequest::output(0, Vec::new());
                        request.setup = setup;
                        self.submit_transfer(
                            key,
                            endpoint,
                            pointer,
                            0,
                            0,
                            Direction::Out,
                            no_response,
                            request,
                        )?;
                    } else if !no_response {
                        endpoint.setup = None;
                        complete_endpoint_transfer(
                            &endpoint.state_machine,
                            pointer,
                            MESSAGE_STATUS_SUCCESS,
                            0,
                        )?;
                    }
                }
                MESSAGE_TYPE_NORMAL_TRANSFER => {
                    let length = (message.data0 & TRANSFER_LENGTH_MASK) as usize;
                    let buffer = message.data1 as usize;
                    let (direction, setup) = if key.endpoint & 0x0f == 0 {
                        let setup = endpoint.setup.ok_or_else(|| {
                            "control data transfer arrived without a setup packet".to_string()
                        })?;
                        let direction = if setup[0] & 0x80 != 0 {
                            Direction::In
                        } else {
                            Direction::Out
                        };
                        (direction, setup)
                    } else {
                        let direction = if key.endpoint & 0x80 != 0 {
                            Direction::In
                        } else {
                            Direction::Out
                        };
                        (direction, [0; 8])
                    };
                    let mut request = transfer_request(direction, key.endpoint, buffer, length)?;
                    request.setup = setup;
                    self.submit_transfer(
                        key,
                        endpoint,
                        pointer,
                        buffer,
                        length,
                        direction,
                        no_response,
                        request,
                    )?;
                }
                MESSAGE_TYPE_ISOCHRONOUS_TRANSFER => {
                    let length = (message.data0 & TRANSFER_LENGTH_MASK) as usize;
                    let buffer = message.data1 as usize;
                    let direction = if key.endpoint & 0x80 != 0 {
                        Direction::In
                    } else {
                        Direction::Out
                    };
                    let mut request = transfer_request(direction, key.endpoint, buffer, length)?;
                    request.iso_packets.push(IsoPacketDescriptor {
                        offset: 0,
                        length: u32::try_from(length)
                            .map_err(|_| "isochronous transfer length exceeds u32".to_string())?,
                        actual_length: 0,
                        status: 0,
                    });
                    let current_frame =
                        u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
                    let low_frame = ((message.control >> ISO_FRAME_PHASE) & 0xff) as u8;
                    request.start_frame = if message.control & ISO_ASAP != 0 {
                        request.transfer_flags |= TRANSFER_FLAG_ISO_ASAP;
                        0
                    } else {
                        rebase_frame(current_frame, low_frame) as u32 as i32
                    };
                    self.submit_transfer(
                        key,
                        endpoint,
                        pointer,
                        buffer,
                        length,
                        direction,
                        no_response,
                        request,
                    )?;
                }
                unsupported => {
                    if !no_response {
                        complete_endpoint_transfer(
                            &endpoint.state_machine,
                            pointer,
                            MESSAGE_STATUS_ERROR,
                            0,
                        )?;
                    }
                    return Err(format!(
                        "unsupported endpoint transfer message 0x{unsupported:02x}"
                    ));
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_transfer(
        &self,
        key: EndpointKey,
        endpoint: &mut Endpoint,
        message: *const IOUSBHostCIMessage,
        buffer: usize,
        requested_length: usize,
        direction: Direction,
        no_response: bool,
        request: TransferRequest,
    ) -> Result<(), String> {
        let Some(session) = &self.session else {
            if !no_response {
                complete_endpoint_transfer(
                    &endpoint.state_machine,
                    message,
                    MESSAGE_STATUS_OFFLINE,
                    0,
                )?;
            }
            return Ok(());
        };
        let pending = match session.submit(request) {
            Ok(pending) => pending,
            Err(error) => {
                if !no_response {
                    complete_endpoint_transfer(
                        &endpoint.state_machine,
                        message,
                        MESSAGE_STATUS_ERROR,
                        0,
                    )?;
                }
                return Err(error.to_string());
            }
        };
        endpoint.pending_sequence = Some(pending.sequence);
        let sender = self.event_sender.clone();
        let message = message as usize;
        thread::Builder::new()
            .name(format!("interdriver-usbip-{}-{:02x}", key.device, key.endpoint))
            .spawn(move || {
                let result = pending.wait();
                let _ = sender.send(ControllerEvent::TransferFinished {
                    key,
                    message,
                    buffer,
                    requested_length,
                    direction,
                    no_response,
                    result,
                });
            })
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_transfer_finished(
        &mut self,
        key: EndpointKey,
        message: usize,
        buffer: usize,
        requested_length: usize,
        direction: Direction,
        no_response: bool,
        result: Result<TransferCompletion, TransferError>,
    ) -> Result<(), String> {
        let Some(mut endpoint) = self.endpoints.remove(&key) else {
            return Ok(());
        };
        endpoint.pending_sequence = None;

        let stopped = endpoint
            .deferred_command
            .as_ref()
            .is_some_and(endpoint_command_stops_transfer);
        let inactive = endpoint.state_machine.endpoint_state() != ENDPOINT_STATE_ACTIVE;
        let (status, actual_length) = match result {
            _ if inactive => (MESSAGE_STATUS_ENDPOINT_STOPPED, 0),
            _ if stopped => (MESSAGE_STATUS_ENDPOINT_STOPPED, 0),
            Ok(completion) => {
                if completion.actual_length > requested_length {
                    return Err(format!(
                        "remote completion length {} exceeds request length {requested_length}",
                        completion.actual_length
                    ));
                }
                if direction == Direction::In && completion.actual_length > 0 {
                    if buffer == 0 {
                        return Err("remote IN completion has a null destination buffer".into());
                    }
                    if completion.data.len() < completion.actual_length {
                        return Err(format!(
                            "remote IN payload has {} bytes for actual length {}",
                            completion.data.len(),
                            completion.actual_length
                        ));
                    }
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            completion.data.as_ptr(),
                            buffer as *mut u8,
                            completion.actual_length,
                        );
                    }
                }
                (
                    usbip_status_to_ci(completion.status),
                    completion.actual_length,
                )
            }
            Err(TransferError::Cancelled | TransferError::Connection(_)) => {
                (MESSAGE_STATUS_OFFLINE, 0)
            }
            Err(TransferError::Protocol(_)) => (MESSAGE_STATUS_ERROR, 0),
        };

        if !no_response {
            complete_endpoint_transfer(
                &endpoint.state_machine,
                message as *const IOUSBHostCIMessage,
                status,
                actual_length,
            )?;
        }

        let deferred = endpoint.deferred_command.take();
        self.endpoints.insert(key, endpoint);
        if let Some(command) = deferred {
            self.respond_endpoint_command(key, command)?;
        }
        if self.endpoints.contains_key(&key) {
            self.drive_endpoint(key)?;
        }
        Ok(())
    }
}

impl Drop for MacController {
    fn drop(&mut self) {
        self.controller.destroy();
    }
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
}

pub fn probe_controller() -> Result<(), String> {
    autoreleasepool(|_| {
        let mut controller = MacController::create(None, DEVICE_SPEED_HIGH)?;
        controller.service_for(Duration::from_millis(250))
    })
}

pub fn run_remote<A: ToSocketAddrs>(
    address: A,
    bus_id: &str,
    connect_timeout: Duration,
) -> Result<(), String> {
    run_remote_with_ticket(address, bus_id, connect_timeout, None)
}

pub fn run_remote_with_ticket<A: ToSocketAddrs>(
    address: A,
    bus_id: &str,
    connect_timeout: Duration,
    ticket: Option<[u8; 32]>,
) -> Result<(), String> {
    let remote = address
        .to_socket_addrs()
        .map_err(|error| error.to_string())?
        .next()
        .ok_or_else(|| "USB/IP host has no usable address".to_string())?;
    let mut connected_once = false;
    loop {
        let imported = match interdriver_usbip_client::import_with_ticket(
            remote,
            bus_id,
            connect_timeout,
            ticket,
        ) {
            Ok(imported) => imported,
            Err(error) if connected_once => {
                eprintln!("waiting for Apple device {bus_id} to re-enumerate: {error}");
                thread::sleep(Duration::from_millis(500));
                continue;
            }
            Err(error) => return Err(error.to_string()),
        };
        if imported.device.vendor_id != 0x05ac {
            return Err(format!(
                "remote device {:04x}:{:04x} is not an Apple USB device",
                imported.device.vendor_id, imported.device.product_id
            ));
        }
        let speed = usbip_speed_to_ci(imported.device.speed);
        let session = Session::from_imported(imported).map_err(|error| error.to_string())?;
        autoreleasepool(|_| -> Result<(), String> {
            let mut controller = MacController::create(Some(session), speed)?;
            while !controller.remote_closed() {
                controller.service_for(Duration::from_millis(250))?;
            }
            Ok(())
        })?;
        connected_once = true;
        eprintln!("remote Apple device disconnected; rebuilding the virtual controller");
        thread::sleep(Duration::from_millis(250));
    }
}

fn transfer_request(
    direction: Direction,
    endpoint_address: u8,
    buffer: usize,
    length: usize,
) -> Result<TransferRequest, String> {
    let endpoint = u32::from(endpoint_address & 0x0f);
    match direction {
        Direction::In => Ok(TransferRequest::input(endpoint, length)),
        Direction::Out => {
            if length > 0 && buffer == 0 {
                return Err("OUT transfer has a null source buffer".into());
            }
            let data = if length == 0 {
                Vec::new()
            } else {
                unsafe { std::slice::from_raw_parts(buffer as *const u8, length) }.to_vec()
            };
            Ok(TransferRequest::output(endpoint, data))
        }
    }
}

fn complete_endpoint_transfer(
    state_machine: &IOUSBHostCIEndpointStateMachine,
    message: *const IOUSBHostCIMessage,
    status: i32,
    length: usize,
) -> Result<(), String> {
    if state_machine.endpoint_state() != ENDPOINT_STATE_ACTIVE {
        return Ok(());
    }
    let mut error = std::ptr::null_mut();
    let completed = unsafe { state_machine.complete_transfer(message, status, length, &mut error) };
    if !completed
        && unsafe { error.as_ref() }
            .is_some_and(|error| error.code() == TRANSFER_COMPLETION_NOT_SENT)
    {
        return Ok(());
    }
    ensure_accepted(completed, error, "endpoint transfer completion")
}

fn endpoint_command_stops_transfer(command: &IOUSBHostCIMessage) -> bool {
    matches!(
        command.control & MESSAGE_TYPE_MASK,
        MESSAGE_TYPE_ENDPOINT_PAUSE | MESSAGE_TYPE_ENDPOINT_RESET
    )
}

fn device_address(message: &IOUSBHostCIMessage) -> u8 {
    (message.data0 & 0xff) as u8
}

fn endpoint_key(message: &IOUSBHostCIMessage) -> EndpointKey {
    EndpointKey {
        device: device_address(message),
        endpoint: ((message.data0 >> 8) & 0xff) as u8,
    }
}

fn rebase_frame(current: u64, low: u8) -> u64 {
    let candidate = (current & !0xff) | u64::from(low);
    if candidate < current {
        candidate + 256
    } else {
        candidate
    }
}

fn usbip_speed_to_ci(speed: u32) -> u32 {
    match speed {
        1 => 2,
        2 => 1,
        3 => 3,
        5 => 4,
        6 => 5,
        _ => 7,
    }
}

fn usbip_status_to_ci(status: i32) -> i32 {
    match status {
        0 => MESSAGE_STATUS_SUCCESS,
        -32 => MESSAGE_STATUS_STALL,
        -19 | -104 => MESSAGE_STATUS_OFFLINE,
        -110 => MESSAGE_STATUS_TIMEOUT,
        -12 => MESSAGE_STATUS_NO_RESOURCES,
        -22 => MESSAGE_STATUS_BAD_ARGUMENT,
        -1 => MESSAGE_STATUS_NOT_PERMITTED,
        -75 => MESSAGE_STATUS_OVERRUN,
        _ => MESSAGE_STATUS_ERROR,
    }
}

fn capabilities() -> [IOUSBHostCIMessage; 2] {
    [
        IOUSBHostCIMessage {
            control: MESSAGE_TYPE_CONTROLLER_CAPABILITIES
                | MESSAGE_CONTROL_NO_RESPONSE
                | MESSAGE_CONTROL_VALID
                | (1 << CAPABILITIES_PORT_COUNT_PHASE),
            data0: (1 << CAPABILITIES_TIMEOUT_PHASE) | (2 << CAPABILITIES_LATENCY_PHASE),
            data1: 0,
        },
        IOUSBHostCIMessage {
            control: MESSAGE_TYPE_PORT_CAPABILITIES
                | MESSAGE_CONTROL_NO_RESPONSE
                | MESSAGE_CONTROL_VALID
                | (1 << PORT_CAPABILITIES_PORT_NUMBER_PHASE),
            data0: ((907 / 8) << PORT_CAPABILITIES_MAX_POWER_PHASE),
            data1: 0,
        },
    ]
}

fn format_error(error: *mut NSError) -> String {
    let Some(error) = (unsafe { error.as_ref() }) else {
        return "IOUSBHost controller creation failed without NSError".into();
    };
    format!(
        "IOUSBHost controller unavailable: domain={} code={} description={}; required entitlement: com.apple.developer.usb.host-controller-interface",
        error.domain(),
        error.code(),
        error.localizedDescription()
    )
}

fn ensure_accepted(accepted: bool, error: *mut NSError, operation: &str) -> Result<(), String> {
    if accepted {
        Ok(())
    } else {
        Err(operation_error(error, operation))
    }
}

fn operation_error(error: *mut NSError, operation: &str) -> String {
    let Some(error) = (unsafe { error.as_ref() }) else {
        return format!("{operation} failed without NSError");
    };
    format!(
        "{operation} failed: domain={} code={} description={}",
        error.domain(),
        error.code(),
        error.localizedDescription()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebases_low_frame_byte_without_scheduling_in_the_past() {
        assert_eq!(rebase_frame(0x1234, 0x40), 0x1240);
        assert_eq!(rebase_frame(0x12f0, 0x20), 0x1320);
    }

    #[test]
    fn maps_linux_usb_speeds_to_apple_values() {
        assert_eq!(usbip_speed_to_ci(1), 2);
        assert_eq!(usbip_speed_to_ci(2), 1);
        assert_eq!(usbip_speed_to_ci(3), 3);
        assert_eq!(usbip_speed_to_ci(5), 4);
        assert_eq!(usbip_speed_to_ci(6), 5);
    }

    #[test]
    fn maps_usbip_errno_to_controller_status() {
        assert_eq!(usbip_status_to_ci(0), MESSAGE_STATUS_SUCCESS);
        assert_eq!(usbip_status_to_ci(-32), MESSAGE_STATUS_STALL);
        assert_eq!(usbip_status_to_ci(-19), MESSAGE_STATUS_OFFLINE);
        assert_eq!(usbip_status_to_ci(-110), MESSAGE_STATUS_TIMEOUT);
        assert_eq!(usbip_status_to_ci(-75), MESSAGE_STATUS_OVERRUN);
    }
}
