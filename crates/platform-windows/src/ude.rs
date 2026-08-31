//! Drives the InterDriver UdeCx virtual host controller from user space.
//!
//! The kernel shim owns the emulated controller; this side supplies the
//! descriptors captured from the real remote device and asks the driver to
//! attach it to the root port.

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::core::GUID;

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;

/// Must match `interdriver_ude_ioctl.h`.
const DEVICE_TYPE: u32 = 0x8123;
const METHOD_BUFFERED: u32 = 0;
const FILE_WRITE_ACCESS: u32 = 2;

const fn ctl_code(function: u32) -> u32 {
    (DEVICE_TYPE << 16) | (FILE_WRITE_ACCESS << 14) | (function << 2) | METHOD_BUFFERED
}

const IOCTL_PLUG_IN: u32 = ctl_code(0x800);
const IOCTL_UNPLUG: u32 = ctl_code(0x801);
/// FETCH uses FILE_READ_ACCESS, so its control code differs in the access bits.
const IOCTL_FETCH_URB: u32 = (DEVICE_TYPE << 16) | (1 << 14) | (0x802 << 2) | METHOD_BUFFERED;
const IOCTL_COMPLETE_URB: u32 = ctl_code(0x803);

pub const MAX_TRANSFER: usize = 65536;

/// One endpoint-0 transfer the driver wants answered.
pub struct Urb {
    pub id: u32,
    pub transfer_length: u32,
    pub setup: [u8; 8],
    /// Payload for a host-to-device request; empty otherwise.
    pub data: Vec<u8>,
}

/// A live handle on the controller, kept open across the URB pump so the
/// driver is not reopened per transfer.
pub struct Controller {
    handle: Handle,
}

impl Controller {
    pub fn open_default() -> io::Result<Self> {
        let path = find_controller()?;
        Ok(Self { handle: open(&path)? })
    }

    /// Blocks in the driver until a transfer is available.
    pub fn fetch(&self) -> io::Result<Urb> {
        let mut buffer = vec![0_u8; 16 + MAX_TRANSFER];
        let mut returned = 0_u32;
        let ok = unsafe {
            DeviceIoControl(
                self.handle.0,
                IOCTL_FETCH_URB,
                std::ptr::null(),
                0,
                buffer.as_mut_ptr().cast::<c_void>(),
                buffer.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if (returned as usize) < 20 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "short URB header"));
        }
        let id = u32::from_le_bytes(buffer[0..4].try_into().unwrap());
        let transfer_length = u32::from_le_bytes(buffer[4..8].try_into().unwrap());
        let data_length = u32::from_le_bytes(buffer[8..12].try_into().unwrap()) as usize;
        let mut setup = [0_u8; 8];
        setup.copy_from_slice(&buffer[12..20]);
        let data = buffer[20..20 + data_length.min(returned as usize - 20)].to_vec();
        Ok(Urb {
            id,
            transfer_length,
            setup,
            data,
        })
    }

    /// Hands the device's answer back to the driver.
    pub fn complete(&self, id: u32, status: i32, data: &[u8]) -> io::Result<()> {
        let mut buffer = Vec::with_capacity(12 + data.len());
        buffer.extend_from_slice(&id.to_le_bytes());
        buffer.extend_from_slice(&status.to_le_bytes());
        buffer.extend_from_slice(&(data.len() as u32).to_le_bytes());
        buffer.extend_from_slice(data);
        let mut returned = 0_u32;
        let ok = unsafe {
            DeviceIoControl(
                self.handle.0,
                IOCTL_COMPLETE_URB,
                buffer.as_ptr().cast::<c_void>(),
                buffer.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

pub const SPEED_FULL: u32 = 1;
pub const SPEED_HIGH: u32 = 2;
pub const SPEED_SUPER: u32 = 3;

/// `{A1C0FFEE-5E11-4C0D-9F3A-1B2C3D4E5F60}` — the device interface the driver
/// publishes, matching `interdriver_ude_ioctl.h`.
const GUID_DEVINTERFACE_INTERDRIVER_UDE: GUID = GUID {
    data1: 0xa1c0_ffee,
    data2: 0x5e11,
    data3: 0x4c0d,
    data4: [0x9f, 0x3a, 0x1b, 0x2c, 0x3d, 0x4e, 0x5f, 0x60],
};

/// Finds the running controller's device-interface path.
pub fn find_controller() -> io::Result<String> {
    crate::enumerate::first_interface_path(&GUID_DEVINTERFACE_INTERDRIVER_UDE)?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no InterDriver virtual controller present; is the driver installed?",
            )
        })
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn open(path: &str) -> io::Result<Handle> {
    let wide: Vec<u16> = std::ffi::OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(Handle(handle))
}

/// Attaches a device described by the given descriptors to the virtual
/// controller. Both descriptors are the real device's bytes, unmodified.
pub fn plug_in(
    controller_path: Option<&str>,
    speed: u32,
    device_descriptor: &[u8],
    config_descriptor: &[u8],
) -> io::Result<()> {
    let path = match controller_path {
        Some(path) => path.to_owned(),
        None => find_controller()?,
    };
    let handle = open(&path)?;

    // struct INTERDRIVER_UDE_PLUG_IN_REQUEST followed by both descriptors.
    let mut buffer = Vec::with_capacity(16 + device_descriptor.len() + config_descriptor.len());
    let size = (16 + device_descriptor.len() + config_descriptor.len()) as u32;
    buffer.extend_from_slice(&size.to_le_bytes());
    buffer.extend_from_slice(&speed.to_le_bytes());
    buffer.extend_from_slice(&(device_descriptor.len() as u32).to_le_bytes());
    buffer.extend_from_slice(&(config_descriptor.len() as u32).to_le_bytes());
    buffer.extend_from_slice(device_descriptor);
    buffer.extend_from_slice(config_descriptor);

    let mut returned = 0_u32;
    let ok = unsafe {
        DeviceIoControl(
            handle.0,
            IOCTL_PLUG_IN,
            buffer.as_ptr().cast::<c_void>(),
            buffer.len() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn unplug(controller_path: Option<&str>) -> io::Result<()> {
    let path = match controller_path {
        Some(path) => path.to_owned(),
        None => find_controller()?,
    };
    let handle = open(&path)?;
    let mut returned = 0_u32;
    let ok = unsafe {
        DeviceIoControl(
            handle.0,
            IOCTL_UNPLUG,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
