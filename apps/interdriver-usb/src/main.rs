use interdriver_driver_core::PLATFORM_ADAPTERS;
use interdriver_usbip_session::{Session, TransferRequest};
use std::env;
use std::process::ExitCode;
use std::time::Duration;

const DEFAULT_PORT: u16 = 3240;
const DEVICE_DESCRIPTOR_LEN: usize = 18;

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    match args.as_slice() {
        [command] if command == "matrix" => {
            print_matrix();
            Ok(())
        }
        [platform, command] if platform == "linux" && command == "list" => linux_list(),
        [platform, command, bus_id] if platform == "linux" && command == "bind" => {
            linux_bind(bus_id)
        }
        [platform, command, bus_id] if platform == "linux" && command == "unbind" => {
            linux_unbind(bus_id)
        }
        [platform, command, bus_id] if platform == "linux" && command == "serve" => {
            linux_serve(bus_id, 3240)
        }
        [platform, command, bus_id, port] if platform == "linux" && command == "serve" => {
            let port = port
                .parse::<u16>()
                .map_err(|_| "port must be an integer from 1 to 65535".to_string())?;
            linux_serve(bus_id, port)
        }
        [platform, command, remote] if platform == "linux" && command == "remote-list" => {
            linux_remote_list(remote)
        }
        [platform, command, remote, bus_id] if platform == "linux" && command == "attach" => {
            linux_attach(remote, bus_id)
        }
        [platform, command, port] if platform == "linux" && command == "detach" => {
            linux_detach(port)
        }
        [platform, command] if platform == "windows" && command == "list" => windows_list(),
        [platform, command, bus_id] if platform == "windows" && command == "export" => {
            windows_export(bus_id, DEFAULT_PORT)
        }
        [platform, command, bus_id, port] if platform == "windows" && command == "export" => {
            let port = port
                .parse::<u16>()
                .map_err(|_| "port must be an integer from 1 to 65535".to_string())?;
            windows_export(bus_id, port)
        }
        [platform, command, remote, bus_id] if platform == "macos" && command == "attach" => {
            macos_attach(remote, bus_id)
        }
        [command, remote, bus_id] if command == "probe" => probe(remote, bus_id),
        [platform, command, bus_id] if platform == "linux" && command == "export" => {
            linux_export(bus_id, DEFAULT_PORT)
        }
        [platform, command, bus_id, port] if platform == "linux" && command == "export" => {
            let port = port
                .parse::<u16>()
                .map_err(|_| "port must be an integer from 1 to 65535".to_string())?;
            linux_export(bus_id, port)
        }
        [] => {
            print_help();
            Ok(())
        }
        _ => {
            print_help();
            Err("invalid command".into())
        }
    }
}

fn print_matrix() {
    println!("platform\texporter\timporter\tkernel boundary");
    for adapter in PLATFORM_ADAPTERS {
        println!(
            "{}\t{}\t{}\t{}",
            adapter.platform, adapter.exporter, adapter.importer, adapter.kernel_boundary
        );
    }
}

fn print_help() {
    println!("InterDriver cross-platform USB driver development utility");
    println!();
    println!("Usage:");
    println!("  interdriver-usb matrix");
    println!("  interdriver-usb linux list");
    println!("  interdriver-usb linux bind <bus-id>");
    println!("  interdriver-usb linux unbind <bus-id>");
    println!("  interdriver-usb linux serve <bus-id> [port]");
    println!("  interdriver-usb linux remote-list <host>");
    println!("  interdriver-usb linux attach <host> <bus-id>");
    println!("  interdriver-usb linux detach <port>");
    println!("  interdriver-usb windows list");
    println!("  interdriver-usb windows export <bus-id> [port]");
    println!("  interdriver-usb macos attach <host> <bus-id>");
    println!("  interdriver-usb linux export <bus-id> [port]");
    println!("  interdriver-usb probe <host> <bus-id>");
}

fn probe(remote: &str, bus_id: &str) -> Result<(), String> {
    let session = Session::connect((remote, DEFAULT_PORT), bus_id, Duration::from_secs(10))
        .map_err(|error| error.to_string())?;
    let mut request = TransferRequest::input(0, DEVICE_DESCRIPTOR_LEN);
    request.setup = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00];
    let completion = session
        .submit(request)
        .map_err(|error| error.to_string())?
        .wait_timeout(Duration::from_secs(10))
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    if completion.status != 0 {
        return Err(format!("device returned USB status {}", completion.status));
    }
    let bytes = completion.data;
    if bytes.len() < DEVICE_DESCRIPTOR_LEN || bytes[1] != 0x01 {
        return Err(format!(
            "expected an 18-byte device descriptor, received {} bytes: {bytes:02x?}",
            bytes.len()
        ));
    }
    println!("descriptor\t{} bytes", bytes.len());
    println!(
        "usb\t{:x}.{:02x}",
        bytes[3],
        bytes[2]
    );
    println!("class\t{}/{}/{}", bytes[4], bytes[5], bytes[6]);
    println!("max-packet-size\t{}", bytes[7]);
    println!(
        "id\t{:04x}:{:04x}",
        u16::from_le_bytes([bytes[8], bytes[9]]),
        u16::from_le_bytes([bytes[10], bytes[11]])
    );
    println!("configurations\t{}", bytes[17]);
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_export(bus_id: &str, port: u16) -> Result<(), String> {
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    let provider = Arc::new(interdriver_usbfs_linux::UsbfsProvider::for_device(bus_id));
    let listener =
        TcpListener::bind(("0.0.0.0", port)).map_err(|error| format!("bind {port}: {error}"))?;
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_flag(&shutdown)?;
    eprintln!("exporting {bus_id} from usbfs on port {port}; press Ctrl-C to stop");
    interdriver_usbip_server::serve(listener, provider, shutdown).map_err(|error| error.to_string())
}

#[cfg(target_os = "linux")]
fn signal_flag(flag: &std::sync::Arc<std::sync::atomic::AtomicBool>) -> Result<(), String> {
    for signal in [libc_sigint(), libc_sigterm()] {
        signal_hook::flag::register(signal, std::sync::Arc::clone(flag))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
const fn libc_sigint() -> i32 {
    signal_hook::consts::SIGINT
}

#[cfg(target_os = "linux")]
const fn libc_sigterm() -> i32 {
    signal_hook::consts::SIGTERM
}

#[cfg(not(target_os = "linux"))]
fn linux_export(_bus_id: &str, _port: u16) -> Result<(), String> {
    Err("Linux adapter is unavailable on this platform".into())
}

#[cfg(target_os = "linux")]
fn linux_list() -> Result<(), String> {
    let devices =
        interdriver_platform_linux::discover_apple_devices().map_err(|error| error.to_string())?;
    if devices.is_empty() {
        println!("No Apple USB devices connected.");
    }
    for device in devices {
        println!(
            "{}\t05ac:{:04x}\tserial={}\tdriver={}",
            device.bus_id,
            device.product_id,
            device.serial.as_deref().unwrap_or("unavailable"),
            device.driver.as_deref().unwrap_or("none")
        );
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn linux_list() -> Result<(), String> {
    Err("Linux adapter is unavailable on this platform".into())
}

#[cfg(target_os = "linux")]
fn linux_bind(bus_id: &str) -> Result<(), String> {
    interdriver_platform_linux::bind(bus_id).map_err(|error| error.to_string())
}

#[cfg(not(target_os = "linux"))]
fn linux_bind(_bus_id: &str) -> Result<(), String> {
    Err("Linux adapter is unavailable on this platform".into())
}

#[cfg(target_os = "linux")]
fn linux_unbind(bus_id: &str) -> Result<(), String> {
    interdriver_platform_linux::unbind(bus_id).map_err(|error| error.to_string())
}

#[cfg(not(target_os = "linux"))]
fn linux_unbind(_bus_id: &str) -> Result<(), String> {
    Err("Linux adapter is unavailable on this platform".into())
}

#[cfg(target_os = "linux")]
fn linux_serve(bus_id: &str, port: u16) -> Result<(), String> {
    interdriver_platform_linux::serve(bus_id, port).map_err(|error| error.to_string())
}

#[cfg(not(target_os = "linux"))]
fn linux_serve(_bus_id: &str, _port: u16) -> Result<(), String> {
    Err("Linux adapter is unavailable on this platform".into())
}

#[cfg(target_os = "linux")]
fn linux_remote_list(remote: &str) -> Result<(), String> {
    let output = interdriver_platform_linux::remote_list(remote).map_err(|error| error.to_string())?;
    print!("{output}");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn linux_remote_list(_remote: &str) -> Result<(), String> {
    Err("Linux adapter is unavailable on this platform".into())
}

#[cfg(target_os = "linux")]
fn linux_attach(remote: &str, bus_id: &str) -> Result<(), String> {
    interdriver_platform_linux::attach(remote, bus_id).map_err(|error| error.to_string())
}

#[cfg(not(target_os = "linux"))]
fn linux_attach(_remote: &str, _bus_id: &str) -> Result<(), String> {
    Err("Linux adapter is unavailable on this platform".into())
}

#[cfg(target_os = "linux")]
fn linux_detach(port: &str) -> Result<(), String> {
    interdriver_platform_linux::detach(port).map_err(|error| error.to_string())
}

#[cfg(not(target_os = "linux"))]
fn linux_detach(_port: &str) -> Result<(), String> {
    Err("Linux adapter is unavailable on this platform".into())
}

#[cfg(target_os = "windows")]
fn windows_list() -> Result<(), String> {
    use interdriver_usbip_server::DeviceProvider as _;

    let devices = interdriver_platform_windows::WinUsbProvider::new()
        .list()
        .map_err(|error| error.to_string())?;
    if devices.is_empty() {
        println!("No WinUSB-bound devices found.");
    }
    for description in devices {
        println!(
            "{}\t{:04x}:{:04x}\tinterfaces={}",
            description.device.bus_id().unwrap_or("invalid"),
            description.device.vendor_id,
            description.device.product_id,
            description.interfaces.len()
        );
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn windows_list() -> Result<(), String> {
    Err("Windows adapter is unavailable on this platform".into())
}

#[cfg(target_os = "windows")]
fn windows_export(bus_id: &str, port: u16) -> Result<(), String> {
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    let provider = Arc::new(interdriver_platform_windows::WinUsbProvider::for_device(bus_id));
    let listener =
        TcpListener::bind(("0.0.0.0", port)).map_err(|error| format!("bind {port}: {error}"))?;
    eprintln!("exporting {bus_id} over WinUSB on port {port}; press Ctrl-C to stop");
    interdriver_usbip_server::serve(listener, provider, Arc::new(AtomicBool::new(false)))
        .map_err(|error| error.to_string())
}

#[cfg(not(target_os = "windows"))]
fn windows_export(_bus_id: &str, _port: u16) -> Result<(), String> {
    Err("Windows adapter is unavailable on this platform".into())
}

#[cfg(target_os = "macos")]
fn macos_attach(remote: &str, bus_id: &str) -> Result<(), String> {
    interdriver_platform_macos::run_remote((remote, 3240), bus_id, std::time::Duration::from_secs(10))
}

#[cfg(not(target_os = "macos"))]
fn macos_attach(_remote: &str, _bus_id: &str) -> Result<(), String> {
    Err("macOS adapter is unavailable on this platform".into())
}
