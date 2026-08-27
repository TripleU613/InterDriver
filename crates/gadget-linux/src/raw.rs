//! `/dev/raw-gadget` ioctl layer.
//!
//! raw-gadget lets a userspace process act as a USB *device* on a UDC. The
//! kernel hands us every control request on endpoint 0 and lets us open the
//! non-zero endpoints the UDC provides, so an arbitrary device can be emulated
//! without writing a gadget driver.
//!
//! ABI mirrors `include/uapi/linux/usb/raw_gadget.h`.

use std::ffi::c_void;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};

pub const UDC_NAME_LEN: usize = 128;
pub const MAX_EP_NAME: usize = 16;
pub const EPS_NUM: usize = 30;
pub const CONTROL_BUFFER: usize = 4096;

const RAW_GADGET_TYPE: u64 = b'U' as u64;

const fn iow(number: u64, size: u64) -> u64 {
    (1 << 30) | (size << 16) | (RAW_GADGET_TYPE << 8) | number
}

const fn ior(number: u64, size: u64) -> u64 {
    (2 << 30) | (size << 16) | (RAW_GADGET_TYPE << 8) | number
}

const fn iowr(number: u64, size: u64) -> u64 {
    (3 << 30) | (size << 16) | (RAW_GADGET_TYPE << 8) | number
}

const fn io_only(number: u64) -> u64 {
    (RAW_GADGET_TYPE << 8) | number
}

/// struct usb_raw_init { __u8 driver_name[128]; __u8 device_name[128]; __u8 speed; }
const USB_RAW_IOCTL_INIT: u64 = iow(0, 257);
const USB_RAW_IOCTL_RUN: u64 = io_only(1);
/// struct usb_raw_event { __u32 type; __u32 length; __u8 data[]; }
const USB_RAW_IOCTL_EVENT_FETCH: u64 = ior(2, 8);
/// struct usb_raw_ep_io { __u16 ep; __u16 flags; __u32 length; __u8 data[]; }
const USB_RAW_IOCTL_EP0_WRITE: u64 = iow(3, 8);
const USB_RAW_IOCTL_EP0_READ: u64 = iowr(4, 8);
/// struct usb_endpoint_descriptor is packed and 9 bytes (audio adds two).
const USB_RAW_IOCTL_EP_ENABLE: u64 = iow(5, 9);
const USB_RAW_IOCTL_EP_DISABLE: u64 = iow(6, 4);
const USB_RAW_IOCTL_EP_WRITE: u64 = iow(7, 8);
const USB_RAW_IOCTL_EP_READ: u64 = iowr(8, 8);
const USB_RAW_IOCTL_CONFIGURE: u64 = io_only(9);
const USB_RAW_IOCTL_VBUS_DRAW: u64 = iow(10, 4);
/// struct usb_raw_eps_info { struct usb_raw_ep_info eps[30]; }
const USB_RAW_IOCTL_EPS_INFO: u64 = ior(11, (EPS_NUM * 32) as u64);

pub const USB_RAW_EVENT_INVALID: u32 = 0;
pub const USB_RAW_EVENT_CONNECT: u32 = 1;
pub const USB_RAW_EVENT_CONTROL: u32 = 2;
pub const USB_RAW_EVENT_SUSPEND: u32 = 3;
pub const USB_RAW_EVENT_RESUME: u32 = 4;
pub const USB_RAW_EVENT_RESET: u32 = 5;
pub const USB_RAW_EVENT_DISCONNECT: u32 = 6;

pub const USB_SPEED_FULL: u8 = 2;
pub const USB_SPEED_HIGH: u8 = 3;
pub const USB_SPEED_SUPER: u8 = 5;

#[repr(C)]
pub struct Init {
    pub driver_name: [u8; UDC_NAME_LEN],
    pub device_name: [u8; UDC_NAME_LEN],
    pub speed: u8,
}

/// Header shared by every raw-gadget transfer ioctl.
#[repr(C)]
pub struct EpIo {
    pub ep: u16,
    pub flags: u16,
    pub length: u32,
    // followed by `length` bytes of payload
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct EpCaps {
    pub bits: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct EpLimits {
    pub maxpacket_limit: u16,
    pub max_streams: u16,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct EpInfo {
    pub name: [u8; MAX_EP_NAME],
    pub addr: u32,
    pub caps: EpCaps,
    pub limits: EpLimits,
}

impl Default for EpInfo {
    fn default() -> Self {
        Self {
            name: [0; MAX_EP_NAME],
            addr: 0,
            caps: EpCaps::default(),
            limits: EpLimits::default(),
        }
    }
}

#[repr(C)]
pub struct EpsInfo {
    pub eps: [EpInfo; EPS_NUM],
}

impl EpCaps {
    #[must_use]
    pub const fn type_control(self) -> bool {
        self.bits & (1 << 0) != 0
    }
    #[must_use]
    pub const fn type_iso(self) -> bool {
        self.bits & (1 << 1) != 0
    }
    #[must_use]
    pub const fn type_bulk(self) -> bool {
        self.bits & (1 << 2) != 0
    }
    #[must_use]
    pub const fn type_int(self) -> bool {
        self.bits & (1 << 3) != 0
    }
    /// Kernel bitfield order is control, iso, bulk, int, dir_in, dir_out — so
    /// dir_in is bit 4 and dir_out bit 5. Verified against dummy_udc, whose
    /// `epNin-bulk` entries must report `in`.
    #[must_use]
    pub const fn dir_in(self) -> bool {
        self.bits & (1 << 4) != 0
    }
    #[must_use]
    pub const fn dir_out(self) -> bool {
        self.bits & (1 << 5) != 0
    }
}

fn check(result: i32) -> io::Result<i32> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

fn ioctl(fd: BorrowedFd<'_>, request: u64, arg: *mut c_void) -> io::Result<i32> {
    check(unsafe { libc::ioctl(fd.as_raw_fd(), request, arg) })
}

pub fn init(fd: BorrowedFd<'_>, driver: &str, device: &str, speed: u8) -> io::Result<()> {
    let mut request = Init {
        driver_name: [0; UDC_NAME_LEN],
        device_name: [0; UDC_NAME_LEN],
        speed,
    };
    copy_name(&mut request.driver_name, driver)?;
    copy_name(&mut request.device_name, device)?;
    ioctl(
        fd,
        USB_RAW_IOCTL_INIT,
        std::ptr::from_mut(&mut request).cast(),
    )?;
    Ok(())
}

pub fn run(fd: BorrowedFd<'_>) -> io::Result<()> {
    ioctl(fd, USB_RAW_IOCTL_RUN, std::ptr::null_mut())?;
    Ok(())
}

pub fn configure(fd: BorrowedFd<'_>) -> io::Result<()> {
    ioctl(fd, USB_RAW_IOCTL_CONFIGURE, std::ptr::null_mut())?;
    Ok(())
}

pub fn vbus_draw(fd: BorrowedFd<'_>, milliamps: u32) -> io::Result<()> {
    let mut value = milliamps;
    ioctl(fd, USB_RAW_IOCTL_VBUS_DRAW, std::ptr::from_mut(&mut value).cast())?;
    Ok(())
}

pub fn eps_info(fd: BorrowedFd<'_>) -> io::Result<Vec<EpInfo>> {
    let mut info = EpsInfo {
        eps: [EpInfo::default(); EPS_NUM],
    };
    let count = ioctl(fd, USB_RAW_IOCTL_EPS_INFO, std::ptr::from_mut(&mut info).cast())?;
    Ok(info.eps[..count as usize].to_vec())
}

/// A fetched raw-gadget event. `data` holds the setup packet for a CONTROL
/// event.
pub struct Event {
    pub kind: u32,
    pub data: Vec<u8>,
}

pub fn event_fetch(fd: BorrowedFd<'_>) -> io::Result<Event> {
    // struct usb_raw_event { u32 type; u32 length; u8 data[]; }
    let mut buffer = vec![0_u8; 8 + CONTROL_BUFFER];
    let length = (CONTROL_BUFFER as u32).to_ne_bytes();
    buffer[4..8].copy_from_slice(&length);
    ioctl(
        fd,
        USB_RAW_IOCTL_EVENT_FETCH,
        buffer.as_mut_ptr().cast(),
    )?;
    let kind = u32::from_ne_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
    let len = u32::from_ne_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]) as usize;
    let end = (8 + len).min(buffer.len());
    Ok(Event {
        kind,
        data: buffer[8..end].to_vec(),
    })
}

fn transfer(fd: BorrowedFd<'_>, request: u64, ep: u16, data: &mut [u8]) -> io::Result<usize> {
    // usb_raw_ep_io is a header immediately followed by the payload, so the
    // header and data must live in one allocation.
    let mut buffer = vec![0_u8; 8 + data.len()];
    buffer[0..2].copy_from_slice(&ep.to_ne_bytes());
    buffer[2..4].copy_from_slice(&0_u16.to_ne_bytes());
    buffer[4..8].copy_from_slice(&(data.len() as u32).to_ne_bytes());
    buffer[8..].copy_from_slice(data);
    let transferred = ioctl(fd, request, buffer.as_mut_ptr().cast())? as usize;
    let copied = transferred.min(data.len());
    data[..copied].copy_from_slice(&buffer[8..8 + copied]);
    Ok(transferred)
}

pub fn ep0_read(fd: BorrowedFd<'_>, data: &mut [u8]) -> io::Result<usize> {
    transfer(fd, USB_RAW_IOCTL_EP0_READ, 0, data)
}

pub fn ep0_write(fd: BorrowedFd<'_>, data: &[u8]) -> io::Result<usize> {
    let mut owned = data.to_vec();
    transfer(fd, USB_RAW_IOCTL_EP0_WRITE, 0, &mut owned)
}

pub fn ep_read(fd: BorrowedFd<'_>, handle: u16, data: &mut [u8]) -> io::Result<usize> {
    transfer(fd, USB_RAW_IOCTL_EP_READ, handle, data)
}

pub fn ep_write(fd: BorrowedFd<'_>, handle: u16, data: &[u8]) -> io::Result<usize> {
    let mut owned = data.to_vec();
    transfer(fd, USB_RAW_IOCTL_EP_WRITE, handle, &mut owned)
}

/// Enables a non-zero endpoint from its 7-byte endpoint descriptor; returns the
/// handle used by `ep_read`/`ep_write`.
///
/// `struct usb_endpoint_descriptor` is nine bytes because audio endpoints carry
/// bRefresh and bSynchAddress, and the ioctl copies all nine from user space,
/// so the descriptor is padded rather than passed as the seven bytes a
/// non-audio endpoint uses.
pub fn ep_enable(fd: BorrowedFd<'_>, descriptor: &[u8; 7]) -> io::Result<u16> {
    let mut buffer = [0_u8; 9];
    buffer[..7].copy_from_slice(descriptor);
    let handle = ioctl(fd, USB_RAW_IOCTL_EP_ENABLE, buffer.as_mut_ptr().cast())?;
    Ok(handle as u16)
}

pub fn ep_disable(fd: BorrowedFd<'_>, handle: u16) -> io::Result<()> {
    let mut value = u32::from(handle);
    ioctl(fd, USB_RAW_IOCTL_EP_DISABLE, std::ptr::from_mut(&mut value).cast())?;
    Ok(())
}

fn copy_name(slot: &mut [u8; UDC_NAME_LEN], name: &str) -> io::Result<()> {
    let bytes = name.as_bytes();
    if bytes.len() >= UDC_NAME_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "name too long"));
    }
    slot[..bytes.len()].copy_from_slice(bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_raw_gadget_uapi() {
        assert_eq!(USB_RAW_IOCTL_INIT, 0x4101_5500);
        assert_eq!(USB_RAW_IOCTL_RUN, 0x0000_5501);
        assert_eq!(USB_RAW_IOCTL_EVENT_FETCH, 0x8008_5502);
        assert_eq!(USB_RAW_IOCTL_EP0_WRITE, 0x4008_5503);
        assert_eq!(USB_RAW_IOCTL_EP0_READ, 0xC008_5504);
        assert_eq!(USB_RAW_IOCTL_EP_ENABLE, 0x4009_5505);
        assert_eq!(USB_RAW_IOCTL_EP_DISABLE, 0x4004_5506);
        assert_eq!(USB_RAW_IOCTL_EP_WRITE, 0x4008_5507);
        assert_eq!(USB_RAW_IOCTL_EP_READ, 0xC008_5508);
        assert_eq!(USB_RAW_IOCTL_CONFIGURE, 0x0000_5509);
        assert_eq!(USB_RAW_IOCTL_VBUS_DRAW, 0x4004_550A);
        assert_eq!(USB_RAW_IOCTL_EPS_INFO, 0x83C0_550B);
    }

    #[test]
    fn ep_caps_decode_direction_and_type() {
        let bulk_in = EpCaps { bits: (1 << 2) | (1 << 4) };
        assert!(bulk_in.type_bulk() && bulk_in.dir_in() && !bulk_in.dir_out());
        let iso_out = EpCaps { bits: (1 << 1) | (1 << 5) };
        assert!(iso_out.type_iso() && iso_out.dir_out() && !iso_out.dir_in());
    }

    #[test]
    fn struct_sizes_match_the_kernel_abi() {
        assert_eq!(size_of::<Init>(), 257);
        assert_eq!(size_of::<EpInfo>(), 32);
    }
}
