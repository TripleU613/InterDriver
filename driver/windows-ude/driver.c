/*
 * InterDriver virtual USB host controller (UdeCx).
 *
 * A thin kernel shim: it owns the emulated controller and its root port and
 * exposes an IOCTL interface. All USB/IP protocol, session and policy logic
 * stays in the user-mode Rust service, so nothing device-specific is compiled
 * into kernel code.
 */

#include <initguid.h>
#include <ntddk.h>
#include <wdf.h>
#include <usb.h>
#include <usbspec.h>
#include <usbdlib.h>
#include <wdfusb.h>
#include <ude/1.0/UdeCx.h>
#include "interdriver_ude_ioctl.h"

typedef struct _GT_PENDING_URB {
    WDFREQUEST Request;
    ULONG      Id;
    BOOLEAN    InUse;
    BOOLEAN    Delivered;
} GT_PENDING_URB, *PGT_PENDING_URB;

typedef struct _GT_CONTROLLER {
    WDFDEVICE        Device;
    UDECXUSBDEVICE   Child;
    UDECXUSBENDPOINT Control;
    WDFQUEUE         ControlQueue;
    WDFQUEUE         FetchQueue;
    WDFSPINLOCK      Lock;
    GT_PENDING_URB   Pending[INTERDRIVER_UDE_MAX_PENDING];
    ULONG            NextId;
} GT_CONTROLLER, *PGT_CONTROLLER;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(GT_CONTROLLER, GtControllerGet)

DRIVER_INITIALIZE DriverEntry;
EVT_WDF_DRIVER_DEVICE_ADD GtEvtDeviceAdd;
EVT_WDF_IO_QUEUE_IO_DEVICE_CONTROL GtEvtIoDeviceControl;
EVT_UDECX_WDF_DEVICE_QUERY_USB_CAPABILITY GtEvtQueryUsbCapability;
EVT_UDECX_USB_ENDPOINT_RESET GtEvtEndpointReset;
EVT_UDECX_USB_DEVICE_D0_ENTRY GtEvtDeviceD0Entry;
EVT_UDECX_USB_DEVICE_D0_EXIT GtEvtDeviceD0Exit;
EVT_WDF_IO_QUEUE_IO_INTERNAL_DEVICE_CONTROL GtEvtControlUrb;

static NTSTATUS GtPlugIn(_In_ PGT_CONTROLLER Controller, _In_ PINTERDRIVER_UDE_PLUG_IN_REQUEST Request);
static NTSTATUS GtUnplug(_In_ PGT_CONTROLLER Controller);
static VOID GtDrainPendingUrbs(_In_ PGT_CONTROLLER Controller, _In_ NTSTATUS Status);

/*
 * UdeCx asks which optional host capabilities the controller supports. The
 * emulated controller offers none of them, which is legal and keeps the shim
 * minimal.
 */
NTSTATUS
GtEvtQueryUsbCapability(
    _In_ WDFDEVICE UdecxWdfDevice,
    _In_ PGUID CapabilityType,
    _In_ ULONG OutputBufferLength,
    _Out_writes_to_opt_(OutputBufferLength, *ResultLength) PVOID OutputBuffer,
    _Out_ PULONG ResultLength
    )
{
    UNREFERENCED_PARAMETER(UdecxWdfDevice);
    UNREFERENCED_PARAMETER(CapabilityType);
    UNREFERENCED_PARAMETER(OutputBufferLength);
    UNREFERENCED_PARAMETER(OutputBuffer);

    *ResultLength = 0;
    return STATUS_NOT_IMPLEMENTED;
}

/*
 * Fills a waiting user-mode fetch request from a pending URB. Caller holds the
 * lock. Returns TRUE if the fetch was completed.
 */
static BOOLEAN
GtDeliver(
    _In_ PGT_CONTROLLER Controller,
    _In_ PGT_PENDING_URB Slot,
    _In_ WDFREQUEST Fetch
    )
{
    NTSTATUS status;
    PINTERDRIVER_UDE_URB out;
    size_t outLength;
    WDF_USB_CONTROL_SETUP_PACKET setup;
    PUCHAR buffer = NULL;
    ULONG length = 0;
    ULONG payload = 0;

    status = WdfRequestRetrieveOutputBuffer(Fetch, sizeof(*out), (PVOID *)&out, &outLength);
    if (!NT_SUCCESS(status)) {
        WdfRequestComplete(Fetch, status);
        return TRUE;
    }
    status = UdecxUrbRetrieveControlSetupPacket(Slot->Request, &setup);
    if (!NT_SUCCESS(status)) {
        WdfRequestComplete(Fetch, status);
        return TRUE;
    }
    if (NT_SUCCESS(UdecxUrbRetrieveBuffer(Slot->Request, &buffer, &length))) {
        if ((setup.Packet.bm.Byte & 0x80) == 0 && buffer != NULL) {
            payload = length;
        }
    }
    if (outLength < sizeof(*out) + payload) {
        WdfRequestComplete(Fetch, STATUS_BUFFER_TOO_SMALL);
        return TRUE;
    }

    out->Id = Slot->Id;
    out->TransferLength = length;
    out->DataLength = payload;
    RtlCopyMemory(out->Setup, &setup, sizeof(out->Setup));
    if (payload != 0) {
        RtlCopyMemory(out + 1, buffer, payload);
    }
    Slot->Delivered = TRUE;
    WdfRequestSetInformation(Fetch, sizeof(*out) + payload);
    WdfRequestComplete(Fetch, STATUS_SUCCESS);
    UNREFERENCED_PARAMETER(Controller);
    return TRUE;
}

/*
 * Endpoint 0 traffic. Each URB is parked in a slot and handed to the user-mode
 * service, which answers it from the real device over USB/IP.
 */
VOID
GtEvtControlUrb(
    _In_ WDFQUEUE Queue,
    _In_ WDFREQUEST Request,
    _In_ size_t OutputBufferLength,
    _In_ size_t InputBufferLength,
    _In_ ULONG IoControlCode
    )
{
    PGT_CONTROLLER controller = GtControllerGet(WdfIoQueueGetDevice(Queue));
    WDFREQUEST fetch = NULL;
    PGT_PENDING_URB slot = NULL;
    ULONG index;

    UNREFERENCED_PARAMETER(OutputBufferLength);
    UNREFERENCED_PARAMETER(InputBufferLength);
    UNREFERENCED_PARAMETER(IoControlCode);

    WdfSpinLockAcquire(controller->Lock);
    for (index = 0; index < INTERDRIVER_UDE_MAX_PENDING; index++) {
        if (!controller->Pending[index].InUse) {
            slot = &controller->Pending[index];
            break;
        }
    }
    if (slot == NULL) {
        WdfSpinLockRelease(controller->Lock);
        UdecxUrbCompleteWithNtStatus(Request, STATUS_INSUFFICIENT_RESOURCES);
        return;
    }
    slot->InUse = TRUE;
    slot->Delivered = FALSE;
    slot->Request = Request;
    slot->Id = ++controller->NextId;

    if (NT_SUCCESS(WdfIoQueueRetrieveNextRequest(controller->FetchQueue, &fetch))) {
        GtDeliver(controller, slot, fetch);
    }
    WdfSpinLockRelease(controller->Lock);
}

VOID
GtEvtEndpointReset(
    _In_ UDECXUSBENDPOINT Endpoint,
    _In_ WDFREQUEST Request
    )
{
    UNREFERENCED_PARAMETER(Endpoint);
    WdfRequestComplete(Request, STATUS_SUCCESS);
}

NTSTATUS
GtEvtDeviceD0Entry(
    _In_ WDFDEVICE UdecxWdfDevice,
    _In_ UDECXUSBDEVICE UdecxUsbDevice
    )
{
    UNREFERENCED_PARAMETER(UdecxWdfDevice);
    UdecxUsbDeviceLinkPowerEntryComplete(UdecxUsbDevice, STATUS_SUCCESS);
    return STATUS_SUCCESS;
}

NTSTATUS
GtEvtDeviceD0Exit(
    _In_ WDFDEVICE UdecxWdfDevice,
    _In_ UDECXUSBDEVICE UdecxUsbDevice,
    _In_ UDECX_USB_DEVICE_WAKE_SETTING WakeSetting
    )
{
    UNREFERENCED_PARAMETER(UdecxWdfDevice);
    UNREFERENCED_PARAMETER(WakeSetting);
    UdecxUsbDeviceLinkPowerExitComplete(UdecxUsbDevice, STATUS_SUCCESS);
    return STATUS_SUCCESS;
}


/*
 * Creates the emulated device from descriptors captured off the real device and
 * attaches it to the root port. Windows enumerates from these descriptors, so
 * the device appears without any endpoint traffic.
 */
static NTSTATUS
GtPlugIn(
    _In_ PGT_CONTROLLER Controller,
    _In_ PINTERDRIVER_UDE_PLUG_IN_REQUEST Request
    )
{
    NTSTATUS status;
    PUDECXUSBDEVICE_INIT init = NULL;
    UDECX_USB_DEVICE_STATE_CHANGE_CALLBACKS callbacks;
    UDECX_USB_DEVICE_PLUG_IN_OPTIONS options;
    UDECX_USB_ENDPOINT_CALLBACKS endpointCallbacks;
    PUDECXUSBENDPOINT_INIT endpointInit = NULL;
    UDECX_USB_DEVICE_SPEED speed;
    PUCHAR bytes;

    if (Controller->Child != NULL) {
        return STATUS_DEVICE_ALREADY_ATTACHED;
    }

    switch (Request->Speed) {
    case INTERDRIVER_UDE_SPEED_FULL:  speed = UdecxUsbFullSpeed;  break;
    case INTERDRIVER_UDE_SPEED_SUPER: speed = UdecxUsbSuperSpeed; break;
    default:                      speed = UdecxUsbHighSpeed;  break;
    }

    init = UdecxUsbDeviceInitAllocate(Controller->Device);
    if (init == NULL) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }

    UdecxUsbDeviceInitSetSpeed(init, speed);
    UdecxUsbDeviceInitSetEndpointsType(init, UdecxEndpointTypeSimple);

    bytes = (PUCHAR)(Request + 1);
    status = UdecxUsbDeviceInitAddDescriptor(init, bytes, Request->DeviceDescriptorLength);
    if (!NT_SUCCESS(status)) {
        goto fail;
    }
    bytes += Request->DeviceDescriptorLength;
    status = UdecxUsbDeviceInitAddDescriptor(init, bytes, Request->ConfigDescriptorLength);
    if (!NT_SUCCESS(status)) {
        goto fail;
    }

    UDECX_USB_DEVICE_CALLBACKS_INIT(&callbacks);
    callbacks.EvtUsbDeviceLinkPowerEntry = GtEvtDeviceD0Entry;
    callbacks.EvtUsbDeviceLinkPowerExit = GtEvtDeviceD0Exit;
    UdecxUsbDeviceInitSetStateChangeCallbacks(init, &callbacks);

    status = UdecxUsbDeviceCreate(&init, WDF_NO_OBJECT_ATTRIBUTES, &Controller->Child);
    if (!NT_SUCCESS(status)) {
        goto fail;
    }
    init = NULL;

    endpointInit = UdecxUsbSimpleEndpointInitAllocate(Controller->Child);
    if (endpointInit == NULL) {
        status = STATUS_INSUFFICIENT_RESOURCES;
        goto fail;
    }
    UdecxUsbEndpointInitSetEndpointAddress(endpointInit, USB_DEFAULT_ENDPOINT_ADDRESS);
    UDECX_USB_ENDPOINT_CALLBACKS_INIT(&endpointCallbacks, GtEvtEndpointReset);
    UdecxUsbEndpointInitSetCallbacks(endpointInit, &endpointCallbacks);

    status = UdecxUsbEndpointCreate(&endpointInit, WDF_NO_OBJECT_ATTRIBUTES,
                                    &Controller->Control);
    if (!NT_SUCCESS(status)) {
        goto fail;
    }
    endpointInit = NULL;
    UdecxUsbEndpointSetWdfIoQueue(Controller->Control, Controller->ControlQueue);

    UDECX_USB_DEVICE_PLUG_IN_OPTIONS_INIT(&options);
    if (speed == UdecxUsbSuperSpeed) {
        options.Usb30PortNumber = 1;
    } else {
        options.Usb20PortNumber = 1;
    }

    status = UdecxUsbDevicePlugIn(Controller->Child, &options);
    if (!NT_SUCCESS(status)) {
        goto fail;
    }
    return STATUS_SUCCESS;

fail:
    if (endpointInit != NULL) {
        UdecxUsbEndpointInitFree(endpointInit);
    }
    if (init != NULL) {
        UdecxUsbDeviceInitFree(init);
    }
    if (Controller->Child != NULL) {
        WdfObjectDelete(Controller->Child);
        Controller->Child = NULL;
    }
    return status;
}

static NTSTATUS
GtUnplug(
    _In_ PGT_CONTROLLER Controller
    )
{
    NTSTATUS status;

    if (Controller->Child == NULL) {
        return STATUS_DEVICE_NOT_CONNECTED;
    }

    GtDrainPendingUrbs(Controller, STATUS_DEVICE_NOT_CONNECTED);

    status = UdecxUsbDevicePlugOutAndDelete(Controller->Child);
    Controller->Child = NULL;
    Controller->Control = NULL;
    return status;
}

/*
 * Detaches every outstanding URB from the bookkeeping table while holding the
 * spin lock, then completes the requests after dropping it. UdeCx completion
 * can synchronously run PnP teardown callbacks; completing while the lock is
 * held therefore risks re-entry, deadlock, and access to an object whose
 * teardown is already in progress.
 */
static VOID
GtDrainPendingUrbs(
    _In_ PGT_CONTROLLER Controller,
    _In_ NTSTATUS Status
    )
{
    WDFREQUEST requests[INTERDRIVER_UDE_MAX_PENDING];
    ULONG count = 0;
    ULONG index;

    WdfSpinLockAcquire(Controller->Lock);
    for (index = 0; index < INTERDRIVER_UDE_MAX_PENDING; index++) {
        if (Controller->Pending[index].InUse) {
            WDFREQUEST urb = Controller->Pending[index].Request;
            Controller->Pending[index].InUse = FALSE;
            Controller->Pending[index].Delivered = FALSE;
            Controller->Pending[index].Request = NULL;
            if (urb != NULL) {
                requests[count++] = urb;
            }
        }
    }
    WdfSpinLockRelease(Controller->Lock);

    for (index = 0; index < count; index++) {
        UdecxUrbCompleteWithNtStatus(requests[index], Status);
    }
}

VOID
GtEvtIoDeviceControl(
    _In_ WDFQUEUE Queue,
    _In_ WDFREQUEST Request,
    _In_ size_t OutputBufferLength,
    _In_ size_t InputBufferLength,
    _In_ ULONG IoControlCode
    )
{
    UNREFERENCED_PARAMETER(Queue);
    UNREFERENCED_PARAMETER(OutputBufferLength);
    UNREFERENCED_PARAMETER(InputBufferLength);

    PGT_CONTROLLER controller = GtControllerGet(WdfIoQueueGetDevice(Queue));
    NTSTATUS status;
    PINTERDRIVER_UDE_PLUG_IN_REQUEST plug;
    size_t length;

    switch (IoControlCode) {
    case IOCTL_INTERDRIVER_UDE_PLUG_IN:
        status = WdfRequestRetrieveInputBuffer(Request, sizeof(*plug), (PVOID *)&plug, &length);
        if (!NT_SUCCESS(status)) {
            break;
        }
        if (plug->Size != length ||
            plug->DeviceDescriptorLength > INTERDRIVER_UDE_MAX_DESCRIPTOR ||
            plug->ConfigDescriptorLength > INTERDRIVER_UDE_MAX_DESCRIPTOR ||
            plug->DeviceDescriptorLength > length - sizeof(*plug) ||
            plug->ConfigDescriptorLength >
                length - sizeof(*plug) - plug->DeviceDescriptorLength) {
            status = STATUS_INVALID_BUFFER_SIZE;
            break;
        }
        status = GtPlugIn(controller, plug);
        break;
    case IOCTL_INTERDRIVER_UDE_UNPLUG:
        status = GtUnplug(controller);
        break;
    case IOCTL_INTERDRIVER_UDE_FETCH_URB: {
        PGT_PENDING_URB slot = NULL;
        ULONG index;
        WdfSpinLockAcquire(controller->Lock);
        for (index = 0; index < INTERDRIVER_UDE_MAX_PENDING; index++) {
            if (controller->Pending[index].InUse && !controller->Pending[index].Delivered) {
                slot = &controller->Pending[index];
                break;
            }
        }
        if (slot != NULL) {
            GtDeliver(controller, slot, Request);
            WdfSpinLockRelease(controller->Lock);
            return;
        }
        /* Nothing waiting: park the fetch until a URB arrives. */
        status = WdfRequestForwardToIoQueue(Request, controller->FetchQueue);
        WdfSpinLockRelease(controller->Lock);
        if (NT_SUCCESS(status)) {
            return;
        }
        break;
    }
    case IOCTL_INTERDRIVER_UDE_COMPLETE_URB: {
        PINTERDRIVER_UDE_URB_RESULT result;
        PUCHAR buffer = NULL;
        ULONG capacity = 0;
        ULONG index;
        WDFREQUEST urb = NULL;

        status = WdfRequestRetrieveInputBuffer(Request, sizeof(*result), (PVOID *)&result, &length);
        if (!NT_SUCCESS(status)) {
            break;
        }
        if (result->DataLength > INTERDRIVER_UDE_MAX_TRANSFER ||
            result->DataLength > length - sizeof(*result)) {
            status = STATUS_INVALID_BUFFER_SIZE;
            break;
        }
        WdfSpinLockAcquire(controller->Lock);
        for (index = 0; index < INTERDRIVER_UDE_MAX_PENDING; index++) {
            if (controller->Pending[index].InUse &&
                controller->Pending[index].Id == result->Id) {
                urb = controller->Pending[index].Request;
                controller->Pending[index].InUse = FALSE;
                controller->Pending[index].Request = NULL;
                break;
            }
        }
        WdfSpinLockRelease(controller->Lock);
        if (urb == NULL) {
            status = STATUS_NOT_FOUND;
            break;
        }
        if (result->Status != 0) {
            UdecxUrbCompleteWithNtStatus(urb, STATUS_UNSUCCESSFUL);
            status = STATUS_SUCCESS;
            break;
        }
        if (result->DataLength != 0 &&
            NT_SUCCESS(UdecxUrbRetrieveBuffer(urb, &buffer, &capacity)) &&
            buffer != NULL) {
            ULONG copy = min(capacity, result->DataLength);
            RtlCopyMemory(buffer, result + 1, copy);
            UdecxUrbSetBytesCompleted(urb, copy);
        } else {
            UdecxUrbSetBytesCompleted(urb, 0);
        }
        UdecxUrbCompleteWithNtStatus(urb, STATUS_SUCCESS);
        status = STATUS_SUCCESS;
        break;
    }
    default:
        status = STATUS_INVALID_DEVICE_REQUEST;
        break;
    }
    WdfRequestComplete(Request, status);
}

NTSTATUS
GtEvtDeviceAdd(
    _In_ WDFDRIVER Driver,
    _Inout_ PWDFDEVICE_INIT DeviceInit
    )
{
    NTSTATUS status;
    WDFDEVICE device;
    WDF_OBJECT_ATTRIBUTES attributes;
    UDECX_WDF_DEVICE_CONFIG config;
    WDF_IO_QUEUE_CONFIG queueConfig;
    WDFQUEUE queue;

    UNREFERENCED_PARAMETER(Driver);

    status = UdecxInitializeWdfDeviceInit(DeviceInit);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, GT_CONTROLLER);

    status = WdfDeviceCreate(&DeviceInit, &attributes, &device);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    RtlZeroMemory(GtControllerGet(device), sizeof(GT_CONTROLLER));
    GtControllerGet(device)->Device = device;

    status = WdfDeviceCreateDeviceInterface(device, &GUID_DEVINTERFACE_INTERDRIVER_UDE, NULL);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    UDECX_WDF_DEVICE_CONFIG_INIT(&config, GtEvtQueryUsbCapability);

    status = UdecxWdfDeviceAddUsbDeviceEmulation(device, &config);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    status = WdfSpinLockCreate(WDF_NO_OBJECT_ATTRIBUTES, &GtControllerGet(device)->Lock);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    WDF_IO_QUEUE_CONFIG_INIT(&queueConfig, WdfIoQueueDispatchManual);
    status = WdfIoQueueCreate(device, &queueConfig, WDF_NO_OBJECT_ATTRIBUTES,
                              &GtControllerGet(device)->FetchQueue);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    // The endpoint queue is owned by the controller, not by the emulated
    // device. Parenting it to the UDECXUSBDEVICE would free it when that device
    // is torn down while this context still referenced it, which faulted on a
    // stale pointer the next time an URB arrived.
    WDF_IO_QUEUE_CONFIG_INIT(&queueConfig, WdfIoQueueDispatchParallel);
    queueConfig.EvtIoInternalDeviceControl = GtEvtControlUrb;
    status = WdfIoQueueCreate(device, &queueConfig, WDF_NO_OBJECT_ATTRIBUTES,
                              &GtControllerGet(device)->ControlQueue);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    // Serialize plug, unplug and completion IOCTLs. In particular, this keeps
    // an explicit unplug from racing a late user-mode completion that still
    // refers to the child being removed.
    WDF_IO_QUEUE_CONFIG_INIT_DEFAULT_QUEUE(&queueConfig, WdfIoQueueDispatchSequential);
    queueConfig.EvtIoDeviceControl = GtEvtIoDeviceControl;

    return WdfIoQueueCreate(device, &queueConfig, WDF_NO_OBJECT_ATTRIBUTES, &queue);
}

NTSTATUS
DriverEntry(
    _In_ PDRIVER_OBJECT DriverObject,
    _In_ PUNICODE_STRING RegistryPath
    )
{
    WDF_DRIVER_CONFIG config;

    WDF_DRIVER_CONFIG_INIT(&config, GtEvtDeviceAdd);

    return WdfDriverCreate(DriverObject, RegistryPath, WDF_NO_OBJECT_ATTRIBUTES,
                           &config, WDF_NO_HANDLE);
}
