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

typedef struct _GT_CONTROLLER {
    WDFDEVICE      Device;
    UDECXUSBDEVICE Child;
} GT_CONTROLLER, *PGT_CONTROLLER;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(GT_CONTROLLER, GtControllerGet)

DRIVER_INITIALIZE DriverEntry;
EVT_WDF_DRIVER_DEVICE_ADD GtEvtDeviceAdd;
EVT_WDF_IO_QUEUE_IO_DEVICE_CONTROL GtEvtIoDeviceControl;
EVT_UDECX_WDF_DEVICE_QUERY_USB_CAPABILITY GtEvtQueryUsbCapability;

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

    switch (IoControlCode) {
    case IOCTL_INTERDRIVER_UDE_PLUG_IN:
    case IOCTL_INTERDRIVER_UDE_UNPLUG:
        /* Device plug-in lands here once the user-mode service drives it. */
        WdfRequestComplete(Request, STATUS_NOT_IMPLEMENTED);
        break;
    default:
        WdfRequestComplete(Request, STATUS_INVALID_DEVICE_REQUEST);
        break;
    }
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

    WDF_IO_QUEUE_CONFIG_INIT_DEFAULT_QUEUE(&queueConfig, WdfIoQueueDispatchParallel);
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
