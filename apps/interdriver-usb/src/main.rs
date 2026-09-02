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
        [platform, command, hardware_id, inf]
            if platform == "windows" && command == "force-driver" =>
        {
            windows_force_driver(hardware_id, inf)
        }
        [platform, command, remote, bus_id] if platform == "windows" && command == "attach" => {
            windows_attach(remote, bus_id, ticket)
        }
        [platform, command] if platform == "windows" && command == "detach" => windows_detach(),
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
            macos_attach(remote, bus_id, ticket)
        }
        [platform, command] if platform == "macos" && command == "list" => macos_list(),
        [platform, command, bus_id] if platform == "macos" && command == "export" => {
            macos_export(bus_id, DEFAULT_PORT)
        }
        [platform, command, bus_id, port] if platform == "macos" && command == "export" => {
            let port = port
                .parse::<u16>()
                .map_err(|_| "port must be an integer from 1 to 65535".to_string())?;
            macos_export(bus_id, port)
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
        [command, sub, dir] if command == "tls" && sub == "init" => tls_init(dir),
        [command, sub, port, forward, dir] if command == "tls" && sub == "serve" => {
            let port = port.parse::<u16>().map_err(|_| "bad port".to_string())?;
            tls_serve(port, forward, dir)
        }
        [command, sub, port, remote, name, dir] if command == "tls" && sub == "forward" => {
            let port = port.parse::<u16>().map_err(|_| "bad port".to_string())?;
            tls_forward(port, remote, name, dir)
        }
        [command, port] if command == "broker" => {
            let port = port
                .parse::<u16>()
                .map_err(|_| "port must be an integer from 1 to 65535".to_string())?;
            run_broker(port)
        }
        [command, addr] if command == "sessions" => run_sessions(addr),
        [command, addr, token] if command == "revoke" => run_revoke(addr, token),
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
    println!("  interdriver-usb windows force-driver <hardware-id> <inf-path>");
    println!("  interdriver-usb windows attach <host> <bus-id> [--ticket <hex>]");
    println!("  interdriver-usb windows detach");
    println!("  interdriver-usb windows export <bus-id> [port]");
    println!("  interdriver-usb macos list");
    println!("  interdriver-usb macos export <bus-id> [port]");
    println!("  interdriver-usb macos attach <host> <bus-id> [--ticket <hex>]");
    println!("  interdriver-usb linux export <bus-id> [port] [--ticket <hex>]");
    println!("  interdriver-usb probe <host> <bus-id> [--ticket <hex>]");
    println!("  interdriver-usb linux gadget <host> <bus-id> [udc-driver udc-device]");
    println!("  interdriver-usb tls init <dir>");
    println!("  interdriver-usb tls serve <listen-port> <forward-host:port> <cert-dir>");
    println!("  interdriver-usb tls forward <local-port> <remote-host:port> <server-name> <cert-dir>");
    println!("  interdriver-usb broker <port>");
    println!("  interdriver-usb issue <broker-addr> <device> <exporter> <importer> [ttl]");
    println!("  interdriver-usb sessions <broker-addr>");
    println!("  interdriver-usb revoke <broker-addr> <token>");
    println!("  interdriver-usb linux export <bus-id> [port] --broker <addr> --exporter <id>");
}

fn tls_init(dir: &str) -> Result<(), String> {
    use std::path::Path;
    let dir = Path::new(dir);
    let ca = interdriver_tls::Authority::generate("InterDriver USB CA").map_err(|e| e.to_string())?;
    ca.save(dir, "interdriver").map_err(|e| e.to_string())?;
    ca.issue_to_files("exporter", dir).map_err(|e| e.to_string())?;
    ca.issue_to_files("importer", dir).map_err(|e| e.to_string())?;
    println!("wrote interdriver-ca.pem, exporter.pem/.key, importer.pem/.key to {}", dir.display());
    Ok(())
}

fn load_role(dir: &std::path::Path, role: &str) -> Result<interdriver_tls::Identity, String> {
    interdriver_tls::load_identity(&dir.join(format!("{role}.pem")), &dir.join(format!("{role}.key")))
        .map_err(|e| e.to_string())
}

fn tls_serve(port: u16, forward: &str, dir: &str) -> Result<(), String> {
    use std::net::TcpListener;
    use std::path::Path;
    let dir = Path::new(dir);
    let identity = load_role(dir, "exporter")?;
    let ca = interdriver_tls::load_ca(&dir.join("interdriver-ca.pem")).map_err(|e| e.to_string())?;
    let config = interdriver_tls::server_config(&identity, &ca).map_err(|e| e.to_string())?;
    let listener = TcpListener::bind(("0.0.0.0", port)).map_err(|e| format!("bind {port}: {e}"))?;
    let forward = forward.to_owned();
    eprintln!("TLS tunnel: mTLS on :{port} -> {forward}; press Ctrl-C to stop");
    interdriver_tls::tunnel::serve(listener, config, forward).map_err(|e| e.to_string())
}

fn tls_forward(port: u16, remote: &str, server_name: &str, dir: &str) -> Result<(), String> {
    use std::net::TcpListener;
    use std::path::Path;
    let dir = Path::new(dir);
    let identity = load_role(dir, "importer")?;
    let ca = interdriver_tls::load_ca(&dir.join("interdriver-ca.pem")).map_err(|e| e.to_string())?;
    let config = interdriver_tls::client_config(&identity, &ca).map_err(|e| e.to_string())?;
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("bind {port}: {e}"))?;
    let remote = remote.to_owned();
    eprintln!("TLS tunnel: 127.0.0.1:{port} -> mTLS -> {remote}; attach/probe to 127.0.0.1:{port}");
    interdriver_tls::tunnel::forward(listener, config, server_name.to_owned(), remote).map_err(|e| e.to_string())
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

fn run_sessions(addr: &str) -> Result<(), String> {
    let rows = interdriver_control_plane::net::list_sessions(addr, std::time::Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    if rows.is_empty() {
        println!("no sessions");
    }
    println!("token\tdevice\texporter\timporter\tstate\texpires");
    for row in rows {
        let fields: Vec<&str> = row.split('|').collect();
        if fields.len() == 6 {
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}",
                &fields[0][..fields[0].len().min(12)],
                fields[1],
                fields[2],
                fields[3],
                fields[4],
                fields[5]
            );
        }
    }
    Ok(())
}

fn run_revoke(addr: &str, token: &str) -> Result<(), String> {
    let device = interdriver_control_plane::net::revoke(addr, token, std::time::Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    println!("revoked; released device {device}");
    Ok(())
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

    /// Poll the broker; if the session stops being ACTIVE (an operator revoked
    /// it, or it was released), shut the live connection down so the device is
    /// freed mid-session.
    fn watch(&self, token: &[u8; 32], connection: std::net::TcpStream) {
        let broker = self.broker.clone();
        let hex = interdriver_control_plane::Token::from_bytes(*token).to_hex();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3));
                match interdriver_control_plane::net::check(
                    &broker,
                    &hex,
                    std::time::Duration::from_secs(5),
                ) {
                    Ok(state) if state == "ACTIVE" || state == "ISSUED" => {}
                    Ok(state) => {
                        eprintln!("session {state}; dropping the connection and releasing the device");
                        let _ = connection.shutdown(std::net::Shutdown::Both);
                        return;
                    }
                    Err(_) => {}
                }
            }
        });
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

#[cfg(target_os = "windows")]
fn windows_force_driver(hardware_id: &str, inf: &str) -> Result<(), String> {
    let reboot = interdriver_platform_windows::force_driver(hardware_id, inf)
        .map_err(|error| format!("force-driver {hardware_id}: {error}"))?;
    println!("forced {hardware_id} onto {inf}");
    if reboot {
        println!("REBOOT REQUIRED before the interface can be opened");
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn windows_force_driver(_hardware_id: &str, _inf: &str) -> Result<(), String> {
    Err("Windows adapter is unavailable on this platform".into())
}

#[cfg(not(target_os = "windows"))]
fn windows_install_driver(_inf: &str) -> Result<(), String> {
    Err("Windows adapter is unavailable on this platform".into())
}

/// Imports a remote device and presents it on the virtual controller. The
/// descriptors handed to the driver are the real device's own bytes, fetched
/// over USB/IP, so Windows enumerates exactly what the far end reports.
#[cfg(target_os = "windows")]
fn windows_attach(remote: &str, bus_id: &str, ticket: Option<[u8; 32]>) -> Result<(), String> {
    use interdriver_platform_windows::ude;

    let imported = interdriver_usbip_client::import_with_ticket(
        (remote, DEFAULT_PORT),
        bus_id,
        Duration::from_secs(10),
        ticket,
    )
    .map_err(|error| error.to_string())?;
    let speed = match imported.device.speed {
        1 | 2 => ude::SPEED_FULL,
        5 | 6 => ude::SPEED_SUPER,
        _ => ude::SPEED_HIGH,
    };
    let session = Session::from_imported(imported).map_err(|error| error.to_string())?;

    let device_descriptor = fetch_descriptor(&session, 1, 0, DEVICE_DESCRIPTOR_LEN as u16)?;
    let header = fetch_descriptor(&session, 2, 0, 9)?;
    if header.len() < 4 {
        return Err("configuration descriptor header is too short".into());
    }
    let total = u16::from_le_bytes([header[2], header[3]]);
    let config_descriptor = fetch_descriptor(&session, 2, 0, total)?;

    ude::plug_in(None, speed, &device_descriptor, &config_descriptor)
        .map_err(|error| error.to_string())?;
    println!(
        "attached {bus_id} from {remote}: {} byte device descriptor, {} byte configuration",
        device_descriptor.len(),
        config_descriptor.len()
    );

    // Answer the host's endpoint-0 traffic from the real device until detached.
    let controller = ude::Controller::open_default().map_err(|error| error.to_string())?;
    println!("serving control transfers; press Ctrl-C to detach");
    loop {
        if session.is_closed() {
            eprintln!("exporter closed the session; detaching");
            break;
        }
        let urb = match controller.fetch() {
            Ok(urb) => urb,
            Err(error) => {
                eprintln!("fetch failed: {error}");
                break;
            }
        };
        // Endpoint zero carries a setup packet; every other endpoint is a
        // plain data transfer whose direction comes from the address.
        let mut request = if urb.endpoint == 0 {
            let device_to_host = urb.setup[0] & 0x80 != 0;
            let length = usize::from(u16::from_le_bytes([urb.setup[6], urb.setup[7]]));
            let mut request = if device_to_host {
                TransferRequest::input(0, length)
            } else {
                TransferRequest::output(0, urb.data.clone())
            };
            request.setup = urb.setup;
            request
        } else {
            let number = u32::from(urb.endpoint & 0x0f);
            if urb.endpoint & 0x80 != 0 {
                TransferRequest::input(number, urb.transfer_length as usize)
            } else {
                TransferRequest::output(number, urb.data.clone())
            }
        };
        let _ = &mut request;
        // An idle interrupt or bulk endpoint legitimately has nothing to
        // report until the device produces data, so it gets the same budget the
        // exporter allows rather than the short control timeout.
        let budget = if urb.endpoint == 0 {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(35)
        };
        let outcome = session
            .submit(request)
            .map_err(|error| error.to_string())
            .and_then(|pending| {
                pending
                    .wait_timeout(budget)
                    .map_err(|error| error.to_string())?
                    .map_err(|error| error.to_string())
            });
        match outcome {
            Ok(completion) if completion.status == 0 => {
                let _ = controller.complete(urb.id, 0, &completion.data);
            }
            Ok(completion) => {
                let _ = controller.complete(urb.id, completion.status, &[]);
            }
            Err(error) if session.is_closed() => {
                let _ = controller.complete(urb.id, -1, &[]);
                eprintln!("exporter went away mid-transfer: {error}");
                break;
            }
            Err(error) => {
                // Endpoint zero is named by its setup packet; other endpoints
                // by address, since their setup is zeroed.
                if urb.endpoint == 0 {
                    eprintln!(
                        "control {:#04x}/{:#04x} failed: {error}",
                        urb.setup[0], urb.setup[1]
                    );
                } else {
                    eprintln!("endpoint {:#04x} failed: {error}", urb.endpoint);
                }
                let _ = controller.complete(urb.id, -1, &[]);
            }
        }
    }
    // Leaving the device plugged in with nothing serving it strands the host,
    // so the emulated device is removed on the way out. The driver also does
    // this from its file-cleanup handler if this process dies outright.
    if let Err(error) = ude::unplug(None) {
        eprintln!("detach failed: {error}");
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn fetch_descriptor(
    session: &Session,
    kind: u16,
    index: u8,
    length: u16,
) -> Result<Vec<u8>, String> {
    let value = (kind << 8) | u16::from(index);
    let value_bytes = value.to_le_bytes();
    let length_bytes = length.to_le_bytes();
    let mut request = TransferRequest::input(0, usize::from(length));
    request.setup = [
        0x80,
        0x06,
        value_bytes[0],
        value_bytes[1],
        0,
        0,
        length_bytes[0],
        length_bytes[1],
    ];
    let completion = session
        .submit(request)
        .map_err(|error| error.to_string())?
        .wait_timeout(Duration::from_secs(10))
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())?;
    if completion.status != 0 {
        return Err(format!("descriptor request returned {}", completion.status));
    }
    Ok(completion.data)
}

#[cfg(target_os = "windows")]
fn windows_detach() -> Result<(), String> {
    interdriver_platform_windows::ude::unplug(None).map_err(|error| error.to_string())?;
    println!("detached");
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn windows_attach(_r: &str, _b: &str, _t: Option<[u8; 32]>) -> Result<(), String> {
    Err("Windows adapter is unavailable on this platform".into())
}

#[cfg(not(target_os = "windows"))]
fn windows_detach() -> Result<(), String> {
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
fn macos_list() -> Result<(), String> {
    use interdriver_usbip_server::DeviceProvider as _;

    let devices = interdriver_platform_macos::exporter::MacProvider::new()
        .list()
        .map_err(|error| error.to_string())?;
    if devices.is_empty() {
        println!("No Apple USB devices available for export.");
    }
    for description in devices {
        println!(
            "{}\t{:04x}:{:04x}\tinterfaces={}\tconfig={}",
            description.device.bus_id().unwrap_or("invalid"),
            description.device.vendor_id,
            description.device.product_id,
            description.interfaces.len(),
            description.device.configuration_value
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_export(bus_id: &str, port: u16) -> Result<(), String> {
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    let provider = Arc::new(interdriver_platform_macos::exporter::MacProvider::for_device(bus_id));
    let listener =
        TcpListener::bind(("0.0.0.0", port)).map_err(|error| format!("bind {port}: {error}"))?;
    let shutdown = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&shutdown))
            .map_err(|error| error.to_string())?;
    }
    eprintln!("exporting {bus_id} from IOUSBHost on port {port}; press Ctrl-C to stop");
    interdriver_usbip_server::serve(listener, provider, shutdown).map_err(|error| error.to_string())
}

#[cfg(not(target_os = "macos"))]
fn macos_list() -> Result<(), String> {
    Err("macOS adapter is unavailable on this platform".into())
}

#[cfg(not(target_os = "macos"))]
fn macos_export(_bus_id: &str, _port: u16) -> Result<(), String> {
    Err("macOS adapter is unavailable on this platform".into())
}

#[cfg(target_os = "macos")]
fn macos_attach(remote: &str, bus_id: &str, ticket: Option<[u8; 32]>) -> Result<(), String> {
    // Short: this is retried in a loop while a device is expected back on the
    // port, so a stalled connect must not become a blackout the host notices.
    interdriver_platform_macos::run_remote_with_ticket(
        (remote, 3240),
        bus_id,
        std::time::Duration::from_secs(3),
        ticket,
    )
}

#[cfg(not(target_os = "macos"))]
fn macos_attach(_remote: &str, _bus_id: &str, _ticket: Option<[u8; 32]>) -> Result<(), String> {
    Err("macOS adapter is unavailable on this platform".into())
}
