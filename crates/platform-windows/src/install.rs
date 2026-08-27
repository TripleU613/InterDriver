//! Creates the root-enumerated devnode the UdeCx driver binds to.
//!
//! `pnputil /add-driver /install` only stages a driver package; a
//! root-enumerated virtual controller has no hardware to match it, so the
//! devnode must be created explicitly. This is what `devcon install` does, and
//! devcon is not shipped with Windows.

use std::ffi::OsStr;
use std::io;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DICD_GENERATE_ID, DIF_REGISTERDEVICE, HDEVINFO, SP_DEVINFO_DATA,
    SetupDiCallClassInstaller, SetupDiCreateDeviceInfoList, SetupDiCreateDeviceInfoW,
    SetupDiDestroyDeviceInfoList, SetupDiSetDeviceRegistryPropertyW, SPDRP_HARDWAREID,
};
use windows_sys::core::GUID;

/// `{4d36e97d-e325-11ce-bfc1-08002be10318}` — the System device class, which is
/// what a virtual host controller installs under.
const GUID_DEVCLASS_SYSTEM: GUID = GUID {
    data1: 0x4d36e97d,
    data2: 0xe325,
    data3: 0x11ce,
    data4: [0xbf, 0xc1, 0x08, 0x00, 0x2b, 0xe1, 0x03, 0x18],
};

const INSTALLFLAG_FORCE: u32 = 0x0000_0001;

#[link(name = "newdev")]
unsafe extern "system" {
    fn UpdateDriverForPlugAndPlayDevicesW(
        parent: *mut core::ffi::c_void,
        hardware_id: *const u16,
        inf_path: *const u16,
        flags: u32,
        reboot_required: *mut i32,
    ) -> i32;
}

fn wide(text: &str) -> Vec<u16> {
    OsStr::new(text).encode_wide().chain(once(0)).collect()
}

/// A REG_MULTI_SZ needs a second terminating NUL after the string.
fn multi_sz(text: &str) -> Vec<u16> {
    let mut bytes = wide(text);
    bytes.push(0);
    bytes
}

struct DeviceInfoList(HDEVINFO);

impl Drop for DeviceInfoList {
    fn drop(&mut self) {
        unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

/// Creates the `root\interdriverude` devnode and binds the driver package at
/// `inf_path` to it. Returns whether Windows asked for a reboot.
pub fn install_root_device(hardware_id: &str, inf_path: &str) -> io::Result<bool> {
    let list = unsafe {
        SetupDiCreateDeviceInfoList(&GUID_DEVCLASS_SYSTEM, std::ptr::null_mut())
    };
    if list == -1 {
        return Err(io::Error::last_os_error());
    }
    let list = DeviceInfoList(list);

    let mut info = SP_DEVINFO_DATA {
        cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
        ClassGuid: GUID_DEVCLASS_SYSTEM,
        DevInst: 0,
        Reserved: 0,
    };
    let class_name = wide("System");
    let created = unsafe {
        SetupDiCreateDeviceInfoW(
            list.0,
            class_name.as_ptr(),
            &GUID_DEVCLASS_SYSTEM,
            std::ptr::null(),
            std::ptr::null_mut(),
            DICD_GENERATE_ID,
            &mut info,
        )
    };
    if created == 0 {
        return Err(io::Error::last_os_error());
    }

    let hwid = multi_sz(hardware_id);
    let bytes = unsafe {
        std::slice::from_raw_parts(hwid.as_ptr().cast::<u8>(), hwid.len() * size_of::<u16>())
    };
    let set = unsafe {
        SetupDiSetDeviceRegistryPropertyW(
            list.0,
            &mut info,
            SPDRP_HARDWAREID,
            bytes.as_ptr(),
            bytes.len() as u32,
        )
    };
    if set == 0 {
        return Err(io::Error::last_os_error());
    }

    let registered =
        unsafe { SetupDiCallClassInstaller(DIF_REGISTERDEVICE, list.0, &mut info) };
    if registered == 0 {
        return Err(io::Error::last_os_error());
    }

    let hwid_single = wide(hardware_id);
    let inf = wide(inf_path);
    let mut reboot = 0_i32;
    let updated = unsafe {
        UpdateDriverForPlugAndPlayDevicesW(
            std::ptr::null_mut(),
            hwid_single.as_ptr(),
            inf.as_ptr(),
            INSTALLFLAG_FORCE,
            &mut reboot,
        )
    };
    if updated == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(reboot != 0)
}
