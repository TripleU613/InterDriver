//! Presents a remote USB/IP device to a real USB host through `raw-gadget`.
//!
//! The bridge synthesises nothing. Every control request the host issues on
//! endpoint 0 is forwarded verbatim to the real device over USB/IP and the
//! device's own answer is returned, so descriptors, string indices, and
//! configuration values are byte-identical to the physical device.

use crate::raw;
use interdriver_usbip_protocol::Direction;
use interdriver_usbip_session::{Session, TransferRequest};
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::time::Duration;

const TRANSFER_TIMEOUT: Duration = Duration::from_secs(5);

/// Tags an io error with the raw-gadget call that produced it, so a failure
/// names the step instead of only the errno.
fn label(step: &'static str) -> impl Fn(io::Error) -> io::Error {
    move |error| io::Error::new(error.kind(), format!("{step}: {error}"))
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
    file: std::fs::File,
    session: Session,
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
        Ok(Self { file, session })
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
                raw::vbus_draw(self.fd(), 250).map_err(label("vbus_draw"))?;
                raw::configure(self.fd()).map_err(label("configure"))?;
            }
        }
        Ok(())
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
