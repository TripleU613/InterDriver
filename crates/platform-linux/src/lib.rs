use interdriver_driver_core::APPLE_VENDOR_ID;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

const USB_DEVICES: &str = "/sys/bus/usb/devices";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppleDevice {
    pub bus_id: String,
    pub product_id: u16,
    pub serial: Option<String>,
    pub driver: Option<String>,
}

#[derive(Debug)]
pub enum Error {
    DeviceNotFound(String),
    Io(io::Error),
    NotLinux,
    NotApple(String),
    ToolFailed {
        program: &'static str,
        code: Option<i32>,
        stderr: String,
    },
    DaemonExited(Option<i32>),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeviceNotFound(bus_id) => write!(formatter, "USB device {bus_id} was not found"),
            Self::Io(error) => error.fmt(formatter),
            Self::NotLinux => formatter.write_str("the Linux USB adapter only runs on Linux"),
            Self::NotApple(bus_id) => {
                write!(formatter, "USB device {bus_id} is not an Apple device")
            }
            Self::ToolFailed {
                program,
                code,
                stderr,
            } => write!(
                formatter,
                "{program} failed with status {}: {}",
                code.map_or_else(|| "signal".to_owned(), |value| value.to_string()),
                stderr.trim()
            ),
            Self::DaemonExited(code) => write!(
                formatter,
                "usbipd exited unexpectedly with status {}",
                code.map_or_else(|| "signal".to_owned(), |value| value.to_string())
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub fn discover_apple_devices() -> Result<Vec<AppleDevice>, Error> {
    ensure_linux()?;
    let mut devices = Vec::new();
    for entry in fs::read_dir(USB_DEVICES)? {
        let entry = entry?;
        let path = entry.path();
        let Some(vendor_id) = read_hex_u16(&path.join("idVendor"))? else {
            continue;
        };
        if vendor_id != APPLE_VENDOR_ID {
            continue;
        }
        let Some(product_id) = read_hex_u16(&path.join("idProduct"))? else {
            continue;
        };
        let Some(bus_id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        devices.push(AppleDevice {
            bus_id,
            product_id,
            serial: read_optional_string(&path.join("serial"))?,
            driver: symlink_name(&path.join("driver"))?,
        });
    }
    devices.sort_by(|left, right| left.bus_id.cmp(&right.bus_id));
    Ok(devices)
}

pub fn bind(bus_id: &str) -> Result<(), Error> {
    ensure_apple(bus_id)?;
    run("modprobe", ["usbip_host"])?;
    run("usbip", ["bind", "--busid", bus_id])?;
    Ok(())
}

pub fn unbind(bus_id: &str) -> Result<(), Error> {
    ensure_apple(bus_id)?;
    run("usbip", ["unbind", "--busid", bus_id])?;
    Ok(())
}

pub fn attach(remote: &str, bus_id: &str) -> Result<(), Error> {
    ensure_linux()?;
    run("modprobe", ["vhci_hcd"])?;
    run("usbip", ["attach", "--remote", remote, "--busid", bus_id])?;
    Ok(())
}

pub fn detach(port: &str) -> Result<(), Error> {
    ensure_linux()?;
    run("usbip", ["detach", "--port", port])?;
    Ok(())
}

pub fn remote_list(remote: &str) -> Result<String, Error> {
    ensure_linux()?;
    let devices = interdriver_usbip_client::list(
        (remote, interdriver_usbip_client::DEFAULT_PORT),
        Duration::from_secs(5),
    )
    .map_err(|error| Error::Io(io::Error::other(error)))?;
    let mut output = String::new();
    for device in devices {
        use std::fmt::Write as _;
        let bus_id = device.device.bus_id().unwrap_or("invalid");
        let _ = writeln!(
            output,
            "{bus_id}\t{:04x}:{:04x}\tinterfaces={}",
            device.device.vendor_id,
            device.device.product_id,
            device.interfaces.len()
        );
    }
    Ok(output)
}

pub fn serve(bus_id: &str, port: u16) -> Result<(), Error> {
    ensure_linux()?;
    run("modprobe", ["usbip_host"])?;
    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stop))?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stop))?;

    let mut daemon = if TcpStream::connect(("127.0.0.1", port)).is_ok() {
        None
    } else {
        Some(start_usbipd(port)?)
    };
    let mut exported = false;
    let mut present = false;

    while !stop.load(Ordering::Acquire) {
        if let Some(child) = daemon.as_mut()
            && let Some(status) = child.try_wait()?
        {
            cleanup_export(bus_id, exported, &mut daemon);
            return Err(Error::DaemonExited(status.code()));
        }

        match ensure_apple(bus_id) {
            Ok(device) => {
                if !present {
                    eprintln!(
                        "Apple device {} appeared (05ac:{:04x}); exporting",
                        device.bus_id, device.product_id
                    );
                    present = true;
                }
                if device.driver.as_deref() != Some("usbip-host")
                    && let Err(error) = bind(bus_id)
                {
                    cleanup_export(bus_id, exported, &mut daemon);
                    return Err(error);
                }
                exported = true;
            }
            Err(Error::DeviceNotFound(_)) => {
                if present {
                    eprintln!("Apple device {bus_id} disconnected; waiting for re-enumeration");
                    present = false;
                    exported = false;
                }
            }
            Err(error) => {
                cleanup_export(bus_id, exported, &mut daemon);
                return Err(error);
            }
        }
        thread::sleep(Duration::from_millis(250));
    }

    cleanup_export(bus_id, exported, &mut daemon);
    Ok(())
}

fn start_usbipd(port: u16) -> Result<Child, Error> {
    let port = port.to_string();
    let child = Command::new("usbipd")
        .args(["--debug", "--tcp-port", &port])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    Ok(child)
}

fn cleanup_export(bus_id: &str, exported: bool, daemon: &mut Option<Child>) {
    if exported {
        let _ = unbind(bus_id);
    }
    if let Some(mut child) = daemon.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn ensure_linux() -> Result<(), Error> {
    if cfg!(target_os = "linux") {
        Ok(())
    } else {
        Err(Error::NotLinux)
    }
}

fn ensure_apple(bus_id: &str) -> Result<AppleDevice, Error> {
    let path = PathBuf::from(USB_DEVICES).join(bus_id);
    let Some(vendor_id) = read_hex_u16(&path.join("idVendor"))? else {
        return Err(Error::DeviceNotFound(bus_id.to_owned()));
    };
    if vendor_id != APPLE_VENDOR_ID {
        return Err(Error::NotApple(bus_id.to_owned()));
    }
    discover_apple_devices()?
        .into_iter()
        .find(|device| device.bus_id == bus_id)
        .ok_or_else(|| Error::DeviceNotFound(bus_id.to_owned()))
}

fn run<I, S>(program: &'static str, args: I) -> Result<Output, Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new(program).args(args).output()?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(Error::ToolFailed {
            program,
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

fn read_hex_u16(path: &Path) -> Result<Option<u16>, Error> {
    match fs::read_to_string(path) {
        Ok(value) => u16::from_str_radix(value.trim(), 16)
            .map(Some)
            .map_err(|error| Error::Io(io::Error::new(io::ErrorKind::InvalidData, error))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::Io(error)),
    }
}

fn read_optional_string(path: &Path) -> Result<Option<String>, Error> {
    match fs::read_to_string(path) {
        Ok(value) => {
            let value = value.trim().to_owned();
            Ok((!value.is_empty()).then_some(value))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::Io(error)),
    }
}

fn symlink_name(path: &Path) -> Result<Option<String>, Error> {
    match fs::read_link(path) {
        Ok(target) => Ok(target
            .file_name()
            .and_then(OsStr::to_str)
            .map(str::to_owned)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::Io(error)),
    }
}
