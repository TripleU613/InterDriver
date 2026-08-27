use interdriver_usbip_client::ImportedDevice;
use interdriver_usbip_protocol::{
    BasicHeader, Command, Direction, ISO_DESCRIPTOR_LEN, IsoPacketDescriptor, MAX_ISO_PACKETS,
    Submit, SubmitReply, URB_HEADER_LEN, Unlink, UnlinkReply, UrbHeader,
};
use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Shutdown, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

pub const MAX_TRANSFER_LENGTH: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferRequest {
    pub endpoint: u32,
    pub direction: Direction,
    pub transfer_flags: u32,
    pub transfer_length: usize,
    pub start_frame: i32,
    pub interval: i32,
    pub setup: [u8; 8],
    pub data: Vec<u8>,
    pub iso_packets: Vec<IsoPacketDescriptor>,
}

impl TransferRequest {
    #[must_use]
    pub fn input(endpoint: u32, length: usize) -> Self {
        Self {
            endpoint,
            direction: Direction::In,
            transfer_flags: 0,
            transfer_length: length,
            start_frame: 0,
            interval: 0,
            setup: [0; 8],
            data: Vec::new(),
            iso_packets: Vec::new(),
        }
    }

    #[must_use]
    pub fn output(endpoint: u32, data: Vec<u8>) -> Self {
        Self {
            endpoint,
            direction: Direction::Out,
            transfer_flags: 0,
            transfer_length: data.len(),
            start_frame: 0,
            interval: 0,
            setup: [0; 8],
            data,
            iso_packets: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferCompletion {
    pub sequence: u32,
    pub status: i32,
    pub actual_length: usize,
    pub start_frame: i32,
    pub error_count: i32,
    pub data: Vec<u8>,
    pub iso_packets: Vec<IsoPacketDescriptor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnlinkCompletion {
    pub sequence: u32,
    pub target_sequence: u32,
    pub status: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransferError {
    Cancelled,
    Connection(String),
    Protocol(String),
}

impl fmt::Display for TransferError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("USB transfer was cancelled"),
            Self::Connection(message) => write!(formatter, "USB/IP connection failed: {message}"),
            Self::Protocol(message) => write!(formatter, "USB/IP protocol failed: {message}"),
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Client(interdriver_usbip_client::Error),
    ConnectionClosed,
    InvalidRequest(&'static str),
    Io(io::Error),
    LockPoisoned,
    SequenceExhausted,
    Timeout,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(error) => error.fmt(formatter),
            Self::ConnectionClosed => formatter.write_str("USB/IP connection closed"),
            Self::InvalidRequest(reason) => write!(formatter, "invalid USB transfer: {reason}"),
            Self::Io(error) => error.fmt(formatter),
            Self::LockPoisoned => formatter.write_str("USB/IP session lock was poisoned"),
            Self::SequenceExhausted => formatter.write_str("USB/IP sequence space exhausted"),
            Self::Timeout => formatter.write_str("USB/IP operation timed out"),
        }
    }
}

impl std::error::Error for Error {}

impl From<interdriver_usbip_client::Error> for Error {
    fn from(value: interdriver_usbip_client::Error) -> Self {
        Self::Client(value)
    }
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Debug)]
pub struct PendingTransfer {
    pub sequence: u32,
    receiver: Receiver<Result<TransferCompletion, TransferError>>,
}

impl PendingTransfer {
    pub fn wait(self) -> Result<TransferCompletion, TransferError> {
        self.receiver.recv().unwrap_or_else(|_| {
            Err(TransferError::Connection(
                "completion channel closed".into(),
            ))
        })
    }

    pub fn wait_timeout(
        self,
        timeout: Duration,
    ) -> Result<Result<TransferCompletion, TransferError>, Error> {
        match self.receiver.recv_timeout(timeout) {
            Ok(completion) => Ok(completion),
            Err(RecvTimeoutError::Timeout) => Err(Error::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(Error::ConnectionClosed),
        }
    }
}

#[derive(Debug)]
pub struct PendingUnlink {
    pub sequence: u32,
    receiver: Receiver<Result<UnlinkCompletion, TransferError>>,
}

impl PendingUnlink {
    pub fn wait_timeout(
        self,
        timeout: Duration,
    ) -> Result<Result<UnlinkCompletion, TransferError>, Error> {
        match self.receiver.recv_timeout(timeout) {
            Ok(completion) => Ok(completion),
            Err(RecvTimeoutError::Timeout) => Err(Error::Timeout),
            Err(RecvTimeoutError::Disconnected) => Err(Error::ConnectionClosed),
        }
    }
}

struct PendingSubmitEntry {
    direction: Direction,
    maximum_length: usize,
    sender: mpsc::Sender<Result<TransferCompletion, TransferError>>,
}

struct PendingUnlinkEntry {
    target_sequence: u32,
    sender: mpsc::Sender<Result<UnlinkCompletion, TransferError>>,
}

struct Shared {
    writer: Mutex<std::net::TcpStream>,
    submits: Mutex<HashMap<u32, PendingSubmitEntry>>,
    unlinks: Mutex<HashMap<u32, PendingUnlinkEntry>>,
    next_sequence: AtomicU32,
    handles: AtomicUsize,
    closed: AtomicBool,
    device_id: u32,
}

pub struct Session {
    shared: Arc<Shared>,
}

impl Session {
    pub fn connect<A: ToSocketAddrs>(
        address: A,
        bus_id: &str,
        timeout: Duration,
    ) -> Result<Self, Error> {
        Self::connect_with_ticket(address, bus_id, timeout, None)
    }

    pub fn connect_with_ticket<A: ToSocketAddrs>(
        address: A,
        bus_id: &str,
        timeout: Duration,
        ticket: Option<[u8; 32]>,
    ) -> Result<Self, Error> {
        let imported = interdriver_usbip_client::import_with_ticket(address, bus_id, timeout, ticket)?;
        Self::from_imported(imported)
    }

    pub fn from_imported(imported: ImportedDevice) -> Result<Self, Error> {
        let device_id = imported.device.device_id();
        let reader = imported.into_stream();
        let writer = reader.try_clone()?;
        let shared = Arc::new(Shared {
            writer: Mutex::new(writer),
            submits: Mutex::new(HashMap::new()),
            unlinks: Mutex::new(HashMap::new()),
            next_sequence: AtomicU32::new(1),
            handles: AtomicUsize::new(1),
            closed: AtomicBool::new(false),
            device_id,
        });
        let reader_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("interdriver-usbip-reader".into())
            .spawn(move || reader_loop(reader, &reader_shared))?;
        Ok(Self { shared })
    }

    pub fn submit(&self, request: TransferRequest) -> Result<PendingTransfer, Error> {
        validate_request(&request)?;
        let sequence = self.next_sequence()?;
        let packet_count = if request.iso_packets.is_empty() {
            -1
        } else {
            i32::try_from(request.iso_packets.len())
                .map_err(|_| Error::InvalidRequest("too many isochronous packets"))?
        };
        let transfer_length = i32::try_from(request.transfer_length)
            .map_err(|_| Error::InvalidRequest("transfer is too large"))?;
        let header = UrbHeader::Submit(Submit {
            basic: BasicHeader {
                command: Command::Submit,
                sequence,
                device_id: self.shared.device_id,
                direction: request.direction,
                endpoint: request.endpoint,
            },
            transfer_flags: request.transfer_flags,
            transfer_buffer_length: transfer_length,
            start_frame: request.start_frame,
            packet_count,
            interval: request.interval,
            setup: request.setup,
        });
        let (sender, receiver) = mpsc::channel();
        lock(&self.shared.submits)?.insert(
            sequence,
            PendingSubmitEntry {
                direction: request.direction,
                maximum_length: request.transfer_length,
                sender,
            },
        );

        let write_result = (|| -> io::Result<()> {
            let mut writer = lock(&self.shared.writer)
                .map_err(|_| io::Error::other("USB/IP writer lock poisoned"))?;
            writer.write_all(&header.encode())?;
            if request.direction == Direction::Out {
                writer.write_all(&request.data)?;
            }
            for packet in request.iso_packets {
                writer.write_all(&packet.encode())?;
            }
            writer.flush()
        })();
        if let Err(error) = write_result {
            lock(&self.shared.submits)?.remove(&sequence);
            return Err(Error::Io(error));
        }
        Ok(PendingTransfer { sequence, receiver })
    }

    pub fn unlink(&self, target_sequence: u32) -> Result<PendingUnlink, Error> {
        if !lock(&self.shared.submits)?.contains_key(&target_sequence) {
            return Err(Error::InvalidRequest("target transfer is not pending"));
        }
        let sequence = self.next_sequence()?;
        let header = UrbHeader::Unlink(Unlink {
            basic: BasicHeader {
                command: Command::Unlink,
                sequence,
                device_id: self.shared.device_id,
                direction: Direction::Out,
                endpoint: 0,
            },
            target_sequence,
        });
        let (sender, receiver) = mpsc::channel();
        lock(&self.shared.unlinks)?.insert(
            sequence,
            PendingUnlinkEntry {
                target_sequence,
                sender,
            },
        );
        if let Err(error) = lock(&self.shared.writer)?.write_all(&header.encode()) {
            lock(&self.shared.unlinks)?.remove(&sequence);
            return Err(Error::Io(error));
        }
        Ok(PendingUnlink { sequence, receiver })
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
    }

    fn next_sequence(&self) -> Result<u32, Error> {
        self.shared
            .next_sequence
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                (current != 0 && current != u32::MAX).then_some(current + 1)
            })
            .map_err(|_| Error::SequenceExhausted)
    }
}

impl Clone for Session {
    fn clone(&self) -> Self {
        self.shared.handles.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.shared.handles.fetch_sub(1, Ordering::AcqRel) == 1
            && let Ok(writer) = self.shared.writer.lock()
        {
            self.shared.closed.store(true, Ordering::Release);
            let _ = writer.shutdown(Shutdown::Both);
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        if let Ok(writer) = self.writer.lock() {
            let _ = writer.shutdown(Shutdown::Both);
        }
    }
}

fn validate_request(request: &TransferRequest) -> Result<(), Error> {
    if request.transfer_length > MAX_TRANSFER_LENGTH {
        return Err(Error::InvalidRequest("transfer exceeds 64 MiB"));
    }
    if request.endpoint > 15 {
        return Err(Error::InvalidRequest("endpoint number exceeds 15"));
    }
    match request.direction {
        Direction::Out if request.data.len() != request.transfer_length => {
            return Err(Error::InvalidRequest(
                "OUT payload length does not match transfer length",
            ));
        }
        Direction::In if !request.data.is_empty() => {
            return Err(Error::InvalidRequest("IN transfer contains an OUT payload"));
        }
        _ => {}
    }
    if request.iso_packets.len() > MAX_ISO_PACKETS as usize {
        return Err(Error::InvalidRequest("too many isochronous packets"));
    }
    Ok(())
}

fn reader_loop(mut reader: std::net::TcpStream, shared: &Arc<Shared>) {
    let result: Result<(), TransferError> = loop {
        let mut bytes = [0; URB_HEADER_LEN];
        if let Err(error) = reader.read_exact(&mut bytes) {
            break Err(TransferError::Connection(error.to_string()));
        }
        let header = match UrbHeader::decode(&bytes) {
            Ok(header) => header,
            Err(error) => break Err(TransferError::Protocol(error.to_string())),
        };
        let result = match header {
            UrbHeader::SubmitReply(reply) => read_submit_reply(&mut reader, shared, reply),
            UrbHeader::UnlinkReply(reply) => handle_unlink_reply(shared, reply),
            _ => Err(TransferError::Protocol(
                "server sent a request on the response stream".into(),
            )),
        };
        if let Err(error) = result {
            break Err(error);
        }
    };
    if let Err(error) = result {
        fail_all(shared, error);
    }
    shared.closed.store(true, Ordering::Release);
}

fn read_submit_reply(
    reader: &mut std::net::TcpStream,
    shared: &Arc<Shared>,
    reply: SubmitReply,
) -> Result<(), TransferError> {
    let pending = lock_transfer(&shared.submits)?
        .remove(&reply.basic.sequence)
        .ok_or_else(|| {
            TransferError::Protocol(format!(
                "response for unknown sequence {}",
                reply.basic.sequence
            ))
        })?;
    let actual_length = usize::try_from(reply.actual_length).map_err(|_| {
        TransferError::Protocol(format!("negative actual length {}", reply.actual_length))
    })?;
    if actual_length > pending.maximum_length || actual_length > MAX_TRANSFER_LENGTH {
        return Err(TransferError::Protocol(format!(
            "actual length {actual_length} exceeds requested length {}",
            pending.maximum_length
        )));
    }
    let mut data = Vec::new();
    if pending.direction == Direction::In {
        data.resize(actual_length, 0);
        reader
            .read_exact(&mut data)
            .map_err(|error| TransferError::Connection(error.to_string()))?;
    }
    let packet_count = if reply.packet_count == -1 {
        0
    } else {
        usize::try_from(reply.packet_count)
            .map_err(|_| TransferError::Protocol("negative packet count".into()))?
    };
    let mut iso_packets = Vec::with_capacity(packet_count);
    for _ in 0..packet_count {
        let mut bytes = [0; ISO_DESCRIPTOR_LEN];
        reader
            .read_exact(&mut bytes)
            .map_err(|error| TransferError::Connection(error.to_string()))?;
        iso_packets.push(
            IsoPacketDescriptor::decode(&bytes)
                .map_err(|error| TransferError::Protocol(error.to_string()))?,
        );
    }
    let _ = pending.sender.send(Ok(TransferCompletion {
        sequence: reply.basic.sequence,
        status: reply.status,
        actual_length,
        start_frame: reply.start_frame,
        error_count: reply.error_count,
        data,
        iso_packets,
    }));
    Ok(())
}

fn handle_unlink_reply(shared: &Arc<Shared>, reply: UnlinkReply) -> Result<(), TransferError> {
    let pending = lock_transfer(&shared.unlinks)?
        .remove(&reply.basic.sequence)
        .ok_or_else(|| {
            TransferError::Protocol(format!(
                "unlink response for unknown sequence {}",
                reply.basic.sequence
            ))
        })?;
    if reply.status != 0
        && let Some(submit) = lock_transfer(&shared.submits)?.remove(&pending.target_sequence)
    {
        let _ = submit.sender.send(Err(TransferError::Cancelled));
    }
    let _ = pending.sender.send(Ok(UnlinkCompletion {
        sequence: reply.basic.sequence,
        target_sequence: pending.target_sequence,
        status: reply.status,
    }));
    Ok(())
}

fn fail_all(shared: &Arc<Shared>, error: TransferError) {
    if let Ok(mut submits) = shared.submits.lock() {
        for (_, pending) in submits.drain() {
            let _ = pending.sender.send(Err(error.clone()));
        }
    }
    if let Ok(mut unlinks) = shared.unlinks.lock() {
        for (_, pending) in unlinks.drain() {
            let _ = pending.sender.send(Err(error.clone()));
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>, Error> {
    mutex.lock().map_err(|_| Error::LockPoisoned)
}

fn lock_transfer<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>, TransferError> {
    mutex
        .lock()
        .map_err(|_| TransferError::Connection("session lock poisoned".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use interdriver_usbip_protocol::{
        BUS_ID_LEN, CONTROL_HEADER_LEN, ControlHeader, DeviceRecord, ImportRequest, OP_REP_IMPORT,
        OP_REQ_IMPORT,
    };
    use std::net::TcpListener;

    fn accept_import(listener: TcpListener) -> std::net::TcpStream {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; CONTROL_HEADER_LEN + BUS_ID_LEN];
        stream.read_exact(&mut request).unwrap();
        assert_eq!(ControlHeader::decode(&request).unwrap().code, OP_REQ_IMPORT);
        assert_eq!(
            ImportRequest::decode(&request[CONTROL_HEADER_LEN..])
                .unwrap()
                .bus_id(),
            Ok("1-2")
        );
        stream
            .write_all(
                &ControlHeader {
                    code: OP_REP_IMPORT,
                    status: 0,
                }
                .encode(),
            )
            .unwrap();
        stream
            .write_all(
                &DeviceRecord::new("/sys/usb/1-2", "1-2", 1, 4, 3, 0x05ac, 0x12a8)
                    .unwrap()
                    .encode(),
            )
            .unwrap();
        stream
    }

    #[test]
    fn completes_input_transfer_from_fragmented_reply() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut stream = accept_import(listener);
            let mut header = [0; URB_HEADER_LEN];
            stream.read_exact(&mut header).unwrap();
            let UrbHeader::Submit(request) = UrbHeader::decode(&header).unwrap() else {
                panic!("expected submit");
            };
            assert_eq!(request.basic.direction, Direction::In);
            assert_eq!(request.packet_count, -1);
            let response = UrbHeader::SubmitReply(SubmitReply {
                basic: BasicHeader {
                    command: Command::SubmitReply,
                    sequence: request.basic.sequence,
                    device_id: 0,
                    direction: Direction::Out,
                    endpoint: 0,
                },
                status: 0,
                actual_length: 4,
                start_frame: 0,
                packet_count: -1,
                error_count: 0,
            })
            .encode();
            for chunk in response.chunks(5) {
                stream.write_all(chunk).unwrap();
            }
            stream.write_all(b"USB!").unwrap();
        });

        let session = Session::connect(address, "1-2", Duration::from_secs(1)).unwrap();
        let endpoint_session = session.clone();
        drop(session);
        let completion = endpoint_session
            .submit(TransferRequest::input(1, 64))
            .unwrap()
            .wait_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(completion.data, b"USB!");
        server.join().unwrap();
    }

    #[test]
    fn serializes_output_payload_after_header() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut stream = accept_import(listener);
            let mut header = [0; URB_HEADER_LEN];
            stream.read_exact(&mut header).unwrap();
            let UrbHeader::Submit(request) = UrbHeader::decode(&header).unwrap() else {
                panic!("expected submit");
            };
            let mut payload = vec![0; request.transfer_buffer_length as usize];
            stream.read_exact(&mut payload).unwrap();
            assert_eq!(payload, b"hello");
            stream
                .write_all(
                    &UrbHeader::SubmitReply(SubmitReply {
                        basic: BasicHeader {
                            command: Command::SubmitReply,
                            sequence: request.basic.sequence,
                            device_id: 0,
                            direction: Direction::Out,
                            endpoint: 0,
                        },
                        status: 0,
                        actual_length: 5,
                        start_frame: 0,
                        packet_count: -1,
                        error_count: 0,
                    })
                    .encode(),
                )
                .unwrap();
        });

        let session = Session::connect(address, "1-2", Duration::from_secs(1)).unwrap();
        let completion = session
            .submit(TransferRequest::output(2, b"hello".to_vec()))
            .unwrap()
            .wait_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert!(completion.data.is_empty());
        assert_eq!(completion.actual_length, 5);
        server.join().unwrap();
    }

    #[test]
    fn rejects_oversized_transfer_without_network_io() {
        let request = TransferRequest::input(1, MAX_TRANSFER_LENGTH + 1);
        assert!(matches!(
            validate_request(&request),
            Err(Error::InvalidRequest(_))
        ));
    }
}
