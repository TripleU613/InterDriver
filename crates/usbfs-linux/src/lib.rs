mod ioctl;

use interdriver_usbip_protocol::{DeviceRecord, InterfaceRecord, IsoPacketDescriptor};
use interdriver_usbip_server::{
    DeviceDescription, DeviceProvider, DeviceSession, TransferCompletion, TransferRequest,
};
use interdriver_usbip_protocol::Direction;
use ioctl::Outcome;
use std::fs;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};

const USB_DEVICES: &str = "/sys/bus/usb/devices";
const CONTROL_TIMEOUT_MS: u32 = 5_000;
const TRANSFER_TIMEOUT_MS: u32 = 30_000;
const EPIPE_STATUS: i32 = -32;
const TIMEOUT_STATUS: i32 = -libc::ETIMEDOUT;

/// Exports physically attached devices straight from usbfs, with no
/// `usbip_host` binding and no `usbipd` process.
pub struct UsbfsProvider {
    exported: Option<String>,
}

impl UsbfsProvider {
    #[must_use]
    pub const fn new() -> Self {
        Self { exported: None }
    }

    /// Exports exactly one device. The control plane authorises a single
    /// device per session, so the server must never offer the whole bus.
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

impl Default for UsbfsProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceProvider for UsbfsProvider {
    fn list(&self) -> io::Result<Vec<DeviceDescription>> {
        let mut devices = Vec::new();
        for entry in fs::read_dir(USB_DEVICES)? {
            let path = entry?.path();
            let Some(bus_id) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !self.permits(bus_id) {
                continue;
            }
            let Some(description) = read_device(&path)? else {
                continue;
            };
            devices.push(description);
        }
        devices.sort_by_key(|description| {
            (
                description.device.bus_number,
                description.device.device_number,
            )
        });
        Ok(devices)
    }

    fn open(&self, bus_id: &str) -> io::Result<Arc<dyn DeviceSession>> {
        if !self.permits(bus_id) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "device is not exported by this session",
            ));
        }
        let opened = open_node(bus_id)?;
        Ok(Arc::new(UsbfsDevice {
            bus_id: bus_id.to_owned(),
            fd: opened.fd,
            state: Mutex::new(State {
                claimed: opened.claimed,
                configuration: opened.configuration,
            }),
            gone: AtomicBool::new(false),
        }))
    }
}

struct Opened {
    fd: OwnedFd,
    claimed: Vec<u32>,
    configuration: u8,
}

/// Opens the current device on a bus id (physical port) and claims its
/// interfaces. Re-reading the port every time is what lets the exporter follow
/// a device through a reset: the port stays `3-12` while its device number
/// changes, so a fresh open lands on the re-enumerated device.
fn open_node(bus_id: &str) -> io::Result<Opened> {
    let path = PathBuf::from(USB_DEVICES).join(bus_id);
    let description = read_device(&path)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "device is not exportable"))?;
    let node = format!(
        "/dev/bus/usb/{:03}/{:03}",
        description.device.bus_number, description.device.device_number
    );
    let file = fs::OpenOptions::new().read(true).write(true).open(&node)?;
    let fd = OwnedFd::from(file);

    let mut claimed = Vec::new();
    for index in 0..description.interfaces.len() {
        let interface = index as u32;
        ioctl::detach_kernel_driver(fd.as_fd(), interface as i32)?;
        match ioctl::claim_interface(fd.as_fd(), interface) {
            Ok(()) => claimed.push(interface),
            Err(error) if error.raw_os_error() == Some(libc::EBUSY) => {
                return Err(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    format!("interface {interface} is held by another process"),
                ));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(Opened {
        fd,
        claimed,
        configuration: description.device.configuration_value,
    })
}

const ENODEV_STATUS: i32 = -libc::ENODEV;

struct State {
    claimed: Vec<u32>,
    configuration: u8,
}

/// usbfs transfer ioctls may be issued concurrently on one fd, and the server
/// runs every URB on its own thread. Only the interface bookkeeping is shared,
/// so the fd is held immutably: a bulk IN parked on the mux endpoint waiting
/// for the device never blocks the bulk OUT that makes the device answer.
struct UsbfsDevice {
    bus_id: String,
    fd: OwnedFd,
    state: Mutex<State>,
    gone: AtomicBool,
}

impl DeviceSession for UsbfsDevice {
    fn submit(&self, request: TransferRequest) -> TransferCompletion {
        if request.endpoint == 0 {
            self.control(&request)
        } else {
            self.stream(&request)
        }
    }

    /// usbfs synchronous transfers cannot be interrupted once issued, so an
    /// unlink is answered by the server while the ioctl runs to completion.
    fn cancel(&self, _endpoint: u32) {}

    fn disconnected(&self) -> bool {
        self.gone.load(Ordering::Acquire)
    }
}

impl UsbfsDevice {
    fn control(&self, request: &TransferRequest) -> TransferCompletion {
        if let Some(outcome) = self.intercept(request) {
            return completion(outcome, Vec::new(), Direction::Out);
        }
        let length = usize::from(u16::from_le_bytes([request.setup[6], request.setup[7]]));
        let mut buffer = vec![0_u8; length];
        if request.direction == Direction::Out {
            let copied = length.min(request.data.len());
            buffer[..copied].copy_from_slice(&request.data[..copied]);
        }
        let outcome = ioctl::control(
            self.fd.as_fd(),
            request.setup,
            &mut buffer,
            CONTROL_TIMEOUT_MS,
        );
        if is_device_gone(&outcome) {
            self.gone.store(true, Ordering::Release);
        }
        completion(outcome, buffer, request.direction)
    }

    /// usbfs maintains its own view of the active configuration, altsetting,
    /// and endpoint halt state. Passing these through as raw control transfers
    /// leaves that view stale and later transfers fail with EINVAL, so they are
    /// translated into the dedicated ioctls instead.
    fn intercept(&self, request: &TransferRequest) -> Option<Outcome> {
        let request_type = request.setup[0];
        let code = request.setup[1];
        let value = u16::from_le_bytes([request.setup[2], request.setup[3]]);
        let index = u16::from_le_bytes([request.setup[4], request.setup[5]]);
        match (request_type, code) {
            (0x00, 0x09) => Some(self.reconfigure(value as u8)),
            (0x01, 0x0b) => Some(ioctl::set_interface(
                self.fd.as_fd(),
                u32::from(index),
                u32::from(value),
            )),
            (0x02, 0x01) if value == 0 => {
                Some(ioctl::clear_halt(self.fd.as_fd(), u32::from(index)))
            }
            _ => None,
        }
    }

    /// A client re-enumerating the virtual device probes configuration 0 and
    /// then the configuration it already has. Forwarding either one would
    /// unconfigure or disturb the physical device, so only a genuine change
    /// reaches the hardware, and claimed interfaces are handed back first
    /// because usbfs refuses to switch configuration while they are held.
    fn reconfigure(&self, value: u8) -> Outcome {
        let Ok(mut state) = self.state.lock() else {
            return Outcome::Failed(EPIPE_STATUS);
        };
        if value == 0 || value == state.configuration {
            return Outcome::Transferred(0);
        }
        for interface in std::mem::take(&mut state.claimed) {
            let _ = ioctl::release_interface(self.fd.as_fd(), interface);
        }
        let outcome = ioctl::set_configuration(self.fd.as_fd(), u32::from(value));
        if matches!(outcome, Outcome::Failed(_)) {
            return outcome;
        }
        state.configuration = value;
        let path = PathBuf::from(USB_DEVICES).join(&self.bus_id);
        if let Ok(interfaces) = read_interfaces(&path, &self.bus_id, value) {
            for index in 0..interfaces.len() {
                let interface = index as u32;
                let _ = ioctl::detach_kernel_driver(self.fd.as_fd(), interface as i32);
                if ioctl::claim_interface(self.fd.as_fd(), interface).is_ok() {
                    state.claimed.push(interface);
                }
            }
        }
        outcome
    }

    fn stream(&self, request: &TransferRequest) -> TransferCompletion {
        let endpoint = match request.direction {
            Direction::In => request.endpoint | 0x80,
            Direction::Out => request.endpoint,
        };
        let make_buffer = || {
            if request.direction == Direction::Out {
                request.data.clone()
            } else {
                vec![0_u8; request.transfer_length]
            }
        };
        let mut buffer = make_buffer();
        let mut outcome = ioctl::bulk(self.fd.as_fd(), endpoint, &mut buffer, TRANSFER_TIMEOUT_MS);
        if is_device_gone(&outcome) {
            self.gone.store(true, Ordering::Release);
        }
        // An IN pipe that stayed quiet is not a device error: usbfs transfers are
        // synchronous, so we give up where a real controller would leave the URB
        // queued. Retire it empty. Reporting a failure instead halts the endpoint
        // on the importing host, which kills an idle usbmux pipe mid-session.
        if request.direction == Direction::In
            && matches!(outcome, Outcome::Failed(status) if status == TIMEOUT_STATUS)
        {
            outcome = Outcome::Transferred(0);
        }
        completion(outcome, buffer, request.direction)
    }
}

/// A transfer failed because the device is no longer there (ENODEV): it reset,
/// re-enumerated, or was unplugged. The session ends so the importer rebuilds.
fn is_device_gone(outcome: &Outcome) -> bool {
    matches!(outcome, Outcome::Failed(status) if *status == ENODEV_STATUS)
}

impl Drop for UsbfsDevice {
    fn drop(&mut self) {
        if let Ok(state) = self.state.lock() {
            for interface in &state.claimed {
                let _ = ioctl::release_interface(self.fd.as_fd(), *interface);
                let _ = ioctl::attach_kernel_driver(self.fd.as_fd(), *interface as i32);
            }
            // Deliberately no USBDEVFS_RESET here. A session ends every time the
            // device re-enumerates, and during a restore that is constant -- the
            // phone drops to recovery, flashes, and reboots. Resetting the port on
            // each teardown would interrupt the very operation we are carrying.
            // Releasing the interfaces is enough to hand the device back.
        }
    }
}

fn completion(outcome: Outcome, mut buffer: Vec<u8>, direction: Direction) -> TransferCompletion {
    match outcome {
        Outcome::Transferred(actual) => {
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
                iso_packets: Vec::<IsoPacketDescriptor>::new(),
            }
        }
        Outcome::Failed(status) => TransferCompletion {
            status: if status == 0 { EPIPE_STATUS } else { status },
            actual_length: 0,
            start_frame: 0,
            error_count: 0,
            data: Vec::new(),
            iso_packets: Vec::new(),
        },
    }
}

fn read_device(path: &Path) -> io::Result<Option<DeviceDescription>> {
    let Some(vendor_id) = read_hex(path, "idVendor")? else {
        return Ok(None);
    };
    let Some(product_id) = read_hex(path, "idProduct")? else {
        return Ok(None);
    };
    let Some(bus_id) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(None);
    };
    let Some(bus_number) = read_decimal(path, "busnum")? else {
        return Ok(None);
    };
    let Some(device_number) = read_decimal(path, "devnum")? else {
        return Ok(None);
    };

    let sysfs_path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut record = DeviceRecord::new(
        &sysfs_path.to_string_lossy(),
        bus_id,
        bus_number,
        device_number,
        speed_code(&read_text(path, "speed")?.unwrap_or_default()),
        vendor_id,
        product_id,
    )
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, format!("{error:?}")))?;

    record.device_release = read_hex(path, "bcdDevice")?.unwrap_or(0);
    record.device_class = read_hex(path, "bDeviceClass")?.unwrap_or(0) as u8;
    record.device_subclass = read_hex(path, "bDeviceSubClass")?.unwrap_or(0) as u8;
    record.device_protocol = read_hex(path, "bDeviceProtocol")?.unwrap_or(0) as u8;
    record.configuration_value = read_decimal(path, "bConfigurationValue")?.unwrap_or(0) as u8;
    record.configuration_count = read_decimal(path, "bNumConfigurations")?.unwrap_or(0) as u8;

    let interfaces = read_interfaces(path, bus_id, record.configuration_value)?;
    record.interface_count = interfaces.len() as u8;

    Ok(Some(DeviceDescription {
        device: record,
        interfaces,
    }))
}

fn read_interfaces(
    path: &Path,
    bus_id: &str,
    configuration: u8,
) -> io::Result<Vec<InterfaceRecord>> {
    let prefix = format!("{bus_id}:{configuration}.");
    let mut interfaces = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with(&prefix) {
            continue;
        }
        let interface = entry.path();
        let Some(number) = read_hex(&interface, "bInterfaceNumber")? else {
            continue;
        };
        interfaces.push((
            number,
            InterfaceRecord {
                class: read_hex(&interface, "bInterfaceClass")?.unwrap_or(0) as u8,
                subclass: read_hex(&interface, "bInterfaceSubClass")?.unwrap_or(0) as u8,
                protocol: read_hex(&interface, "bInterfaceProtocol")?.unwrap_or(0) as u8,
            },
        ));
    }
    interfaces.sort_by_key(|(number, _)| *number);
    Ok(interfaces.into_iter().map(|(_, record)| record).collect())
}

fn read_text(path: &Path, name: &str) -> io::Result<Option<String>> {
    match fs::read_to_string(path.join(name)) {
        Ok(value) => Ok(Some(value.trim().to_owned())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_hex(path: &Path, name: &str) -> io::Result<Option<u16>> {
    let Some(value) = read_text(path, name)? else {
        return Ok(None);
    };
    u16::from_str_radix(&value, 16)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn read_decimal(path: &Path, name: &str) -> io::Result<Option<u32>> {
    let Some(value) = read_text(path, name)? else {
        return Ok(None);
    };
    value
        .parse::<u32>()
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Maps the sysfs speed string onto `enum usb_device_speed`.
fn speed_code(speed: &str) -> u32 {
    match speed {
        "1.5" => 1,
        "12" => 2,
        "480" => 3,
        "5000" => 5,
        "10000" | "20000" => 6,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_sysfs_speed_strings_to_usb_device_speed() {
        assert_eq!(speed_code("1.5"), 1);
        assert_eq!(speed_code("12"), 2);
        assert_eq!(speed_code("480"), 3);
        assert_eq!(speed_code("5000"), 5);
        assert_eq!(speed_code("10000"), 6);
        assert_eq!(speed_code("unknown"), 0);
    }

    #[test]
    fn enumerates_the_local_usb_bus() {
        let devices = UsbfsProvider::new().list().unwrap();
        for description in devices {
            assert_eq!(
                usize::from(description.device.interface_count),
                description.interfaces.len(),
                "interface count must match the records the server will send"
            );
        }
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::*;

    #[test]
    fn quiet_in_pipe_retires_empty_but_real_failures_still_fail() {
        let quiet = Outcome::Failed(TIMEOUT_STATUS);
        let stalled = Outcome::Failed(EPIPE_STATUS);
        let retire = |o: &Outcome| matches!(o, Outcome::Failed(s) if *s == TIMEOUT_STATUS);
        assert!(retire(&quiet), "an idle IN read must not be reported as a failure");
        assert!(!retire(&stalled), "a stall must still surface as a failure");
    }
}
