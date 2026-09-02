/*
 * Exclusive KMDF function driver for exporting a physical USB device.
 *
 * Unlike winusb.sys, this driver owns the composite parent and supports USB
 * devices with multiple configurations. User mode keeps USB/IP, networking,
 * policy, and lifecycle logic; this driver only translates bounded IOCTLs to
 * KMDF USB target operations.
 */

#include <initguid.h>
#include <ntddk.h>
#include <wdf.h>
#include <usb.h>
#include <usbspec.h>
#include <usbdlib.h>
#include <wdfusb.h>
#include "interdriver_export_ioctl.h"

#define GT_MAX_PIPES 32
#define GT_TRANSFER_HEADER_SIZE FIELD_OFFSET(INTERDRIVER_EXPORT_TRANSFER, Reserved) + sizeof(unsigned long)

typedef struct _GT_EXPORT_DEVICE {
    WDFUSBDEVICE UsbDevice;
    WDFWAITLOCK StateLock;
    USBD_CONFIGURATION_HANDLE ConfigurationHandle;
    USBD_PIPE_HANDLE Pipes[GT_MAX_PIPES];
    UCHAR PipeAddresses[GT_MAX_PIPES];
    UCHAR PipeInterfaces[GT_MAX_PIPES];
    ULONG PipeCount;
    volatile LONG ActivePipeTransfers;
    KEVENT NoActivePipeTransfers;
    volatile LONG ActiveInterfaceTransfers[256];
    KEVENT NoActiveInterfaceTransfers[256];
    UCHAR ConfigurationValue;
    USB_DEVICE_DESCRIPTOR DeviceDescriptor;
    ULONG Speed;
} GT_EXPORT_DEVICE, *PGT_EXPORT_DEVICE;

WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(GT_EXPORT_DEVICE, GtExportGet)

DRIVER_INITIALIZE DriverEntry;
EVT_WDF_DRIVER_DEVICE_ADD GtEvtDeviceAdd;
EVT_WDF_DEVICE_PREPARE_HARDWARE GtEvtPrepareHardware;
EVT_WDF_DEVICE_RELEASE_HARDWARE GtEvtReleaseHardware;
EVT_WDF_IO_QUEUE_IO_DEVICE_CONTROL GtEvtIoDeviceControl;

static NTSTATUS GtControlTransfer(
    _In_ PGT_EXPORT_DEVICE Context,
    _Inout_ PINTERDRIVER_EXPORT_TRANSFER Transfer,
    _In_ ULONG Capacity);
static NTSTATUS GtPipeTransfer(
    _In_ PGT_EXPORT_DEVICE Context,
    _Inout_ PINTERDRIVER_EXPORT_TRANSFER Transfer,
    _In_ ULONG Capacity);
static NTSTATUS GtSelectConfiguration(
    _In_ PGT_EXPORT_DEVICE Context,
    _In_ UCHAR Value,
    _In_ ULONG TimeoutMs);
static NTSTATUS GtSelectInterface(
    _In_ PGT_EXPORT_DEVICE Context,
    _In_ UCHAR InterfaceNumber,
    _In_ UCHAR AlternateSetting,
    _In_ ULONG TimeoutMs);
static VOID GtStoreInterfacePipes(
    _In_ PGT_EXPORT_DEVICE Context,
    _In_ PUSBD_INTERFACE_INFORMATION Interface);
static NTSTATUS GtQuiescePipes(
    _In_ PGT_EXPORT_DEVICE Context,
    _In_ LONG InterfaceNumber,
    _In_ ULONG TimeoutMs);

static VOID
GtInitTimeout(
    _Out_ PWDF_REQUEST_SEND_OPTIONS Options,
    _In_ ULONG TimeoutMs
    )
{
    WDF_REQUEST_SEND_OPTIONS_INIT(Options, WDF_REQUEST_SEND_OPTION_TIMEOUT);
    WDF_REQUEST_SEND_OPTIONS_SET_TIMEOUT(Options, WDF_REL_TIMEOUT_IN_MS(TimeoutMs == 0 ? 30000 : TimeoutMs));
}

static USBD_PIPE_HANDLE
GtFindPipe(
    _In_ PGT_EXPORT_DEVICE Context,
    _In_ UCHAR Address,
    _Out_opt_ PUCHAR InterfaceNumber
    )
{
    ULONG index;
    for (index = 0; index < Context->PipeCount; index++) {
        if (Context->PipeAddresses[index] == Address) {
            if (InterfaceNumber != NULL) {
                *InterfaceNumber = Context->PipeInterfaces[index];
            }
            return Context->Pipes[index];
        }
    }
    return NULL;
}

static NTSTATUS
GtSendDescriptor(
    _In_ PGT_EXPORT_DEVICE Context,
    _In_ UCHAR Type,
    _In_ UCHAR Index,
    _Out_writes_bytes_(Length) PUCHAR Buffer,
    _In_ ULONG Length,
    _In_ ULONG TimeoutMs,
    _Out_opt_ PULONG Transferred
    )
{
    WDF_USB_CONTROL_SETUP_PACKET setup;
    WDF_MEMORY_DESCRIPTOR memory;
    WDF_REQUEST_SEND_OPTIONS options;

    WDF_USB_CONTROL_SETUP_PACKET_INIT(
        &setup,
        BmRequestDeviceToHost,
        BmRequestToDevice,
        USB_REQUEST_GET_DESCRIPTOR,
        (USHORT)(((USHORT)Type << 8) | Index),
        0);
    WDF_MEMORY_DESCRIPTOR_INIT_BUFFER(&memory, Buffer, Length);
    GtInitTimeout(&options, TimeoutMs);
    return WdfUsbTargetDeviceSendControlTransferSynchronously(
        Context->UsbDevice, NULL, &options, &setup, &memory, Transferred);
}

static NTSTATUS
GtReadConfigurationDescriptor(
    _In_ PGT_EXPORT_DEVICE Context,
    _In_ UCHAR Index,
    _In_ ULONG TimeoutMs,
    _Out_ WDFMEMORY *Memory,
    _Out_ PUSB_CONFIGURATION_DESCRIPTOR *Descriptor
    )
{
    USB_CONFIGURATION_DESCRIPTOR header;
    WDF_OBJECT_ATTRIBUTES attributes;
    PUCHAR bytes;
    ULONG transferred = 0;
    NTSTATUS status;

    RtlZeroMemory(&header, sizeof(header));
    status = GtSendDescriptor(
        Context, USB_CONFIGURATION_DESCRIPTOR_TYPE, Index,
        (PUCHAR)&header, sizeof(header), TimeoutMs, &transferred);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    if (transferred < sizeof(header) ||
        header.wTotalLength < sizeof(header) ||
        header.wTotalLength > 65535) {
        return STATUS_DEVICE_DATA_ERROR;
    }

    WDF_OBJECT_ATTRIBUTES_INIT(&attributes);
    attributes.ParentObject = Context->UsbDevice;
    status = WdfMemoryCreate(
        &attributes, NonPagedPoolNx, 'cEtG', header.wTotalLength,
        Memory, (PVOID *)&bytes);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    status = GtSendDescriptor(
        Context, USB_CONFIGURATION_DESCRIPTOR_TYPE, Index,
        bytes, header.wTotalLength, TimeoutMs, &transferred);
    if (!NT_SUCCESS(status) || transferred < header.wTotalLength) {
        WdfObjectDelete(*Memory);
        *Memory = NULL;
        return NT_SUCCESS(status) ? STATUS_DEVICE_DATA_ERROR : status;
    }
    *Descriptor = (PUSB_CONFIGURATION_DESCRIPTOR)bytes;
    return STATUS_SUCCESS;
}

static NTSTATUS
GtFindConfiguration(
    _In_ PGT_EXPORT_DEVICE Context,
    _In_ UCHAR Value,
    _In_ ULONG TimeoutMs,
    _Out_ WDFMEMORY *Memory,
    _Out_ PUSB_CONFIGURATION_DESCRIPTOR *Descriptor
    )
{
    UCHAR index;
    for (index = 0; index < Context->DeviceDescriptor.bNumConfigurations; index++) {
        NTSTATUS status = GtReadConfigurationDescriptor(
            Context, index, TimeoutMs, Memory, Descriptor);
        if (!NT_SUCCESS(status)) {
            return status;
        }
        if ((*Descriptor)->bConfigurationValue == Value) {
            return STATUS_SUCCESS;
        }
        WdfObjectDelete(*Memory);
        *Memory = NULL;
        *Descriptor = NULL;
    }
    return STATUS_NOT_FOUND;
}

static NTSTATUS
GtSelectConfiguration(
    _In_ PGT_EXPORT_DEVICE Context,
    _In_ UCHAR Value,
    _In_ ULONG TimeoutMs
    )
{
    WDFMEMORY descriptorMemory = NULL;
    PUSB_CONFIGURATION_DESCRIPTOR descriptor = NULL;
    USBD_INTERFACE_LIST_ENTRY interfaces[257];
    PURB urb = NULL;
    URB deconfigure;
    WDF_REQUEST_SEND_OPTIONS options;
    PUCHAR bytes;
    ULONG offset;
    ULONG count = 0;
    NTSTATUS status;

    WdfWaitLockAcquire(Context->StateLock, NULL);
    if (Value == Context->ConfigurationValue) {
        WdfWaitLockRelease(Context->StateLock);
        return STATUS_SUCCESS;
    }
    if (Value == 0) {
        RtlZeroMemory(&deconfigure, sizeof(deconfigure));
        UsbBuildSelectConfigurationRequest(
            &deconfigure, sizeof(struct _URB_SELECT_CONFIGURATION), NULL);
        GtInitTimeout(&options, TimeoutMs);
        status = WdfUsbTargetDeviceSendUrbSynchronously(
            Context->UsbDevice, NULL, &options, &deconfigure);
        if (NT_SUCCESS(status) && !USBD_SUCCESS(deconfigure.UrbHeader.Status)) {
            status = (NTSTATUS)deconfigure.UrbHeader.Status;
        }
        if (NT_SUCCESS(status)) {
            Context->ConfigurationValue = 0;
            Context->ConfigurationHandle = NULL;
            Context->PipeCount = 0;
        }
        WdfWaitLockRelease(Context->StateLock);
        return status;
    }

    status = GtFindConfiguration(
        Context, Value, TimeoutMs, &descriptorMemory, &descriptor);
    if (!NT_SUCCESS(status)) {
        WdfWaitLockRelease(Context->StateLock);
        return status;
    }

    RtlZeroMemory(interfaces, sizeof(interfaces));
    bytes = (PUCHAR)descriptor;
    offset = descriptor->bLength;
    while (offset + 2 <= descriptor->wTotalLength) {
        UCHAR length = bytes[offset];
        UCHAR type = bytes[offset + 1];
        if (length < 2 || offset + length > descriptor->wTotalLength) {
            status = STATUS_DEVICE_DATA_ERROR;
            goto done;
        }
        if (type == USB_INTERFACE_DESCRIPTOR_TYPE && length >= sizeof(USB_INTERFACE_DESCRIPTOR)) {
            PUSB_INTERFACE_DESCRIPTOR interface = (PUSB_INTERFACE_DESCRIPTOR)(bytes + offset);
            if (interface->bAlternateSetting == 0 && count < RTL_NUMBER_OF(interfaces)) {
                interfaces[count++].InterfaceDescriptor = interface;
            }
        }
        offset += length;
    }
    if (count == 0) {
        status = STATUS_DEVICE_DATA_ERROR;
        goto done;
    }

    if (Context->ConfigurationValue != 0) {
        status = GtQuiescePipes(Context, -1, TimeoutMs);
        if (!NT_SUCCESS(status)) {
            goto done;
        }
        RtlZeroMemory(&deconfigure, sizeof(deconfigure));
        UsbBuildSelectConfigurationRequest(
            &deconfigure, sizeof(struct _URB_SELECT_CONFIGURATION), NULL);
        GtInitTimeout(&options, TimeoutMs);
        status = WdfUsbTargetDeviceSendUrbSynchronously(
            Context->UsbDevice, NULL, &options, &deconfigure);
        if (NT_SUCCESS(status) && !USBD_SUCCESS(deconfigure.UrbHeader.Status)) {
            status = (NTSTATUS)deconfigure.UrbHeader.Status;
        }
        if (!NT_SUCCESS(status)) {
            goto done;
        }
        Context->ConfigurationValue = 0;
        Context->ConfigurationHandle = NULL;
        Context->PipeCount = 0;
    }

    urb = USBD_CreateConfigurationRequestEx(descriptor, interfaces);
    if (urb == NULL) {
        status = STATUS_INSUFFICIENT_RESOURCES;
        goto done;
    }
    GtInitTimeout(&options, TimeoutMs);
    status = WdfUsbTargetDeviceSendUrbSynchronously(
        Context->UsbDevice, NULL, &options, urb);
    if (NT_SUCCESS(status) && !USBD_SUCCESS(urb->UrbHeader.Status)) {
        status = (NTSTATUS)urb->UrbHeader.Status;
    }
    if (NT_SUCCESS(status)) {
        Context->ConfigurationValue = Value;
        Context->ConfigurationHandle =
            urb->UrbSelectConfiguration.ConfigurationHandle;
        Context->PipeCount = 0;
        for (offset = 0; offset < count; offset++) {
            GtStoreInterfacePipes(Context, interfaces[offset].Interface);
        }
    }

done:
    if (urb != NULL) {
        ExFreePool(urb);
    }
    if (descriptorMemory != NULL) {
        WdfObjectDelete(descriptorMemory);
    }
    WdfWaitLockRelease(Context->StateLock);
    return status;
}

static VOID
GtStoreInterfacePipes(
    _In_ PGT_EXPORT_DEVICE Context,
    _In_ PUSBD_INTERFACE_INFORMATION Interface
    )
{
    ULONG pipeIndex;
    if (Interface == NULL) {
        return;
    }
    for (pipeIndex = 0;
         pipeIndex < Interface->NumberOfPipes && Context->PipeCount < GT_MAX_PIPES;
         pipeIndex++) {
        Context->Pipes[Context->PipeCount] = Interface->Pipes[pipeIndex].PipeHandle;
        Context->PipeAddresses[Context->PipeCount] =
            Interface->Pipes[pipeIndex].EndpointAddress;
        Context->PipeInterfaces[Context->PipeCount] = Interface->InterfaceNumber;
        Context->PipeCount++;
    }
}

static NTSTATUS
GtQuiescePipes(
    _In_ PGT_EXPORT_DEVICE Context,
    _In_ LONG InterfaceNumber,
    _In_ ULONG TimeoutMs
    )
{
    ULONG index;
    NTSTATUS firstFailure = STATUS_SUCCESS;
    LARGE_INTEGER timeout;

    for (index = 0; index < Context->PipeCount; index++) {
        URB urb;
        WDF_REQUEST_SEND_OPTIONS options;
        NTSTATUS status;

        if (InterfaceNumber >= 0 &&
            Context->PipeInterfaces[index] != (UCHAR)InterfaceNumber) {
            continue;
        }
        RtlZeroMemory(&urb, sizeof(urb));
        urb.UrbPipeRequest.Hdr.Length = sizeof(struct _URB_PIPE_REQUEST);
        urb.UrbPipeRequest.Hdr.Function = URB_FUNCTION_ABORT_PIPE;
        urb.UrbPipeRequest.PipeHandle = Context->Pipes[index];
        GtInitTimeout(&options, TimeoutMs);
        status = WdfUsbTargetDeviceSendUrbSynchronously(
            Context->UsbDevice, NULL, &options, &urb);
        if (NT_SUCCESS(status) && !USBD_SUCCESS(urb.UrbHeader.Status)) {
            status = (NTSTATUS)urb.UrbHeader.Status;
        }
        if (!NT_SUCCESS(status) && NT_SUCCESS(firstFailure)) {
            firstFailure = status;
        }
    }

    timeout.QuadPart = -((LONGLONG)(TimeoutMs == 0 ? 5000 : TimeoutMs) * 10000);
    if (KeWaitForSingleObject(
            InterfaceNumber < 0
                ? &Context->NoActivePipeTransfers
                : &Context->NoActiveInterfaceTransfers[InterfaceNumber],
            Executive,
            KernelMode,
            FALSE,
            &timeout) == STATUS_TIMEOUT) {
        return STATUS_IO_TIMEOUT;
    }
    return firstFailure;
}

static NTSTATUS
GtSelectInterface(
    _In_ PGT_EXPORT_DEVICE Context,
    _In_ UCHAR InterfaceNumber,
    _In_ UCHAR AlternateSetting,
    _In_ ULONG TimeoutMs
    )
{
    WDFMEMORY descriptorMemory = NULL;
    PUSB_CONFIGURATION_DESCRIPTOR configuration = NULL;
    PUSB_INTERFACE_DESCRIPTOR descriptor = NULL;
    PURB urb = NULL;
    PUCHAR bytes;
    ULONG offset;
    ULONG urbSize;
    ULONG readIndex;
    ULONG writeIndex;
    WDF_REQUEST_SEND_OPTIONS options;
    NTSTATUS status = STATUS_NOT_FOUND;

    WdfWaitLockAcquire(Context->StateLock, NULL);
    if (Context->ConfigurationValue == 0 || Context->ConfigurationHandle == NULL) {
        WdfWaitLockRelease(Context->StateLock);
        return STATUS_INVALID_DEVICE_STATE;
    }
    status = GtQuiescePipes(Context, InterfaceNumber, TimeoutMs);
    if (!NT_SUCCESS(status)) {
        goto done;
    }
    status = GtFindConfiguration(
        Context, Context->ConfigurationValue, TimeoutMs,
        &descriptorMemory, &configuration);
    if (!NT_SUCCESS(status)) {
        goto done;
    }
    bytes = (PUCHAR)configuration;
    offset = configuration->bLength;
    while (offset + 2 <= configuration->wTotalLength) {
        UCHAR length = bytes[offset];
        UCHAR type = bytes[offset + 1];
        if (length < 2 || offset + length > configuration->wTotalLength) {
            status = STATUS_DEVICE_DATA_ERROR;
            goto done;
        }
        if (type == USB_INTERFACE_DESCRIPTOR_TYPE &&
            length >= sizeof(USB_INTERFACE_DESCRIPTOR)) {
            PUSB_INTERFACE_DESCRIPTOR candidate =
                (PUSB_INTERFACE_DESCRIPTOR)(bytes + offset);
            if (candidate->bInterfaceNumber == InterfaceNumber &&
                candidate->bAlternateSetting == AlternateSetting) {
                descriptor = candidate;
                break;
            }
        }
        offset += length;
    }
    if (descriptor == NULL) {
        status = STATUS_NOT_FOUND;
        goto done;
    }
    urbSize = GET_SELECT_INTERFACE_REQUEST_SIZE(descriptor->bNumEndpoints);
    if (urbSize > MAXUSHORT) {
        status = STATUS_INVALID_BUFFER_SIZE;
        goto done;
    }
    urb = ExAllocatePoolWithTag(NonPagedPoolNx, urbSize, 'iEtG');
    if (urb == NULL) {
        status = STATUS_INSUFFICIENT_RESOURCES;
        goto done;
    }
    RtlZeroMemory(urb, urbSize);
    UsbBuildSelectInterfaceRequest(
        urb, (USHORT)urbSize, Context->ConfigurationHandle,
        InterfaceNumber, AlternateSetting);
    urb->UrbSelectInterface.Interface.NumberOfPipes = descriptor->bNumEndpoints;
    GtInitTimeout(&options, TimeoutMs);
    status = WdfUsbTargetDeviceSendUrbSynchronously(
        Context->UsbDevice, NULL, &options, urb);
    if (NT_SUCCESS(status) && !USBD_SUCCESS(urb->UrbHeader.Status)) {
        status = (NTSTATUS)urb->UrbHeader.Status;
    }
    if (NT_SUCCESS(status)) {
        writeIndex = 0;
        for (readIndex = 0; readIndex < Context->PipeCount; readIndex++) {
            if (Context->PipeInterfaces[readIndex] != InterfaceNumber) {
                Context->Pipes[writeIndex] = Context->Pipes[readIndex];
                Context->PipeAddresses[writeIndex] = Context->PipeAddresses[readIndex];
                Context->PipeInterfaces[writeIndex] = Context->PipeInterfaces[readIndex];
                writeIndex++;
            }
        }
        Context->PipeCount = writeIndex;
        GtStoreInterfacePipes(Context, &urb->UrbSelectInterface.Interface);
    }

done:
    if (urb != NULL) {
        ExFreePool(urb);
    }
    if (descriptorMemory != NULL) {
        WdfObjectDelete(descriptorMemory);
    }
    WdfWaitLockRelease(Context->StateLock);
    return status;
}

static NTSTATUS
GtControlTransfer(
    _In_ PGT_EXPORT_DEVICE Context,
    _Inout_ PINTERDRIVER_EXPORT_TRANSFER Transfer,
    _In_ ULONG Capacity
    )
{
    WDF_USB_CONTROL_SETUP_PACKET setup;
    WDF_MEMORY_DESCRIPTOR memory;
    WDF_REQUEST_SEND_OPTIONS options;
    PUCHAR data = (PUCHAR)Transfer + GT_TRANSFER_HEADER_SIZE;
    ULONG transferred = 0;
    USHORT value;
    USHORT index;
    BOOLEAN deviceToHost;
    NTSTATUS status;

    RtlCopyMemory(&setup, Transfer->Setup, sizeof(setup));
    value = (USHORT)(Transfer->Setup[2] | ((USHORT)Transfer->Setup[3] << 8));
    index = (USHORT)(Transfer->Setup[4] | ((USHORT)Transfer->Setup[5] << 8));
    deviceToHost = (Transfer->Setup[0] & 0x80) != 0;

    if (Transfer->Setup[0] == 0x00 && Transfer->Setup[1] == USB_REQUEST_SET_CONFIGURATION) {
        return GtSelectConfiguration(Context, (UCHAR)value, Transfer->TimeoutMs);
    }
    if (Transfer->Setup[0] == 0x01 && Transfer->Setup[1] == USB_REQUEST_SET_INTERFACE) {
        return GtSelectInterface(
            Context, (UCHAR)index, (UCHAR)value, Transfer->TimeoutMs);
    }
    if (Transfer->Setup[0] == 0x02 &&
        Transfer->Setup[1] == USB_REQUEST_CLEAR_FEATURE && value == 0) {
        USBD_PIPE_HANDLE pipe;
        URB urb;
        WdfWaitLockAcquire(Context->StateLock, NULL);
        pipe = GtFindPipe(Context, (UCHAR)index, NULL);
        WdfWaitLockRelease(Context->StateLock);
        if (pipe == NULL) {
            return STATUS_NOT_FOUND;
        }
        RtlZeroMemory(&urb, sizeof(urb));
        urb.UrbPipeRequest.Hdr.Length = sizeof(struct _URB_PIPE_REQUEST);
        urb.UrbPipeRequest.Hdr.Function =
            URB_FUNCTION_SYNC_RESET_PIPE_AND_CLEAR_STALL;
        urb.UrbPipeRequest.PipeHandle = pipe;
        GtInitTimeout(&options, Transfer->TimeoutMs);
        status = WdfUsbTargetDeviceSendUrbSynchronously(
            Context->UsbDevice, NULL, &options, &urb);
        if (NT_SUCCESS(status) && !USBD_SUCCESS(urb.UrbHeader.Status)) {
            status = (NTSTATUS)urb.UrbHeader.Status;
        }
        return status;
    }

    if (deviceToHost && Transfer->TransferLength > Capacity) {
        return STATUS_BUFFER_TOO_SMALL;
    }
    GtInitTimeout(&options, Transfer->TimeoutMs);
    if (Transfer->TransferLength != 0) {
        WDF_MEMORY_DESCRIPTOR_INIT_BUFFER(&memory, data, Transfer->TransferLength);
        status = WdfUsbTargetDeviceSendControlTransferSynchronously(
            Context->UsbDevice, NULL, &options, &setup, &memory, &transferred);
    } else {
        status = WdfUsbTargetDeviceSendControlTransferSynchronously(
            Context->UsbDevice, NULL, &options, &setup, NULL, &transferred);
    }
    Transfer->ActualLength = transferred;
    return status;
}

static NTSTATUS
GtPipeTransfer(
    _In_ PGT_EXPORT_DEVICE Context,
    _Inout_ PINTERDRIVER_EXPORT_TRANSFER Transfer,
    _In_ ULONG Capacity
    )
{
    USBD_PIPE_HANDLE pipe;
    URB urb;
    WDF_REQUEST_SEND_OPTIONS options;
    PUCHAR data = (PUCHAR)Transfer + GT_TRANSFER_HEADER_SIZE;
    ULONG transferred = 0;
    UCHAR interfaceNumber = 0;
    NTSTATUS status;

    if ((Transfer->EndpointAddress & 0x80) != 0 &&
        Transfer->TransferLength > Capacity) {
        return STATUS_BUFFER_TOO_SMALL;
    }
    WdfWaitLockAcquire(Context->StateLock, NULL);
    pipe = GtFindPipe(
        Context, (UCHAR)Transfer->EndpointAddress, &interfaceNumber);
    if (pipe != NULL && InterlockedIncrement(&Context->ActivePipeTransfers) == 1) {
        KeClearEvent(&Context->NoActivePipeTransfers);
    }
    if (pipe != NULL &&
        InterlockedIncrement(&Context->ActiveInterfaceTransfers[interfaceNumber]) == 1) {
        KeClearEvent(&Context->NoActiveInterfaceTransfers[interfaceNumber]);
    }
    WdfWaitLockRelease(Context->StateLock);
    if (pipe == NULL) {
        return STATUS_NOT_FOUND;
    }

    RtlZeroMemory(&urb, sizeof(urb));
    UsbBuildInterruptOrBulkTransferRequest(
        &urb,
        sizeof(struct _URB_BULK_OR_INTERRUPT_TRANSFER),
        pipe,
        data,
        NULL,
        Transfer->TransferLength,
        (Transfer->EndpointAddress & 0x80) != 0
            ? USBD_TRANSFER_DIRECTION_IN | USBD_SHORT_TRANSFER_OK
            : USBD_TRANSFER_DIRECTION_OUT,
        NULL);
    GtInitTimeout(&options, Transfer->TimeoutMs);
    status = WdfUsbTargetDeviceSendUrbSynchronously(
        Context->UsbDevice, NULL, &options, &urb);
    if (NT_SUCCESS(status) && !USBD_SUCCESS(urb.UrbHeader.Status)) {
        status = (NTSTATUS)urb.UrbHeader.Status;
    }
    transferred = urb.UrbBulkOrInterruptTransfer.TransferBufferLength;
    Transfer->ActualLength = transferred;
    if (InterlockedDecrement(
            &Context->ActiveInterfaceTransfers[interfaceNumber]) == 0) {
        KeSetEvent(
            &Context->NoActiveInterfaceTransfers[interfaceNumber],
            IO_NO_INCREMENT,
            FALSE);
    }
    if (InterlockedDecrement(&Context->ActivePipeTransfers) == 0) {
        KeSetEvent(&Context->NoActivePipeTransfers, IO_NO_INCREMENT, FALSE);
    }
    return status;
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
    PGT_EXPORT_DEVICE context = GtExportGet(WdfIoQueueGetDevice(Queue));
    NTSTATUS status = STATUS_INVALID_DEVICE_REQUEST;
    size_t length;

    switch (IoControlCode) {
    case IOCTL_INTERDRIVER_EXPORT_GET_INFO: {
        PINTERDRIVER_EXPORT_INFO info;
        status = WdfRequestRetrieveOutputBuffer(
            Request, sizeof(*info), (PVOID *)&info, &length);
        if (!NT_SUCCESS(status)) {
            break;
        }
        RtlZeroMemory(info, sizeof(*info));
        info->Size = sizeof(*info);
        info->Version = INTERDRIVER_EXPORT_PROTOCOL_VERSION;
        info->Speed = context->Speed;
        RtlCopyMemory(
            info->DeviceDescriptor,
            &context->DeviceDescriptor,
            sizeof(info->DeviceDescriptor));
        WdfRequestSetInformation(Request, sizeof(*info));
        status = STATUS_SUCCESS;
        break;
    }
    case IOCTL_INTERDRIVER_EXPORT_TRANSFER: {
        PINTERDRIVER_EXPORT_TRANSFER transfer;
        ULONG capacity;
        BOOLEAN deviceToHost;
        status = WdfRequestRetrieveInputBuffer(
            Request, GT_TRANSFER_HEADER_SIZE, (PVOID *)&transfer, &length);
        if (!NT_SUCCESS(status)) {
            break;
        }
        if (transfer->Size < GT_TRANSFER_HEADER_SIZE ||
            transfer->TransferLength > INTERDRIVER_EXPORT_MAX_TRANSFER ||
            transfer->Size > InputBufferLength ||
            OutputBufferLength < GT_TRANSFER_HEADER_SIZE) {
            status = STATUS_INVALID_BUFFER_SIZE;
            break;
        }
        capacity = (ULONG)OutputBufferLength - GT_TRANSFER_HEADER_SIZE;
        deviceToHost = ((UCHAR)transfer->EndpointAddress == 0)
            ? ((transfer->Setup[0] & 0x80) != 0)
            : ((transfer->EndpointAddress & 0x80) != 0);
        if (!deviceToHost &&
            transfer->TransferLength > InputBufferLength - GT_TRANSFER_HEADER_SIZE) {
            status = STATUS_INVALID_BUFFER_SIZE;
            break;
        }
        transfer->Status = STATUS_PENDING;
        transfer->ActualLength = 0;
        if ((UCHAR)transfer->EndpointAddress == 0) {
            status = GtControlTransfer(context, transfer, capacity);
        } else {
            status = GtPipeTransfer(context, transfer, capacity);
        }
        transfer->Status = status;
        WdfRequestSetInformation(
            Request,
            GT_TRANSFER_HEADER_SIZE +
                (NT_SUCCESS(status) && deviceToHost
                    ? transfer->ActualLength : 0));
        status = STATUS_SUCCESS;
        break;
    }
    case IOCTL_INTERDRIVER_EXPORT_ABORT_ENDPOINT: {
        PINTERDRIVER_EXPORT_ABORT abort;
        ULONG index;
        USBD_PIPE_HANDLE pipes[GT_MAX_PIPES];
        ULONG pipeCount = 0;
        status = WdfRequestRetrieveInputBuffer(
            Request, sizeof(*abort), (PVOID *)&abort, &length);
        if (!NT_SUCCESS(status)) {
            break;
        }
        if (abort->Size != sizeof(*abort) || abort->EndpointNumber > 15) {
            status = STATUS_INVALID_PARAMETER;
            break;
        }
        status = STATUS_NOT_FOUND;
        WdfWaitLockAcquire(context->StateLock, NULL);
        for (index = 0; index < context->PipeCount; index++) {
            if ((context->PipeAddresses[index] & 0x0f) == abort->EndpointNumber &&
                pipeCount < GT_MAX_PIPES) {
                pipes[pipeCount++] = context->Pipes[index];
            }
        }
        for (index = 0; index < pipeCount; index++) {
            URB urb;
            WDF_REQUEST_SEND_OPTIONS options;
            RtlZeroMemory(&urb, sizeof(urb));
            urb.UrbPipeRequest.Hdr.Length = sizeof(struct _URB_PIPE_REQUEST);
            urb.UrbPipeRequest.Hdr.Function = URB_FUNCTION_ABORT_PIPE;
            urb.UrbPipeRequest.PipeHandle = pipes[index];
            GtInitTimeout(&options, 5000);
            status = WdfUsbTargetDeviceSendUrbSynchronously(
                context->UsbDevice, NULL, &options, &urb);
            if (NT_SUCCESS(status) && !USBD_SUCCESS(urb.UrbHeader.Status)) {
                status = (NTSTATUS)urb.UrbHeader.Status;
            }
        }
        WdfWaitLockRelease(context->StateLock);
        break;
    }
    }
    WdfRequestComplete(Request, status);
}

NTSTATUS
GtEvtPrepareHardware(
    _In_ WDFDEVICE Device,
    _In_ WDFCMRESLIST ResourcesRaw,
    _In_ WDFCMRESLIST ResourcesTranslated
    )
{
    PGT_EXPORT_DEVICE context = GtExportGet(Device);
    WDF_USB_DEVICE_INFORMATION information;
    ULONG index;
    NTSTATUS status;

    UNREFERENCED_PARAMETER(ResourcesRaw);
    UNREFERENCED_PARAMETER(ResourcesTranslated);

    status = WdfUsbTargetDeviceCreate(Device, NULL, &context->UsbDevice);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    WdfUsbTargetDeviceGetDeviceDescriptor(
        context->UsbDevice, &context->DeviceDescriptor);
    WDF_USB_DEVICE_INFORMATION_INIT(&information);
    status = WdfUsbTargetDeviceRetrieveInformation(
        context->UsbDevice, &information);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    if ((information.Traits & WDF_USB_DEVICE_TRAIT_AT_HIGH_SPEED) != 0) {
        context->Speed = INTERDRIVER_EXPORT_SPEED_HIGH;
    } else {
        context->Speed = INTERDRIVER_EXPORT_SPEED_FULL;
    }
    context->ConfigurationValue = 0;
    context->ConfigurationHandle = NULL;
    context->PipeCount = 0;
    context->ActivePipeTransfers = 0;
    KeInitializeEvent(&context->NoActivePipeTransfers, NotificationEvent, TRUE);
    for (index = 0; index < RTL_NUMBER_OF(context->ActiveInterfaceTransfers); index++) {
        context->ActiveInterfaceTransfers[index] = 0;
        KeInitializeEvent(
            &context->NoActiveInterfaceTransfers[index], NotificationEvent, TRUE);
    }
    return STATUS_SUCCESS;
}

NTSTATUS
GtEvtReleaseHardware(
    _In_ WDFDEVICE Device,
    _In_ WDFCMRESLIST ResourcesTranslated
    )
{
    PGT_EXPORT_DEVICE context = GtExportGet(Device);
    UNREFERENCED_PARAMETER(ResourcesTranslated);
    context->UsbDevice = NULL;
    context->PipeCount = 0;
    context->ConfigurationValue = 0;
    context->ConfigurationHandle = NULL;
    return STATUS_SUCCESS;
}

NTSTATUS
GtEvtDeviceAdd(
    _In_ WDFDRIVER Driver,
    _Inout_ PWDFDEVICE_INIT DeviceInit
    )
{
    WDF_PNPPOWER_EVENT_CALLBACKS callbacks;
    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_IO_QUEUE_CONFIG queueConfig;
    WDFDEVICE device;
    PGT_EXPORT_DEVICE context;
    NTSTATUS status;

    UNREFERENCED_PARAMETER(Driver);
    WdfDeviceInitSetDeviceType(DeviceInit, FILE_DEVICE_UNKNOWN);
    WdfDeviceInitSetIoType(DeviceInit, WdfDeviceIoBuffered);

    WDF_PNPPOWER_EVENT_CALLBACKS_INIT(&callbacks);
    callbacks.EvtDevicePrepareHardware = GtEvtPrepareHardware;
    callbacks.EvtDeviceReleaseHardware = GtEvtReleaseHardware;
    WdfDeviceInitSetPnpPowerEventCallbacks(DeviceInit, &callbacks);

    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, GT_EXPORT_DEVICE);
    attributes.ExecutionLevel = WdfExecutionLevelPassive;
    attributes.SynchronizationScope = WdfSynchronizationScopeNone;
    status = WdfDeviceCreate(&DeviceInit, &attributes, &device);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    context = GtExportGet(device);
    RtlZeroMemory(context, sizeof(*context));
    status = WdfWaitLockCreate(WDF_NO_OBJECT_ATTRIBUTES, &context->StateLock);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    status = WdfDeviceCreateDeviceInterface(
        device, &GUID_DEVINTERFACE_INTERDRIVER_EXPORT, NULL);
    if (!NT_SUCCESS(status)) {
        return status;
    }

    WDF_IO_QUEUE_CONFIG_INIT_DEFAULT_QUEUE(
        &queueConfig, WdfIoQueueDispatchParallel);
    queueConfig.EvtIoDeviceControl = GtEvtIoDeviceControl;
    return WdfIoQueueCreate(
        device, &queueConfig, WDF_NO_OBJECT_ATTRIBUTES, WDF_NO_HANDLE);
}

NTSTATUS
DriverEntry(
    _In_ PDRIVER_OBJECT DriverObject,
    _In_ PUNICODE_STRING RegistryPath
    )
{
    WDF_DRIVER_CONFIG config;
    WDF_DRIVER_CONFIG_INIT(&config, GtEvtDeviceAdd);
    return WdfDriverCreate(
        DriverObject, RegistryPath, WDF_NO_OBJECT_ATTRIBUTES, &config, WDF_NO_HANDLE);
}
