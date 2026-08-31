#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
mod exporter_probe;

#[cfg(target_os = "macos")]
mod iokit;

#[cfg(target_os = "macos")]
pub mod exporter;

#[cfg(target_os = "macos")]
pub use exporter_probe::probe_export;

#[cfg(target_os = "macos")]
pub use macos::{probe_controller, run_remote};

#[cfg(not(target_os = "macos"))]
pub fn probe_export() -> Result<String, String> {
    Err("IOUSBHostDevice is only available on macOS".into())
}

#[cfg(not(target_os = "macos"))]
pub fn probe_controller() -> Result<(), String> {
    Err("IOUSBHostControllerInterface is only available on macOS".into())
}

#[cfg(not(target_os = "macos"))]
pub fn run_remote<A: std::net::ToSocketAddrs>(
    _address: A,
    _bus_id: &str,
    _connect_timeout: std::time::Duration,
) -> Result<(), String> {
    Err("IOUSBHostControllerInterface is only available on macOS".into())
}
