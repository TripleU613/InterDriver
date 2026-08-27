use crate::ids::{parse_hardware_id, parse_interface_number};
use std::io;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SP_DEVICE_INTERFACE_DATA,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::core::GUID;

/// The device-interface class Windows registers for every WinUSB-bound
/// function, including the interface an iPhone exposes once `MI_01` is on
/// WinUSB. `GUID_DEVINTERFACE_USB_DEVICE` only returns composite parents, which
/// are owned by usbccgp and cannot be opened with WinUSB.
const GUID_DEVINTERFACE_WINUSB: GUID = GUID {
    data1: 0xdee824ef,
    data2: 0x729b,
    data3: 0x4a0e,
    data4: [0x9c, 0x14, 0xb7, 0x11, 0x7d, 0x33, 0xa8, 0x17],
};

/// One WinUSB-accessible interface, addressed by the device-interface path that
/// `CreateFileW` opens.
pub struct FoundDevice {
    pub bus_id: String,
    pub path: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub interface_number: u8,
}

struct DeviceInfoList(HDEVINFO);

impl Drop for DeviceInfoList {
    fn drop(&mut self) {
        unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

pub fn list_devices() -> io::Result<Vec<FoundDevice>> {
    let handle = unsafe {
        SetupDiGetClassDevsW(
            &GUID_DEVINTERFACE_WINUSB,
            std::ptr::null(),
            std::ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    if handle == -1 {
        return Err(io::Error::last_os_error());
    }
    let list = DeviceInfoList(handle);

    let mut devices = Vec::new();
    let mut index = 0_u32;
    loop {
        let mut interface = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            InterfaceClassGuid: GUID_DEVINTERFACE_WINUSB,
            Flags: 0,
            Reserved: 0,
        };
        let found = unsafe {
            SetupDiEnumDeviceInterfaces(
                list.0,
                std::ptr::null(),
                &GUID_DEVINTERFACE_WINUSB,
                index,
                &mut interface,
            )
        };
        if found == 0 {
            break;
        }
        index += 1;

        let Some(path) = interface_path(list.0, &mut interface)? else {
            continue;
        };
        let Some((vendor_id, product_id)) = parse_hardware_id(&path) else {
            continue;
        };
        let interface_number = parse_interface_number(&path).unwrap_or(0);
        devices.push(FoundDevice {
            bus_id: bus_id_from_path(&path),
            path,
            vendor_id,
            product_id,
            interface_number,
        });
    }
    devices.sort_by(|left, right| left.bus_id.cmp(&right.bus_id));
    Ok(devices)
}

fn interface_path(
    list: HDEVINFO,
    interface: *mut SP_DEVICE_INTERFACE_DATA,
) -> io::Result<Option<String>> {
    let mut required = 0_u32;
    unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            list,
            interface,
            std::ptr::null_mut(),
            0,
            &mut required,
            std::ptr::null_mut(),
        )
    };
    if required == 0 {
        return Ok(None);
    }

    // SP_DEVICE_INTERFACE_DETAIL_DATA_W is a u32 cbSize followed by the path.
    // The buffer is a u32 slice so the leading cbSize write is aligned; cbSize
    // itself describes only the fixed header (8 on 64-bit, 6 on 32-bit).
    let words = (required as usize).div_ceil(size_of::<u32>()) + 1;
    let mut buffer = vec![0_u32; words];
    buffer[0] = if cfg!(target_pointer_width = "64") { 8 } else { 6 };
    let ok = unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            list,
            interface,
            buffer.as_mut_ptr().cast(),
            required,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), buffer.len() * size_of::<u32>())
    };
    Ok(Some(crate::ids::wide_string_at(&bytes[size_of::<u32>()..])))
}

/// Windows has no USB/IP bus IDs, so the device-interface path's instance
/// segment is sanitised into a stable identifier that `open` can match back to
/// the same path. Example path segment `8&10379175&1&0001` becomes
/// `8_10379175_1_0001`.
fn bus_id_from_path(path: &str) -> String {
    let instance = path
        .split('#')
        .nth(2)
        .or_else(|| path.split('#').nth(1))
        .unwrap_or(path);
    let sanitized: String = instance
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    sanitized.chars().take(31).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_a_stable_bus_id_from_an_interface_path() {
        let path = r"\\?\usb#vid_05ac&pid_12a8&mi_01#8&10379175&1&0001#{dee824ef-729b-4a0e-9c14-b7117d33a817}";
        assert_eq!(bus_id_from_path(path), "8_10379175_1_0001");
    }
}
