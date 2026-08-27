//! Viability probe for the macOS exporter: can a userspace Rust process take
//! exclusive ownership of the iPhone through `IOUSBHostDevice`? Per the SDK
//! header, `initWithIOService:queue:error:interestHandler:` establishes
//! exclusive ownership on success — which is what exporting a device requires.
//! This only attempts the Apple device (matched by vendor id) so it never
//! seizes a hub or HID device.

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use block2::DynBlock;
use objc2::runtime::Sel;
use objc2::{AnyThread, extern_class, msg_send, sel};
use objc2_foundation::NSError;
use std::ffi::{CString, c_char, c_void};

type IoObject = u32;
type KernReturn = i32;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingServices(
        main_port: u32,
        matching: *const c_void,
        existing: *mut IoObject,
    ) -> KernReturn;
    fn IOIteratorNext(iterator: IoObject) -> IoObject;
    fn IOObjectRelease(object: IoObject) -> KernReturn;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(
        alloc: *const c_void,
        c_str: *const c_char,
        encoding: u32,
    ) -> *const c_void;
    fn CFNumberGetValue(number: *const c_void, number_type: i32, value: *mut c_void) -> bool;
    fn CFRelease(cf: *const c_void);
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IORegistryEntryCreateCFProperty(
        entry: IoObject,
        key: *const c_void,
        allocator: *const c_void,
        options: u32,
    ) -> *const c_void;
}

const KCFSTRING_ENCODING_UTF8: u32 = 0x0800_0100;
const KCFNUMBER_SINT32: i32 = 3;
const APPLE_VENDOR_ID: i32 = 0x05ac;

extern_class!(
    #[unsafe(super(NSObject))]
    #[name = "IOUSBHostDevice"]
    struct IOUSBHostDevice;
);

fn cfstr(text: &str) -> *const c_void {
    let c = CString::new(text).unwrap();
    unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), KCFSTRING_ENCODING_UTF8) }
}

/// Reads an integer registry property without opening (seizing) the device.
fn read_u16_property(service: IoObject, key: &str) -> Option<u16> {
    unsafe {
        let cf_key = cfstr(key);
        if cf_key.is_null() {
            return None;
        }
        let value = IORegistryEntryCreateCFProperty(service, cf_key, std::ptr::null(), 0);
        CFRelease(cf_key);
        if value.is_null() {
            return None;
        }
        let mut out: i32 = 0;
        let ok = CFNumberGetValue(value, KCFNUMBER_SINT32, std::ptr::from_mut(&mut out).cast());
        CFRelease(value);
        if ok { Some(out as u16) } else { None }
    }
}

/// Reads a descriptor straight off the device.
///
/// `descriptorWithType:length:index:languageID:error:` returns a pointer to a
/// packed descriptor, and objc2's encoding check rejects a `*const u8` return
/// against the framework's struct-pointer encoding, so the selector is invoked
/// through `objc_msgSend` directly.
fn read_descriptor(
    device: &IOUSBHostDevice,
    kind: usize,
    index: usize,
    want: usize,
) -> Option<Vec<u8>> {
    type Fn = unsafe extern "C" fn(
        *const IOUSBHostDevice,
        Sel,
        usize,
        *mut usize,
        usize,
        usize,
        *mut *mut NSError,
    ) -> *const u8;
    unsafe {
        let send: Fn = std::mem::transmute(objc_msgSend as *const c_void);
        let mut length: usize = want;
        let mut error: *mut NSError = std::ptr::null_mut();
        let bytes = send(
            device,
            sel!(descriptorWithType:length:index:languageID:error:),
            kind,
            &mut length,
            index,
            0,
            &mut error,
        );
        if bytes.is_null() || length == 0 || length > 4096 {
            return None;
        }
        Some(std::slice::from_raw_parts(bytes, length).to_vec())
    }
}

unsafe extern "C" {
    fn objc_msgSend();
}

pub fn probe_export() -> Result<String, String> {
    unsafe {
        let class_name = CString::new("IOUSBHostDevice").unwrap();
        let matching = IOServiceMatching(class_name.as_ptr());
        if matching.is_null() {
            return Err("IOServiceMatching(IOUSBHostDevice) returned null".into());
        }
        let mut iterator: IoObject = 0;
        let kr = IOServiceGetMatchingServices(0, matching, &mut iterator);
        if kr != 0 {
            return Err(format!("IOServiceGetMatchingServices failed: kr=0x{kr:08x}"));
        }

        let mut report = String::new();
        let mut found = false;
        loop {
            let service = IOIteratorNext(iterator);
            if service == 0 {
                break;
            }
            let vendor = read_u16_property(service, "idVendor");
            if vendor != Some(APPLE_VENDOR_ID as u16) {
                IOObjectRelease(service);
                continue;
            }
            found = true;
            let mut error: *mut NSError = std::ptr::null_mut();
            let device: Option<Retained<IOUSBHostDevice>> = msg_send![
                IOUSBHostDevice::alloc(),
                initWithIOService: service,
                queue: std::ptr::null_mut::<AnyObject>(),
                error: &mut error,
                interestHandler: None::<&DynBlock<dyn Fn()>>,
            ];
            match device {
                Some(device) => {
                    let pid = read_u16_property(service, "idProduct").unwrap_or(0);
                    report.push_str(&format!(
                        "EXCLUSIVE OPEN OK: {APPLE_VENDOR_ID:04x}:{pid:04x}\n"
                    ));
                    match read_descriptor(&device, 1, 0, 18) {
                        Some(bytes) => report.push_str(&format!(
                            "device descriptor: {} bytes {:02x?}\n",
                            bytes.len(),
                            &bytes[..bytes.len().min(18)]
                        )),
                        None => report.push_str("device descriptor: FAILED\n"),
                    }
                    match read_descriptor(&device, 2, 0, 9) {
                        Some(header) if header.len() >= 4 => {
                            let total = usize::from(u16::from_le_bytes([header[2], header[3]]));
                            match read_descriptor(&device, 2, 0, total) {
                                Some(full) => report.push_str(&format!(
                                    "configuration descriptor: {} bytes (total {total})\n",
                                    full.len()
                                )),
                                None => report.push_str("configuration descriptor: FAILED\n"),
                            }
                        }
                        _ => report.push_str("configuration header: FAILED\n"),
                    }
                    let _: () = msg_send![&*device, destroy];
                }
                None => {
                    let (domain, code) = if error.is_null() {
                        ("unknown".to_owned(), 0)
                    } else {
                        let error_ref: &NSError = &*error;
                        (error_ref.domain().to_string(), error_ref.code())
                    };
                    report.push_str(&format!(
                        "exclusive open FAILED: domain={domain} code={code}\n"
                    ));
                }
            }
            IOObjectRelease(service);
        }
        IOObjectRelease(iterator);

        if !found {
            return Err("no Apple IOUSBHostDevice present (is the iPhone attached?)".into());
        }
        Ok(report)
    }
}
