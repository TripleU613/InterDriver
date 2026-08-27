//! Reports the endpoints a UDC actually provides. This is the check that sizes
//! hardware: a device can only be proxied if the UDC has enough endpoints of
//! the right type and direction.
//!
//! Usage: interdriver-eps-probe [udc-driver] [udc-device]   (defaults to dummy_hcd)

use interdriver_gadget_linux::raw;
use std::fs::OpenOptions;
use std::os::fd::AsFd;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let driver = args.first().map_or("dummy_udc", String::as_str);
    let device = args.get(1).map_or("dummy_udc.0", String::as_str);

    let file = match OpenOptions::new().read(true).write(true).open("/dev/raw-gadget") {
        Ok(file) => file,
        Err(error) => {
            eprintln!("open /dev/raw-gadget: {error} (needs root, and modprobe raw_gadget)");
            return ExitCode::FAILURE;
        }
    };
    let fd = file.as_fd();

    if let Err(error) = raw::init(fd, driver, device, raw::USB_SPEED_HIGH) {
        eprintln!("USB_RAW_IOCTL_INIT({driver}, {device}): {error}");
        return ExitCode::FAILURE;
    }
    if let Err(error) = raw::run(fd) {
        eprintln!("USB_RAW_IOCTL_RUN: {error}");
        return ExitCode::FAILURE;
    }
    let eps = match raw::eps_info(fd) {
        Ok(eps) => eps,
        Err(error) => {
            eprintln!("USB_RAW_IOCTL_EPS_INFO: {error}");
            return ExitCode::FAILURE;
        }
    };

    println!("udc\t{driver}/{device}");
    println!("endpoints\t{}", eps.len());
    let mut bulk_in = 0;
    let mut bulk_out = 0;
    for ep in &eps {
        let end = ep.name.iter().position(|b| *b == 0).unwrap_or(ep.name.len());
        let name = String::from_utf8_lossy(&ep.name[..end]);
        let mut kinds = Vec::new();
        if ep.caps.type_control() {
            kinds.push("control");
        }
        if ep.caps.type_iso() {
            kinds.push("iso");
        }
        if ep.caps.type_bulk() {
            kinds.push("bulk");
        }
        if ep.caps.type_int() {
            kinds.push("int");
        }
        let dir = match (ep.caps.dir_in(), ep.caps.dir_out()) {
            (true, true) => "in/out",
            (true, false) => "in",
            (false, true) => "out",
            (false, false) => "-",
        };
        if ep.caps.type_bulk() {
            if ep.caps.dir_in() {
                bulk_in += 1;
            }
            if ep.caps.dir_out() {
                bulk_out += 1;
            }
        }
        println!(
            "  {name}\taddr={}\tdir={dir}\ttypes={}\tmaxpacket={}",
            ep.addr,
            kinds.join(","),
            ep.limits.maxpacket_limit
        );
    }
    println!("bulk-capable\tin={bulk_in} out={bulk_out}");
    println!(
        "usbmux-proxy-viable\t{}",
        if bulk_in >= 1 && bulk_out >= 1 { "yes" } else { "NO" }
    );
    ExitCode::SUCCESS
}
