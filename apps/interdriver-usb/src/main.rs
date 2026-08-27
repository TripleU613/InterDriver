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

fn run(mut args: Vec<String>) -> Result<(), String> {
    let ticket = take_ticket(&mut args)?;
    let broker_addr = take_flag(&mut args, "--broker");
    let exporter_id = take_flag(&mut args, "--exporter");
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
        [platform, command, inf] if platform == "windows" && command == "install-driver" => {
            windows_install_driver(inf)
        }
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
        [command, remote, bus_id] if command == "probe" => probe(remote, bus_id, ticket),
        [platform, command, remote, bus_id] if platform == "linux" && command == "gadget" => {
            linux_gadget(remote, bus_id, ticket, "dummy_udc", "dummy_udc.0")
        }
        [platform, command, remote, bus_id, driver, device]
            if platform == "linux" && command == "gadget" =>
        {
            linux_gadget(remote, bus_id, ticket, driver, device)
        }
        [platform, command, bus_id] if platform == "linux" && command == "export" => {
            linux_export(bus_id, DEFAULT_PORT, ticket, broker_addr, exporter_id)
        }
        [platform, command, bus_id, port] if platform == "linux" && command == "export" => {
            let port = port
                .parse::<u16>()
                .map_err(|_| "port must be an integer from 1 to 65535".to_string())?;
            linux_export(bus_id, port, ticket, broker_addr, exporter_id)
        }
        [command, port] if command == "broker" => {
            let port = port
                .parse::<u16>()
                .map_err(|_| "port must be an integer from 1 to 65535".to_string())?;
            run_broker(port)
        }
        [command, addr, device, exporter, importer] if command == "issue" => {
            run_issue(addr, device, exporter, importer, 300)
        }
        [command, addr, device, exporter, importer, ttl] if command == "issue" => {
            let ttl = ttl
                .parse::<u64>()
                .map_err(|_| "ttl must be an integer number of seconds".to_string())?;
            run_issue(addr, device, exporter, importer, ttl)
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
    println!("  interdriver-usb windows install-driver <inf-path>");
    println!("  interdriver-usb windows export <bus-id> [port]");
    println!("  interdriver-usb macos attach <host> <bus-id>");
    println!("  interdriver-usb linux export <bus-id> [port] [--ticket <hex>]");
    println!("  interdriver-usb probe <host> <bus-id> [--ticket <hex>]");
    println!("  interdriver-usb linux gadget <host> <bus-id> [udc-driver udc-device]");
    println!("  interdriver-usb broker <port>");
    println!("  interdriver-usb issue <broker-addr> <device> <exporter> <importer> [ttl]");
    println!("  interdriver-usb linux export <bus-id> [port] --broker <addr> --exporter <id>");
}

fn run_broker(port: u16) -> Result<(), String> {
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    let listener =
        TcpListener::bind(("0.0.0.0", port)).map_err(|error| format!("bind {port}: {error}"))?;
    let broker = Arc::new(Mutex::new(interdriver_control_plane::Broker::new()));
    eprintln!("control-plane broker listening on port {port}");
    interdriver_control_plane::net::serve(listener, broker).map_err(|error| error.to_string())
}

fn run_issue(
    addr: &str,
    device: &str,
    exporter: &str,
    importer: &str,
    ttl: u64,
) -> Result<(), String> {
    let token = interdriver_control_plane::net::issue(
        addr,
        device,
        exporter,
        importer,
        ttl,
        std::time::Duration::from_secs(5),
    )
    .map_err(|error| error.to_string())?;
    println!("{token}");
    Ok(())
}

fn take_flag(args: &mut Vec<String>, name: &str) -> Option<String> {
    let index = args.iter().position(|arg| arg == name)?;
    let value = args.get(index + 1).cloned();
    if value.is_some() {
        args.drain(index..=index + 1);
    }
    value
}

fn take_ticket(args: &mut Vec<String>) -> Result<Option<[u8; 32]>, String> {
    let Some(index) = args.iter().position(|arg| arg == "--ticket") else {
        return Ok(None);
    };
    let hex = args
        .get(index + 1)
        .ok_or_else(|| "--ticket needs a 64-character hex token".to_string())?
        .clone();
    let token = interdriver_control_plane::Token::from_hex(&hex)
        .ok_or_else(|| "--ticket must be 64 hex characters".to_string())?;
    args.drain(index..=index + 1);
    Ok(Some(token.as_bytes()))
}

fn probe(remote: &str, bus_id: &str, ticket: Option<[u8; 32]>) -> Result<(), String> {
    let session =
        Session::connect_with_ticket((remote, DEFAULT_PORT), bus_id, Duration::from_secs(10), ticket)
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
fn linux_gadget(
    remote: &str,
    bus_id: &str,
    ticket: Option<[u8; 32]>,
    udc_driver: &str,
    udc_device: &str,
) -> Result<(), String> {
    let imported = interdriver_usbip_client::import_with_ticket(
        (remote, DEFAULT_PORT),
        bus_id,
        Duration::from_secs(10),
        ticket,
    )
    .map_err(|error| error.to_string())?;
    let speed = imported.device.speed;
    let session = Session::from_imported(imported).map_err(|error| error.to_string())?;
    eprintln!("presenting {bus_id} on {udc_driver}/{udc_device}; Ctrl-C to stop");
    let mut bridge = interdriver_gadget_linux::bridge::Bridge::new(udc_driver, udc_device, session, speed)
        .map_err(|error| error.to_string())?;
    bridge.run().map_err(|error| error.to_string())
}

#[cfg(not(target_os = "linux"))]
fn linux_gadget(
    _remote: &str,
    _bus_id: &str,
    _ticket: Option<[u8; 32]>,
    _udc_driver: &str,
    _udc_device: &str,
) -> Result<(), String> {
    Err("raw-gadget is only available on Linux".into())
}

#[cfg(target_os = "linux")]
struct FixedTicket(interdriver_control_plane::Token);

#[cfg(target_os = "linux")]
impl interdriver_usbip_server::Authorizer for FixedTicket {
    fn authorize(&self, token: &[u8; 32]) -> bool {
        self.0 == interdriver_control_plane::Token::from_bytes(*token)
    }
}

#[cfg(target_os = "linux")]
struct BrokerTicket {
    broker: String,
    exporter: String,
}

#[cfg(target_os = "linux")]
impl interdriver_usbip_server::Authorizer for BrokerTicket {
    fn authorize(&self, token: &[u8; 32]) -> bool {
        use interdriver_control_plane::net::Outcome;
        let hex = interdriver_control_plane::Token::from_bytes(*token).to_hex();
        match interdriver_control_plane::net::redeem(
            &self.broker,
            &hex,
            &self.exporter,
            std::time::Duration::from_secs(5),
        ) {
            Outcome::Allow => true,
            Outcome::Deny(reason) => {
                eprintln!("ticket rejected by broker: {reason}");
                false
            }
            Outcome::Unreachable(error) => {
                eprintln!("broker unreachable, denying: {error}");
                false
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn linux_export(
    bus_id: &str,
    port: u16,
    ticket: Option<[u8; 32]>,
    broker: Option<String>,
    exporter: Option<String>,
) -> Result<(), String> {
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    let provider = Arc::new(interdriver_usbfs_linux::UsbfsProvider::for_device(bus_id));
    let listener =
        TcpListener::bind(("0.0.0.0", port)).map_err(|error| format!("bind {port}: {error}"))?;
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_flag(&shutdown)?;

    let authorizer: Option<Arc<dyn interdriver_usbip_server::Authorizer>> = match (broker, ticket) {
        (Some(broker), _) => {
            let exporter = exporter
                .ok_or_else(|| "--broker requires --exporter <id>".to_string())?;
            eprintln!(
                "exporting {bus_id} on port {port} (broker {broker}, exporter {exporter}); Ctrl-C to stop"
            );
            Some(Arc::new(BrokerTicket { broker, exporter }))
        }
        (None, Some(token)) => {
            eprintln!("exporting {bus_id} on port {port} (fixed ticket); Ctrl-C to stop");
            Some(Arc::new(FixedTicket(interdriver_control_plane::Token::from_bytes(token))))
        }
        (None, None) => {
            eprintln!("exporting {bus_id} from usbfs on port {port}; press Ctrl-C to stop");
            None
        }
    };
    interdriver_usbip_server::serve_authorized(listener, provider, shutdown, authorizer)
        .map_err(|error| error.to_string())
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
fn linux_export(
    _bus_id: &str,
    _port: u16,
    _ticket: Option<[u8; 32]>,
    _broker: Option<String>,
    _exporter: Option<String>,
) -> Result<(), String> {
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
fn windows_install_driver(inf: &str) -> Result<(), String> {
    let reboot = interdriver_platform_windows::install_root_device(r"root\interdriverude", inf)
        .map_err(|error| error.to_string())?;
    println!("virtual controller devnode created from {inf}");
    if reboot {
        println!("windows requests a reboot to finish");
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn windows_install_driver(_inf: &str) -> Result<(), String> {
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
