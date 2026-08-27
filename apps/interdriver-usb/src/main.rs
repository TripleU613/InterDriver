use interdriver_driver_core::PLATFORM_ADAPTERS;
use std::env;
use std::process::ExitCode;

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
        [platform, command, remote, bus_id] if platform == "macos" && command == "attach" => {
            macos_attach(remote, bus_id)
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
    println!("  interdriver-usb macos attach <host> <bus-id>");
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

#[cfg(target_os = "macos")]
fn macos_attach(remote: &str, bus_id: &str) -> Result<(), String> {
    interdriver_platform_macos::run_remote((remote, 3240), bus_id, std::time::Duration::from_secs(10))
}

#[cfg(not(target_os = "macos"))]
fn macos_attach(_remote: &str, _bus_id: &str) -> Result<(), String> {
    Err("macOS adapter is unavailable on this platform".into())
}
