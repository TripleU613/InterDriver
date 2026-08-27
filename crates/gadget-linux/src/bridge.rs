//! Presents a remote USB/IP device to a real USB host through `raw-gadget`.
//!
//! The bridge synthesises nothing. Every control request the host issues on
//! endpoint 0 is forwarded verbatim to the real device over USB/IP and the
//! device's own answer is returned, so descriptors, string indices, and
//! configuration values are byte-identical to the physical device.

use crate::descriptors::{self, Endpoint};
use crate::raw;
use interdriver_usbip_protocol::Direction;
use interdriver_usbip_session::{Session, TransferRequest};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

const DESCRIPTOR_TYPE_CONFIGURATION: u16 = 2;
const MAX_CONFIGURATIONS: u8 = 8;

const TRANSFER_TIMEOUT: Duration = Duration::from_secs(5);
const ENDPOINT_TIMEOUT: Duration = Duration::from_secs(60);

/// Tags an io error with the raw-gadget call that produced it, so a failure
/// names the step instead of only the errno.
fn label(step: &'static str) -> impl Fn(io::Error) -> io::Error {
    move |error| io::Error::new(error.kind(), format!("{step}: {error}"))
}

/// Host wrote to an OUT endpoint: read it from the UDC and hand it to the
/// device.
fn pump_out(
    file: &File,
    session: &Session,
    handle: u16,
    endpoint: u32,
    size: usize,
) -> io::Result<()> {
    let mut buffer = vec![0_u8; size];
    let read = raw::ep_read(file.as_fd(), handle, &mut buffer)?;
    buffer.truncate(read);
    let pending = session
        .submit(TransferRequest::output(endpoint, buffer))
        .map_err(|error| io::Error::other(error.to_string()))?;
    pending
        .wait_timeout(ENDPOINT_TIMEOUT)
        .map_err(|error| io::Error::other(error.to_string()))?
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(())
}

/// Host wants to read an IN endpoint: pull from the device and queue the answer
/// on the UDC.
fn pump_in(
    file: &File,
    session: &Session,
    handle: u16,
    endpoint: u32,
    size: usize,
) -> io::Result<()> {
    let pending = session
        .submit(TransferRequest::input(endpoint, size))
        .map_err(|error| io::Error::other(error.to_string()))?;
    let completion = pending
        .wait_timeout(ENDPOINT_TIMEOUT)
        .map_err(|error| io::Error::other(error.to_string()))?
        .map_err(|error| io::Error::other(error.to_string()))?;
    if completion.status != 0 || completion.data.is_empty() {
        return Ok(());
    }
    raw::ep_write(file.as_fd(), handle, &completion.data)?;
    Ok(())
}
const SETUP_LEN: usize = 8;

/// Maps a USB/IP speed code onto the raw-gadget speed enum.
fn gadget_speed(usbip_speed: u32) -> u8 {
    match usbip_speed {
        1 | 2 => raw::USB_SPEED_FULL,
        5 | 6 => raw::USB_SPEED_SUPER,
        _ => raw::USB_SPEED_HIGH,
    }
}

pub struct Bridge {
    file: File,
    session: Session,
    enabled: Vec<u16>,
    stop: Arc<AtomicBool>,
}

impl Bridge {
    /// Binds the UDC and connects to the exporter. `speed` comes from the
    /// imported device so the gadget advertises what the real device is.
    pub fn new(
        udc_driver: &str,
        udc_device: &str,
        session: Session,
        usbip_speed: u32,
    ) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/raw-gadget")?;
        let fd = file.as_fd();
        raw::init(fd, udc_driver, udc_device, gadget_speed(usbip_speed))?;
        raw::run(fd)?;
        Ok(Self {
            file,
            session,
            enabled: Vec::new(),
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    fn fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }

    /// Services host events until the connection drops. Control requests are
    /// proxied; a successful SET_CONFIGURATION also tells the UDC to configure.
    pub fn run(&mut self) -> io::Result<()> {
        loop {
            let event = raw::event_fetch(self.fd())?;
            match event.kind {
                raw::USB_RAW_EVENT_CONNECT => {
                    eprintln!("host connected; proxying control traffic");
                }
                raw::USB_RAW_EVENT_CONTROL => {
                    if event.data.len() < SETUP_LEN {
                        continue;
                    }
                    let mut setup = [0_u8; SETUP_LEN];
                    setup.copy_from_slice(&event.data[..SETUP_LEN]);
                    if let Err(error) = self.proxy_control(setup) {
                        eprintln!(
                            "control proxy failed for bmRequestType={:#04x} bRequest={:#04x}: {error}",
                            setup[0], setup[1]
                        );
                    }
                }
                raw::USB_RAW_EVENT_RESET => eprintln!("bus reset"),
                raw::USB_RAW_EVENT_SUSPEND | raw::USB_RAW_EVENT_RESUME => {}
                raw::USB_RAW_EVENT_DISCONNECT => {
                    eprintln!("host disconnected");
                    return Ok(());
                }
                other => eprintln!("ignoring raw-gadget event {other}"),
            }
            if self.session.is_closed() {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "exporter closed the USB/IP session",
                ));
            }
        }
    }

    fn proxy_control(&mut self, setup: [u8; SETUP_LEN]) -> io::Result<()> {
        let device_to_host = setup[0] & 0x80 != 0;
        let length = usize::from(u16::from_le_bytes([setup[6], setup[7]]));

        if device_to_host {
            let mut request = TransferRequest::input(0, length);
            request.setup = setup;
            let completion = self.submit(request).map_err(label("remote-in"))?;
            raw::ep0_write(self.fd(), &completion).map_err(label("ep0_write-in"))?;
        } else {
            // A host-to-device request leaves ep0_out pending, so it is
            // completed with ep0_read — including the zero-length case, where
            // the read is purely the status stage. ep0_write would report
            // EBUSY here because no IN request is pending.
            let mut payload = vec![0_u8; length];
            raw::ep0_read(self.fd(), &mut payload).map_err(label("ep0_read"))?;
            let mut request = TransferRequest::output(0, payload);
            request.setup = setup;
            request.direction = Direction::Out;
            self.submit(request).map_err(label("remote-out"))?;
            if setup[0] == 0x00 && setup[1] == 0x09 {
                let value = u16::from_le_bytes([setup[2], setup[3]]) as u8;
                raw::vbus_draw(self.fd(), 250).map_err(label("vbus_draw"))?;
                raw::configure(self.fd()).map_err(label("configure"))?;
                if let Err(error) = self.mirror_endpoints(value) {
                    eprintln!("endpoint setup failed: {error}");
                }
            }
        }
        Ok(())
    }

    /// Fetches the real device's configuration, enables the same endpoints on
    /// the UDC, and starts a proxy thread for each one.
    fn mirror_endpoints(&mut self, configuration: u8) -> io::Result<()> {
        self.release_endpoints();
        let Some(bytes) = self.fetch_configuration(configuration)? else {
            return Err(io::Error::other(format!(
                "device has no configuration {configuration}"
            )));
        };
        let found = descriptors::endpoints(&bytes);
        eprintln!(
            "configuration {configuration}: {} bytes, {} endpoint(s) at altsetting 0",
            bytes.len(),
            found.len()
        );
        for endpoint in found {
            if endpoint.is_isochronous() {
                eprintln!(
                    "skipping isochronous endpoint {:#04x}; not carried by this bridge",
                    endpoint.address
                );
                continue;
            }
            match raw::ep_enable(self.fd(), &endpoint.to_descriptor()) {
                Ok(handle) => {
                    eprintln!(
                        "endpoint {:#04x} ({}) enabled as handle {handle}, maxpacket {}",
                        endpoint.address,
                        if endpoint.is_input() { "in" } else { "out" },
                        endpoint.max_packet_size
                    );
                    self.enabled.push(handle);
                    self.spawn_proxy(endpoint, handle)?;
                }
                Err(error) => eprintln!(
                    "cannot enable endpoint {:#04x} on this UDC: {error}",
                    endpoint.address
                ),
            }
        }
        Ok(())
    }

    /// Reads configuration descriptors by index until the one whose
    /// bConfigurationValue matches is found, because index and value differ on
    /// multi-configuration devices such as an iPhone.
    fn fetch_configuration(&self, value: u8) -> io::Result<Option<Vec<u8>>> {
        for index in 0..MAX_CONFIGURATIONS {
            let header = match self.get_descriptor(DESCRIPTOR_TYPE_CONFIGURATION, index, 9) {
                Ok(header) if header.len() >= 9 => header,
                _ => break,
            };
            if header[5] != value {
                continue;
            }
            let Some(total) = descriptors::total_length(&header) else {
                continue;
            };
            let full = self.get_descriptor(DESCRIPTOR_TYPE_CONFIGURATION, index, total)?;
            return Ok(Some(full));
        }
        Ok(None)
    }

    fn get_descriptor(&self, kind: u16, index: u8, length: u16) -> io::Result<Vec<u8>> {
        let value = (kind << 8) | u16::from(index);
        let mut request = TransferRequest::input(0, usize::from(length));
        let value_bytes = value.to_le_bytes();
        let length_bytes = length.to_le_bytes();
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
        self.submit(request)
    }

    /// One thread per endpoint. An OUT endpoint waits for the host and pushes
    /// to the device; an IN endpoint pulls from the device and answers the
    /// host. A transfer error is retried rather than fatal, because an idle
    /// interrupt endpoint legitimately times out.
    fn spawn_proxy(&self, endpoint: Endpoint, handle: u16) -> io::Result<()> {
        let file = self.file.try_clone()?;
        let session = self.session.clone();
        let stop = Arc::clone(&self.stop);
        let size = usize::from(endpoint.max_packet_size.max(64));
        let number = u32::from(endpoint.number());
        let input = endpoint.is_input();
        thread::Builder::new()
            .name(format!("interdriver-gadget-ep{:02x}", endpoint.address))
            .spawn(move || {
                while !stop.load(Ordering::Acquire) && !session.is_closed() {
                    let outcome = if input {
                        pump_in(&file, &session, handle, number, size)
                    } else {
                        pump_out(&file, &session, handle, number, size)
                    };
                    if let Err(error) = outcome {
                        // A closed session ends the thread; anything else is
                        // transient and the endpoint keeps serving.
                        if session.is_closed() {
                            break;
                        }
                        let _ = error;
                    }
                }
            })?;
        Ok(())
    }

    fn release_endpoints(&mut self) {
        for handle in self.enabled.drain(..) {
            let _ = raw::ep_disable(self.file.as_fd(), handle);
        }
    }

    fn submit(&self, request: TransferRequest) -> io::Result<Vec<u8>> {
        let pending = self
            .session
            .submit(request)
            .map_err(|error| io::Error::other(error.to_string()))?;
        let completion = pending
            .wait_timeout(TRANSFER_TIMEOUT)
            .map_err(|error| io::Error::other(error.to_string()))?
            .map_err(|error| io::Error::other(error.to_string()))?;
        if completion.status != 0 {
            return Err(io::Error::other(format!(
                "device returned USB status {}",
                completion.status
            )));
        }
        Ok(completion.data)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.release_endpoints();
    }
}
