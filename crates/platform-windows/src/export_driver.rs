use crate::enumerate;
use interdriver_usbip_protocol::{DeviceRecord, Direction};
use interdriver_usbip_server::{
    DeviceDescription, DeviceProvider, DeviceSession, TransferCompletion, TransferRequest,
};
use std::collections::HashMap;
use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::core::GUID;

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const DEVICE_TYPE: u32 = 0x8124;
const METHOD_BUFFERED: u32 = 0;
const FILE_READ_ACCESS: u32 = 1;
const FILE_WRITE_ACCESS: u32 = 2;
const INFO_LEN: usize = 36;
const TRANSFER_HEADER_LEN: usize = 36;
const MAX_TRANSFER: usize = 1024 * 1024;
const PROTOCOL_VERSION: u32 = 1;

const fn ctl_code(function: u32, access: u32) -> u32 {
    (DEVICE_TYPE << 16) | (access << 14) | (function << 2) | METHOD_BUFFERED
}

const IOCTL_GET_INFO: u32 = ctl_code(0x800, FILE_READ_ACCESS);
const IOCTL_TRANSFER: u32 = ctl_code(0x801, FILE_READ_ACCESS | FILE_WRITE_ACCESS);
const IOCTL_ABORT_ENDPOINT: u32 = ctl_code(0x802, FILE_WRITE_ACCESS);

const STATUS_IO_TIMEOUT: u32 = 0xc000_00b5;
const STATUS_DEVICE_NOT_CONNECTED: u32 = 0xc000_009d;
const STATUS_NO_SUCH_DEVICE: u32 = 0xc000_000e;
const STATUS_CANCELLED: u32 = 0xc000_0120;

const GUID_DEVINTERFACE_INTERDRIVER_EXPORT: GUID = GUID {
    data1: 0x3a44_f2d1,
    data2: 0x7acb,
    data3: 0x4b49,
    data4: [0x91, 0xd5, 0x73, 0xac, 0x92, 0xd4, 0x27, 0x11],
};

#[derive(Clone)]
struct FoundDevice {
    bus_id: String,
    path: String,
    location_path: String,
    inf_path: String,
    info: DeviceInfo,
}

#[derive(Clone, Copy)]
struct DeviceInfo {
    speed: u32,
    descriptor: [u8; 18],
}

pub struct ExportDriverProvider {
    exported: Option<String>,
}

impl ExportDriverProvider {
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

    pub fn monitor_reenumeration(&self) -> io::Result<()> {
        let bus_id = self.exported.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "export bus id is required")
        })?;
        let found = find_devices()?
            .into_iter()
            .find(|device| device.bus_id == *bus_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "device is not attached"))?;
        if found.inf_path.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "export driver INF path is unavailable",
            ));
        }
        let mut inf_path =
            PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into()));
        inf_path.push("INF");
        inf_path.push(found.inf_path);
        let inf_path = inf_path.to_string_lossy().into_owned();
        let bus_id = bus_id.clone();
        std::thread::spawn(move || monitor_reenumeration(bus_id, inf_path));
        Ok(())
    }
}

impl Default for ExportDriverProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceProvider for ExportDriverProvider {
    fn list(&self) -> io::Result<Vec<DeviceDescription>> {
        let mut descriptions = Vec::new();
        for found in find_devices()? {
            if !self.permits(&found.bus_id) {
                continue;
            }
            let descriptor = found.info.descriptor;
            let mut record = DeviceRecord::new(
                &found.path,
                &found.bus_id,
                bus_number(&found.location_path),
                device_number(&found.path),
                usbip_speed(found.info.speed),
                u16::from_le_bytes([descriptor[8], descriptor[9]]),
                u16::from_le_bytes([descriptor[10], descriptor[11]]),
            )
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, format!("{error:?}")))?;
            record.device_release = u16::from_le_bytes([descriptor[12], descriptor[13]]);
            record.device_class = descriptor[4];
            record.device_subclass = descriptor[5];
            record.device_protocol = descriptor[6];
            record.configuration_value = 0;
            record.configuration_count = descriptor[17];
            record.interface_count = 0;
            descriptions.push(DeviceDescription {
                device: record,
                interfaces: Vec::new(),
            });
        }
        Ok(descriptions)
    }

    fn open(&self, bus_id: &str) -> io::Result<Arc<dyn DeviceSession>> {
        if !self.permits(bus_id) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "device is not exported by this session",
            ));
        }
        let found = find_devices()?
            .into_iter()
            .find(|device| device.bus_id == bus_id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "device is not attached"))?;
        Ok(Arc::new(DriverSession {
            path: found.path,
            bus_id: found.bus_id,
            cancels: Mutex::new(HashMap::new()),
        }))
    }
}

struct DriverSession {
    path: String,
    bus_id: String,
    cancels: Mutex<HashMap<u32, u64>>,
}

impl DeviceSession for DriverSession {
    fn submit(&self, request: TransferRequest) -> TransferCompletion {
        let generation = self.cancel_generation(request.endpoint);
        let mut result = transfer(&self.path, &request);
        while request.endpoint != 0
            && request.direction == Direction::In
            && matches!(&result, Ok(completion) if completion.status == -110)
            && !self.disconnected()
            && self.cancel_generation(request.endpoint) == generation
        {
            result = transfer(&self.path, &request);
        }
        let completion = match result {
            Ok(completion) => completion,
            Err(error) => {
                eprintln!(
                    "{}: endpoint {} ioctl failed: {error}",
                    self.bus_id, request.endpoint
                );
                failed(-error.raw_os_error().unwrap_or(5))
            }
        };
        if request.endpoint == 0
            && (request.setup[1] == 9 || request.setup[1] == 11 || completion.status != 0)
        {
            eprintln!(
                "{}: control {:02x} request {:02x} value {:04x} index {:04x} -> status {} length {}",
                self.bus_id,
                request.setup[0],
                request.setup[1],
                u16::from_le_bytes([request.setup[2], request.setup[3]]),
                u16::from_le_bytes([request.setup[4], request.setup[5]]),
                completion.status,
                completion.actual_length
            );
        } else if completion.status != 0 {
            eprintln!(
                "{}: endpoint {} {:?} -> status {} length {}",
                self.bus_id,
                request.endpoint,
                request.direction,
                completion.status,
                completion.actual_length
            );
        }
        completion
    }

    fn cancel(&self, endpoint: u32) {
        if let Ok(mut cancels) = self.cancels.lock() {
            *cancels.entry(endpoint).or_insert(0) += 1;
        }
        let _ = abort_endpoint(&self.path, endpoint);
    }

    fn disconnected(&self) -> bool {
        match find_devices() {
            Ok(devices) => {
                let disconnected = !devices
                    .iter()
                    .any(|device| device.bus_id == self.bus_id && device.path == self.path);
                if disconnected {
                    eprintln!("{}: exporter driver interface disappeared", self.bus_id);
                }
                disconnected
            }
            Err(error) => {
                eprintln!("{}: exporter presence check failed: {error}", self.bus_id);
                false
            }
        }
    }
}

impl DriverSession {
    fn cancel_generation(&self, endpoint: u32) -> u64 {
        self.cancels
            .lock()
            .ok()
            .and_then(|cancels| cancels.get(&endpoint).copied())
            .unwrap_or(0)
    }
}

fn find_devices() -> io::Result<Vec<FoundDevice>> {
    enumerate::list_driver_interfaces(&GUID_DEVINTERFACE_INTERDRIVER_EXPORT)?
        .into_iter()
        .map(|interface| {
            let info = get_info(&interface.path)?;
            Ok(FoundDevice {
                bus_id: stable_bus_id(&interface.location_path),
                path: interface.path,
                location_path: interface.location_path,
                inf_path: interface.inf_path,
                info,
            })
        })
        .collect()
}

fn monitor_reenumeration(bus_id: String, inf_path: String) {
    const APPLE_MODES: [&str; 3] = [
        r"USB\VID_05AC&PID_12A8",
        r"USB\VID_05AC&PID_1281",
        r"USB\VID_05AC&PID_1227",
    ];
    let mut missing_since = None;
    let mut last_attempt = None;
    loop {
        let present = find_devices()
            .map(|devices| devices.iter().any(|device| device.bus_id == bus_id))
            .unwrap_or(true);
        if present {
            missing_since = None;
            last_attempt = None;
        } else {
            let now = std::time::Instant::now();
            if missing_since.is_none() {
                eprintln!("{bus_id}: device changed mode; waiting for the exporter driver");
                missing_since = Some(now);
            }
            let grace_elapsed =
                missing_since.is_some_and(|missing| now.duration_since(missing).as_secs() >= 2);
            let retry_due =
                last_attempt.is_none_or(|attempt| now.duration_since(attempt).as_secs() >= 3);
            if grace_elapsed && retry_due {
                for hardware_id in APPLE_MODES {
                    let _ = crate::install::force_driver(hardware_id, &inf_path);
                }
                last_attempt = Some(now);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

fn get_info(path: &str) -> io::Result<DeviceInfo> {
    let handle = open(path)?;
    let mut bytes = vec![0_u8; INFO_LEN];
    ioctl(&handle, IOCTL_GET_INFO, &mut bytes, 0, INFO_LEN)?;
    if read_u32(&bytes, 0) != INFO_LEN as u32 || read_u32(&bytes, 4) != PROTOCOL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported InterDriver exporter driver protocol",
        ));
    }
    let mut descriptor = [0_u8; 18];
    descriptor.copy_from_slice(&bytes[16..34]);
    Ok(DeviceInfo {
        speed: read_u32(&bytes, 8),
        descriptor,
    })
}

fn transfer(path: &str, request: &TransferRequest) -> io::Result<TransferCompletion> {
    if request.transfer_length > MAX_TRANSFER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "transfer exceeds exporter driver limit",
        ));
    }
    let device_to_host = request.direction == Direction::In;
    let payload_len = if device_to_host {
        request.transfer_length
    } else {
        request.data.len()
    };
    let mut bytes = vec![0_u8; TRANSFER_HEADER_LEN + payload_len];
    let input_len = if device_to_host {
        TRANSFER_HEADER_LEN
    } else {
        bytes.len()
    };
    write_u32(&mut bytes, 0, input_len as u32);
    let address = if request.endpoint == 0 {
        0
    } else if device_to_host {
        request.endpoint | 0x80
    } else {
        request.endpoint
    };
    write_u32(&mut bytes, 4, address);
    write_u32(&mut bytes, 8, request.transfer_length as u32);
    write_u32(
        &mut bytes,
        12,
        if request.endpoint == 0 { 5_000 } else { 30_000 },
    );
    bytes[24..32].copy_from_slice(&request.setup);
    if !device_to_host {
        bytes[TRANSFER_HEADER_LEN..TRANSFER_HEADER_LEN + request.data.len()]
            .copy_from_slice(&request.data);
    }

    let handle = open(path)?;
    let returned = ioctl(
        &handle,
        IOCTL_TRANSFER,
        &mut bytes,
        input_len,
        TRANSFER_HEADER_LEN
            + if device_to_host {
                request.transfer_length
            } else {
                0
            },
    )?;
    if returned < TRANSFER_HEADER_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "short transfer result",
        ));
    }
    let status = read_u32(&bytes, 16);
    let actual = read_u32(&bytes, 20) as usize;
    if actual > request.transfer_length
        || (device_to_host && TRANSFER_HEADER_LEN + actual > returned)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid transfer result length",
        ));
    }
    let data = if device_to_host {
        bytes[TRANSFER_HEADER_LEN..TRANSFER_HEADER_LEN + actual].to_vec()
    } else {
        Vec::new()
    };
    Ok(TransferCompletion {
        status: map_status(status),
        actual_length: if status == 0 { actual } else { 0 },
        start_frame: 0,
        error_count: 0,
        data,
        iso_packets: Vec::new(),
    })
}

fn abort_endpoint(path: &str, endpoint: u32) -> io::Result<()> {
    let handle = open(path)?;
    let mut bytes = vec![0_u8; 8];
    write_u32(&mut bytes, 0, 8);
    write_u32(&mut bytes, 4, endpoint & 0x0f);
    ioctl(&handle, IOCTL_ABORT_ENDPOINT, &mut bytes, 8, 0)?;
    Ok(())
}

fn map_status(status: u32) -> i32 {
    match status {
        0 => 0,
        STATUS_IO_TIMEOUT => -110,
        STATUS_DEVICE_NOT_CONNECTED | STATUS_NO_SUCH_DEVICE => -19,
        STATUS_CANCELLED => -104,
        _ => -32,
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

fn stable_bus_id(location: &str) -> String {
    format!("win-{:016x}", fnv1a(location.as_bytes()))
}

fn bus_number(location: &str) -> u32 {
    (fnv1a(location.as_bytes()) >> 32) as u32
}

fn device_number(path: &str) -> u32 {
    fnv1a(path.as_bytes()) as u32
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn usbip_speed(speed: u32) -> u32 {
    match speed {
        1 => 2,
        3 => 5,
        _ => 3,
    }
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn open(path: &str) -> io::Result<Handle> {
    let wide: Vec<u16> = std::ffi::OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(Handle(handle))
}

fn ioctl(
    handle: &Handle,
    code: u32,
    buffer: &mut [u8],
    input_len: usize,
    output_len: usize,
) -> io::Result<usize> {
    let mut returned = 0_u32;
    let ok = unsafe {
        DeviceIoControl(
            handle.0,
            code,
            if input_len == 0 {
                std::ptr::null()
            } else {
                buffer.as_ptr().cast::<c_void>()
            },
            input_len as u32,
            if output_len == 0 {
                std::ptr::null_mut()
            } else {
                buffer.as_mut_ptr().cast::<c_void>()
            },
            output_len as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(returned as usize)
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_id_is_stable_and_short() {
        let first = stable_bus_id("PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(5)");
        let second = stable_bus_id("PCIROOT(0)#PCI(1400)#USBROOT(0)#USB(5)");
        assert_eq!(first, second);
        assert!(first.len() <= 31);
    }
}
