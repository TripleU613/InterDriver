//! A rendezvous relay so both ends dial out.
//!
//! In the fleet case neither machine can accept an inbound connection: the
//! customer's Windows PC is behind a firewall, and the Mac running Configurator
//! may be too. Both instead dial OUT to a relay that pairs them. The exporter
//! and the importer each open a mutually authenticated connection to the relay
//! and announce the same rendezvous key; the relay splices the two streams into
//! one byte pipe and then stays out of the way. USB/IP flows importer-to-exporter
//! through it exactly as if the two were connected directly — the relay never
//! interprets a USB byte.
//!
//! The relay authenticates both peers (each presents a CA-signed certificate)
//! and both peers authenticate the relay, so an unauthorized party cannot park
//! on a key or observe a session.

use interdriver_transport::{Connect, Transport};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Sent by a peer right after the TLS handshake, before any USB/IP: a magic, a
/// role, and a 32-byte rendezvous key. The magic cannot collide with a USB/IP
/// version header (which begins 0x01 0x11).
pub const RELAY_MAGIC: [u8; 4] = *b"GRLY";
pub const KEY_LEN: usize = 32;

pub const ROLE_EXPORTER: u8 = 0;
pub const ROLE_IMPORTER: u8 = 1;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a peer may wait for its opposite before the relay drops it, so an
/// exporter that redials (leaving an orphaned parked connection behind) does not
/// leak one live TLS connection per orphaned dial.
const PARK_TTL: Duration = Duration::from_secs(60);

/// Announces this peer to the relay: writes the rendezvous header. The exporter
/// and importer both call this on the connection they opened to the relay before
/// speaking USB/IP.
pub fn announce(stream: &mut dyn Transport, role: u8, key: &[u8; KEY_LEN]) -> io::Result<()> {
    stream.write_all(&RELAY_MAGIC)?;
    stream.write_all(&[role])?;
    stream.write_all(key)?;
    stream.flush()
}

/// Reads a peer's rendezvous header. Returns the role and key.
fn read_header(stream: &mut dyn Transport) -> io::Result<(u8, [u8; KEY_LEN])> {
    stream.set_timeouts(Some(HANDSHAKE_TIMEOUT))?;
    let mut magic = [0_u8; 4];
    stream.read_exact(&mut magic)?;
    if magic != RELAY_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "relay peer did not send the rendezvous magic",
        ));
    }
    let mut role = [0_u8; 1];
    stream.read_exact(&mut role)?;
    if role[0] != ROLE_EXPORTER && role[0] != ROLE_IMPORTER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "relay peer sent an unknown role",
        ));
    }
    let mut key = [0_u8; KEY_LEN];
    stream.read_exact(&mut key)?;
    stream.set_timeouts(None)?;
    Ok((role[0], key))
}

/// One waiting peer, parked until its opposite arrives.
struct Parked {
    stream: Box<dyn Transport>,
    since: Instant,
}

type Waiting = Arc<Mutex<HashMap<([u8; KEY_LEN], u8), Parked>>>;

/// Runs the relay over `connect` (an mTLS `Acceptor`). Each accepted peer sends
/// its rendezvous header; when an exporter and an importer share a key the relay
/// splices them. Runs until `shutdown` is set.
pub fn serve(
    mut connect: Box<dyn Connect>,
    shutdown: Arc<AtomicBool>,
) -> io::Result<()> {
    let waiting: Waiting = Arc::new(Mutex::new(HashMap::new()));
    spawn_reaper(Arc::clone(&waiting), Arc::clone(&shutdown))?;
    while !shutdown.load(Ordering::Acquire) {
        match connect.connect() {
            Ok(stream) => {
                let waiting = Arc::clone(&waiting);
                thread::Builder::new()
                    .name("interdriver-relay-peer".into())
                    .spawn(move || {
                        let _ = accept_peer(stream, &waiting);
                    })?;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn accept_peer(mut stream: Box<dyn Transport>, waiting: &Waiting) -> io::Result<()> {
    let (role, key) = read_header(&mut stream)?;
    let opposite = if role == ROLE_EXPORTER {
        ROLE_IMPORTER
    } else {
        ROLE_EXPORTER
    };
    let mut map = waiting
        .lock()
        .map_err(|_| io::Error::other("relay waiting map poisoned"))?;
    if let Some(parked) = map.remove(&(key, opposite)) {
        // The opposite peer is already here: release the lock and splice.
        drop(map);
        splice(parked.stream, stream)
    } else {
        // No opposite yet: park this peer until its opposite arrives. If a peer
        // of the same role/key was already parked (an exporter that redialled
        // without its old connection being reaped yet), replace it and close the
        // stale one deliberately rather than leaking it.
        if let Some(stale) = map.insert(
            (key, role),
            Parked {
                stream,
                since: Instant::now(),
            },
        ) {
            eprintln!("relay: replaced a stale parked peer (role {role}) on its key");
            let _ = stale.stream.shutdown();
        }
        Ok(())
    }
}

/// Periodically drops parked peers whose opposite never arrived, closing their
/// connections, so orphaned dials cannot accumulate.
fn spawn_reaper(waiting: Waiting, shutdown: Arc<AtomicBool>) -> io::Result<()> {
    thread::Builder::new()
        .name("interdriver-relay-reaper".into())
        .spawn(move || {
            while !shutdown.load(Ordering::Acquire) {
                thread::sleep(Duration::from_secs(5));
                let expired: Vec<Parked> = match waiting.lock() {
                    Ok(mut map) => {
                        let stale_keys: Vec<_> = map
                            .iter()
                            .filter(|(_, parked)| parked.since.elapsed() > PARK_TTL)
                            .map(|(key, _)| *key)
                            .collect();
                        stale_keys.into_iter().filter_map(|key| map.remove(&key)).collect()
                    }
                    Err(_) => Vec::new(),
                };
                for parked in expired {
                    let _ = parked.stream.shutdown();
                }
            }
        })?;
    Ok(())
}

/// Copies bytes both ways between two peers until either closes, one thread per
/// direction. The relay never looks at the bytes.
fn splice(a: Box<dyn Transport>, b: Box<dyn Transport>) -> io::Result<()> {
    let a_read = a;
    let a_write = a_read.split()?;
    let b_read = b;
    let b_write = b_read.split()?;

    let up = {
        let mut source = a_read;
        let mut sink = b_write;
        thread::Builder::new()
            .name("interdriver-relay-a2b".into())
            .spawn(move || {
                let _ = io::copy(&mut source, &mut sink);
                let _ = sink.shutdown();
                let _ = source.shutdown();
            })?
    };
    {
        let mut source = b_read;
        let mut sink = a_write;
        let _ = io::copy(&mut source, &mut sink);
        let _ = sink.shutdown();
        let _ = source.shutdown();
    }
    let _ = up.join();
    Ok(())
}

/// A `Connect` for the exporter that dials the relay and announces itself. On
/// each (re)dial it opens a fresh mTLS connection to the relay, announces the
/// exporter role and key, and returns the connection ready for USB/IP — which
/// the relay will have spliced to the importer sharing the key.
pub struct RelayExporter {
    inner: Box<dyn Connect>,
    key: [u8; KEY_LEN],
}

impl RelayExporter {
    #[must_use]
    pub fn new(inner: Box<dyn Connect>, key: [u8; KEY_LEN]) -> Self {
        Self { inner, key }
    }
}

impl Connect for RelayExporter {
    fn connect(&mut self) -> io::Result<Box<dyn Transport>> {
        let mut stream = self.inner.connect()?;
        // A relay that accepts the TCP/TLS connection but dies mid-announce is a
        // transient failure, like an unreachable relay: report WouldBlock so the
        // caller keeps retrying instead of treating it as fatal.
        if let Err(error) = announce(stream.as_mut(), ROLE_EXPORTER, &self.key) {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, error));
        }
        Ok(stream)
    }

    fn concurrent(&self) -> bool {
        // One device, one relayed session at a time; redial when it drops.
        false
    }

    fn describe(&self) -> String {
        format!("relaying via {}", self.inner.describe())
    }
}

/// Dials the relay as the importer, announces the key, and returns the
/// connection ready to import over (the relay has spliced it to the exporter).
pub fn importer_via_relay(
    mut connect: Box<dyn Connect>,
    key: [u8; KEY_LEN],
) -> io::Result<Box<dyn Transport>> {
    let mut stream = connect.connect()?;
    announce(stream.as_mut(), ROLE_IMPORTER, &key)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use interdriver_transport::{Acceptor, Dialer, plain};
    use interdriver_usbip_server::{
        DeviceDescription, DeviceProvider, DeviceSession, TransferCompletion, TransferRequest,
    };
    use std::time::{Duration, Instant};

    struct Provider;
    impl DeviceProvider for Provider {
        fn list(&self) -> io::Result<Vec<DeviceDescription>> {
            let mut device = interdriver_usbip_protocol::DeviceRecord::new(
                "/sys/usb/1-2", "1-2", 1, 2, 3, 0x05ac, 0x12a8,
            )
            .unwrap();
            device.interface_count = 0;
            Ok(vec![DeviceDescription {
                device,
                interfaces: Vec::new(),
            }])
        }
        fn open(&self, _bus_id: &str) -> io::Result<Arc<dyn DeviceSession>> {
            Ok(Arc::new(Echo))
        }
    }
    struct Echo;
    impl DeviceSession for Echo {
        fn submit(&self, _request: TransferRequest) -> TransferCompletion {
            TransferCompletion {
                status: 0,
                actual_length: 4,
                start_frame: 0,
                error_count: 0,
                data: b"phon".to_vec(),
                iso_packets: Vec::new(),
            }
        }
        fn cancel(&self, _endpoint: u32) {}
    }

    #[test]
    fn exporter_and_importer_meet_through_the_relay() {
        // Relay listens; exporter dials in and announces; importer dials in and
        // announces the same key; the relay splices; the importer reads the
        // device descriptor across the spliced pipe.
        let relay_listener = Acceptor::bind(("127.0.0.1", 0), Duration::from_secs(1), plain()).unwrap();
        let relay_addr = relay_listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let relay_shutdown = Arc::clone(&shutdown);
        let relay = thread::spawn(move || {
            let _ = serve(Box::new(relay_listener), relay_shutdown);
        });

        let key = [7_u8; KEY_LEN];

        // Exporter: dial the relay, announce, serve the device.
        let exporter_shutdown = Arc::new(AtomicBool::new(false));
        let es = Arc::clone(&exporter_shutdown);
        let exporter = thread::spawn(move || {
            let dialer = Dialer::new(relay_addr, Duration::from_secs(1), plain()).unwrap();
            let connect = RelayExporter::new(Box::new(dialer), key);
            let _ = interdriver_usbip_server::serve_via(
                Box::new(connect),
                Arc::new(Provider),
                es,
                None,
            );
        });

        // Importer: dial the relay, announce, import.
        let dialer = Dialer::new(relay_addr, Duration::from_secs(1), plain()).unwrap();
        let transport = importer_via_relay(Box::new(dialer), key).unwrap();
        let imported = interdriver_usbip_client::import_on(
            transport,
            "1-2",
            Duration::from_secs(2),
            None,
        )
        .unwrap();
        assert_eq!(imported.device.vendor_id, 0x05ac);
        let session = interdriver_usbip_session::Session::from_imported(imported).unwrap();
        let completion = session
            .submit(interdriver_usbip_session::TransferRequest::input(1, 4))
            .unwrap()
            .wait_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert_eq!(completion.data, b"phon");

        drop(session);
        exporter_shutdown.store(true, Ordering::Release);
        shutdown.store(true, Ordering::Release);
        let _ = exporter.join();
        let _ = relay.join();
    }
}
