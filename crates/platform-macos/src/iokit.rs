//! Minimal IOKit bindings for finding and inspecting Apple USB devices without
//! opening (and therefore seizing) them.

use std::ffi::{CString, c_char, c_void};
use std::io;

pub type IoObject = u32;
type KernReturn = i32;

const KCFSTRING_ENCODING_UTF8: u32 = 0x0800_0100;
const KCFNUMBER_SINT32: i32 = 3;
pub const APPLE_VENDOR_ID: u16 = 0x05ac;

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
    fn IORegistryEntryCreateCFProperty(
        entry: IoObject,
        key: *const c_void,
        allocator: *const c_void,
        options: u32,
    ) -> *const c_void;
    fn IORegistryEntryGetChildIterator(
        entry: IoObject,
        plane: *const c_char,
        iterator: *mut IoObject,
    ) -> KernReturn;
    fn IOObjectConformsTo(object: IoObject, class_name: *const c_char) -> bool;
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

/// Releases the IOKit object when dropped.
pub struct Service(IoObject);

impl Service {
    #[must_use]
    pub const fn get(&self) -> IoObject {
        self.0
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        unsafe { IOObjectRelease(self.0) };
    }
}

fn cfstr(text: &str) -> *const c_void {
    let Ok(c) = CString::new(text) else {
        return std::ptr::null();
    };
    unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), KCFSTRING_ENCODING_UTF8) }
}

/// Reads an integer registry property without opening the device.
#[must_use]
pub fn read_u32_property(service: IoObject, key: &str) -> Option<u32> {
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
        if ok { Some(out as u32) } else { None }
    }
}

/// The `IOUSBHostInterface` children of a device, in registry order. These only
/// exist once the device is configured.
pub fn child_interfaces(device: IoObject) -> Vec<Service> {
    unsafe {
        let Ok(plane) = CString::new("IOService") else {
            return Vec::new();
        };
        let Ok(class_name) = CString::new("IOUSBHostInterface") else {
            return Vec::new();
        };
        let mut iterator: IoObject = 0;
        if IORegistryEntryGetChildIterator(device, plane.as_ptr(), &mut iterator) != 0 {
            return Vec::new();
        }
        let mut found = Vec::new();
        loop {
            let child = IOIteratorNext(iterator);
            if child == 0 {
                break;
            }
            if IOObjectConformsTo(child, class_name.as_ptr()) {
                found.push(Service(child));
            } else {
                IOObjectRelease(child);
            }
        }
        IOObjectRelease(iterator);
        found
    }
}

/// Every present `IOUSBHostDevice` whose vendor is Apple. Matching on the class
/// alone and filtering by registry property avoids building a CFNumber into the
/// matching dictionary, which is easy to get subtly wrong.
pub fn apple_devices() -> io::Result<Vec<Service>> {
    unsafe {
        let class_name = CString::new("IOUSBHostDevice")
            .map_err(|_| io::Error::other("bad class name"))?;
        let matching = IOServiceMatching(class_name.as_ptr());
        if matching.is_null() {
            return Err(io::Error::other("IOServiceMatching returned null"));
        }
        let mut iterator: IoObject = 0;
        let kr = IOServiceGetMatchingServices(0, matching, &mut iterator);
        if kr != 0 {
            return Err(io::Error::other(format!(
                "IOServiceGetMatchingServices failed: 0x{kr:08x}"
            )));
        }
        let mut found = Vec::new();
        loop {
            let service = IOIteratorNext(iterator);
            if service == 0 {
                break;
            }
            if read_u32_property(service, "idVendor") == Some(u32::from(APPLE_VENDOR_ID)) {
                found.push(Service(service));
            } else {
                IOObjectRelease(service);
            }
        }
        IOObjectRelease(iterator);
        Ok(found)
    }
}
