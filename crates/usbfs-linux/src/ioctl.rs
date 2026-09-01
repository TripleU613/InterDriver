use std::ffi::c_void;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

const USBDEVFS_TYPE: u64 = 0x55;

const fn iowr(number: u64, size: u64) -> u64 {
    (3 << 30) | (size << 16) | (USBDEVFS_TYPE << 8) | number
}

const fn ior(number: u64, size: u64) -> u64 {
    (2 << 30) | (size << 16) | (USBDEVFS_TYPE << 8) | number
}

const USBDEVFS_CONTROL: u64 = iowr(0, 24);
const USBDEVFS_BULK: u64 = iowr(2, 24);
const USBDEVFS_CLAIMINTERFACE: u64 = ior(15, 4);
const USBDEVFS_RELEASEINTERFACE: u64 = ior(16, 4);
const USBDEVFS_IOCTL: u64 = iowr(18, 16);
const USBDEVFS_DISCONNECT: i32 = ((USBDEVFS_TYPE << 8) | 22) as i32;
const USBDEVFS_CONNECT: i32 = ((USBDEVFS_TYPE << 8) | 23) as i32;
const USBDEVFS_SETINTERFACE: u64 = ior(4, 8);
const USBDEVFS_SETCONFIGURATION: u64 = ior(5, 4);
const USBDEVFS_CLEAR_HALT: u64 = ior(21, 4);

#[repr(C)]
struct ControlTransfer {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
    timeout: u32,
    data: *mut c_void,
}

#[repr(C)]
struct BulkTransfer {
    endpoint: u32,
    length: u32,
    timeout: u32,
    data: *mut c_void,
}

#[repr(C)]
struct SetInterface {
    interface: u32,
    alternate: u32,
}

#[repr(C)]
struct DriverIoctl {
    interface: i32,
    code: i32,
    data: *mut c_void,
}

/// Result of a transfer: the kernel's byte count, or a negative USB/IP status.
pub enum Outcome {
    Transferred(usize),
    Failed(i32),
}

fn outcome(result: i32) -> Outcome {
    if result >= 0 {
        Outcome::Transferred(result as usize)
    } else {
        Outcome::Failed(-io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO))
    }
}

/// Setup packet fields are little endian on the wire; usbfs wants host order.
pub fn control(
    fd: BorrowedFd<'_>,
    setup: [u8; 8],
    buffer: &mut [u8],
    timeout: u32,
) -> Outcome {
    let mut transfer = ControlTransfer {
        request_type: setup[0],
        request: setup[1],
        value: u16::from_le_bytes([setup[2], setup[3]]),
        index: u16::from_le_bytes([setup[4], setup[5]]),
        length: u16::from_le_bytes([setup[6], setup[7]]),
        timeout,
        data: buffer.as_mut_ptr().cast(),
    };
    let result = unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            USBDEVFS_CONTROL,
            std::ptr::from_mut(&mut transfer),
        )
    };
    outcome(result)
}

pub fn bulk(fd: BorrowedFd<'_>, endpoint: u32, buffer: &mut [u8], timeout: u32) -> Outcome {
    let mut transfer = BulkTransfer {
        endpoint,
        length: buffer.len() as u32,
        timeout,
        data: buffer.as_mut_ptr().cast(),
    };
    let result = unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            USBDEVFS_BULK,
            std::ptr::from_mut(&mut transfer),
        )
    };
    outcome(result)
}

pub fn claim_interface(fd: BorrowedFd<'_>, interface: u32) -> io::Result<()> {
    let result = unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            USBDEVFS_CLAIMINTERFACE,
            std::ptr::from_ref(&interface),
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn release_interface(fd: BorrowedFd<'_>, interface: u32) -> io::Result<()> {
    let result = unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            USBDEVFS_RELEASEINTERFACE,
            std::ptr::from_ref(&interface),
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Detach whatever kernel driver currently owns the interface. Missing driver
/// reports ENODATA, which is success for our purposes.
pub fn detach_kernel_driver(fd: BorrowedFd<'_>, interface: i32) -> io::Result<()> {
    let mut request = DriverIoctl {
        interface,
        code: USBDEVFS_DISCONNECT,
        data: std::ptr::null_mut(),
    };
    let result = unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            USBDEVFS_IOCTL,
            std::ptr::from_mut(&mut request),
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENODATA) | Some(libc::ENOENT) => Ok(()),
            _ => Err(error),
        };
    }
    Ok(())
}

/// usbfs tracks the active altsetting itself, so a passed-through
/// SET_INTERFACE control transfer would leave its bookkeeping stale and every
/// later transfer on an alternate endpoint would fail with EINVAL.
pub fn set_interface(fd: BorrowedFd<'_>, interface: u32, alternate: u32) -> Outcome {
    let request = SetInterface {
        interface,
        alternate,
    };
    let result = unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            USBDEVFS_SETINTERFACE,
            std::ptr::from_ref(&request),
        )
    };
    if result < 0 { outcome(result) } else { Outcome::Transferred(0) }
}

pub fn set_configuration(fd: BorrowedFd<'_>, configuration: u32) -> Outcome {
    let result = unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            USBDEVFS_SETCONFIGURATION,
            std::ptr::from_ref(&configuration),
        )
    };
    if result < 0 { outcome(result) } else { Outcome::Transferred(0) }
}

pub fn clear_halt(fd: BorrowedFd<'_>, endpoint: u32) -> Outcome {
    let result = unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            USBDEVFS_CLEAR_HALT,
            std::ptr::from_ref(&endpoint),
        )
    };
    if result < 0 { outcome(result) } else { Outcome::Transferred(0) }
}

/// Hands the interface back to whatever kernel driver claimed it before the
/// export. Without this the device is left with no driver bound and, for a
/// webcam, no /dev/video node.
pub fn attach_kernel_driver(fd: BorrowedFd<'_>, interface: i32) -> io::Result<()> {
    let mut request = DriverIoctl {
        interface,
        code: USBDEVFS_CONNECT,
        data: std::ptr::null_mut(),
    };
    let result = unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            USBDEVFS_IOCTL,
            std::ptr::from_mut(&mut request),
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENODATA) | Some(libc::ENOENT) | Some(libc::EBUSY) => Ok(()),
            _ => Err(error),
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_usbdevice_fs_header() {
        assert_eq!(USBDEVFS_CONTROL, 0xC018_5500);
        assert_eq!(USBDEVFS_BULK, 0xC018_5502);
        assert_eq!(USBDEVFS_CLAIMINTERFACE, 0x8004_550F);
        assert_eq!(USBDEVFS_RELEASEINTERFACE, 0x8004_5510);
        assert_eq!(USBDEVFS_IOCTL, 0xC010_5512);
        assert_eq!(USBDEVFS_DISCONNECT, 0x5516);
        assert_eq!(USBDEVFS_CONNECT, 0x5517);
        assert_eq!(USBDEVFS_SETINTERFACE, 0x8008_5504);
        assert_eq!(USBDEVFS_SETCONFIGURATION, 0x8004_5505);
        assert_eq!(USBDEVFS_CLEAR_HALT, 0x8004_5515);
    }

    #[test]
    fn transfer_structs_match_kernel_layout() {
        assert_eq!(size_of::<ControlTransfer>(), 24);
        assert_eq!(size_of::<BulkTransfer>(), 24);
        assert_eq!(size_of::<DriverIoctl>(), 16);
        assert_eq!(size_of::<SetInterface>(), 8);
    }
}
