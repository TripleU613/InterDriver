#![no_std]

pub const DEVICE_TYPE: u32 = 0x8124;
pub const METHOD_BUFFERED: u32 = 0;
pub const FILE_READ_ACCESS: u32 = 1;
pub const FILE_WRITE_ACCESS: u32 = 2;
pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_TRANSFER: usize = 1024 * 1024;

pub const fn ctl_code(function: u32, access: u32) -> u32 {
    (DEVICE_TYPE << 16) | (access << 14) | (function << 2) | METHOD_BUFFERED
}

pub const IOCTL_GET_INFO: u32 = ctl_code(0x800, FILE_READ_ACCESS);
pub const IOCTL_TRANSFER: u32 = ctl_code(0x801, FILE_READ_ACCESS | FILE_WRITE_ACCESS);
pub const IOCTL_ABORT_ENDPOINT: u32 = ctl_code(0x802, FILE_WRITE_ACCESS);

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ExportInfo {
    pub size: u32,
    pub version: u32,
    pub speed: u32,
    pub flags: u32,
    pub device_descriptor: [u8; 18],
    pub reserved: [u8; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ExportTransfer {
    pub size: u32,
    pub endpoint_address: u32,
    pub transfer_length: u32,
    pub timeout_ms: u32,
    pub status: i32,
    pub actual_length: u32,
    pub setup: [u8; 8],
    pub reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct ExportAbort {
    pub size: u32,
    pub endpoint_number: u32,
}

const _: () = assert!(size_of::<ExportInfo>() == 36);
const _: () = assert!(size_of::<ExportTransfer>() == 36);
const _: () = assert!(size_of::<ExportAbort>() == 8);
