#![no_std]

extern crate wdk_panic;

use core::cell::UnsafeCell;
use core::ffi::{c_char, c_void};
use core::mem::{offset_of, size_of, zeroed};
use core::ptr::{addr_of, addr_of_mut, copy_nonoverlapping, null, null_mut, write_bytes};
use core::sync::atomic::{AtomicI32, Ordering};

use interdriver_windows_export_abi::{
    ExportAbort, ExportInfo, ExportTransfer, IOCTL_ABORT_ENDPOINT, IOCTL_GET_INFO, IOCTL_TRANSFER,
    MAX_TRANSFER, PROTOCOL_VERSION,
};
use wdk_sys::ntddk::{
    ExAllocatePool2, ExFreePool, KeClearEvent, KeInitializeEvent, KeSetEvent, KeWaitForSingleObject,
};
use wdk_sys::usb::USBD_CreateConfigurationRequestEx;
use wdk_sys::*;

const MAX_PIPES: usize = 32;
const TRANSFER_HEADER_SIZE: usize = size_of::<ExportTransfer>();
const SPEED_FULL: u32 = 1;
const SPEED_HIGH: u32 = 2;
const DEFAULT_TIMEOUT_MS: u32 = 30_000;
const QUIESCE_TIMEOUT_MS: u32 = 5_000;
const POOL_FLAG_NON_PAGED: u64 = 0x40;
const POOL_TAG_CONFIG: u32 = u32::from_le_bytes(*b"GtEc");
const POOL_TAG_INTERFACE: u32 = u32::from_le_bytes(*b"GtEi");
const FILE_DEVICE_UNKNOWN: u32 = 0x22;

static DEVICE_INTERFACE_GUID: GUID = GUID {
    Data1: 0x3a44_f2d1,
    Data2: 0x7acb,
    Data3: 0x4b49,
    Data4: [0x91, 0xd5, 0x73, 0xac, 0x92, 0xd4, 0x27, 0x11],
};

#[repr(C)]
struct RawContextTypeInfo {
    size: u32,
    name: *const c_char,
    context_size: usize,
    unique_type: PCWDF_OBJECT_CONTEXT_TYPE_INFO,
    get_unique_type: PFN_GET_UNIQUE_CONTEXT_TYPE,
}

unsafe impl Sync for RawContextTypeInfo {}

static CONTEXT_NAME: &[u8] = b"DeviceContext\0";
#[unsafe(link_section = ".data")]
static CONTEXT_TYPE_INFO: RawContextTypeInfo = RawContextTypeInfo {
    size: 40,
    name: CONTEXT_NAME.as_ptr().cast(),
    context_size: size_of::<DeviceContext>(),
    unique_type: addr_of!(CONTEXT_TYPE_INFO).cast(),
    get_unique_type: None,
};

const _: () = assert!(size_of::<RawContextTypeInfo>() == 40);
const _: () = assert!(size_of::<RawObjectAttributes>() == 56);
const _: () = assert!(size_of::<USB_DEVICE_DESCRIPTOR>() == 18);
const _: () = assert!(size_of::<USB_CONFIGURATION_DESCRIPTOR>() == 9);
const _: () = assert!(size_of::<USB_INTERFACE_DESCRIPTOR>() == 9);

#[repr(C)]
struct RawObjectAttributes {
    size: u32,
    cleanup: PFN_WDF_OBJECT_CONTEXT_CLEANUP,
    destroy: PFN_WDF_OBJECT_CONTEXT_DESTROY,
    execution_level: WDF_EXECUTION_LEVEL,
    synchronization_scope: WDF_SYNCHRONIZATION_SCOPE,
    parent: WDFOBJECT,
    context_size_override: usize,
    context_type_info: PCWDF_OBJECT_CONTEXT_TYPE_INFO,
}

impl RawObjectAttributes {
    fn for_device_context() -> Self {
        Self {
            size: 56,
            cleanup: None,
            destroy: None,
            execution_level: _WDF_EXECUTION_LEVEL::WdfExecutionLevelPassive,
            synchronization_scope: _WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeNone,
            parent: null_mut(),
            context_size_override: 0,
            context_type_info: addr_of!(CONTEXT_TYPE_INFO).cast(),
        }
    }

    fn as_wdf_ptr(&mut self) -> PWDF_OBJECT_ATTRIBUTES {
        (self as *mut Self).cast()
    }
}

#[repr(C)]
struct PipeState {
    configuration_handle: USBD_CONFIGURATION_HANDLE,
    pipes: [USBD_PIPE_HANDLE; MAX_PIPES],
    pipe_addresses: [u8; MAX_PIPES],
    pipe_interfaces: [u8; MAX_PIPES],
    pipe_count: u32,
    configuration_value: u8,
}

#[repr(C)]
struct DeviceContext {
    usb_device: WDFUSBDEVICE,
    state_lock: WDFWAITLOCK,
    pipe_state: UnsafeCell<PipeState>,
    active_pipe_transfers: AtomicI32,
    no_active_pipe_transfers: UnsafeCell<KEVENT>,
    active_interface_transfers: [AtomicI32; 256],
    no_active_interface_transfers: [UnsafeCell<KEVENT>; 256],
    device_descriptor: USB_DEVICE_DESCRIPTOR,
    speed: u32,
}

// KMDF dispatches the default queue in parallel. Mutable pipe/configuration
// state is protected by state_lock, transfer counts are atomic, and KEVENT is
// mutated only by the kernel through its UnsafeCell pointer.
unsafe impl Sync for DeviceContext {}

unsafe fn get_context(device: WDFDEVICE) -> *mut DeviceContext {
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfObjectGetTypedContextWorker,
            device.cast::<c_void>(),
            addr_of!(CONTEXT_TYPE_INFO).cast(),
        )
        .cast()
    }
}

fn nt_success(status: NTSTATUS) -> bool {
    status >= 0
}

fn request_options(timeout_ms: u32) -> WDF_REQUEST_SEND_OPTIONS {
    WDF_REQUEST_SEND_OPTIONS {
        Size: size_of::<WDF_REQUEST_SEND_OPTIONS>() as u32,
        Flags: _WDF_REQUEST_SEND_OPTIONS_FLAGS::WDF_REQUEST_SEND_OPTION_TIMEOUT as u32,
        Timeout: -i64::from(if timeout_ms == 0 {
            DEFAULT_TIMEOUT_MS
        } else {
            timeout_ms
        }) * 10_000,
    }
}

fn memory_descriptor(buffer: *mut u8, length: u32) -> WDF_MEMORY_DESCRIPTOR {
    WDF_MEMORY_DESCRIPTOR {
        Type: _WDF_MEMORY_DESCRIPTOR_TYPE::WdfMemoryDescriptorTypeBuffer,
        u: _WDF_MEMORY_DESCRIPTOR__bindgen_ty_1 {
            BufferType: _WDF_MEMORY_DESCRIPTOR__bindgen_ty_1__bindgen_ty_1 {
                Buffer: buffer.cast(),
                Length: length,
            },
        },
    }
}

fn setup_packet(bytes: [u8; 8]) -> WDF_USB_CONTROL_SETUP_PACKET {
    WDF_USB_CONTROL_SETUP_PACKET {
        Generic: _WDF_USB_CONTROL_SETUP_PACKET__bindgen_ty_2 { Bytes: bytes },
    }
}

unsafe fn send_urb(context: &DeviceContext, urb: PURB, timeout_ms: u32) -> NTSTATUS {
    let mut options = request_options(timeout_ms);
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfUsbTargetDeviceSendUrbSynchronously,
            context.usb_device,
            null_mut(),
            addr_of_mut!(options),
            urb,
        )
    };
    if nt_success(status) {
        let usb_status = unsafe { (*urb.cast::<_URB_HEADER>()).Status };
        if usb_status < 0 {
            return usb_status;
        }
    }
    status
}

unsafe fn send_control(
    context: &DeviceContext,
    setup: [u8; 8],
    buffer: *mut u8,
    length: u32,
    timeout_ms: u32,
    transferred: *mut u32,
) -> NTSTATUS {
    let mut setup = setup_packet(setup);
    let mut options = request_options(timeout_ms);
    let mut memory = memory_descriptor(buffer, length);
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfUsbTargetDeviceSendControlTransferSynchronously,
            context.usb_device,
            null_mut(),
            addr_of_mut!(options),
            addr_of_mut!(setup),
            if length == 0 {
                null_mut()
            } else {
                addr_of_mut!(memory)
            },
            transferred,
        )
    }
}

unsafe fn send_descriptor(
    context: &DeviceContext,
    descriptor_type: u8,
    index: u8,
    buffer: *mut u8,
    length: u32,
    timeout_ms: u32,
    transferred: *mut u32,
) -> NTSTATUS {
    unsafe {
        send_control(
            context,
            [
                0x80,
                6,
                index,
                descriptor_type,
                0,
                0,
                length as u8,
                (length >> 8) as u8,
            ],
            buffer,
            length,
            timeout_ms,
            transferred,
        )
    }
}

unsafe fn read_configuration_descriptor(
    context: &DeviceContext,
    index: u8,
    timeout_ms: u32,
    descriptor: *mut PUSB_CONFIGURATION_DESCRIPTOR,
) -> NTSTATUS {
    let mut header = [0u8; 9];
    let mut transferred = 0u32;
    let status = unsafe {
        send_descriptor(
            context,
            USB_CONFIGURATION_DESCRIPTOR_TYPE as u8,
            index,
            header.as_mut_ptr(),
            header.len() as u32,
            timeout_ms,
            addr_of_mut!(transferred),
        )
    };
    if !nt_success(status) {
        return status;
    }
    let total_length = u16::from_le_bytes([header[2], header[3]]) as usize;
    if transferred < header.len() as u32 || total_length < header.len() {
        return STATUS_DEVICE_DATA_ERROR;
    }
    let bytes =
        unsafe { ExAllocatePool2(POOL_FLAG_NON_PAGED, total_length as u64, POOL_TAG_CONFIG) }
            .cast::<u8>();
    if bytes.is_null() {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    unsafe { write_bytes(bytes, 0, total_length) };
    transferred = 0;
    let status = unsafe {
        send_descriptor(
            context,
            USB_CONFIGURATION_DESCRIPTOR_TYPE as u8,
            index,
            bytes,
            total_length as u32,
            timeout_ms,
            addr_of_mut!(transferred),
        )
    };
    if !nt_success(status) || transferred < total_length as u32 {
        unsafe { ExFreePool(bytes.cast()) };
        return if nt_success(status) {
            STATUS_DEVICE_DATA_ERROR
        } else {
            status
        };
    }
    unsafe { *descriptor = bytes.cast() };
    STATUS_SUCCESS
}

unsafe fn find_configuration(
    context: &DeviceContext,
    value: u8,
    timeout_ms: u32,
    descriptor: *mut PUSB_CONFIGURATION_DESCRIPTOR,
) -> NTSTATUS {
    for index in 0..context.device_descriptor.bNumConfigurations {
        let status =
            unsafe { read_configuration_descriptor(context, index, timeout_ms, descriptor) };
        if !nt_success(status) {
            return status;
        }
        let bytes = unsafe { (*descriptor).cast::<u8>() };
        if unsafe { *bytes.add(5) } == value {
            return STATUS_SUCCESS;
        }
        unsafe {
            ExFreePool((*descriptor).cast());
            *descriptor = null_mut();
        }
    }
    STATUS_NOT_FOUND
}

unsafe fn find_pipe(state: &PipeState, address: u8, interface_number: *mut u8) -> USBD_PIPE_HANDLE {
    for index in 0..state.pipe_count as usize {
        if state.pipe_addresses[index] == address {
            if !interface_number.is_null() {
                unsafe { *interface_number = state.pipe_interfaces[index] };
            }
            return state.pipes[index];
        }
    }
    null_mut()
}

unsafe fn store_interface_pipes(state: &mut PipeState, interface: PUSBD_INTERFACE_INFORMATION) {
    if interface.is_null() {
        return;
    }
    let count = unsafe { (*interface).NumberOfPipes } as usize;
    for pipe_index in 0..count {
        if state.pipe_count as usize >= MAX_PIPES {
            break;
        }
        let pipe = unsafe { (*interface).Pipes.as_ptr().add(pipe_index).read() };
        let index = state.pipe_count as usize;
        state.pipes[index] = pipe.PipeHandle;
        state.pipe_addresses[index] = pipe.EndpointAddress;
        state.pipe_interfaces[index] = unsafe { (*interface).InterfaceNumber };
        state.pipe_count += 1;
    }
}

unsafe fn abort_pipe(
    context: &DeviceContext,
    pipe: USBD_PIPE_HANDLE,
    function: u16,
    timeout: u32,
) -> NTSTATUS {
    let mut urb = _URB_PIPE_REQUEST::default();
    urb.Hdr.Length = size_of::<_URB_PIPE_REQUEST>() as u16;
    urb.Hdr.Function = function;
    urb.PipeHandle = pipe;
    unsafe { send_urb(context, addr_of_mut!(urb).cast(), timeout) }
}

unsafe fn quiesce_pipes(
    context: &DeviceContext,
    state: &PipeState,
    interface_number: i32,
    timeout_ms: u32,
) -> NTSTATUS {
    let mut first_failure = STATUS_SUCCESS;
    for index in 0..state.pipe_count as usize {
        if interface_number >= 0 && state.pipe_interfaces[index] != interface_number as u8 {
            continue;
        }
        let status = unsafe {
            abort_pipe(
                context,
                state.pipes[index],
                URB_FUNCTION_ABORT_PIPE as u16,
                timeout_ms,
            )
        };
        if !nt_success(status) && nt_success(first_failure) {
            first_failure = status;
        }
    }
    let mut timeout = LARGE_INTEGER {
        QuadPart: -i64::from(if timeout_ms == 0 {
            QUIESCE_TIMEOUT_MS
        } else {
            timeout_ms
        }) * 10_000,
    };
    let event = if interface_number < 0 {
        context.no_active_pipe_transfers.get()
    } else {
        context.no_active_interface_transfers[interface_number as usize].get()
    };
    let wait_status = unsafe {
        KeWaitForSingleObject(
            event.cast(),
            _KWAIT_REASON::Executive,
            _MODE::KernelMode as i8,
            0,
            addr_of_mut!(timeout),
        )
    };
    if wait_status == STATUS_TIMEOUT {
        STATUS_IO_TIMEOUT
    } else {
        first_failure
    }
}

unsafe fn deconfigure(context: &DeviceContext, state: &mut PipeState, timeout_ms: u32) -> NTSTATUS {
    let mut urb = _URB_SELECT_CONFIGURATION::default();
    urb.Hdr.Length = size_of::<_URB_SELECT_CONFIGURATION>() as u16;
    urb.Hdr.Function = URB_FUNCTION_SELECT_CONFIGURATION as u16;
    urb.ConfigurationDescriptor = null_mut();
    let status = unsafe { send_urb(context, addr_of_mut!(urb).cast(), timeout_ms) };
    if nt_success(status) {
        state.configuration_value = 0;
        state.configuration_handle = null_mut();
        state.pipe_count = 0;
    }
    status
}

unsafe fn select_configuration(context: &DeviceContext, value: u8, timeout_ms: u32) -> NTSTATUS {
    let _ = unsafe {
        call_unsafe_wdf_function_binding!(WdfWaitLockAcquire, context.state_lock, null_mut())
    };
    let state = unsafe { &mut *context.pipe_state.get() };
    if value == state.configuration_value {
        unsafe { call_unsafe_wdf_function_binding!(WdfWaitLockRelease, context.state_lock) };
        return STATUS_SUCCESS;
    }
    if value == 0 {
        let status = unsafe { deconfigure(context, state, timeout_ms) };
        unsafe { call_unsafe_wdf_function_binding!(WdfWaitLockRelease, context.state_lock) };
        return status;
    }
    let mut descriptor: PUSB_CONFIGURATION_DESCRIPTOR = null_mut();
    let mut status =
        unsafe { find_configuration(context, value, timeout_ms, addr_of_mut!(descriptor)) };
    if !nt_success(status) {
        unsafe { call_unsafe_wdf_function_binding!(WdfWaitLockRelease, context.state_lock) };
        return status;
    }
    let bytes = descriptor.cast::<u8>();
    let total_length = unsafe { u16::from_le_bytes([*bytes.add(2), *bytes.add(3)]) as usize };
    let mut interfaces = [USBD_INTERFACE_LIST_ENTRY::default(); 257];
    let mut offset = unsafe { *bytes } as usize;
    let mut count = 0usize;
    while offset + 2 <= total_length {
        let length = unsafe { *bytes.add(offset) } as usize;
        let descriptor_type = unsafe { *bytes.add(offset + 1) };
        if length < 2 || offset + length > total_length {
            status = STATUS_DEVICE_DATA_ERROR;
            break;
        }
        if descriptor_type == USB_INTERFACE_DESCRIPTOR_TYPE as u8
            && length >= size_of::<USB_INTERFACE_DESCRIPTOR>()
            && unsafe { *bytes.add(offset + 3) } == 0
            && count < 256
        {
            interfaces[count].InterfaceDescriptor = unsafe { bytes.add(offset).cast() };
            count += 1;
        }
        offset += length;
    }
    if nt_success(status) && count == 0 {
        status = STATUS_DEVICE_DATA_ERROR;
    }
    if nt_success(status) && state.configuration_value != 0 {
        status = unsafe { quiesce_pipes(context, state, -1, timeout_ms) };
        if nt_success(status) {
            status = unsafe { deconfigure(context, state, timeout_ms) };
        }
    }
    if nt_success(status) {
        let urb = unsafe { USBD_CreateConfigurationRequestEx(descriptor, interfaces.as_mut_ptr()) };
        if urb.is_null() {
            status = STATUS_INSUFFICIENT_RESOURCES;
        } else {
            status = unsafe { send_urb(context, urb, timeout_ms) };
            if nt_success(status) {
                let select = urb.cast::<_URB_SELECT_CONFIGURATION>();
                state.configuration_value = value;
                state.configuration_handle = unsafe { (*select).ConfigurationHandle };
                state.pipe_count = 0;
                for entry in interfaces.iter().take(count) {
                    unsafe { store_interface_pipes(state, entry.Interface) };
                }
            }
            unsafe { ExFreePool(urb.cast()) };
        }
    }
    unsafe {
        ExFreePool(descriptor.cast());
        call_unsafe_wdf_function_binding!(WdfWaitLockRelease, context.state_lock);
    }
    status
}

fn interface_info_size(pipe_count: usize) -> usize {
    offset_of!(USBD_INTERFACE_INFORMATION, Pipes) + pipe_count * size_of::<USBD_PIPE_INFORMATION>()
}

fn select_interface_urb_size(pipe_count: usize) -> usize {
    offset_of!(_URB_SELECT_INTERFACE, Interface) + interface_info_size(pipe_count)
}

unsafe fn select_interface(
    context: &DeviceContext,
    interface_number: u8,
    alternate_setting: u8,
    timeout_ms: u32,
) -> NTSTATUS {
    let _ = unsafe {
        call_unsafe_wdf_function_binding!(WdfWaitLockAcquire, context.state_lock, null_mut())
    };
    let state = unsafe { &mut *context.pipe_state.get() };
    if state.configuration_value == 0 || state.configuration_handle.is_null() {
        unsafe { call_unsafe_wdf_function_binding!(WdfWaitLockRelease, context.state_lock) };
        return STATUS_INVALID_DEVICE_STATE;
    }
    let mut status =
        unsafe { quiesce_pipes(context, state, i32::from(interface_number), timeout_ms) };
    let mut configuration: PUSB_CONFIGURATION_DESCRIPTOR = null_mut();
    if nt_success(status) {
        status = unsafe {
            find_configuration(
                context,
                state.configuration_value,
                timeout_ms,
                addr_of_mut!(configuration),
            )
        };
    }
    let mut descriptor: PUSB_INTERFACE_DESCRIPTOR = null_mut();
    if nt_success(status) {
        let bytes = configuration.cast::<u8>();
        let total_length = unsafe { u16::from_le_bytes([*bytes.add(2), *bytes.add(3)]) as usize };
        let mut offset = unsafe { *bytes } as usize;
        while offset + 2 <= total_length {
            let length = unsafe { *bytes.add(offset) } as usize;
            let descriptor_type = unsafe { *bytes.add(offset + 1) };
            if length < 2 || offset + length > total_length {
                status = STATUS_DEVICE_DATA_ERROR;
                break;
            }
            if descriptor_type == USB_INTERFACE_DESCRIPTOR_TYPE as u8
                && length >= size_of::<USB_INTERFACE_DESCRIPTOR>()
                && unsafe { *bytes.add(offset + 2) } == interface_number
                && unsafe { *bytes.add(offset + 3) } == alternate_setting
            {
                descriptor = unsafe { bytes.add(offset).cast() };
                break;
            }
            offset += length;
        }
        if nt_success(status) && descriptor.is_null() {
            status = STATUS_NOT_FOUND;
        }
    }
    let mut urb: PURB = null_mut();
    if nt_success(status) {
        let pipe_count = unsafe { *descriptor.cast::<u8>().add(4) } as usize;
        let urb_size = select_interface_urb_size(pipe_count);
        if urb_size > u16::MAX as usize {
            status = STATUS_INVALID_BUFFER_SIZE;
        } else {
            urb = unsafe {
                ExAllocatePool2(POOL_FLAG_NON_PAGED, urb_size as u64, POOL_TAG_INTERFACE)
            }
            .cast();
            if urb.is_null() {
                status = STATUS_INSUFFICIENT_RESOURCES;
            } else {
                unsafe { write_bytes(urb.cast::<u8>(), 0, urb_size) };
                let select = urb.cast::<_URB_SELECT_INTERFACE>();
                unsafe {
                    (*select).Hdr.Length = urb_size as u16;
                    (*select).Hdr.Function = URB_FUNCTION_SELECT_INTERFACE as u16;
                    (*select).ConfigurationHandle = state.configuration_handle;
                    (*select).Interface.Length = interface_info_size(pipe_count) as u16;
                    (*select).Interface.InterfaceNumber = interface_number;
                    (*select).Interface.AlternateSetting = alternate_setting;
                    (*select).Interface.NumberOfPipes = pipe_count as u32;
                }
                status = unsafe { send_urb(context, urb, timeout_ms) };
                if nt_success(status) {
                    let mut write_index = 0usize;
                    for read_index in 0..state.pipe_count as usize {
                        if state.pipe_interfaces[read_index] != interface_number {
                            state.pipes[write_index] = state.pipes[read_index];
                            state.pipe_addresses[write_index] = state.pipe_addresses[read_index];
                            state.pipe_interfaces[write_index] = state.pipe_interfaces[read_index];
                            write_index += 1;
                        }
                    }
                    state.pipe_count = write_index as u32;
                    unsafe { store_interface_pipes(state, addr_of_mut!((*select).Interface)) };
                }
            }
        }
    }
    unsafe {
        if !urb.is_null() {
            ExFreePool(urb.cast());
        }
        if !configuration.is_null() {
            ExFreePool(configuration.cast());
        }
        call_unsafe_wdf_function_binding!(WdfWaitLockRelease, context.state_lock);
    }
    status
}

unsafe fn control_transfer(
    context: &DeviceContext,
    transfer: &mut ExportTransfer,
    capacity: u32,
) -> NTSTATUS {
    let value = u16::from_le_bytes([transfer.setup[2], transfer.setup[3]]);
    let index = u16::from_le_bytes([transfer.setup[4], transfer.setup[5]]);
    if transfer.setup[0] == 0 && transfer.setup[1] == USB_REQUEST_SET_CONFIGURATION as u8 {
        return unsafe { select_configuration(context, value as u8, transfer.timeout_ms) };
    }
    if transfer.setup[0] == 1 && transfer.setup[1] == USB_REQUEST_SET_INTERFACE as u8 {
        return unsafe { select_interface(context, index as u8, value as u8, transfer.timeout_ms) };
    }
    if transfer.setup[0] == 2 && transfer.setup[1] == USB_REQUEST_CLEAR_FEATURE as u8 && value == 0
    {
        let _ = unsafe {
            call_unsafe_wdf_function_binding!(WdfWaitLockAcquire, context.state_lock, null_mut())
        };
        let state = unsafe { &*context.pipe_state.get() };
        let pipe = unsafe { find_pipe(state, index as u8, null_mut()) };
        unsafe { call_unsafe_wdf_function_binding!(WdfWaitLockRelease, context.state_lock) };
        if pipe.is_null() {
            return STATUS_NOT_FOUND;
        }
        return unsafe {
            abort_pipe(
                context,
                pipe,
                URB_FUNCTION_SYNC_RESET_PIPE_AND_CLEAR_STALL as u16,
                transfer.timeout_ms,
            )
        };
    }
    let device_to_host = transfer.setup[0] & 0x80 != 0;
    if device_to_host && transfer.transfer_length > capacity {
        return STATUS_BUFFER_TOO_SMALL;
    }
    let data = unsafe {
        (transfer as *mut ExportTransfer)
            .cast::<u8>()
            .add(TRANSFER_HEADER_SIZE)
    };
    let mut transferred = 0;
    let status = unsafe {
        send_control(
            context,
            transfer.setup,
            data,
            transfer.transfer_length,
            transfer.timeout_ms,
            addr_of_mut!(transferred),
        )
    };
    transfer.actual_length = transferred;
    status
}

unsafe fn pipe_transfer(
    context: &DeviceContext,
    transfer: &mut ExportTransfer,
    capacity: u32,
) -> NTSTATUS {
    let input = transfer.endpoint_address & 0x80 != 0;
    if input && transfer.transfer_length > capacity {
        return STATUS_BUFFER_TOO_SMALL;
    }
    let mut interface_number = 0u8;
    let _ = unsafe {
        call_unsafe_wdf_function_binding!(WdfWaitLockAcquire, context.state_lock, null_mut())
    };
    let state = unsafe { &*context.pipe_state.get() };
    let pipe = unsafe {
        find_pipe(
            state,
            transfer.endpoint_address as u8,
            addr_of_mut!(interface_number),
        )
    };
    if !pipe.is_null() && context.active_pipe_transfers.fetch_add(1, Ordering::SeqCst) == 0 {
        unsafe { KeClearEvent(context.no_active_pipe_transfers.get()) };
    }
    if !pipe.is_null()
        && context.active_interface_transfers[interface_number as usize]
            .fetch_add(1, Ordering::SeqCst)
            == 0
    {
        unsafe {
            KeClearEvent(context.no_active_interface_transfers[interface_number as usize].get())
        };
    }
    unsafe { call_unsafe_wdf_function_binding!(WdfWaitLockRelease, context.state_lock) };
    if pipe.is_null() {
        return STATUS_NOT_FOUND;
    }
    let data = unsafe {
        (transfer as *mut ExportTransfer)
            .cast::<u8>()
            .add(TRANSFER_HEADER_SIZE)
    };
    let mut urb = _URB_BULK_OR_INTERRUPT_TRANSFER::default();
    urb.Hdr.Length = size_of::<_URB_BULK_OR_INTERRUPT_TRANSFER>() as u16;
    urb.Hdr.Function = URB_FUNCTION_BULK_OR_INTERRUPT_TRANSFER as u16;
    urb.PipeHandle = pipe;
    urb.TransferFlags = if input {
        USBD_TRANSFER_DIRECTION_IN | USBD_SHORT_TRANSFER_OK
    } else {
        0
    };
    urb.TransferBufferLength = transfer.transfer_length;
    urb.TransferBuffer = data.cast();
    let status = unsafe { send_urb(context, addr_of_mut!(urb).cast(), transfer.timeout_ms) };
    transfer.actual_length = urb.TransferBufferLength;
    if context.active_interface_transfers[interface_number as usize].fetch_sub(1, Ordering::SeqCst)
        == 1
    {
        unsafe {
            KeSetEvent(
                context.no_active_interface_transfers[interface_number as usize].get(),
                IO_NO_INCREMENT as i32,
                0,
            )
        };
    }
    if context.active_pipe_transfers.fetch_sub(1, Ordering::SeqCst) == 1 {
        unsafe {
            KeSetEvent(
                context.no_active_pipe_transfers.get(),
                IO_NO_INCREMENT as i32,
                0,
            )
        };
    }
    status
}

unsafe extern "C" fn io_device_control(
    queue: WDFQUEUE,
    request: WDFREQUEST,
    output_buffer_length: usize,
    input_buffer_length: usize,
    io_control_code: u32,
) {
    let device = unsafe { call_unsafe_wdf_function_binding!(WdfIoQueueGetDevice, queue) };
    let context = unsafe { &*get_context(device) };
    let mut status = STATUS_INVALID_DEVICE_REQUEST;
    match io_control_code {
        IOCTL_GET_INFO => {
            let mut buffer: *mut c_void = null_mut();
            status = unsafe {
                call_unsafe_wdf_function_binding!(
                    WdfRequestRetrieveOutputBuffer,
                    request,
                    size_of::<ExportInfo>(),
                    addr_of_mut!(buffer),
                    null_mut()
                )
            };
            if nt_success(status) {
                let info = unsafe { &mut *buffer.cast::<ExportInfo>() };
                *info = ExportInfo::default();
                info.size = size_of::<ExportInfo>() as u32;
                info.version = PROTOCOL_VERSION;
                info.speed = context.speed;
                unsafe {
                    copy_nonoverlapping(
                        addr_of!(context.device_descriptor).cast::<u8>(),
                        info.device_descriptor.as_mut_ptr(),
                        info.device_descriptor.len(),
                    );
                    call_unsafe_wdf_function_binding!(
                        WdfRequestSetInformation,
                        request,
                        size_of::<ExportInfo>() as u64
                    );
                }
                status = STATUS_SUCCESS;
            }
        }
        IOCTL_TRANSFER => {
            let mut buffer: *mut c_void = null_mut();
            status = unsafe {
                call_unsafe_wdf_function_binding!(
                    WdfRequestRetrieveInputBuffer,
                    request,
                    TRANSFER_HEADER_SIZE,
                    addr_of_mut!(buffer),
                    null_mut()
                )
            };
            if nt_success(status) {
                let transfer = unsafe { &mut *buffer.cast::<ExportTransfer>() };
                if transfer.size < TRANSFER_HEADER_SIZE as u32
                    || transfer.transfer_length as usize > MAX_TRANSFER
                    || transfer.size as usize > input_buffer_length
                    || output_buffer_length < TRANSFER_HEADER_SIZE
                {
                    status = STATUS_INVALID_BUFFER_SIZE;
                } else {
                    let capacity = (output_buffer_length - TRANSFER_HEADER_SIZE) as u32;
                    let device_to_host = if transfer.endpoint_address as u8 == 0 {
                        transfer.setup[0] & 0x80 != 0
                    } else {
                        transfer.endpoint_address & 0x80 != 0
                    };
                    if !device_to_host
                        && transfer.transfer_length as usize
                            > input_buffer_length - TRANSFER_HEADER_SIZE
                    {
                        status = STATUS_INVALID_BUFFER_SIZE;
                    } else {
                        transfer.status = STATUS_PENDING;
                        transfer.actual_length = 0;
                        let operation_status = if transfer.endpoint_address as u8 == 0 {
                            unsafe { control_transfer(context, transfer, capacity) }
                        } else {
                            unsafe { pipe_transfer(context, transfer, capacity) }
                        };
                        transfer.status = operation_status;
                        unsafe {
                            call_unsafe_wdf_function_binding!(
                                WdfRequestSetInformation,
                                request,
                                (TRANSFER_HEADER_SIZE
                                    + if nt_success(operation_status) && device_to_host {
                                        transfer.actual_length as usize
                                    } else {
                                        0
                                    }) as u64,
                            )
                        };
                        status = STATUS_SUCCESS;
                    }
                }
            }
        }
        IOCTL_ABORT_ENDPOINT => {
            let mut buffer: *mut c_void = null_mut();
            status = unsafe {
                call_unsafe_wdf_function_binding!(
                    WdfRequestRetrieveInputBuffer,
                    request,
                    size_of::<ExportAbort>(),
                    addr_of_mut!(buffer),
                    null_mut()
                )
            };
            if nt_success(status) {
                let abort = unsafe { &*buffer.cast::<ExportAbort>() };
                if abort.size != size_of::<ExportAbort>() as u32 || abort.endpoint_number > 15 {
                    status = STATUS_INVALID_PARAMETER;
                } else {
                    let mut pipes = [null_mut(); MAX_PIPES];
                    let mut count = 0usize;
                    status = STATUS_NOT_FOUND;
                    let _ = unsafe {
                        call_unsafe_wdf_function_binding!(
                            WdfWaitLockAcquire,
                            context.state_lock,
                            null_mut()
                        )
                    };
                    let state = unsafe { &*context.pipe_state.get() };
                    for index in 0..state.pipe_count as usize {
                        if state.pipe_addresses[index] & 0x0f == abort.endpoint_number as u8 {
                            pipes[count] = state.pipes[index];
                            count += 1;
                        }
                    }
                    for pipe in pipes.iter().take(count) {
                        status = unsafe {
                            abort_pipe(
                                context,
                                *pipe,
                                URB_FUNCTION_ABORT_PIPE as u16,
                                QUIESCE_TIMEOUT_MS,
                            )
                        };
                    }
                    unsafe {
                        call_unsafe_wdf_function_binding!(WdfWaitLockRelease, context.state_lock)
                    };
                }
            }
        }
        _ => {}
    }
    unsafe { call_unsafe_wdf_function_binding!(WdfRequestComplete, request, status) };
}

unsafe extern "C" fn prepare_hardware(
    device: WDFDEVICE,
    _resources_raw: WDFCMRESLIST,
    _resources_translated: WDFCMRESLIST,
) -> NTSTATUS {
    let context = unsafe { &mut *get_context(device) };
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfUsbTargetDeviceCreate,
            device,
            WDF_NO_OBJECT_ATTRIBUTES,
            addr_of_mut!(context.usb_device)
        )
    };
    if !nt_success(status) {
        return status;
    }
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfUsbTargetDeviceGetDeviceDescriptor,
            context.usb_device,
            addr_of_mut!(context.device_descriptor)
        )
    };
    let mut information = WDF_USB_DEVICE_INFORMATION {
        Size: size_of::<WDF_USB_DEVICE_INFORMATION>() as u32,
        ..WDF_USB_DEVICE_INFORMATION::default()
    };
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfUsbTargetDeviceRetrieveInformation,
            context.usb_device,
            addr_of_mut!(information)
        )
    };
    if !nt_success(status) {
        return status;
    }
    context.speed = if information.Traits
        & _WDF_USB_DEVICE_TRAITS::WDF_USB_DEVICE_TRAIT_AT_HIGH_SPEED as u32
        != 0
    {
        SPEED_HIGH
    } else {
        SPEED_FULL
    };
    let state = unsafe { &mut *context.pipe_state.get() };
    state.configuration_value = 0;
    state.configuration_handle = null_mut();
    state.pipe_count = 0;
    context.active_pipe_transfers.store(0, Ordering::SeqCst);
    unsafe {
        KeInitializeEvent(
            context.no_active_pipe_transfers.get(),
            _EVENT_TYPE::NotificationEvent,
            1,
        )
    };
    for index in 0..256 {
        context.active_interface_transfers[index].store(0, Ordering::SeqCst);
        unsafe {
            KeInitializeEvent(
                context.no_active_interface_transfers[index].get(),
                _EVENT_TYPE::NotificationEvent,
                1,
            )
        };
    }
    STATUS_SUCCESS
}

unsafe extern "C" fn release_hardware(
    device: WDFDEVICE,
    _resources_translated: WDFCMRESLIST,
) -> NTSTATUS {
    let context = unsafe { &mut *get_context(device) };
    context.usb_device = null_mut();
    let state = unsafe { &mut *context.pipe_state.get() };
    state.pipe_count = 0;
    state.configuration_value = 0;
    state.configuration_handle = null_mut();
    STATUS_SUCCESS
}

unsafe extern "C" fn device_add(_driver: WDFDRIVER, mut device_init: PWDFDEVICE_INIT) -> NTSTATUS {
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceInitSetDeviceType,
            device_init,
            FILE_DEVICE_UNKNOWN
        );
        call_unsafe_wdf_function_binding!(
            WdfDeviceInitSetIoType,
            device_init,
            _WDF_DEVICE_IO_TYPE::WdfDeviceIoBuffered
        );
    }
    let mut callbacks = WDF_PNPPOWER_EVENT_CALLBACKS {
        Size: size_of::<WDF_PNPPOWER_EVENT_CALLBACKS>() as u32,
        EvtDevicePrepareHardware: Some(prepare_hardware),
        EvtDeviceReleaseHardware: Some(release_hardware),
        ..WDF_PNPPOWER_EVENT_CALLBACKS::default()
    };
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceInitSetPnpPowerEventCallbacks,
            device_init,
            addr_of_mut!(callbacks)
        )
    };
    let mut attributes = RawObjectAttributes::for_device_context();
    let mut device: WDFDEVICE = null_mut();
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreate,
            addr_of_mut!(device_init),
            attributes.as_wdf_ptr(),
            addr_of_mut!(device)
        )
    };
    if !nt_success(status) {
        return status;
    }
    let context = unsafe { get_context(device) };
    unsafe { context.write(zeroed()) };
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfWaitLockCreate,
            WDF_NO_OBJECT_ATTRIBUTES,
            addr_of_mut!((*context).state_lock)
        )
    };
    if !nt_success(status) {
        return status;
    }
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreateDeviceInterface,
            device,
            addr_of!(DEVICE_INTERFACE_GUID),
            null()
        )
    };
    if !nt_success(status) {
        return status;
    }
    let mut queue_config = WDF_IO_QUEUE_CONFIG {
        Size: size_of::<WDF_IO_QUEUE_CONFIG>() as u32,
        DispatchType: _WDF_IO_QUEUE_DISPATCH_TYPE::WdfIoQueueDispatchParallel,
        PowerManaged: _WDF_TRI_STATE::WdfUseDefault,
        DefaultQueue: 1,
        EvtIoDeviceControl: Some(io_device_control),
        Settings: _WDF_IO_QUEUE_CONFIG__bindgen_ty_1 {
            Parallel: _WDF_IO_QUEUE_CONFIG__bindgen_ty_1__bindgen_ty_1 {
                NumberOfPresentedRequests: u32::MAX,
            },
        },
        ..WDF_IO_QUEUE_CONFIG::default()
    };
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfIoQueueCreate,
            device,
            addr_of_mut!(queue_config),
            WDF_NO_OBJECT_ATTRIBUTES,
            WDF_NO_HANDLE.cast()
        )
    }
}

#[unsafe(export_name = "DriverEntry")]
extern "system" fn driver_entry(
    driver: &mut DRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    let mut config = WDF_DRIVER_CONFIG {
        Size: size_of::<WDF_DRIVER_CONFIG>() as u32,
        EvtDriverDeviceAdd: Some(device_add),
        ..WDF_DRIVER_CONFIG::default()
    };
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDriverCreate,
            driver as PDRIVER_OBJECT,
            registry_path,
            WDF_NO_OBJECT_ATTRIBUTES,
            addr_of_mut!(config),
            WDF_NO_HANDLE.cast::<WDFDRIVER>(),
        )
    }
}
