#![no_std]

use core::fmt;

pub const VERSION: u16 = 0x0111;
pub const CONTROL_HEADER_LEN: usize = 8;
pub const BUS_ID_LEN: usize = 32;
pub const DEVICE_PATH_LEN: usize = 256;
pub const DEVICE_RECORD_LEN: usize = 312;
pub const INTERFACE_RECORD_LEN: usize = 4;
pub const URB_HEADER_LEN: usize = 48;
pub const ISO_DESCRIPTOR_LEN: usize = 16;
pub const MAX_ISO_PACKETS: i32 = 1024;

pub const OP_REQUEST: u16 = 0x8000;
pub const OP_REPLY: u16 = 0x0000;
pub const OP_IMPORT: u16 = 0x0003;
pub const OP_DEVLIST: u16 = 0x0005;
pub const OP_REQ_IMPORT: u16 = OP_REQUEST | OP_IMPORT;
pub const OP_REP_IMPORT: u16 = OP_REPLY | OP_IMPORT;
pub const OP_REQ_DEVLIST: u16 = OP_REQUEST | OP_DEVLIST;
pub const OP_REP_DEVLIST: u16 = OP_REPLY | OP_DEVLIST;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Command {
    Submit = 0x0001,
    Unlink = 0x0002,
    SubmitReply = 0x0003,
    UnlinkReply = 0x0004,
}

impl TryFrom<u32> for Command {
    type Error = DecodeError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0x0001 => Ok(Self::Submit),
            0x0002 => Ok(Self::Unlink),
            0x0003 => Ok(Self::SubmitReply),
            0x0004 => Ok(Self::UnlinkReply),
            other => Err(DecodeError::UnknownCommand(other)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Direction {
    Out = 0,
    In = 1,
}

impl TryFrom<u32> for Direction {
    type Error = DecodeError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Out),
            1 => Ok(Self::In),
            other => Err(DecodeError::UnknownDirection(other)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlHeader {
    pub code: u16,
    pub status: u32,
}

impl ControlHeader {
    #[must_use]
    pub fn encode(self) -> [u8; CONTROL_HEADER_LEN] {
        let mut bytes = [0; CONTROL_HEADER_LEN];
        bytes[0..2].copy_from_slice(&VERSION.to_be_bytes());
        bytes[2..4].copy_from_slice(&self.code.to_be_bytes());
        bytes[4..8].copy_from_slice(&self.status.to_be_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        require_len(bytes, CONTROL_HEADER_LEN)?;
        let version = read_u16(bytes, 0);
        if version != VERSION {
            return Err(DecodeError::UnsupportedVersion(version));
        }
        Ok(Self {
            code: read_u16(bytes, 2),
            status: read_u32(bytes, 4),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImportRequest {
    bus_id: [u8; BUS_ID_LEN],
}

impl ImportRequest {
    pub fn new(bus_id: &str) -> Result<Self, EncodeError> {
        let source = bus_id.as_bytes();
        if source.is_empty() || source.len() >= BUS_ID_LEN || source.contains(&0) {
            return Err(EncodeError::InvalidBusId);
        }
        let mut encoded = [0; BUS_ID_LEN];
        encoded[..source.len()].copy_from_slice(source);
        Ok(Self { bus_id: encoded })
    }

    #[must_use]
    pub const fn encode(self) -> [u8; BUS_ID_LEN] {
        self.bus_id
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        require_len(bytes, BUS_ID_LEN)?;
        let mut bus_id = [0; BUS_ID_LEN];
        bus_id.copy_from_slice(&bytes[..BUS_ID_LEN]);
        let end = bus_id
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(BUS_ID_LEN);
        if end == 0 || core::str::from_utf8(&bus_id[..end]).is_err() {
            return Err(DecodeError::InvalidBusId);
        }
        Ok(Self { bus_id })
    }

    pub fn bus_id(&self) -> Result<&str, DecodeError> {
        let end = self
            .bus_id
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(BUS_ID_LEN);
        core::str::from_utf8(&self.bus_id[..end]).map_err(|_| DecodeError::InvalidBusId)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceRecord {
    path: [u8; DEVICE_PATH_LEN],
    bus_id: [u8; BUS_ID_LEN],
    pub bus_number: u32,
    pub device_number: u32,
    pub speed: u32,
    pub vendor_id: u16,
    pub product_id: u16,
    pub device_release: u16,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub configuration_value: u8,
    pub configuration_count: u8,
    pub interface_count: u8,
}

impl DeviceRecord {
    pub fn new(
        path: &str,
        bus_id: &str,
        bus_number: u32,
        device_number: u32,
        speed: u32,
        vendor_id: u16,
        product_id: u16,
    ) -> Result<Self, EncodeError> {
        Ok(Self {
            path: encode_string::<DEVICE_PATH_LEN>(path, EncodeError::InvalidDevicePath)?,
            bus_id: encode_string::<BUS_ID_LEN>(bus_id, EncodeError::InvalidBusId)?,
            bus_number,
            device_number,
            speed,
            vendor_id,
            product_id,
            device_release: 0,
            device_class: 0,
            device_subclass: 0,
            device_protocol: 0,
            configuration_value: 0,
            configuration_count: 0,
            interface_count: 0,
        })
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        require_len(bytes, DEVICE_RECORD_LEN)?;
        let mut path = [0; DEVICE_PATH_LEN];
        path.copy_from_slice(&bytes[..DEVICE_PATH_LEN]);
        validate_string(&path, DecodeError::InvalidDevicePath)?;
        let mut bus_id = [0; BUS_ID_LEN];
        bus_id.copy_from_slice(&bytes[DEVICE_PATH_LEN..DEVICE_PATH_LEN + BUS_ID_LEN]);
        validate_string(&bus_id, DecodeError::InvalidBusId)?;
        Ok(Self {
            path,
            bus_id,
            bus_number: read_u32(bytes, 288),
            device_number: read_u32(bytes, 292),
            speed: read_u32(bytes, 296),
            vendor_id: read_u16(bytes, 300),
            product_id: read_u16(bytes, 302),
            device_release: read_u16(bytes, 304),
            device_class: bytes[306],
            device_subclass: bytes[307],
            device_protocol: bytes[308],
            configuration_value: bytes[309],
            configuration_count: bytes[310],
            interface_count: bytes[311],
        })
    }

    #[must_use]
    pub fn encode(self) -> [u8; DEVICE_RECORD_LEN] {
        let mut bytes = [0; DEVICE_RECORD_LEN];
        bytes[..DEVICE_PATH_LEN].copy_from_slice(&self.path);
        bytes[DEVICE_PATH_LEN..DEVICE_PATH_LEN + BUS_ID_LEN].copy_from_slice(&self.bus_id);
        bytes[288..292].copy_from_slice(&self.bus_number.to_be_bytes());
        bytes[292..296].copy_from_slice(&self.device_number.to_be_bytes());
        bytes[296..300].copy_from_slice(&self.speed.to_be_bytes());
        bytes[300..302].copy_from_slice(&self.vendor_id.to_be_bytes());
        bytes[302..304].copy_from_slice(&self.product_id.to_be_bytes());
        bytes[304..306].copy_from_slice(&self.device_release.to_be_bytes());
        bytes[306] = self.device_class;
        bytes[307] = self.device_subclass;
        bytes[308] = self.device_protocol;
        bytes[309] = self.configuration_value;
        bytes[310] = self.configuration_count;
        bytes[311] = self.interface_count;
        bytes
    }

    pub fn path(&self) -> Result<&str, DecodeError> {
        decode_string(&self.path, DecodeError::InvalidDevicePath)
    }

    pub fn bus_id(&self) -> Result<&str, DecodeError> {
        decode_string(&self.bus_id, DecodeError::InvalidBusId)
    }

    #[must_use]
    pub const fn device_id(&self) -> u32 {
        (self.bus_number << 16) | self.device_number
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InterfaceRecord {
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
}

impl InterfaceRecord {
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        require_len(bytes, INTERFACE_RECORD_LEN)?;
        Ok(Self {
            class: bytes[0],
            subclass: bytes[1],
            protocol: bytes[2],
        })
    }

    #[must_use]
    pub const fn encode(self) -> [u8; INTERFACE_RECORD_LEN] {
        [self.class, self.subclass, self.protocol, 0]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BasicHeader {
    pub command: Command,
    pub sequence: u32,
    pub device_id: u32,
    pub direction: Direction,
    pub endpoint: u32,
}

impl BasicHeader {
    fn encode_into(self, bytes: &mut [u8; URB_HEADER_LEN]) {
        write_u32(bytes, 0, self.command as u32);
        write_u32(bytes, 4, self.sequence);
        write_u32(bytes, 8, self.device_id);
        write_u32(bytes, 12, self.direction as u32);
        write_u32(bytes, 16, self.endpoint);
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        Ok(Self {
            command: Command::try_from(read_u32(bytes, 0))?,
            sequence: read_u32(bytes, 4),
            device_id: read_u32(bytes, 8),
            direction: Direction::try_from(read_u32(bytes, 12))?,
            endpoint: read_u32(bytes, 16),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Submit {
    pub basic: BasicHeader,
    pub transfer_flags: u32,
    pub transfer_buffer_length: i32,
    pub start_frame: i32,
    pub packet_count: i32,
    pub interval: i32,
    pub setup: [u8; 8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubmitReply {
    pub basic: BasicHeader,
    pub status: i32,
    pub actual_length: i32,
    pub start_frame: i32,
    pub packet_count: i32,
    pub error_count: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Unlink {
    pub basic: BasicHeader,
    pub target_sequence: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnlinkReply {
    pub basic: BasicHeader,
    pub status: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UrbHeader {
    Submit(Submit),
    SubmitReply(SubmitReply),
    Unlink(Unlink),
    UnlinkReply(UnlinkReply),
}

impl UrbHeader {
    #[must_use]
    pub fn encode(self) -> [u8; URB_HEADER_LEN] {
        let mut bytes = [0; URB_HEADER_LEN];
        match self {
            Self::Submit(value) => {
                value.basic.encode_into(&mut bytes);
                write_u32(&mut bytes, 20, value.transfer_flags);
                write_i32(&mut bytes, 24, value.transfer_buffer_length);
                write_i32(&mut bytes, 28, value.start_frame);
                write_i32(&mut bytes, 32, value.packet_count);
                write_i32(&mut bytes, 36, value.interval);
                bytes[40..48].copy_from_slice(&value.setup);
            }
            Self::SubmitReply(value) => {
                value.basic.encode_into(&mut bytes);
                write_i32(&mut bytes, 20, value.status);
                write_i32(&mut bytes, 24, value.actual_length);
                write_i32(&mut bytes, 28, value.start_frame);
                write_i32(&mut bytes, 32, value.packet_count);
                write_i32(&mut bytes, 36, value.error_count);
            }
            Self::Unlink(value) => {
                value.basic.encode_into(&mut bytes);
                write_u32(&mut bytes, 20, value.target_sequence);
            }
            Self::UnlinkReply(value) => {
                value.basic.encode_into(&mut bytes);
                write_i32(&mut bytes, 20, value.status);
            }
        }
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        require_len(bytes, URB_HEADER_LEN)?;
        let basic = BasicHeader::decode(bytes)?;
        match basic.command {
            Command::Submit => Ok(Self::Submit(Submit {
                basic,
                transfer_flags: read_u32(bytes, 20),
                transfer_buffer_length: read_i32(bytes, 24),
                start_frame: read_i32(bytes, 28),
                packet_count: checked_packet_count(read_i32(bytes, 32))?,
                interval: read_i32(bytes, 36),
                setup: bytes[40..48].try_into().expect("fixed-size slice"),
            })),
            Command::SubmitReply => Ok(Self::SubmitReply(SubmitReply {
                basic,
                status: read_i32(bytes, 20),
                actual_length: read_i32(bytes, 24),
                start_frame: read_i32(bytes, 28),
                packet_count: checked_packet_count(read_i32(bytes, 32))?,
                error_count: read_i32(bytes, 36),
            })),
            Command::Unlink => Ok(Self::Unlink(Unlink {
                basic,
                target_sequence: read_u32(bytes, 20),
            })),
            Command::UnlinkReply => Ok(Self::UnlinkReply(UnlinkReply {
                basic,
                status: read_i32(bytes, 20),
            })),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IsoPacketDescriptor {
    pub offset: u32,
    pub length: u32,
    pub actual_length: u32,
    pub status: i32,
}

impl IsoPacketDescriptor {
    #[must_use]
    pub fn encode(self) -> [u8; ISO_DESCRIPTOR_LEN] {
        let mut bytes = [0; ISO_DESCRIPTOR_LEN];
        bytes[0..4].copy_from_slice(&self.offset.to_be_bytes());
        bytes[4..8].copy_from_slice(&self.length.to_be_bytes());
        bytes[8..12].copy_from_slice(&self.actual_length.to_be_bytes());
        bytes[12..16].copy_from_slice(&self.status.to_be_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        require_len(bytes, ISO_DESCRIPTOR_LEN)?;
        Ok(Self {
            offset: read_u32(bytes, 0),
            length: read_u32(bytes, 4),
            actual_length: read_u32(bytes, 8),
            status: read_i32(bytes, 12),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncodeError {
    InvalidBusId,
    InvalidDevicePath,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBusId => formatter.write_str("bus ID must contain 1 to 31 non-NUL bytes"),
            Self::InvalidDevicePath => {
                formatter.write_str("device path must contain 1 to 255 non-NUL bytes")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    InvalidBusId,
    InvalidDevicePath,
    InvalidIsoPacketCount(i32),
    ShortMessage { expected: usize, actual: usize },
    UnknownCommand(u32),
    UnknownDirection(u32),
    UnsupportedVersion(u16),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBusId => formatter.write_str("invalid bus ID"),
            Self::InvalidDevicePath => formatter.write_str("invalid device path"),
            Self::InvalidIsoPacketCount(count) => {
                write!(formatter, "invalid isochronous packet count {count}")
            }
            Self::ShortMessage { expected, actual } => {
                write!(
                    formatter,
                    "short message: expected {expected} bytes, received {actual}"
                )
            }
            Self::UnknownCommand(command) => {
                write!(formatter, "unknown USB/IP command 0x{command:08x}")
            }
            Self::UnknownDirection(direction) => {
                write!(formatter, "unknown USB/IP direction {direction}")
            }
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported USB/IP version 0x{version:04x}")
            }
        }
    }
}

fn encode_string<const N: usize>(value: &str, error: EncodeError) -> Result<[u8; N], EncodeError> {
    let source = value.as_bytes();
    if source.is_empty() || source.len() >= N || source.contains(&0) {
        return Err(error);
    }
    let mut encoded = [0; N];
    encoded[..source.len()].copy_from_slice(source);
    Ok(encoded)
}

fn validate_string<const N: usize>(bytes: &[u8; N], error: DecodeError) -> Result<(), DecodeError> {
    decode_string(bytes, error).map(|_| ())
}

fn decode_string<const N: usize>(bytes: &[u8; N], error: DecodeError) -> Result<&str, DecodeError> {
    let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(N);
    if end == 0 {
        return Err(error);
    }
    core::str::from_utf8(&bytes[..end]).map_err(|_| error)
}

fn checked_packet_count(value: i32) -> Result<i32, DecodeError> {
    if value == -1 || (0..=MAX_ISO_PACKETS).contains(&value) {
        Ok(value)
    } else {
        Err(DecodeError::InvalidIsoPacketCount(value))
    }
}

fn require_len(bytes: &[u8], expected: usize) -> Result<(), DecodeError> {
    if bytes.len() < expected {
        Err(DecodeError::ShortMessage {
            expected,
            actual: bytes.len(),
        })
    } else {
        Ok(())
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes(
        bytes[offset..offset + 2]
            .try_into()
            .expect("validated slice"),
    )
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("validated slice"),
    )
}

fn read_i32(bytes: &[u8], offset: usize) -> i32 {
    i32::from_be_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("validated slice"),
    )
}

fn write_u32(bytes: &mut [u8; URB_HEADER_LEN], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

fn write_i32(bytes: &mut [u8; URB_HEADER_LEN], offset: usize, value: i32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_header_uses_usbip_111_and_network_byte_order() {
        let header = ControlHeader {
            code: OP_REQ_IMPORT,
            status: 0,
        };
        let encoded = header.encode();
        assert_eq!(encoded, [0x01, 0x11, 0x80, 0x03, 0, 0, 0, 0]);
        assert_eq!(ControlHeader::decode(&encoded), Ok(header));
    }

    #[test]
    fn import_bus_id_is_nul_padded() {
        let request = ImportRequest::new("1-2.3").unwrap();
        let encoded = request.encode();
        assert_eq!(&encoded[..6], b"1-2.3\0");
        assert_eq!(
            ImportRequest::decode(&encoded).unwrap().bus_id(),
            Ok("1-2.3")
        );
    }

    #[test]
    fn submit_header_round_trips() {
        let basic = BasicHeader {
            command: Command::Submit,
            sequence: 17,
            device_id: 0x0002_0009,
            direction: Direction::Out,
            endpoint: 2,
        };
        let header = UrbHeader::Submit(Submit {
            basic,
            transfer_flags: 0x200,
            transfer_buffer_length: 4096,
            start_frame: 0,
            packet_count: -1,
            interval: 0,
            setup: [0; 8],
        });
        let encoded = header.encode();
        assert_eq!(&encoded[..4], &[0, 0, 0, 1]);
        assert_eq!(&encoded[4..8], &[0, 0, 0, 17]);
        assert_eq!(UrbHeader::decode(&encoded), Ok(header));
    }

    #[test]
    fn rejects_unbounded_iso_packet_array() {
        let header = UrbHeader::Submit(Submit {
            basic: BasicHeader {
                command: Command::Submit,
                sequence: 1,
                device_id: 1,
                direction: Direction::In,
                endpoint: 1,
            },
            transfer_flags: 0,
            transfer_buffer_length: 0,
            start_frame: 0,
            packet_count: MAX_ISO_PACKETS + 1,
            interval: 0,
            setup: [0; 8],
        })
        .encode();
        assert_eq!(
            UrbHeader::decode(&header),
            Err(DecodeError::InvalidIsoPacketCount(MAX_ISO_PACKETS + 1))
        );
    }

    #[test]
    fn accepts_non_iso_packet_sentinel() {
        let header = UrbHeader::Submit(Submit {
            basic: BasicHeader {
                command: Command::Submit,
                sequence: 1,
                device_id: 1,
                direction: Direction::In,
                endpoint: 1,
            },
            transfer_flags: 0,
            transfer_buffer_length: 64,
            start_frame: 0,
            packet_count: -1,
            interval: 0,
            setup: [0; 8],
        })
        .encode();
        assert!(matches!(
            UrbHeader::decode(&header),
            Ok(UrbHeader::Submit(Submit {
                packet_count: -1,
                ..
            }))
        ));
    }

    #[test]
    fn device_record_matches_kernel_wire_layout() {
        let mut record = DeviceRecord::new(
            "/sys/devices/pci0000:00/usb2/2-3",
            "2-3",
            2,
            9,
            3,
            0x05ac,
            0x12a8,
        )
        .unwrap();
        record.device_release = 0x1600;
        record.configuration_value = 1;
        record.configuration_count = 1;
        record.interface_count = 3;
        let encoded = record.encode();
        assert_eq!(encoded.len(), 312);
        assert_eq!(&encoded[288..292], &[0, 0, 0, 2]);
        assert_eq!(&encoded[300..304], &[0x05, 0xac, 0x12, 0xa8]);
        let decoded = DeviceRecord::decode(&encoded).unwrap();
        assert_eq!(decoded, record);
        assert_eq!(decoded.bus_id(), Ok("2-3"));
        assert_eq!(decoded.device_id(), 0x0002_0009);
    }
}
