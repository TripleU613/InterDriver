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

/// Interface number from a hardware id or device-interface path segment such as
/// `...&mi_01#...`, i.e. the `MI_xx` suffix.
#[must_use]
pub fn parse_interface_number(text: &str) -> Option<u8> {
    let upper = text.to_ascii_uppercase();
    let start = upper.find("MI_")? + "MI_".len();
    let digits: String = upper[start..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .take(2)
        .collect();
    u8::from_str_radix(&digits, 16).ok()
}

/// The devnode portion of an interface path: everything before the trailing
/// `#{guid}`. One devnode can publish several interface classes, which surfaces
/// the same physical interface under several paths differing only in that
/// suffix.
#[must_use]
pub fn devnode_of(path: &str) -> String {
    let trimmed = match path.rfind('#') {
        Some(index) => &path[..index],
        None => path,
    };
    trimmed.to_ascii_lowercase()
}

/// Windows has no USB/IP bus IDs, so the interface path's instance segment is
/// sanitised into a stable identifier that `open` can match back to the path.
/// `8&10379175&1&0001` becomes `8_10379175_1_0001`.
#[must_use]
pub fn bus_id_from_path(path: &str) -> String {
    let instance = path
        .split('#')
        .nth(2)
        .or_else(|| path.split('#').nth(1))
        .unwrap_or(path);
    instance
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(31)
        .collect()
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
    fn reads_interface_number_from_a_path_segment() {
        assert_eq!(
            parse_interface_number(r"\\?\usb#vid_05ac&pid_12a8&mi_01#8&x#{g}"),
            Some(1)
        );
        assert_eq!(parse_interface_number("USB\\VID_05AC&PID_12A8"), None);
    }

    #[test]
    fn one_devnode_published_under_two_guids_collapses_to_one() {
        let winusb = r"\\?\usb#vid_05ac&pid_12a8&mi_01#8&10379175&1&0001#{dee824ef-729b-4a0e-9c14-b7117d33a817}";
        let apple = r"\\?\usb#vid_05ac&pid_12a8&mi_01#8&10379175&1&0001#{664be590-54bd-4964-8a8c-6cd1314f6dc2}";
        assert_eq!(devnode_of(winusb), devnode_of(apple));
        let other = r"\\?\usb#vid_05ac&pid_12a8&mi_01#8&99999999&1&0001#{dee824ef-729b-4a0e-9c14-b7117d33a817}";
        assert_ne!(devnode_of(winusb), devnode_of(other));
    }

    #[test]
    fn derives_a_stable_bus_id_from_an_interface_path() {
        let path = r"\\?\usb#vid_05ac&pid_12a8&mi_01#8&10379175&1&0001#{dee824ef-729b-4a0e-9c14-b7117d33a817}";
        assert_eq!(bus_id_from_path(path), "8_10379175_1_0001");
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
