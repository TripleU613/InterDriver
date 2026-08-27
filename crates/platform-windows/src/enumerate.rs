use crate::ids::{parse_hardware_id, parse_location, wide_string_at};
use std::io;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SP_DEVICE_INTERFACE_DATA, SP_DEVINFO_DATA,
    SPDRP_HARDWAREID, SPDRP_LOCATION_INFORMATION, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
    SetupDiGetDeviceRegistryPropertyW,
};
use windows_sys::Win32::Devices::Usb::GUID_DEVINTERFACE_USB_DEVICE;

/// One physically attached USB device, as SetupAPI reports it.
pub struct FoundDevice {
    pub bus_id: String,
    pub path: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub hub: u32,
    pub port: u32,
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
            &GUID_DEVINTERFACE_USB_DEVICE,
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
            InterfaceClassGuid: GUID_DEVINTERFACE_USB_DEVICE,
            Flags: 0,
            Reserved: 0,
        };
        let found = unsafe {
            SetupDiEnumDeviceInterfaces(
                list.0,
                std::ptr::null(),
                &GUID_DEVINTERFACE_USB_DEVICE,
                index,
                &mut interface,
            )
        };
        if found == 0 {
            break;
        }
        index += 1;

        let mut info = SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ClassGuid: GUID_DEVINTERFACE_USB_DEVICE,
            DevInst: 0,
            Reserved: 0,
        };
        let Some(path) = interface_path(list.0, &mut interface, &mut info)? else {
            continue;
        };
        let Some(hardware_id) = registry_string(list.0, &mut info, SPDRP_HARDWAREID) else {
            continue;
        };
        let Some((vendor_id, product_id)) = parse_hardware_id(&hardware_id) else {
            continue;
        };
        let location = registry_string(list.0, &mut info, SPDRP_LOCATION_INFORMATION)
            .unwrap_or_default();
        let (hub, port) = parse_location(&location).unwrap_or((0, index));

        devices.push(FoundDevice {
            bus_id: format!("{hub}-{port}"),
            path,
            vendor_id,
            product_id,
            hub,
            port,
        });
    }
    devices.sort_by(|left, right| (left.hub, left.port).cmp(&(right.hub, right.port)));
    Ok(devices)
}

fn interface_path(
    list: HDEVINFO,
    interface: *mut SP_DEVICE_INTERFACE_DATA,
    info: *mut SP_DEVINFO_DATA,
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

    // SP_DEVICE_INTERFACE_DETAIL_DATA_W is a variable-length struct: a u32
    // cbSize followed by the path. The buffer is over-aligned to u32 so the
    // leading cbSize write is aligned, and cbSize itself must describe the
    // fixed header only, not the allocation.
    let words = (required as usize).div_ceil(size_of::<u32>()) + 1;
    let mut buffer = vec![0_u32; words];
    buffer[0] = if cfg!(target_pointer_width = "64") {
        8
    } else {
        6
    };
    let ok = unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            list,
            interface,
            buffer.as_mut_ptr().cast(),
            required,
            std::ptr::null_mut(),
            info,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), buffer.len() * size_of::<u32>())
    };
    Ok(Some(wide_string_at(&bytes[size_of::<u32>()..])))
}

fn registry_string(
    list: HDEVINFO,
    info: *mut SP_DEVINFO_DATA,
    property: u32,
) -> Option<String> {
    let mut required = 0_u32;
    unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            list,
            info,
            property,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
            &mut required,
        )
    };
    if required == 0 {
        return None;
    }
    let mut buffer = vec![0_u8; required as usize + 2];
    let ok = unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            list,
            info,
            property,
            std::ptr::null_mut(),
            buffer.as_mut_ptr(),
            required,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }
    Some(wide_string_at(&buffer))
}

