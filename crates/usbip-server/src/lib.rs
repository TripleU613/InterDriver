use interdriver_transport::{Acceptor, Connect, Transport, plain};
pub use interdriver_transport::Transport as TransportHandle;
use interdriver_usbip_protocol::{TICKET_LEN, TICKET_MAGIC,

    BUS_ID_LEN, BasicHeader, CONTROL_HEADER_LEN, Command, ControlHeader, DEVICE_RECORD_LEN,
    Direction, INTERFACE_RECORD_LEN, ISO_DESCRIPTOR_LEN, ImportRequest, InterfaceRecord,
    IsoPacketDescriptor, OP_REP_DEVLIST, OP_REP_IMPORT, OP_REQ_DEVLIST, OP_REQ_IMPORT, Submit,
    SubmitReply, URB_HEADER_LEN, UrbHeader,
};
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

pub const MAX_EXPORTED_DEVICES: usize = 64;
pub const MAX_TRANSFER_LENGTH: usize = 64 * 1024 * 1024;

/// How long an unfinished ticket handshake may block before the connection is
/// dropped, so a peer that connects and then goes silent cannot hold a slot.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceDescription {
    pub device: interdriver_usbip_protocol::DeviceRecord,
    pub interfaces: Vec<InterfaceRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferRequest {
    pub sequence: u32,
    pub direction: Direction,
    pub endpoint: u32,
    pub transfer_flags: u32,
    pub transfer_length: usize,
    pub start_frame: i32,
    pub interval: i32,
    pub setup: [u8; 8],
    pub data: Vec<u8>,
    pub iso_packets: Vec<IsoPacketDescriptor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferCompletion {
    pub status: i32,
    pub actual_length: usize,
    pub start_frame: i32,
    pub error_count: i32,
    pub data: Vec<u8>,
    pub iso_packets: Vec<IsoPacketDescriptor>,
}

const DISCONNECT_POLL: Duration = Duration::from_millis(250);

pub trait DeviceSession: Send + Sync + 'static {
    fn submit(&self, request: TransferRequest) -> TransferCompletion;
    fn cancel(&self, endpoint: u32);

    /// Releases connection-scoped state after all pending endpoints have been
    /// canceled. Physical exporters can use this to return a device to its
    /// unconfigured state so a later import starts like a real USB replug.
    fn release(&self) {}

    /// The device backing this session is gone for good: it re-enumerated, was
    /// unplugged, or reset into a different descriptor set. The server drops the
    /// connection so the importer tears its virtual device down and re-imports,
    /// which is what a physical re-enumeration looks like to a USB host. The
    /// default is a session whose device never changes underneath it.
    fn disconnected(&self) -> bool {
        false
    }
}

pub trait DeviceProvider: Send + Sync + 'static {
    fn list(&self) -> io::Result<Vec<DeviceDescription>>;
    fn open(&self, bus_id: &str) -> io::Result<Arc<dyn DeviceSession>>;
}

/// Gate on the session ticket presented before the USB/IP request. The exporter
/// consults the control plane through this; a `false` verdict drops the
/// connection before any device is touched.
pub trait Authorizer: Send + Sync + 'static {
    fn authorize(&self, token: &[u8; TICKET_LEN]) -> bool;

    /// Called once after a ticket is accepted, with a handle on the connection.
    /// An implementation may watch the control plane and, on revocation, shut
    /// the connection down so the exporter releases the device mid-session. The
    /// default does nothing (a fixed ticket cannot be revoked).
    fn watch(&self, _token: &[u8; TICKET_LEN], _connection: Box<dyn Transport>) {}
}

/// Serves the provider over `connect`, which decides how the exporter reaches
/// the outside world:
///
/// - a listening `Acceptor` is the classic model — importers dial the exporter,
///   one connection per importer, spawned concurrently;
/// - a dialing `Dialer` is the reverse model — the exporter dials the importer
///   or a rendezvous relay, so a workstation behind a customer firewall needs
///   no inbound port. It holds one connection at a time and redials when it
///   drops.
///
/// The USB/IP roles are unchanged either way: the importer still speaks first.
/// This is the only difference between the two, and it lives entirely in
/// `connect`, so no device or protocol code depends on who opened the socket.
pub fn serve_via(
    mut connect: Box<dyn Connect>,
    provider: Arc<dyn DeviceProvider>,
    shutdown: Arc<AtomicBool>,
    authorizer: Option<Arc<dyn Authorizer>>,
) -> io::Result<()> {
    let concurrent = connect.concurrent();
    // A listener polls accept and wants a short, fixed idle sleep. A dialer
    // reaching an importer that is down should back off instead of hammering it
    // (a fleet of exporters pointed at one relay is otherwise self-inflicted
    // load), so its idle sleep grows to a ceiling and resets on a good connect.
    let idle_min = if concurrent {
        Duration::from_millis(25)
    } else {
        Duration::from_millis(200)
    };
    let idle_max = Duration::from_secs(5);
    let mut idle = idle_min;
    while !shutdown.load(Ordering::Acquire) {
        match connect.connect() {
            Ok(stream) => {
                idle = idle_min;
                let provider = Arc::clone(&provider);
                let authorizer = authorizer.clone();
                if concurrent {
                    thread::Builder::new()
                        .name("interdriver-usbip-server".into())
                        .spawn(move || {
                            let _ = serve_connection_authorized(stream, provider, authorizer);
                        })?;
                } else {
                    // Reverse mode holds one importer at a time; serve it here
                    // and redial only once it is gone. A watcher shuts the live
                    // connection down the instant `shutdown` flips, so Ctrl-C is
                    // observed mid-session rather than only between sessions --
                    // which is what makes the exporter release the device (and
                    // run its restore) promptly on exit.
                    let done = Arc::new(AtomicBool::new(false));
                    if let Ok(handle) = stream.split() {
                        let shutdown = Arc::clone(&shutdown);
                        let watcher_done = Arc::clone(&done);
                        thread::Builder::new()
                            .name("interdriver-usbip-dial-watch".into())
                            .spawn(move || {
                                while !watcher_done.load(Ordering::Acquire) {
                                    if shutdown.load(Ordering::Acquire) {
                                        let _ = handle.shutdown();
                                        return;
                                    }
                                    thread::sleep(Duration::from_millis(100));
                                }
                            })?;
                    }
                    let _ = serve_connection_authorized(stream, provider, authorizer);
                    done.store(true, Ordering::Release);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(idle);
                // A listener keeps polling accept at a steady, snappy interval;
                // only a dialer backs off toward the ceiling while it cannot
                // reach the far end.
                if !concurrent {
                    idle = (idle * 2).min(idle_max);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Classic listen entry point: serve the provider on a listener the caller
/// opened, one spawned connection per importer, plain TCP. A thin shim over
/// [`serve_via`] with a plaintext [`Acceptor`]; kept so existing callers and
/// the smoke tests do not change.
pub fn serve(
    listener: std::net::TcpListener,
    provider: Arc<dyn DeviceProvider>,
    shutdown: Arc<AtomicBool>,
) -> io::Result<()> {
    serve_authorized(listener, provider, shutdown, None)
}

/// As [`serve`], but gating each connection on a session ticket.
pub fn serve_authorized(
    listener: std::net::TcpListener,
    provider: Arc<dyn DeviceProvider>,
    shutdown: Arc<AtomicBool>,
    authorizer: Option<Arc<dyn Authorizer>>,
) -> io::Result<()> {
    let acceptor = Acceptor::from_listener(listener, HANDSHAKE_TIMEOUT, plain())?;
    serve_via(Box::new(acceptor), provider, shutdown, authorizer)
}

pub fn serve_connection(
    stream: Box<dyn Transport>,
    provider: Arc<dyn DeviceProvider>,
) -> io::Result<()> {
    serve_connection_authorized(stream, provider, None)
}

pub fn serve_connection_authorized(
    mut stream: Box<dyn Transport>,
    provider: Arc<dyn DeviceProvider>,
    authorizer: Option<Arc<dyn Authorizer>>,
) -> io::Result<()> {
    // Bound the whole handshake — the optional ticket AND the control request —
    // so a peer that connects and then goes silent cannot pin a device slot.
    // This matters most on the unauthenticated reverse path (windows/macos
    // export dial out with no authorizer), where a silent peer would otherwise
    // hold the one live session open forever.
    stream.set_timeouts(Some(HANDSHAKE_TIMEOUT))?;
    if let Some(authorizer) = authorizer {
        let mut magic = [0_u8; TICKET_MAGIC.len()];
        stream.read_exact(&mut magic)?;
        if magic != TICKET_MAGIC {
            return Err(invalid_data("session ticket required"));
        }
        let mut token = [0_u8; TICKET_LEN];
        stream.read_exact(&mut token)?;
        if !authorizer.authorize(&token) {
            return Err(invalid_data("session ticket rejected"));
        }
        if let Ok(connection) = stream.split() {
            authorizer.watch(&token, connection);
        }
    }
    let request = read_control_header(&mut stream)?;
    // The data plane is long-lived and must not time out between transfers.
    stream.set_timeouts(None)?;
    match request.code {
        OP_REQ_DEVLIST => send_device_list(&mut stream, provider.as_ref()),
        OP_REQ_IMPORT => import_device(stream, provider),
        code => Err(invalid_data(format!(
            "unsupported USB/IP control request 0x{code:04x}"
        ))),
    }
}

fn send_device_list(
    stream: &mut Box<dyn Transport>,
    provider: &dyn DeviceProvider,
) -> io::Result<()> {
    let devices = provider.list()?;
    if devices.len() > MAX_EXPORTED_DEVICES {
        return Err(invalid_data("too many exported devices"));
    }
    stream.write_all(
        &ControlHeader {
            code: OP_REP_DEVLIST,
            status: 0,
        }
        .encode(),
    )?;
    stream.write_all(&(devices.len() as u32).to_be_bytes())?;
    for description in devices {
        if description.interfaces.len() != usize::from(description.device.interface_count) {
            return Err(invalid_data("device interface count mismatch"));
        }
        stream.write_all(&description.device.encode())?;
        for interface in description.interfaces {
            stream.write_all(&interface.encode())?;
        }
    }
    stream.flush()
}

fn import_device(
    mut stream: Box<dyn Transport>,
    provider: Arc<dyn DeviceProvider>,
) -> io::Result<()> {
    let mut bytes = [0; BUS_ID_LEN];
    stream.read_exact(&mut bytes)?;
    let request = ImportRequest::decode(&bytes).map_err(protocol_error)?;
    let bus_id = request.bus_id().map_err(protocol_error)?;
    let descriptions = provider.list()?;
    let Some(description) = descriptions
        .into_iter()
        .find(|description| description.device.bus_id() == Ok(bus_id))
    else {
        stream.write_all(
            &ControlHeader {
                code: OP_REP_IMPORT,
                status: 1,
            }
            .encode(),
        )?;
        return Ok(());
    };
    let device = match provider.open(bus_id) {
        Ok(device) => device,
        Err(_) => {
            stream.write_all(
                &ControlHeader {
                    code: OP_REP_IMPORT,
                    status: 1,
                }
                .encode(),
            )?;
            return Ok(());
        }
    };
    stream.write_all(
        &ControlHeader {
            code: OP_REP_IMPORT,
            status: 0,
        }
        .encode(),
    )?;
    stream.write_all(&description.device.encode())?;
    stream.flush()?;
    let watchdog_done = watch_for_disconnect(stream.as_ref(), Arc::clone(&device))?;
    let result = transfer_loop(stream, device);
    watchdog_done.store(true, Ordering::Release);
    result
}

/// Ends the connection when the device goes away, even with nothing in flight.
/// A device is not only lost through a failed transfer: while the importing host
/// waits for a rebooting device there is no traffic to fail, so without this the
/// session would sit on a device that is already gone.
fn watch_for_disconnect(
    stream: &dyn Transport,
    device: Arc<dyn DeviceSession>,
) -> io::Result<Arc<AtomicBool>> {
    let stream = stream.split()?;
    let done = Arc::new(AtomicBool::new(false));
    let thread_done = Arc::clone(&done);
    thread::Builder::new()
        .name("interdriver-usbip-watchdog".into())
        .spawn(move || {
            while !thread_done.load(Ordering::Acquire) && !device.disconnected() {
                thread::sleep(DISCONNECT_POLL);
            }
            if thread_done.load(Ordering::Acquire) {
                return;
            }
            let _ = stream.shutdown();
        })?;
    Ok(done)
}

struct Pending {
    endpoint: u32,
}

/// Per-connection transfer bookkeeping, under one lock so that "is this
/// transfer still wanted?" and "is it the one currently running on its
/// endpoint?" are decided atomically against the worker starting it.
#[derive(Default)]
struct Book {
    /// Every transfer the host has submitted and not yet been answered for.
    pending: HashMap<u32, Pending>,
    /// The sequence currently executing on each endpoint's worker, if any.
    running: HashMap<u32, u32>,
}

/// Per-endpoint queue depth. Bounded so a fast importer cannot push the
/// exporter out of memory; a full queue blocks the reader, which is ordinary
/// TCP backpressure onto the importer rather than unbounded growth.
const ENDPOINT_QUEUE_DEPTH: usize = 32;

fn transfer_loop(reader: Box<dyn Transport>, device: Arc<dyn DeviceSession>) -> io::Result<()> {
    let writer = Arc::new(Mutex::new(reader.split()?));
    let mut reader = reader;
    let book = Arc::new(Mutex::new(Book::default()));
    let result = transfer_loop_inner(&mut reader, &device, &writer, &book);

    // A dead USB/IP connection cannot deliver completions for its outstanding
    // transfers. Abort every endpoint still owned by this connection so those
    // backend requests cannot survive into the next import and consume data
    // meant for the replacement session (notably usbmux's one-shot reply).
    let endpoints = lock(&book)
        .map(|mut book| {
            book.pending
                .drain()
                .map(|(_, request)| request.endpoint)
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();
    for endpoint in endpoints {
        device.cancel(endpoint);
    }
    device.release();

    result
}

fn transfer_loop_inner(
    reader: &mut Box<dyn Transport>,
    device: &Arc<dyn DeviceSession>,
    writer: &Arc<Mutex<Box<dyn Transport>>>,
    book: &Arc<Mutex<Book>>,
) -> io::Result<()> {
    // One worker thread per endpoint. Same-endpoint transfers are handed to the
    // same worker and run strictly in arrival order (which is submission order,
    // since one TCP stream carries them), so a pipelined bulk pipe's reads reach
    // the device and come back in order — a reordered bulk stream is a corrupted
    // transfer. Different endpoints have different workers and run concurrently,
    // so a parked IN read never blocks another endpoint. With a one-in-flight
    // importer this is identical to before: each worker only ever holds one.
    let mut workers: HashMap<u32, mpsc::SyncSender<TransferRequest>> = HashMap::new();
    loop {
        let mut bytes = [0; URB_HEADER_LEN];
        reader.read_exact(&mut bytes)?;
        match UrbHeader::decode(&bytes).map_err(protocol_error)? {
            UrbHeader::Submit(submit) => {
                let request = read_submit_request(reader, submit)?;
                let sequence = request.sequence;
                let endpoint = request.endpoint;
                lock(book)?.pending.insert(sequence, Pending { endpoint });
                if let std::collections::hash_map::Entry::Vacant(slot) = workers.entry(endpoint) {
                    let (sender, receiver) =
                        mpsc::sync_channel::<TransferRequest>(ENDPOINT_QUEUE_DEPTH);
                    let device = Arc::clone(device);
                    let writer = Arc::clone(writer);
                    let book = Arc::clone(book);
                    thread::Builder::new()
                        .name(format!("interdriver-usbip-ep-{endpoint}"))
                        .spawn(move || endpoint_worker(&receiver, &device, &writer, &book))?;
                    slot.insert(sender);
                }
                // A full queue blocks here: that is the backpressure that keeps
                // memory bounded, and it stalls only an importer that floods one
                // endpoint faster than the device can drain it.
                if workers[&endpoint].send(request).is_err() {
                    // The worker is gone (only on teardown); drop the pending
                    // entry so the bookkeeping stays consistent.
                    lock(book)?.pending.remove(&sequence);
                }
            }
            UrbHeader::Unlink(unlink) => {
                // Cancel is sequence-scoped. Removing the entry from `pending`
                // is what actually retires the transfer: a worker that has not
                // started it yet will skip it entirely, so it never reaches the
                // device. Only if this exact sequence is the one running on its
                // endpoint right now do we also tell the device to abort -- a
                // whole-endpoint cancel for a merely-queued transfer would kill
                // whichever *different* transfer happened to be in flight.
                let (removed_endpoint, is_running) = {
                    let mut book = lock(book)?;
                    let removed = book.pending.remove(&unlink.target_sequence);
                    let is_running = removed.as_ref().is_some_and(|entry| {
                        book.running.get(&entry.endpoint) == Some(&unlink.target_sequence)
                    });
                    (removed.map(|entry| entry.endpoint), is_running)
                };
                let status = match removed_endpoint {
                    Some(endpoint) => {
                        if is_running {
                            device.cancel(endpoint);
                        }
                        -104
                    }
                    None => 0,
                };
                let reply = UrbHeader::UnlinkReply(interdriver_usbip_protocol::UnlinkReply {
                    basic: BasicHeader {
                        command: Command::UnlinkReply,
                        sequence: unlink.basic.sequence,
                        device_id: 0,
                        direction: Direction::Out,
                        endpoint: 0,
                    },
                    status,
                });
                let mut writer = lock(writer)?;
                writer.write_all(&reply.encode())?;
                writer.flush()?;
            }
            _ => return Err(invalid_data("USB/IP client sent a reply packet")),
        }
    }
}

/// Processes one endpoint's transfers in the order they arrive, serially. This
/// is what preserves per-endpoint ordering: the next transfer on this endpoint
/// does not touch the device until this one has completed and been answered.
fn endpoint_worker(
    receiver: &mpsc::Receiver<TransferRequest>,
    device: &Arc<dyn DeviceSession>,
    writer: &Arc<Mutex<Box<dyn Transport>>>,
    book: &Arc<Mutex<Book>>,
) {
    for request in receiver {
        let sequence = request.sequence;
        let endpoint = request.endpoint;
        let direction = request.direction;
        // Claim the transfer atomically: if the host unlinked it while it sat
        // in the queue it is no longer in `pending`, and it must never touch the
        // device (an OUT write here would land data the host discarded).
        // Otherwise mark it as this endpoint's running transfer so an unlink
        // that arrives from now on knows to abort the device.
        {
            let Ok(mut book) = lock(book) else { break };
            if !book.pending.contains_key(&sequence) {
                continue;
            }
            book.running.insert(endpoint, sequence);
        }
        let completion = device.submit(request);
        let should_reply = lock(book)
            .ok()
            .map(|mut book| {
                book.running.remove(&endpoint);
                book.pending.remove(&sequence).is_some()
            })
            .unwrap_or(false);
        if should_reply {
            let _ = write_submit_reply(writer, sequence, direction, completion);
        }
        // Fast path only: shut the connection the moment a completed transfer
        // shows the device is gone. A transfer that is legitimately parked (a
        // quiet IN read re-arming forever) never reaches here, so it is the
        // independent `watch_for_disconnect` watchdog, not this check, that
        // detects a device leaving while an endpoint is parked.
        if device.disconnected()
            && let Ok(writer) = lock(writer)
        {
            let _ = writer.shutdown();
        }
    }
}

fn read_submit_request(
    reader: &mut Box<dyn Transport>,
    submit: Submit,
) -> io::Result<TransferRequest> {
    let transfer_length = usize::try_from(submit.transfer_buffer_length)
        .map_err(|_| invalid_data("negative transfer length"))?;
    if transfer_length > MAX_TRANSFER_LENGTH {
        return Err(invalid_data("transfer exceeds 64 MiB"));
    }
    let mut data = Vec::new();
    if submit.basic.direction == Direction::Out {
        data.resize(transfer_length, 0);
        reader.read_exact(&mut data)?;
    }
    let packet_count = if submit.packet_count == -1 {
        0
    } else {
        usize::try_from(submit.packet_count).map_err(|_| invalid_data("negative packet count"))?
    };
    let mut iso_packets = Vec::with_capacity(packet_count);
    for _ in 0..packet_count {
        let mut bytes = [0; ISO_DESCRIPTOR_LEN];
        reader.read_exact(&mut bytes)?;
        iso_packets.push(IsoPacketDescriptor::decode(&bytes).map_err(protocol_error)?);
    }
    Ok(TransferRequest {
        sequence: submit.basic.sequence,
        direction: submit.basic.direction,
        endpoint: submit.basic.endpoint,
        transfer_flags: submit.transfer_flags,
        transfer_length,
        start_frame: submit.start_frame,
        interval: submit.interval,
        setup: submit.setup,
        data,
        iso_packets,
    })
}

fn write_submit_reply(
    writer: &Mutex<Box<dyn Transport>>,
    sequence: u32,
    direction: Direction,
    completion: TransferCompletion,
) -> io::Result<()> {
    if completion.actual_length > MAX_TRANSFER_LENGTH {
        return Err(invalid_data("backend completion exceeds 64 MiB"));
    }
    if direction == Direction::In && completion.data.len() < completion.actual_length {
        return Err(invalid_data(
            "backend IN payload is shorter than actual length",
        ));
    }
    let packet_count = if completion.iso_packets.is_empty() {
        -1
    } else {
        i32::try_from(completion.iso_packets.len())
            .map_err(|_| invalid_data("too many completion packets"))?
    };
    let reply = UrbHeader::SubmitReply(SubmitReply {
        basic: BasicHeader {
            command: Command::SubmitReply,
            sequence,
            device_id: 0,
            direction: Direction::Out,
            endpoint: 0,
        },
        status: completion.status,
        actual_length: i32::try_from(completion.actual_length)
            .map_err(|_| invalid_data("completion length exceeds i32"))?,
        start_frame: completion.start_frame,
        packet_count,
        error_count: completion.error_count,
    });
    let mut writer = lock(writer)?;
    writer.write_all(&reply.encode())?;
    if direction == Direction::In {
        writer.write_all(&completion.data[..completion.actual_length])?;
    }
    for packet in completion.iso_packets {
        writer.write_all(&packet.encode())?;
    }
    writer.flush()
}

fn read_control_header(stream: &mut Box<dyn Transport>) -> io::Result<ControlHeader> {
    let mut bytes = [0; CONTROL_HEADER_LEN];
    stream.read_exact(&mut bytes)?;
    ControlHeader::decode(&bytes).map_err(protocol_error)
}

fn lock<T>(mutex: &Mutex<T>) -> io::Result<MutexGuard<'_, T>> {
    mutex.lock().map_err(|_| io::Error::other("lock poisoned"))
}

fn protocol_error(error: interdriver_usbip_protocol::DecodeError) -> io::Error {
    invalid_data(error.to_string())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

const _: usize = DEVICE_RECORD_LEN;
const _: usize = INTERFACE_RECORD_LEN;

#[cfg(test)]
mod tests {
    use super::*;
    use interdriver_transport::{Acceptor, Transport, plain};
    use std::net::TcpStream;
    use std::sync::Condvar;
    use std::time::Duration;

    struct Provider {
        session: Arc<TestDevice>,
    }

    impl DeviceProvider for Provider {
        fn list(&self) -> io::Result<Vec<DeviceDescription>> {
            let mut device = interdriver_usbip_protocol::DeviceRecord::new(
                "/sys/usb/1-2",
                "1-2",
                1,
                2,
                3,
                0x05ac,
                0x12a8,
            )
            .unwrap();
            device.interface_count = 1;
            Ok(vec![DeviceDescription {
                device,
                interfaces: vec![InterfaceRecord {
                    class: 255,
                    subclass: 254,
                    protocol: 2,
                }],
            }])
        }

        fn open(&self, bus_id: &str) -> io::Result<Arc<dyn DeviceSession>> {
            if bus_id != "1-2" {
                return Err(io::Error::new(io::ErrorKind::NotFound, "missing device"));
            }
            Ok(self.session.clone())
        }
    }

    #[derive(Default)]
    struct TestDevice {
        canceled: Mutex<bool>,
        cancel_changed: Condvar,
        blocking_started: Mutex<bool>,
        blocking_changed: Condvar,
        released: Mutex<bool>,
    }

    impl DeviceSession for TestDevice {
        fn submit(&self, request: TransferRequest) -> TransferCompletion {
            if request.endpoint == 9 {
                *self.blocking_started.lock().unwrap() = true;
                self.blocking_changed.notify_all();
                let mut canceled = self.canceled.lock().unwrap();
                while !*canceled {
                    canceled = self.cancel_changed.wait(canceled).unwrap();
                }
            }
            let data = if request.direction == Direction::In {
                b"phone".to_vec()
            } else {
                Vec::new()
            };
            TransferCompletion {
                status: 0,
                actual_length: data.len().max(request.data.len()),
                start_frame: 0,
                error_count: 0,
                data,
                iso_packets: Vec::new(),
            }
        }

        fn cancel(&self, _endpoint: u32) {
            *self.canceled.lock().unwrap() = true;
            self.cancel_changed.notify_all();
        }

        fn release(&self) {
            *self.released.lock().unwrap() = true;
        }
    }

    fn provider() -> Arc<Provider> {
        Arc::new(Provider {
            session: Arc::new(TestDevice::default()),
        })
    }

    /// Runs one connection through the listen path against a real socket pair.
    fn one_connection(provider: Arc<Provider>) -> (std::net::SocketAddr, thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream.set_nodelay(true).unwrap();
            let _ = serve_connection(Box::new(stream), provider);
        });
        (address, server)
    }

    struct RevokingAuthorizer;
    impl Authorizer for RevokingAuthorizer {
        fn authorize(&self, _token: &[u8; TICKET_LEN]) -> bool {
            true
        }
        fn watch(&self, _token: &[u8; TICKET_LEN], connection: Box<dyn Transport>) {
            // simulate an immediate revocation: drop the live connection
            let _ = connection.shutdown();
        }
    }

    #[test]
    fn watch_can_drop_a_live_connection_after_auth() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let provider = provider();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = serve_connection_authorized(
                Box::new(stream),
                provider,
                Some(Arc::new(RevokingAuthorizer)),
            );
        });

        let mut client = TcpStream::connect(address).unwrap();
        client
            .write_all(&interdriver_usbip_protocol::TICKET_MAGIC)
            .unwrap();
        client.write_all(&[0_u8; TICKET_LEN]).unwrap();
        client.flush().unwrap();
        // the watcher shut the connection down; the server-side read hits EOF and
        // the client sees the connection closed on its next read.
        let mut buffer = [0_u8; 8];
        let read = client.read(&mut buffer).unwrap_or(0);
        assert_eq!(read, 0, "revoked connection must be dropped");
        server.join().unwrap();
    }

    #[test]
    fn lists_provider_devices() {
        let (address, server) = one_connection(provider());
        let devices = interdriver_usbip_client::list(address, Duration::from_secs(1)).unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device.vendor_id, 0x05ac);
        assert_eq!(devices[0].interfaces.len(), 1);
        server.join().unwrap();
    }

    #[test]
    fn imports_and_completes_input_transfer() {
        let (address, server) = one_connection(provider());
        let session =
            interdriver_usbip_session::Session::connect(address, "1-2", Duration::from_secs(1))
                .unwrap();
        let completion = session
            .submit(interdriver_usbip_session::TransferRequest::input(1, 64))
            .unwrap()
            .wait_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(completion.data, b"phone");
        drop(session);
        server.join().unwrap();
    }

    #[test]
    fn successful_unlink_suppresses_submit_reply() {
        let (address, server) = one_connection(provider());
        let session =
            interdriver_usbip_session::Session::connect(address, "1-2", Duration::from_secs(1))
                .unwrap();
        let pending = session
            .submit(interdriver_usbip_session::TransferRequest::input(9, 64))
            .unwrap();
        let unlink = session.unlink(pending.sequence).unwrap();
        let completion = unlink
            .wait_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(completion.status, -104);
        assert_eq!(
            pending.wait(),
            Err(interdriver_usbip_session::TransferError::Cancelled)
        );
        drop(session);
        server.join().unwrap();
    }

    #[test]
    fn disconnect_cancels_pending_backend_transfers() {
        let provider = provider();
        let device = Arc::clone(&provider.session);
        let (address, server) = one_connection(provider);
        let session =
            interdriver_usbip_session::Session::connect(address, "1-2", Duration::from_secs(1))
                .unwrap();
        let _pending = session
            .submit(interdriver_usbip_session::TransferRequest::input(9, 64))
            .unwrap();

        let mut started = device.blocking_started.lock().unwrap();
        while !*started {
            let (next, timeout) = device
                .blocking_changed
                .wait_timeout(started, Duration::from_secs(1))
                .unwrap();
            started = next;
            assert!(!timeout.timed_out(), "backend transfer did not start");
        }
        drop(started);

        drop(session);
        server.join().unwrap();
        assert!(
            *device.canceled.lock().unwrap(),
            "connection teardown must cancel the pending endpoint"
        );
        assert!(
            *device.released.lock().unwrap(),
            "connection teardown must release backend session state"
        );
    }

    #[test]
    fn reverse_dial_exporter_reaches_a_listening_importer() {
        // The importer listens; the exporter dials out to it, exactly the
        // topology a workstation behind a customer firewall needs. USB/IP still
        // flows importer-first over the connection the exporter opened.
        let importer_listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let importer_addr = importer_listener.local_addr().unwrap();

        let provider = provider();
        let shutdown = Arc::new(AtomicBool::new(false));
        let exporter_shutdown = Arc::clone(&shutdown);
        let dialer =
            interdriver_transport::Dialer::new(importer_addr, Duration::from_secs(1), plain()).unwrap();
        let exporter = thread::spawn(move || {
            let _ = serve_via(Box::new(dialer), provider, exporter_shutdown, None);
        });

        // Importer side: accept the exporter's dial, then import over it.
        let (stream, _) = importer_listener.accept().unwrap();
        stream.set_nodelay(true).unwrap();
        let transport: Box<dyn Transport> = Box::new(stream);
        let imported =
            interdriver_usbip_client::import_on(transport, "1-2", Duration::from_secs(1), None).unwrap();
        assert_eq!(imported.device.vendor_id, 0x05ac);
        let session = interdriver_usbip_session::Session::from_imported(imported).unwrap();
        let completion = session
            .submit(interdriver_usbip_session::TransferRequest::input(1, 64))
            .unwrap()
            .wait_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(completion.data, b"phone");
        drop(session);
        shutdown.store(true, Ordering::Release);
        exporter.join().unwrap();
    }

    #[test]
    fn acceptor_serves_a_dialing_importer() {
        // The classic direction, but exercised through serve_via + Acceptor to
        // prove the listen path still works after the transport rewrite.
        let acceptor =
            Acceptor::bind(("127.0.0.1", 0), Duration::from_secs(1), plain()).unwrap();
        let address = acceptor.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = Arc::clone(&shutdown);
        let provider = provider();
        let server = thread::spawn(move || {
            let _ = serve_via(Box::new(acceptor), provider, server_shutdown, None);
        });
        let session =
            interdriver_usbip_session::Session::connect(address, "1-2", Duration::from_secs(1))
                .unwrap();
        let completion = session
            .submit(interdriver_usbip_session::TransferRequest::input(1, 64))
            .unwrap()
            .wait_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(completion.data, b"phone");
        drop(session);
        shutdown.store(true, Ordering::Release);
        server.join().unwrap();
    }

    /// A device whose IN completion is delayed by `setup[0]` milliseconds, so a
    /// later submission can finish before an earlier one and the pipeline's
    /// reordering is actually exercised rather than trivially satisfied.
    struct DelayProvider;

    impl DeviceProvider for DelayProvider {
        fn list(&self) -> io::Result<Vec<DeviceDescription>> {
            let mut device = interdriver_usbip_protocol::DeviceRecord::new(
                "/sys/usb/1-2", "1-2", 1, 2, 3, 0x05ac, 0x12a8,
            )
            .unwrap();
            device.interface_count = 0;
            Ok(vec![DeviceDescription { device, interfaces: Vec::new() }])
        }
        fn open(&self, _bus_id: &str) -> io::Result<Arc<dyn DeviceSession>> {
            Ok(Arc::new(DelayDevice))
        }
    }

    struct DelayDevice;
    impl DeviceSession for DelayDevice {
        fn submit(&self, request: TransferRequest) -> TransferCompletion {
            let millis = u64::from(request.setup[0]);
            thread::sleep(Duration::from_millis(millis));
            // Echo the tag byte back so the test can identify which submission
            // this completion belongs to.
            TransferCompletion {
                status: 0,
                actual_length: 1,
                start_frame: 0,
                error_count: 0,
                data: vec![request.setup[1]],
                iso_packets: Vec::new(),
            }
        }
        fn cancel(&self, _endpoint: u32) {}
    }

    fn delay_server() -> (std::net::SocketAddr, Arc<AtomicBool>, thread::JoinHandle<()>) {
        let acceptor = Acceptor::bind(("127.0.0.1", 0), Duration::from_secs(1), plain()).unwrap();
        let address = acceptor.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = Arc::clone(&shutdown);
        let handle = thread::spawn(move || {
            let _ = serve_via(Box::new(acceptor), Arc::new(DelayProvider), server_shutdown, None);
        });
        (address, shutdown, handle)
    }

    /// A request whose backend delay is `delay_ms` and whose echoed tag is `tag`.
    fn tagged(endpoint: u32, delay_ms: u8, tag: u8) -> interdriver_usbip_session::TransferRequest {
        let mut request = interdriver_usbip_session::TransferRequest::input(endpoint, 1);
        request.setup = [delay_ms, tag, 0, 0, 0, 0, 1, 0];
        request
    }

    #[test]
    fn pipeline_delivers_one_endpoint_in_submission_order() {
        use interdriver_usbip_session::{Pipeline, Session};
        let (address, shutdown, server) = delay_server();
        let session = Session::connect(address, "1-2", Duration::from_secs(2)).unwrap();

        let order = Arc::new(Mutex::new(Vec::<u8>::new()));
        let recorded = Arc::clone(&order);
        let mut pipeline = Pipeline::new(session, 8, move |_endpoint, token: u8, result| {
            let completion = result.unwrap();
            // the backend echoed the submission tag; both must agree
            assert_eq!(completion.data, vec![token]);
            recorded.lock().unwrap().push(token);
        });

        // Submit three on endpoint 3, first the slowest. If the pipeline did not
        // reorder, the callback order would be 3,2,1 (fastest first).
        pipeline.submit(3, 1_u8, tagged(3, 90, 1)).unwrap();
        pipeline.submit(3, 2_u8, tagged(3, 40, 2)).unwrap();
        pipeline.submit(3, 3_u8, tagged(3, 5, 3)).unwrap();
        pipeline.drain();
        assert_eq!(*order.lock().unwrap(), vec![1, 2, 3], "one endpoint must emit in submission order");

        drop(pipeline);
        shutdown.store(true, Ordering::Release);
        server.join().unwrap();
    }

    #[test]
    fn pipeline_does_not_head_of_line_block_across_endpoints() {
        use interdriver_usbip_session::{Pipeline, Session};
        let (address, shutdown, server) = delay_server();
        let session = Session::connect(address, "1-2", Duration::from_secs(2)).unwrap();

        let order = Arc::new(Mutex::new(Vec::<u8>::new()));
        let recorded = Arc::clone(&order);
        let mut pipeline = Pipeline::new(session, 8, move |_endpoint, token: u8, _result| {
            recorded.lock().unwrap().push(token);
        });

        // A slow transfer on endpoint 3 and a fast one on endpoint 5. The fast
        // one must be delivered first even though it was submitted second,
        // because a different endpoint must not be held up.
        pipeline.submit(3, 10_u8, tagged(3, 120, 10)).unwrap();
        pipeline.submit(5, 20_u8, tagged(5, 5, 20)).unwrap();
        pipeline.drain();
        let recorded = order.lock().unwrap().clone();
        assert_eq!(recorded, vec![20, 10], "a slow endpoint must not block a fast one");

        drop(pipeline);
        shutdown.store(true, Ordering::Release);
        server.join().unwrap();
    }


    /// A backend that stamps each submit with a monotonically increasing byte,
    /// so the reply data reveals the order in which the exporter actually ran
    /// the transfers. With per-endpoint serialization the stamp must match the
    /// submission order on that endpoint.
    struct OrderProvider;
    impl DeviceProvider for OrderProvider {
        fn list(&self) -> io::Result<Vec<DeviceDescription>> {
            let mut device = interdriver_usbip_protocol::DeviceRecord::new(
                "/sys/usb/1-2", "1-2", 1, 2, 3, 0x05ac, 0x12a8,
            ).unwrap();
            device.interface_count = 0;
            Ok(vec![DeviceDescription { device, interfaces: Vec::new() }])
        }
        fn open(&self, _bus_id: &str) -> io::Result<Arc<dyn DeviceSession>> {
            Ok(Arc::new(OrderDevice { next: std::sync::atomic::AtomicU32::new(0) }))
        }
    }
    struct OrderDevice { next: std::sync::atomic::AtomicU32 }
    impl DeviceSession for OrderDevice {
        fn submit(&self, request: TransferRequest) -> TransferCompletion {
            // A small variable delay makes the thread race real: without
            // per-endpoint ordering the stamps would interleave.
            let jitter = 8 - (request.setup[0] as u64 % 8);
            thread::sleep(Duration::from_millis(jitter));
            let stamp = self.next.fetch_add(1, Ordering::SeqCst) as u8;
            TransferCompletion {
                status: 0, actual_length: 2, start_frame: 0, error_count: 0,
                data: vec![request.setup[0], stamp], iso_packets: Vec::new(),
            }
        }
        fn cancel(&self, _endpoint: u32) {}
    }

    #[test]
    fn exporter_runs_one_endpoint_in_submission_order() {
        // Submit many reads on ONE endpoint concurrently through the session
        // (which allows unlimited outstanding transfers), and confirm the
        // exporter stamped them in submission order — i.e. same-endpoint order
        // is preserved end to end, which is what makes importer pipelining safe.
        let acceptor = Acceptor::bind(("127.0.0.1", 0), Duration::from_secs(1), plain()).unwrap();
        let address = acceptor.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = Arc::clone(&shutdown);
        let server = thread::spawn(move || {
            let _ = serve_via(Box::new(acceptor), Arc::new(OrderProvider), server_shutdown, None);
        });
        let session = std::sync::Arc::new(
            interdriver_usbip_session::Session::connect(address, "1-2", Duration::from_secs(2)).unwrap(),
        );

        // Fire 24 reads on endpoint 5 as fast as possible; the Nth carries tag N.
        const N: u8 = 24;
        let mut pendings = Vec::new();
        for tag in 0..N {
            let mut request = interdriver_usbip_session::TransferRequest::input(5, 2);
            request.setup = [tag, 0, 0, 0, 0, 0, 2, 0];
            pendings.push((tag, session.submit(request).unwrap()));
        }
        // Collect completions in submission order (by our tag) and read the stamp.
        let mut stamps = Vec::new();
        for (tag, pending) in pendings {
            let c = pending.wait_timeout(Duration::from_secs(5)).unwrap().unwrap();
            assert_eq!(c.data[0], tag, "reply must match the submission it answers");
            stamps.push(c.data[1]);
        }
        // The stamp assigned by the exporter must be strictly increasing in
        // submission order: transfer submitted first ran first.
        assert_eq!(
            stamps,
            (0..N).collect::<Vec<_>>(),
            "same-endpoint transfers must run in submission order, got {stamps:?}"
        );

        drop(session);
        shutdown.store(true, Ordering::Release);
        server.join().unwrap();
    }


    /// A backend that returns the next byte of a known stream on each IN read of
    /// one endpoint — a stand-in for reading a file off a bulk pipe in order.
    struct StreamProvider;
    impl DeviceProvider for StreamProvider {
        fn list(&self) -> io::Result<Vec<DeviceDescription>> {
            let mut device = interdriver_usbip_protocol::DeviceRecord::new(
                "/sys/usb/1-2", "1-2", 1, 2, 3, 0x05ac, 0x12a8,
            ).unwrap();
            device.interface_count = 0;
            Ok(vec![DeviceDescription { device, interfaces: Vec::new() }])
        }
        fn open(&self, _bus_id: &str) -> io::Result<Arc<dyn DeviceSession>> {
            Ok(Arc::new(StreamDevice { offset: std::sync::atomic::AtomicU32::new(0) }))
        }
    }
    struct StreamDevice { offset: std::sync::atomic::AtomicU32 }
    impl DeviceSession for StreamDevice {
        fn submit(&self, request: TransferRequest) -> TransferCompletion {
            let jitter = 6 - (request.setup[0] as u64 % 6);
            thread::sleep(Duration::from_millis(jitter));
            let n = self.offset.fetch_add(1, Ordering::SeqCst);
            // the stream byte at this position is just its low 8 bits
            TransferCompletion {
                status: 0, actual_length: 1, start_frame: 0, error_count: 0,
                data: vec![(n & 0xff) as u8], iso_packets: Vec::new(),
            }
        }
        fn cancel(&self, _endpoint: u32) {}
    }

    #[test]
    fn pipelined_bulk_stream_reassembles_byte_correct() {
        // The capstone: a pipelined importer with many transfers in flight, on an
        // exporter that serializes per endpoint, reassembles a bulk stream in the
        // exact order it was produced. This is the property a restore depends on.
        use interdriver_usbip_session::{Pipeline, Session};
        let acceptor = Acceptor::bind(("127.0.0.1", 0), Duration::from_secs(1), plain()).unwrap();
        let address = acceptor.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = Arc::clone(&shutdown);
        let server = thread::spawn(move || {
            let _ = serve_via(Box::new(acceptor), Arc::new(StreamProvider), server_shutdown, None);
        });
        let session = Session::connect(address, "1-2", Duration::from_secs(3)).unwrap();

        const N: u32 = 300;
        let received = Arc::new(Mutex::new(Vec::<(u32, u8)>::new()));
        let sink = Arc::clone(&received);
        let mut pipeline = Pipeline::new(session, 16, move |_ep, seq: u32, result| {
            let c = result.unwrap();
            sink.lock().unwrap().push((seq, c.data[0]));
        });
        for i in 0..N {
            let mut request = interdriver_usbip_session::TransferRequest::input(5, 1);
            request.setup = [(i & 0xff) as u8, 0, 0, 0, 0, 0, 1, 0];
            pipeline.submit(5, i, request).unwrap();
        }
        pipeline.drain();
        let mut out = received.lock().unwrap().clone();
        out.sort_by_key(|(seq, _)| *seq);              // reassemble by submission order
        let bytes: Vec<u8> = out.into_iter().map(|(_, b)| b).collect();
        let expected: Vec<u8> = (0..N).map(|n| (n & 0xff) as u8).collect();
        assert_eq!(bytes, expected, "pipelined bulk stream must reassemble in order");

        drop(pipeline);
        shutdown.store(true, Ordering::Release);
        server.join().unwrap();
    }


    /// Records every submit and every cancel so a test can prove what actually
    /// reached the device. The first submit on endpoint 4 blocks until told to
    /// finish, so a second one on the same endpoint is genuinely queued behind it.
    #[derive(Default)]
    struct RecordingDevice {
        submitted: Mutex<Vec<u8>>,
        cancels: Mutex<u32>,
        first_started: Mutex<bool>,
        first_started_changed: Condvar,
        release_first: Mutex<bool>,
        release_changed: Condvar,
    }
    impl DeviceSession for RecordingDevice {
        fn submit(&self, request: TransferRequest) -> TransferCompletion {
            let tag = request.setup[0];
            self.submitted.lock().unwrap().push(tag);
            if request.endpoint == 4 && tag == 0 {
                *self.first_started.lock().unwrap() = true;
                self.first_started_changed.notify_all();
                let mut go = self.release_first.lock().unwrap();
                while !*go {
                    go = self.release_changed.wait(go).unwrap();
                }
            }
            TransferCompletion {
                status: 0, actual_length: 0, start_frame: 0, error_count: 0,
                data: Vec::new(), iso_packets: Vec::new(),
            }
        }
        fn cancel(&self, _endpoint: u32) {
            *self.cancels.lock().unwrap() += 1;
        }
    }
    struct RecordingProvider(Arc<RecordingDevice>);
    impl DeviceProvider for RecordingProvider {
        fn list(&self) -> io::Result<Vec<DeviceDescription>> {
            let mut device = interdriver_usbip_protocol::DeviceRecord::new(
                "/sys/usb/1-2", "1-2", 1, 2, 3, 0x05ac, 0x12a8,
            ).unwrap();
            device.interface_count = 0;
            Ok(vec![DeviceDescription { device, interfaces: Vec::new() }])
        }
        fn open(&self, _bus_id: &str) -> io::Result<Arc<dyn DeviceSession>> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn unlinking_a_queued_transfer_never_reaches_the_device_and_cancels_nothing_else() {
        // The reviewer's bug: with per-endpoint queues, unlinking a transfer that
        // is still QUEUED must (a) not run it on the device later and (b) not
        // cancel the *different* transfer currently running on that endpoint.
        let device = Arc::new(RecordingDevice::default());
        let acceptor = Acceptor::bind(("127.0.0.1", 0), Duration::from_secs(1), plain()).unwrap();
        let address = acceptor.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = Arc::clone(&shutdown);
        let provider = RecordingProvider(Arc::clone(&device));
        let server = thread::spawn(move || {
            let _ = serve_via(Box::new(acceptor), Arc::new(provider), server_shutdown, None);
        });
        let session = interdriver_usbip_session::Session::connect(address, "1-2", Duration::from_secs(2)).unwrap();

        // Transfer 0 on endpoint 4 starts and blocks inside the device.
        let mut first = interdriver_usbip_session::TransferRequest::output(4, vec![0xaa]);
        first.setup = [0, 0, 0, 0, 0, 0, 0, 0];
        let _first = session.submit(first).unwrap();
        {
            let mut started = device.first_started.lock().unwrap();
            while !*started {
                let (next, timeout) = device.first_started_changed
                    .wait_timeout(started, Duration::from_secs(2)).unwrap();
                started = next;
                assert!(!timeout.timed_out(), "first transfer did not start");
            }
        }
        // Transfer 1 on the SAME endpoint is now queued behind it.
        let mut second = interdriver_usbip_session::TransferRequest::output(4, vec![0xbb]);
        second.setup = [1, 0, 0, 0, 0, 0, 0, 0];
        let second_pending = session.submit(second).unwrap();
        // Host cancels the queued one.
        let unlink = session.unlink(second_pending.sequence).unwrap()
            .wait_timeout(Duration::from_secs(2)).unwrap().unwrap();
        assert_eq!(unlink.status, -104, "host must be told the transfer was cancelled");

        // (b) Nothing was cancelled on the device: the running transfer (tag 0)
        //     was NOT the one unlinked, so no whole-endpoint abort was issued.
        assert_eq!(*device.cancels.lock().unwrap(), 0,
            "unlinking a queued transfer must not abort the running one");

        // Let the first finish; the worker will now reach the queued slot.
        *device.release_first.lock().unwrap() = true;
        device.release_changed.notify_all();
        thread::sleep(Duration::from_millis(200));

        // (a) The cancelled transfer never touched the device: only tag 0 ran.
        assert_eq!(*device.submitted.lock().unwrap(), vec![0_u8],
            "an unlinked-while-queued transfer must never be submitted to the device");

        drop(session);
        shutdown.store(true, Ordering::Release);
        server.join().unwrap();
    }

}
