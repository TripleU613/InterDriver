use crate::ids::{bus_id_from_path, devnode_of, parse_hardware_id, parse_interface_number};
use std::io;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SP_DEVICE_INTERFACE_DATA,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::core::GUID;

/// Device-interface classes worth trying, in priority order.
///
/// A WinUSB-bound function is normally reachable through the generic WinUSB
/// class, but a device whose INF publishes its own `DeviceInterfaceGUIDs` is
/// only reachable through that GUID. An iPhone on Apple's stock driver
/// publishes `{664be590-...}` and has `WUDFRd`/`AppleKmdfFilter` above it, so
/// opening the generic WinUSB interface reports ERROR_NOT_SUPPORTED even though
/// the devnode's service really is WINUSB. `GUID_DEVINTERFACE_USB_DEVICE` is
/// deliberately absent: it returns composite parents owned by usbccgp, which
/// WinUSB cannot open at all.
const INTERFACE_CLASSES: [(&str, GUID); 3] = [
    (
        "winusb",
        GUID {
            data1: 0xdee824ef,
            data2: 0x729b,
            data3: 0x4a0e,
            data4: [0x9c, 0x14, 0xb7, 0x11, 0x7d, 0x33, 0xa8, 0x17],
        },
    ),
    (
        "apple-mobile-device",
        GUID {
            data1: 0x664be590,
            data2: 0x54bd,
            data3: 0x4964,
            data4: [0x8a, 0x8c, 0x6c, 0xd1, 0x31, 0x4f, 0x6d, 0xc2],
        },
    ),
    (
        "interdriver-winusb",
        GUID {
            data1: 0xd72d1a95,
            data2: 0x9a65,
            data3: 0x4f2b,
            data4: [0x8b, 0x17, 0x2f, 0x1a, 0x3d, 0x8b, 0x8a, 0x1c],
        },
    ),
];

/// One WinUSB-accessible interface, addressed by the device-interface path that
/// `CreateFileW` opens.
pub struct FoundDevice {
    pub bus_id: String,
    pub path: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub interface_number: u8,
    /// Which interface class surfaced this path, for diagnostics.
    pub class: &'static str,
}

struct DeviceInfoList(HDEVINFO);

impl Drop for DeviceInfoList {
    fn drop(&mut self) {
        unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

pub fn list_devices() -> io::Result<Vec<FoundDevice>> {
    let mut devices = Vec::new();
    for (name, guid) in INTERFACE_CLASSES {
        match list_by_class(name, &guid) {
            Ok(found) => devices.extend(found),
            Err(error) => eprintln!("enumerating {name}: {error}"),
        }
    }
    // One devnode can publish several interface classes, which surfaces the
    // same physical interface under several paths differing only in their GUID
    // suffix. Deduplicate on the devnode part so it is offered once.
    devices.sort_by(|left, right| left.bus_id.cmp(&right.bus_id));
    devices.dedup_by(|left, right| devnode_of(&left.path) == devnode_of(&right.path));
    Ok(devices)
}

fn list_by_class(class: &'static str, guid: &GUID) -> io::Result<Vec<FoundDevice>> {
    let handle = unsafe {
        SetupDiGetClassDevsW(
            guid,
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
            InterfaceClassGuid: *guid,
            Flags: 0,
            Reserved: 0,
        };
        let found = unsafe {
            SetupDiEnumDeviceInterfaces(list.0, std::ptr::null(), guid, index, &mut interface)
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
            class,
        });
    }
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
