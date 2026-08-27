//! Walks a USB configuration descriptor to find the endpoints that must be
//! mirrored onto the UDC. Pure parsing, so it is tested without hardware.

pub const DESCRIPTOR_TYPE_CONFIGURATION: u8 = 2;
pub const DESCRIPTOR_TYPE_INTERFACE: u8 = 4;
pub const DESCRIPTOR_TYPE_ENDPOINT: u8 = 5;
pub const ENDPOINT_DESCRIPTOR_LEN: usize = 7;

pub const TRANSFER_TYPE_MASK: u8 = 0x03;
pub const TRANSFER_TYPE_CONTROL: u8 = 0;
pub const TRANSFER_TYPE_ISOCHRONOUS: u8 = 1;
pub const TRANSFER_TYPE_BULK: u8 = 2;
pub const TRANSFER_TYPE_INTERRUPT: u8 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Endpoint {
    /// Raw `bEndpointAddress`, direction bit included.
    pub address: u8,
    pub attributes: u8,
    pub max_packet_size: u16,
    pub interval: u8,
    /// The interface this endpoint belongs to, for logging.
    pub interface: u8,
}

impl Endpoint {
    #[must_use]
    pub const fn number(&self) -> u8 {
        self.address & 0x0f
    }

    #[must_use]
    pub const fn is_input(&self) -> bool {
        self.address & 0x80 != 0
    }

    #[must_use]
    pub const fn transfer_type(&self) -> u8 {
        self.attributes & TRANSFER_TYPE_MASK
    }

    #[must_use]
    pub const fn is_isochronous(&self) -> bool {
        self.transfer_type() == TRANSFER_TYPE_ISOCHRONOUS
    }

    /// Rebuilds the seven-byte descriptor raw-gadget wants for EP_ENABLE.
    #[must_use]
    pub fn to_descriptor(&self) -> [u8; ENDPOINT_DESCRIPTOR_LEN] {
        let size = self.max_packet_size.to_le_bytes();
        [
            ENDPOINT_DESCRIPTOR_LEN as u8,
            DESCRIPTOR_TYPE_ENDPOINT,
            self.address,
            self.attributes,
            size[0],
            size[1],
            self.interval,
        ]
    }
}

/// Collects every endpoint in a configuration, in descriptor order. Alternate
/// settings other than zero are skipped: the host selects them explicitly with
/// SET_INTERFACE, and enabling them up front would exhaust the UDC.
#[must_use]
pub fn endpoints(configuration: &[u8]) -> Vec<Endpoint> {
    let mut found = Vec::new();
    let mut offset = 0_usize;
    let mut interface = 0_u8;
    let mut alternate = 0_u8;
    while offset + 2 <= configuration.len() {
        let length = usize::from(configuration[offset]);
        let kind = configuration[offset + 1];
        if length < 2 || offset + length > configuration.len() {
            break;
        }
        match kind {
            DESCRIPTOR_TYPE_INTERFACE if length >= 4 => {
                interface = configuration[offset + 2];
                alternate = configuration[offset + 3];
            }
            DESCRIPTOR_TYPE_ENDPOINT if length >= ENDPOINT_DESCRIPTOR_LEN && alternate == 0 => {
                found.push(Endpoint {
                    address: configuration[offset + 2],
                    attributes: configuration[offset + 3],
                    max_packet_size: u16::from_le_bytes([
                        configuration[offset + 4],
                        configuration[offset + 5],
                    ]),
                    interval: configuration[offset + 6],
                    interface,
                });
            }
            _ => {}
        }
        offset += length;
    }
    found
}

/// Total configuration length from the nine-byte header, so the full
/// descriptor can be fetched in a second request.
#[must_use]
pub fn total_length(header: &[u8]) -> Option<u16> {
    if header.len() < 4 || header[1] != DESCRIPTOR_TYPE_CONFIGURATION {
        return None;
    }
    Some(u16::from_le_bytes([header[2], header[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Config header, one interface, a bulk OUT and a bulk IN endpoint.
    fn usbmux_like() -> Vec<u8> {
        let mut bytes = vec![9, DESCRIPTOR_TYPE_CONFIGURATION, 32, 0, 1, 1, 0, 0x80, 250];
        bytes.extend_from_slice(&[9, DESCRIPTOR_TYPE_INTERFACE, 0, 0, 2, 0xff, 0xfe, 0x02, 0]);
        bytes.extend_from_slice(&[7, DESCRIPTOR_TYPE_ENDPOINT, 0x02, 0x02, 0x00, 0x02, 0]);
        bytes.extend_from_slice(&[7, DESCRIPTOR_TYPE_ENDPOINT, 0x85, 0x02, 0x00, 0x02, 0]);
        bytes
    }

    #[test]
    fn finds_bulk_endpoints_and_their_directions() {
        let eps = endpoints(&usbmux_like());
        assert_eq!(eps.len(), 2);
        assert_eq!(eps[0].number(), 2);
        assert!(!eps[0].is_input());
        assert_eq!(eps[0].transfer_type(), TRANSFER_TYPE_BULK);
        assert_eq!(eps[0].max_packet_size, 512);
        assert_eq!(eps[1].number(), 5);
        assert!(eps[1].is_input());
        assert_eq!(eps[1].interface, 0);
    }

    #[test]
    fn descriptor_round_trips() {
        let eps = endpoints(&usbmux_like());
        let rebuilt = eps[1].to_descriptor();
        assert_eq!(rebuilt[1], DESCRIPTOR_TYPE_ENDPOINT);
        assert_eq!(rebuilt[2], 0x85);
        assert_eq!(u16::from_le_bytes([rebuilt[4], rebuilt[5]]), 512);
        assert_eq!(endpoints(&{
            let mut c = usbmux_like();
            c.truncate(9 + 9);
            c
        }).len(), 0);
    }

    #[test]
    fn skips_alternate_settings_so_the_udc_is_not_exhausted() {
        let mut config = usbmux_like();
        // alt setting 1 with its own endpoint must be ignored
        config.extend_from_slice(&[9, DESCRIPTOR_TYPE_INTERFACE, 0, 1, 1, 0xff, 0xfe, 0x02, 0]);
        config.extend_from_slice(&[7, DESCRIPTOR_TYPE_ENDPOINT, 0x87, 0x01, 0x00, 0x04, 1]);
        assert_eq!(endpoints(&config).len(), 2);
    }

    #[test]
    fn stops_on_a_truncated_or_malformed_descriptor() {
        assert_eq!(endpoints(&[9, DESCRIPTOR_TYPE_CONFIGURATION]).len(), 0);
        assert_eq!(endpoints(&[0, 0, 0, 0]).len(), 0);
        assert_eq!(endpoints(&[]).len(), 0);
    }

    #[test]
    fn reads_total_length_from_the_header() {
        assert_eq!(total_length(&usbmux_like()), Some(32));
        assert_eq!(total_length(&[9, 1, 0, 0]), None);
        assert_eq!(total_length(&[9]), None);
    }

    #[test]
    fn identifies_isochronous_endpoints_the_bridge_cannot_carry() {
        let iso = Endpoint {
            address: 0x81,
            attributes: TRANSFER_TYPE_ISOCHRONOUS,
            max_packet_size: 1024,
            interval: 1,
            interface: 1,
        };
        assert!(iso.is_isochronous());
    }
}
