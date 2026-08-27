use interdriver_usbip_protocol::{TICKET_LEN, TICKET_MAGIC,

    BUS_ID_LEN, BasicHeader, CONTROL_HEADER_LEN, Command, ControlHeader, DEVICE_RECORD_LEN,
    Direction, INTERFACE_RECORD_LEN, ISO_DESCRIPTOR_LEN, ImportRequest, InterfaceRecord,
    IsoPacketDescriptor, OP_REP_DEVLIST, OP_REP_IMPORT, OP_REQ_DEVLIST, OP_REQ_IMPORT, Submit,
    SubmitReply, URB_HEADER_LEN, UrbHeader,
};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

pub const MAX_EXPORTED_DEVICES: usize = 64;
pub const MAX_TRANSFER_LENGTH: usize = 64 * 1024 * 1024;

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

pub trait DeviceSession: Send + Sync + 'static {
    fn submit(&self, request: TransferRequest) -> TransferCompletion;
    fn cancel(&self, endpoint: u32);
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
}

pub fn serve(
    listener: TcpListener,
    provider: Arc<dyn DeviceProvider>,
    shutdown: Arc<AtomicBool>,
) -> io::Result<()> {
    serve_authorized(listener, provider, shutdown, None)
}

pub fn serve_authorized(
    listener: TcpListener,
    provider: Arc<dyn DeviceProvider>,
    shutdown: Arc<AtomicBool>,
    authorizer: Option<Arc<dyn Authorizer>>,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    while !shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                // Windows hands back an accepted socket that inherits the
                // listener's non-blocking mode, so every read would fail with
                // WouldBlock; Linux does not inherit it. Force blocking mode.
                if let Err(error) = stream.set_nonblocking(false) {
                    eprintln!("cannot set accepted socket blocking: {error}");
                    continue;
                }
                let provider = Arc::clone(&provider);
                let authorizer = authorizer.clone();
                thread::Builder::new()
                    .name("interdriver-usbip-server".into())
                    .spawn(move || {
                        let _ = serve_connection_authorized(stream, provider, authorizer);
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

pub fn serve_connection(
    stream: TcpStream,
    provider: Arc<dyn DeviceProvider>,
) -> io::Result<()> {
    serve_connection_authorized(stream, provider, None)
}

pub fn serve_connection_authorized(
    mut stream: TcpStream,
    provider: Arc<dyn DeviceProvider>,
    authorizer: Option<Arc<dyn Authorizer>>,
) -> io::Result<()> {
    stream.set_nodelay(true)?;
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
    }
    let request = read_control_header(&mut stream)?;
    match request.code {
        OP_REQ_DEVLIST => send_device_list(&mut stream, provider.as_ref()),
        OP_REQ_IMPORT => import_device(stream, provider),
        code => Err(invalid_data(format!(
            "unsupported USB/IP control request 0x{code:04x}"
        ))),
    }
}

fn send_device_list(stream: &mut TcpStream, provider: &dyn DeviceProvider) -> io::Result<()> {
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

fn import_device(mut stream: TcpStream, provider: Arc<dyn DeviceProvider>) -> io::Result<()> {
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
    transfer_loop(stream, device)
}

struct Pending {
    endpoint: u32,
}

fn transfer_loop(mut reader: TcpStream, device: Arc<dyn DeviceSession>) -> io::Result<()> {
    let writer = Arc::new(Mutex::new(reader.try_clone()?));
    let pending = Arc::new(Mutex::new(HashMap::<u32, Pending>::new()));
    loop {
        let mut bytes = [0; URB_HEADER_LEN];
        reader.read_exact(&mut bytes)?;
        match UrbHeader::decode(&bytes).map_err(protocol_error)? {
            UrbHeader::Submit(submit) => {
                let request = read_submit_request(&mut reader, submit)?;
                let sequence = request.sequence;
                let endpoint = request.endpoint;
                lock(&pending)?.insert(sequence, Pending { endpoint });
                let device = Arc::clone(&device);
                let writer = Arc::clone(&writer);
                let pending = Arc::clone(&pending);
                thread::Builder::new()
                    .name(format!("interdriver-usbip-urb-{sequence}"))
                    .spawn(move || {
                        let direction = request.direction;
                        let completion = device.submit(request);
                        let should_reply = lock(&pending)
                            .ok()
                            .and_then(|mut pending| pending.remove(&sequence))
                            .is_some();
                        if should_reply {
                            let _ = write_submit_reply(&writer, sequence, direction, completion);
                        }
                    })?;
            }
            UrbHeader::Unlink(unlink) => {
                let removed = lock(&pending)?.remove(&unlink.target_sequence);
                let status = if let Some(pending) = removed {
                    device.cancel(pending.endpoint);
                    -104
                } else {
                    0
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
                let mut writer = lock(&writer)?;
                writer.write_all(&reply.encode())?;
                writer.flush()?;
            }
            _ => return Err(invalid_data("USB/IP client sent a reply packet")),
        }
    }
}

fn read_submit_request(reader: &mut TcpStream, submit: Submit) -> io::Result<TransferRequest> {
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
    writer: &Mutex<TcpStream>,
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

fn read_control_header(stream: &mut TcpStream) -> io::Result<ControlHeader> {
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
    use std::net::TcpListener;
    use std::sync::Condvar;

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
    }

    impl DeviceSession for TestDevice {
        fn submit(&self, request: TransferRequest) -> TransferCompletion {
            if request.endpoint == 9 {
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
    }

    fn provider() -> Arc<Provider> {
        Arc::new(Provider {
            session: Arc::new(TestDevice::default()),
        })
    }

    fn one_connection(provider: Arc<Provider>) -> (std::net::SocketAddr, thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = serve_connection(stream, provider);
        });
        (address, server)
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
}
