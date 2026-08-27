//! Identifier parsing shared with the Windows enumerator. Kept free of Win32
//! so it can be tested on any host.

pub fn wide_string_at(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|unit| *unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Hardware IDs look like `USB\VID_05AC&PID_12A8&REV_0100`.
#[must_use]
pub fn parse_hardware_id(hardware_id: &str) -> Option<(u16, u16)> {
    let upper = hardware_id.to_ascii_uppercase();
    Some((field_after(&upper, "VID_")?, field_after(&upper, "PID_")?))
}

fn field_after(text: &str, marker: &str) -> Option<u16> {
    let start = text.find(marker)? + marker.len();
    let digits: String = text[start..]
        .chars()
        .take_while(char::is_ascii_hexdigit)
        .take(4)
        .collect();
    u16::from_str_radix(&digits, 16).ok()
}

/// Location information looks like `Port_#0002.Hub_#0003`, which maps onto the
/// hub and port pair that USB/IP bus IDs express as `3-2`.
#[must_use]
pub fn parse_location(location: &str) -> Option<(u32, u32)> {
    let port = number_after(location, "Port_#")?;
    let hub = number_after(location, "Hub_#")?;
    Some((hub, port))
}

fn number_after(text: &str, marker: &str) -> Option<u32> {
    let start = text.find(marker)? + marker.len();
    let digits: String = text[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_vendor_and_product_from_a_hardware_id() {
        assert_eq!(
            parse_hardware_id(r"USB\VID_05AC&PID_12A8&REV_0100"),
            Some((0x05ac, 0x12a8))
        );
        assert_eq!(parse_hardware_id(r"USB\VID_05ac&PID_12a8"), Some((0x05ac, 0x12a8)));
        assert_eq!(parse_hardware_id(r"USB\CLASS_09"), None);
    }

    #[test]
    fn maps_windows_location_onto_a_hub_port_pair() {
        assert_eq!(parse_location("Port_#0002.Hub_#0003"), Some((3, 2)));
        assert_eq!(parse_location("Port_#0011.Hub_#0001"), Some((1, 11)));
        assert_eq!(parse_location("nonsense"), None);
    }

    #[test]
    fn decodes_a_nul_terminated_utf16_property() {
        let bytes: Vec<u8> = "USB\0trailing"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(wide_string_at(&bytes), "USB");
    }
}
