use interdriver_usbip_protocol::{
    BUS_ID_LEN, CONTROL_HEADER_LEN, ControlHeader, DEVICE_RECORD_LEN, DeviceRecord,
    INTERFACE_RECORD_LEN, ImportRequest, InterfaceRecord, OP_REP_DEVLIST, OP_REP_IMPORT,
    OP_REQ_DEVLIST, OP_REQ_IMPORT,
};
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 3240;
pub const MAX_DEVICES: u32 = 64;
pub const MAX_INTERFACES: u8 = 32;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListedDevice {
    pub device: DeviceRecord,
    pub interfaces: Vec<InterfaceRecord>,
}

#[derive(Debug)]
pub struct ImportedDevice {
    pub device: DeviceRecord,
    stream: TcpStream,
}

impl ImportedDevice {
    #[must_use]
    pub fn into_stream(self) -> TcpStream {
        self.stream
    }

    #[must_use]
    pub const fn stream(&self) -> &TcpStream {
        &self.stream
    }

    #[must_use]
    pub const fn stream_mut(&mut self) -> &mut TcpStream {
        &mut self.stream
    }
}

#[derive(Debug)]
pub enum Error {
    AddressUnavailable,
    DeviceLimit(u32),
    InterfaceLimit(u8),
    Io(io::Error),
    Protocol(interdriver_usbip_protocol::DecodeError),
    Request(interdriver_usbip_protocol::EncodeError),
    Server {
        operation: &'static str,
        status: u32,
    },
    UnexpectedReply {
        expected: u16,
        actual: u16,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AddressUnavailable => formatter.write_str("USB/IP host has no usable address"),
            Self::DeviceLimit(count) => write!(formatter, "server advertised {count} devices"),
            Self::InterfaceLimit(count) => {
                write!(
                    formatter,
                    "server advertised {count} interfaces for one device"
                )
            }
            Self::Io(error) => error.fmt(formatter),
            Self::Protocol(error) => error.fmt(formatter),
            Self::Request(error) => error.fmt(formatter),
            Self::Server { operation, status } => {
                write!(
                    formatter,
                    "USB/IP {operation} failed with server status {status}"
                )
            }
            Self::UnexpectedReply { expected, actual } => write!(
                formatter,
                "unexpected USB/IP reply 0x{actual:04x}; expected 0x{expected:04x}"
            ),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<interdriver_usbip_protocol::DecodeError> for Error {
    fn from(value: interdriver_usbip_protocol::DecodeError) -> Self {
        Self::Protocol(value)
    }
}

impl From<interdriver_usbip_protocol::EncodeError> for Error {
    fn from(value: interdriver_usbip_protocol::EncodeError) -> Self {
        Self::Request(value)
    }
}

pub fn list<A: ToSocketAddrs>(address: A, timeout: Duration) -> Result<Vec<ListedDevice>, Error> {
    let mut stream = connect(address, timeout)?;
    stream.write_all(
        &ControlHeader {
            code: OP_REQ_DEVLIST,
            status: 0,
        }
        .encode(),
    )?;
    let reply = read_control_header(&mut stream, OP_REP_DEVLIST, "device list")?;
    ensure_success(reply, "device list")?;

    let count = read_u32(&mut stream)?;
    if count > MAX_DEVICES {
        return Err(Error::DeviceLimit(count));
    }

    let mut devices = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let device = read_device(&mut stream)?;
        if device.interface_count > MAX_INTERFACES {
            return Err(Error::InterfaceLimit(device.interface_count));
        }
        let mut interfaces = Vec::with_capacity(device.interface_count as usize);
        for _ in 0..device.interface_count {
            let mut bytes = [0; INTERFACE_RECORD_LEN];
            stream.read_exact(&mut bytes)?;
            interfaces.push(InterfaceRecord::decode(&bytes)?);
        }
        devices.push(ListedDevice { device, interfaces });
    }
    Ok(devices)
}

pub fn import<A: ToSocketAddrs>(
    address: A,
    bus_id: &str,
    timeout: Duration,
) -> Result<ImportedDevice, Error> {
    let mut stream = connect(address, timeout)?;
    stream.write_all(
        &ControlHeader {
            code: OP_REQ_IMPORT,
            status: 0,
        }
        .encode(),
    )?;
    let request = ImportRequest::new(bus_id)?;
    let encoded = request.encode();
    debug_assert_eq!(encoded.len(), BUS_ID_LEN);
    stream.write_all(&encoded)?;

    let reply = read_control_header(&mut stream, OP_REP_IMPORT, "import")?;
    ensure_success(reply, "import")?;
    let device = read_device(&mut stream)?;
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    Ok(ImportedDevice { device, stream })
}

fn connect<A: ToSocketAddrs>(address: A, timeout: Duration) -> Result<TcpStream, Error> {
    let addresses: Vec<SocketAddr> = address.to_socket_addrs()?.collect();
    if addresses.is_empty() {
        return Err(Error::AddressUnavailable);
    }
    let mut last_error = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, timeout) {
            Ok(stream) => {
                stream.set_nodelay(true)?;
                stream.set_read_timeout(Some(timeout))?;
                stream.set_write_timeout(Some(timeout))?;
                return Ok(stream);
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(Error::Io(last_error.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::AddrNotAvailable, "no USB/IP address")
    })))
}

fn read_control_header(
    stream: &mut TcpStream,
    expected: u16,
    _operation: &'static str,
) -> Result<ControlHeader, Error> {
    let mut bytes = [0; CONTROL_HEADER_LEN];
    stream.read_exact(&mut bytes)?;
    let reply = ControlHeader::decode(&bytes)?;
    if reply.code != expected {
        return Err(Error::UnexpectedReply {
            expected,
            actual: reply.code,
        });
    }
    Ok(reply)
}

fn ensure_success(header: ControlHeader, operation: &'static str) -> Result<(), Error> {
    if header.status == 0 {
        Ok(())
    } else {
        Err(Error::Server {
            operation,
            status: header.status,
        })
    }
}

fn read_device(stream: &mut TcpStream) -> Result<DeviceRecord, Error> {
    let mut bytes = [0; DEVICE_RECORD_LEN];
    stream.read_exact(&mut bytes)?;
    DeviceRecord::decode(&bytes).map_err(Error::from)
}

fn read_u32(stream: &mut TcpStream) -> Result<u32, Error> {
    let mut bytes = [0; 4];
    stream.read_exact(&mut bytes)?;
    Ok(u32::from_be_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    fn phone() -> DeviceRecord {
        let mut device =
            DeviceRecord::new("/sys/devices/usb1/1-2", "1-2", 1, 4, 3, 0x05ac, 0x12a8).unwrap();
        device.interface_count = 1;
        device
    }

    #[test]
    fn lists_device_from_fragmented_server_response() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; CONTROL_HEADER_LEN];
            stream.read_exact(&mut request).unwrap();
            assert_eq!(
                ControlHeader::decode(&request).unwrap().code,
                OP_REQ_DEVLIST
            );
            let mut response = Vec::new();
            response.extend_from_slice(
                &ControlHeader {
                    code: OP_REP_DEVLIST,
                    status: 0,
                }
                .encode(),
            );
            response.extend_from_slice(&1_u32.to_be_bytes());
            response.extend_from_slice(&phone().encode());
            response.extend_from_slice(
                &InterfaceRecord {
                    class: 255,
                    subclass: 254,
                    protocol: 2,
                }
                .encode(),
            );
            for chunk in response.chunks(7) {
                stream.write_all(chunk).unwrap();
            }
        });

        let devices = list(address, Duration::from_secs(1)).unwrap();
        server.join().unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device.vendor_id, 0x05ac);
        assert_eq!(devices[0].interfaces[0].class, 255);
    }

    #[test]
    fn imports_device_and_keeps_data_connection_open() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
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
            stream.write_all(&phone().encode()).unwrap();
            stream.write_all(b"URB!").unwrap();
        });

        let mut imported = import(address, "1-2", Duration::from_secs(1)).unwrap();
        assert_eq!(imported.device.device_id(), 0x0001_0004);
        let mut marker = [0; 4];
        imported.stream_mut().read_exact(&mut marker).unwrap();
        assert_eq!(&marker, b"URB!");
        server.join().unwrap();
    }

    #[test]
    fn rejects_device_count_before_allocating() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; CONTROL_HEADER_LEN];
            stream.read_exact(&mut request).unwrap();
            stream
                .write_all(
                    &ControlHeader {
                        code: OP_REP_DEVLIST,
                        status: 0,
                    }
                    .encode(),
                )
                .unwrap();
            stream.write_all(&(MAX_DEVICES + 1).to_be_bytes()).unwrap();
        });
        assert!(matches!(
            list(address, Duration::from_secs(1)),
            Err(Error::DeviceLimit(_))
        ));
        server.join().unwrap();
    }
}
