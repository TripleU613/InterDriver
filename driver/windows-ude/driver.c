/*
 * InterDriver virtual USB host controller (UdeCx).
 *
 * A thin kernel shim: it owns the emulated controller and root port, and
 * exposes IOCTLs so a user-mode Rust service can plug in a device, feed
 * endpoint traffic, and unplug it. All USB/IP protocol, policy and session
 * logic stays in user mode; nothing device-specific lives here.
 */

#include <ntddk.h>
#include <wdf.h>
#include <usb.h>
#include <usbspec.h>
#include <usbdlib.h>
#include <wdfusb.h>
#include <ude/1.0/UdeCx.h>

DRIVER_INITIALIZE DriverEntry;
EVT_WDF_DRIVER_DEVICE_ADD GtUdeEvtDeviceAdd;

static NTSTATUS
GtUdeCreateController(
    _In_ WDFDEVICE Device
    )
{
    NTSTATUS status;
    UDECX_WDF_DEVICE_CONFIG config;

    UDECX_WDF_DEVICE_CONFIG_INIT(&config, NULL);

    status = UdecxWdfDeviceAddUsbDeviceEmulation(Device, &config);
    return status;
}

NTSTATUS
GtUdeEvtDeviceAdd(
    _In_ WDFDRIVER Driver,
    _Inout_ PWDFDEVICE_INIT DeviceInit
    )
{
    NTSTATUS status;
    WDFDEVICE device;

    UNREFERENCED_PARAMETER(Driver);

    status = UdecxInitializeWdfDeviceInit(DeviceInit);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    status = WdfDeviceCreate(&DeviceInit, WDF_NO_OBJECT_ATTRIBUTES, &device);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    return GtUdeCreateController(device);
}

NTSTATUS
DriverEntry(
    _In_ PDRIVER_OBJECT DriverObject,
    _In_ PUNICODE_STRING RegistryPath
    )
{
    WDF_DRIVER_CONFIG config;

    WDF_DRIVER_CONFIG_INIT(&config, GtUdeEvtDeviceAdd);

    return WdfDriverCreate(DriverObject, RegistryPath, WDF_NO_OBJECT_ATTRIBUTES,
                           &config, WDF_NO_HANDLE);
}
