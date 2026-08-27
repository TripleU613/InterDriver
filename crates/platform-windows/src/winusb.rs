use crate::enumerate;
use interdriver_usbip_protocol::{DeviceRecord, Direction, InterfaceRecord};
use interdriver_usbip_server::{
    DeviceDescription, DeviceProvider, DeviceSession, TransferCompletion, TransferRequest,
};
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use windows_sys::Win32::Devices::Usb::{
    USB_DEVICE_DESCRIPTOR_TYPE, USB_INTERFACE_DESCRIPTOR, WINUSB_INTERFACE_HANDLE,
    WINUSB_PIPE_INFORMATION, WINUSB_SETUP_PACKET, WinUsb_ControlTransfer, WinUsb_Free,
    WinUsb_GetAssociatedInterface, WinUsb_GetDescriptor, WinUsb_Initialize, WinUsb_QueryPipe,
    WinUsb_QueryInterfaceSettings, WinUsb_ReadPipe, WinUsb_WritePipe,
};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const DEVICE_DESCRIPTOR_LEN: usize = 18;
const EPIPE_STATUS: i32 = -32;

/// Exports a physically attached device that is bound to WinUSB.
///
/// Windows has no bus IDs, so a device is addressed by the hub and port that
/// SetupAPI reports, formatted the way USB/IP expects.
pub struct WinUsbProvider {
    exported: Option<String>,
}

impl WinUsbProvider {
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

impl Default for WinUsbProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceProvider for WinUsbProvider {
    fn list(&self) -> io::Result<Vec<DeviceDescription>> {
        let mut devices = Vec::new();
        for found in enumerate::list_devices()? {
            if !self.permits(&found.bus_id) {
                continue;
            }
            let device = match OpenDevice::open(&found.path) {
                Ok(device) => device,
                Err(error) => {
                    eprintln!("skip {} [{}]: {error}", found.bus_id, found.class);
                    continue;
                }
            };
            let descriptor = device.device_descriptor().unwrap_or([0; DEVICE_DESCRIPTOR_LEN]);
            let interfaces = device.interface_records();

            let mut record = DeviceRecord::new(
                &found.path,
                &found.bus_id,
                0,
                u32::from(found.interface_number),
                0,
                found.vendor_id,
                found.product_id,
            )
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, format!("{error:?}")))?;
            record.device_release = u16::from_le_bytes([descriptor[12], descriptor[13]]);
            record.device_class = descriptor[4];
            record.device_subclass = descriptor[5];
            record.device_protocol = descriptor[6];
            record.configuration_value = 1;
            record.configuration_count = descriptor[17].max(1);
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
        let found = enumerate::list_devices()?
            .into_iter()
            .find(|device| device.bus_id == bus_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "device is not attached"))?;
        Ok(Arc::new(OpenDevice::open(&found.path)?))
    }
}

struct OpenDevice {
    file: HANDLE,
    interfaces: Vec<WINUSB_INTERFACE_HANDLE>,
    pipes: HashMap<u8, Pipe>,
}

#[derive(Clone, Copy)]
struct Pipe {
    interface: usize,
    id: u8,
}

// WinUSB serialises per-pipe access internally, and each transfer here targets
// one pipe, so the handles may cross threads with the server's per-URB workers.
unsafe impl Send for OpenDevice {}
unsafe impl Sync for OpenDevice {}

impl OpenDevice {
    fn open(path: &str) -> io::Result<Self> {
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let file = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if file == INVALID_HANDLE_VALUE {
            return Err(io::Error::new(
                io::Error::last_os_error().kind(),
                format!("CreateFileW({path}): {}", io::Error::last_os_error()),
            ));
        }

        let mut primary: WINUSB_INTERFACE_HANDLE = std::ptr::null_mut();
        if unsafe { WinUsb_Initialize(file, &mut primary) } == 0 {
            let error = io::Error::last_os_error();
            unsafe { CloseHandle(file) };
            return Err(io::Error::new(
                error.kind(),
                format!("WinUsb_Initialize: {error}"),
            ));
        }

        let mut interfaces = vec![primary];
        let mut index = 0_u8;
        loop {
            let mut associated: WINUSB_INTERFACE_HANDLE = std::ptr::null_mut();
            if unsafe { WinUsb_GetAssociatedInterface(primary, index, &mut associated) } == 0 {
                break;
            }
            interfaces.push(associated);
            index += 1;
        }

        let mut device = Self {
            file,
            interfaces,
            pipes: HashMap::new(),
        };
        device.map_pipes();
        Ok(device)
    }

    fn map_pipes(&mut self) {
        for (index, handle) in self.interfaces.iter().enumerate() {
            let Some(descriptor) = query_interface(*handle) else {
                continue;
            };
            for pipe_index in 0..descriptor.bNumEndpoints {
                let mut information: WINUSB_PIPE_INFORMATION = unsafe { std::mem::zeroed() };
                let ok = unsafe {
                    WinUsb_QueryPipe(*handle, 0, pipe_index, &mut information)
                };
                if ok == 0 {
                    continue;
                }
                self.pipes.insert(
                    information.PipeId,
                    Pipe {
                        interface: index,
                        id: information.PipeId,
                    },
                );
            }
        }
    }

    fn device_descriptor(&self) -> Option<[u8; DEVICE_DESCRIPTOR_LEN]> {
        let mut buffer = [0_u8; DEVICE_DESCRIPTOR_LEN];
        let mut transferred = 0_u32;
        let ok = unsafe {
            WinUsb_GetDescriptor(
                self.interfaces[0],
                USB_DEVICE_DESCRIPTOR_TYPE as u8,
                0,
                0,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                &mut transferred,
            )
        };
        (ok != 0 && transferred as usize == DEVICE_DESCRIPTOR_LEN).then_some(buffer)
    }

    fn interface_records(&self) -> Vec<InterfaceRecord> {
        self.interfaces
            .iter()
            .filter_map(|handle| query_interface(*handle))
            .map(|descriptor| InterfaceRecord {
                class: descriptor.bInterfaceClass,
                subclass: descriptor.bInterfaceSubClass,
                protocol: descriptor.bInterfaceProtocol,
            })
            .collect()
    }

    fn control(&self, request: &TransferRequest) -> TransferCompletion {
        let length = u16::from_le_bytes([request.setup[6], request.setup[7]]);
        let packet = WINUSB_SETUP_PACKET {
            RequestType: request.setup[0],
            Request: request.setup[1],
            Value: u16::from_le_bytes([request.setup[2], request.setup[3]]),
            Index: u16::from_le_bytes([request.setup[4], request.setup[5]]),
            Length: length,
        };
        let mut buffer = vec![0_u8; usize::from(length)];
        if request.direction == Direction::Out {
            let copied = buffer.len().min(request.data.len());
            buffer[..copied].copy_from_slice(&request.data[..copied]);
        }
        let mut transferred = 0_u32;
        let ok = unsafe {
            WinUsb_ControlTransfer(
                self.interfaces[0],
                packet,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                &mut transferred,
                std::ptr::null_mut(),
            )
        };
        finish(ok, transferred, buffer, request.direction)
    }

    fn stream(&self, request: &TransferRequest) -> TransferCompletion {
        let address = match request.direction {
            Direction::In => (request.endpoint as u8) | 0x80,
            Direction::Out => request.endpoint as u8,
        };
        let Some(pipe) = self.pipes.get(&address).copied() else {
            return failed(EPIPE_STATUS);
        };
        let handle = self.interfaces[pipe.interface];
        let mut transferred = 0_u32;
        if request.direction == Direction::In {
            let mut buffer = vec![0_u8; request.transfer_length];
            let ok = unsafe {
                WinUsb_ReadPipe(
                    handle,
                    pipe.id,
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    &mut transferred,
                    std::ptr::null_mut(),
                )
            };
            finish(ok, transferred, buffer, Direction::In)
        } else {
            let mut buffer = request.data.clone();
            let ok = unsafe {
                WinUsb_WritePipe(
                    handle,
                    pipe.id,
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    &mut transferred,
                    std::ptr::null_mut(),
                )
            };
            finish(ok, transferred, buffer, Direction::Out)
        }
    }
}

impl DeviceSession for OpenDevice {
    fn submit(&self, request: TransferRequest) -> TransferCompletion {
        if request.endpoint == 0 {
            self.control(&request)
        } else {
            self.stream(&request)
        }
    }

    /// WinUSB cancels per pipe rather than per transfer, and the server already
    /// answers the unlink, so an in-flight read is left to complete or time out.
    fn cancel(&self, _endpoint: u32) {}
}

impl Drop for OpenDevice {
    fn drop(&mut self) {
        for handle in self.interfaces.drain(..) {
            unsafe { WinUsb_Free(handle) };
        }
        unsafe { CloseHandle(self.file) };
    }
}

fn query_interface(handle: WINUSB_INTERFACE_HANDLE) -> Option<USB_INTERFACE_DESCRIPTOR> {
    let mut descriptor: USB_INTERFACE_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let ok = unsafe { WinUsb_QueryInterfaceSettings(handle, 0, &mut descriptor) };
    (ok != 0).then_some(descriptor)
}

fn finish(
    ok: i32,
    transferred: u32,
    mut buffer: Vec<u8>,
    direction: Direction,
) -> TransferCompletion {
    if ok == 0 {
        let status = io::Error::last_os_error()
            .raw_os_error()
            .map_or(EPIPE_STATUS, |code| -code);
        return failed(status);
    }
    let actual = transferred as usize;
    if direction == Direction::In {
        buffer.truncate(actual);
    } else {
        buffer.clear();
    }
    TransferCompletion {
        status: 0,
        actual_length: actual,
        start_frame: 0,
        error_count: 0,
        data: buffer,
        iso_packets: Vec::new(),
    }
}

fn failed(status: i32) -> TransferCompletion {
    TransferCompletion {
        status,
        actual_length: 0,
        start_frame: 0,
        error_count: 0,
        data: Vec::new(),
        iso_packets: Vec::new(),
    }
}
