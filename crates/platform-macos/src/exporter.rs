//! macOS exporter: shares a locally attached Apple device over USB/IP.
//!
//! Device capture through `IOUSBHostDevice` needs no entitlement and no root,
//! unlike the virtual controller the importer uses. Selectors are invoked
//! through `objc_msgSend` directly because objc2's encoding check rejects the
//! framework's struct-pointer returns and by-value request struct.

use crate::iokit;
use interdriver_usbip_protocol::{DeviceRecord, Direction, InterfaceRecord, IsoPacketDescriptor};
use interdriver_usbip_server::{
    DeviceDescription, DeviceProvider, DeviceSession, TransferCompletion, TransferRequest,
};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, Sel};
use objc2::{AnyThread, class, extern_class, msg_send, sel};
use objc2_foundation::NSError;
use std::ffi::c_void;
use std::io;
use std::sync::{Arc, Mutex};

const APPLE_VENDOR_ID: u16 = 0x05ac;
const DESCRIPTOR_DEVICE: usize = 1;
const DESCRIPTOR_CONFIGURATION: usize = 2;
const CONTROL_TIMEOUT_SECONDS: f64 = 5.0;
const EPIPE_STATUS: i32 = -32;

extern_class!(
    #[unsafe(super(NSObject))]
    #[name = "IOUSBHostDevice"]
    pub struct IOUSBHostDevice;
);

extern_class!(
    #[unsafe(super(NSObject))]
    #[name = "NSMutableData"]
    pub struct NSMutableData;
);

unsafe extern "C" {
    fn objc_msgSend();
}

/// Opens the device for exclusive ownership. The designated initialiser
/// establishes exclusive access, which is what exporting requires.
fn open_device(service: u32) -> Option<Retained<IOUSBHostDevice>> {
    unsafe {
        let mut error: *mut NSError = std::ptr::null_mut();
        msg_send![
            IOUSBHostDevice::alloc(),
            initWithIOService: service,
            queue: std::ptr::null_mut::<AnyObject>(),
            error: &mut error,
            interestHandler: None::<&block2::DynBlock<dyn Fn()>>,
        ]
    }
}

fn read_descriptor(
    device: &IOUSBHostDevice,
    kind: usize,
    index: usize,
    want: usize,
) -> Option<Vec<u8>> {
    type Send = unsafe extern "C" fn(
        *const IOUSBHostDevice,
        Sel,
        usize,
        *mut usize,
        usize,
        usize,
        *mut *mut NSError,
    ) -> *const u8;
    unsafe {
        let send: Send = std::mem::transmute(objc_msgSend as *const c_void);
        let mut length = want;
        let mut error: *mut NSError = std::ptr::null_mut();
        let bytes = send(
            device,
            sel!(descriptorWithType:length:index:languageID:error:),
            kind,
            &mut length,
            index,
            0,
            &mut error,
        );
        if bytes.is_null() || length == 0 || length > 4096 {
            return None;
        }
        Some(std::slice::from_raw_parts(bytes, length).to_vec())
    }
}

/// Reads the configuration whose `bConfigurationValue` matches, since index and
/// value differ on a multi-configuration device such as an iPhone.
fn read_configuration(device: &IOUSBHostDevice, value: u8) -> Option<Vec<u8>> {
    for index in 0..8 {
        let header = read_descriptor(device, DESCRIPTOR_CONFIGURATION, index, 9)?;
        if header.len() < 9 {
            return None;
        }
        if header[5] != value {
            continue;
        }
        let total = usize::from(u16::from_le_bytes([header[2], header[3]]));
        return read_descriptor(device, DESCRIPTOR_CONFIGURATION, index, total);
    }
    None
}

/// Walks a configuration descriptor for its interface records.
fn interfaces_of(configuration: &[u8]) -> Vec<InterfaceRecord> {
    let mut found = Vec::new();
    let mut offset = 0;
    while offset + 2 <= configuration.len() {
        let length = usize::from(configuration[offset]);
        if length < 2 || offset + length > configuration.len() {
            break;
        }
        if configuration[offset + 1] == 4 && length >= 9 && configuration[offset + 3] == 0 {
            found.push(InterfaceRecord {
                class: configuration[offset + 5],
                subclass: configuration[offset + 6],
                protocol: configuration[offset + 7],
            });
        }
        offset += length;
    }
    found
}

pub struct MacProvider {
    exported: Option<String>,
}

impl MacProvider {
    #[must_use]
    pub const fn new() -> Self {
        Self { exported: None }
    }

    #[must_use]
    pub fn for_device(bus_id: &str) -> Self {
        Self {
            exported: Some(bus_id.to_owned()),
        }
    }

    fn permits(&self, bus_id: &str) -> bool {
        self.exported
            .as_ref()
            .is_none_or(|allowed| allowed == bus_id)
    }
}

impl Default for MacProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// macOS has no USB/IP bus IDs; the IOKit location ID is stable while the
/// device stays on its port, which is what the session core matches on.
fn bus_id_for(location: u32) -> String {
    format!("{}-{}", (location >> 24) & 0xff, (location >> 20) & 0x0f)
}

impl DeviceProvider for MacProvider {
    fn list(&self) -> io::Result<Vec<DeviceDescription>> {
        let mut devices = Vec::new();
        for service in iokit::apple_devices()? {
            let location = iokit::read_u32_property(service.get(), "locationID").unwrap_or(0);
            let bus_id = bus_id_for(location);
            if !self.permits(&bus_id) {
                continue;
            }
            let Some(device) = open_device(service.get()) else {
                continue;
            };
            let Some(descriptor) = read_descriptor(&device, DESCRIPTOR_DEVICE, 0, 18) else {
                unsafe { let _: () = msg_send![&*device, destroy]; }
                continue;
            };
            let configuration_value =
                iokit::read_u32_property(service.get(), "bConfigurationValue").unwrap_or(1) as u8;
            let configuration = read_configuration(&device, configuration_value);
            unsafe { let _: () = msg_send![&*device, destroy]; }

            if descriptor.len() < 18 {
                continue;
            }
            let interfaces = configuration.as_deref().map(interfaces_of).unwrap_or_default();
            let mut record = DeviceRecord::new(
                &format!("locationID:{location:08x}"),
                &bus_id,
                (location >> 24) & 0xff,
                (location >> 20) & 0x0f,
                3,
                u16::from_le_bytes([descriptor[8], descriptor[9]]),
                u16::from_le_bytes([descriptor[10], descriptor[11]]),
            )
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, format!("{error:?}")))?;
            record.device_release = u16::from_le_bytes([descriptor[12], descriptor[13]]);
            record.device_class = descriptor[4];
            record.device_subclass = descriptor[5];
            record.device_protocol = descriptor[6];
            record.configuration_value = configuration_value;
            record.configuration_count = descriptor[17];
            record.interface_count = interfaces.len() as u8;
            devices.push(DeviceDescription {
                device: record,
                interfaces,
            });
        }
        Ok(devices)
    }

    fn open(&self, bus_id: &str) -> io::Result<Arc<dyn DeviceSession>> {
        if !self.permits(bus_id) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "device is not exported by this session",
            ));
        }
        for service in iokit::apple_devices()? {
            let location = iokit::read_u32_property(service.get(), "locationID").unwrap_or(0);
            if bus_id_for(location) != bus_id {
                continue;
            }
            let device = open_device(service.get()).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    "another client already owns this device",
                )
            })?;
            return Ok(Arc::new(MacDevice {
                device: Mutex::new(device),
            }));
        }
        Err(io::Error::new(io::ErrorKind::NotFound, "device not attached"))
    }
}

struct MacDevice {
    device: Mutex<Retained<IOUSBHostDevice>>,
}

// The framework serialises requests on the object's own queue.
unsafe impl Send for MacDevice {}
unsafe impl Sync for MacDevice {}

impl MacDevice {
    /// `IOUSBDeviceRequest` is the eight-byte setup packet and is passed by
    /// value, which fits one integer register on both architectures.
    fn control(&self, request: &TransferRequest) -> TransferCompletion {
        type Send = unsafe extern "C" fn(
            *const IOUSBHostDevice,
            Sel,
            u64,
            *mut NSMutableData,
            *mut usize,
            f64,
            *mut *mut NSError,
        ) -> bool;

        let length = usize::from(u16::from_le_bytes([request.setup[6], request.setup[7]]));
        let packed = u64::from_le_bytes(request.setup);
        let Ok(device) = self.device.lock() else {
            return failed(EPIPE_STATUS);
        };

        unsafe {
            let data: *mut NSMutableData = if length == 0 {
                std::ptr::null_mut()
            } else {
                msg_send![class!(NSMutableData), dataWithLength: length]
            };
            if length != 0 && data.is_null() {
                return failed(EPIPE_STATUS);
            }
            if request.direction == Direction::Out && length != 0 {
                let raw: *mut c_void = msg_send![&*data, mutableBytes];
                if !raw.is_null() {
                    let copy = length.min(request.data.len());
                    std::ptr::copy_nonoverlapping(request.data.as_ptr(), raw.cast::<u8>(), copy);
                }
            }
            let send: Send = std::mem::transmute(objc_msgSend as *const c_void);
            let mut transferred: usize = 0;
            let mut error: *mut NSError = std::ptr::null_mut();
            let ok = send(
                &**device,
                sel!(sendDeviceRequest:data:bytesTransferred:completionTimeout:error:),
                packed,
                data,
                &mut transferred,
                CONTROL_TIMEOUT_SECONDS,
                &mut error,
            );
            if !ok {
                return failed(EPIPE_STATUS);
            }
            let mut out = Vec::new();
            if request.direction == Direction::In && !data.is_null() && transferred > 0 {
                let raw: *const c_void = msg_send![&*data, bytes];
                if !raw.is_null() {
                    out = std::slice::from_raw_parts(raw.cast::<u8>(), transferred.min(length))
                        .to_vec();
                }
            }
            TransferCompletion {
                status: 0,
                actual_length: transferred,
                start_frame: 0,
                error_count: 0,
                data: out,
                iso_packets: Vec::new(),
            }
        }
    }
}

impl DeviceSession for MacDevice {
    fn submit(&self, request: TransferRequest) -> TransferCompletion {
        if request.endpoint == 0 {
            self.control(&request)
        } else {
            // Endpoint traffic needs IOUSBHostInterface pipes, which are not
            // wired yet; report a stall rather than pretending it succeeded.
            failed(EPIPE_STATUS)
        }
    }

    fn cancel(&self, _endpoint: u32) {}
}

impl Drop for MacDevice {
    fn drop(&mut self) {
        if let Ok(device) = self.device.lock() {
            unsafe { let _: () = msg_send![&**device, destroy]; }
        }
    }
}

fn failed(status: i32) -> TransferCompletion {
    TransferCompletion {
        status,
        actual_length: 0,
        start_frame: 0,
        error_count: 0,
        data: Vec::new(),
        iso_packets: Vec::<IsoPacketDescriptor>::new(),
    }
}
