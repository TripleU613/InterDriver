/*
 * User/kernel contract for the InterDriver physical USB exporter.
 */

#ifndef INTERDRIVER_EXPORT_IOCTL_H
#define INTERDRIVER_EXPORT_IOCTL_H

// {3A44F2D1-7ACB-4B49-91D5-73AC92D42711}
DEFINE_GUID(GUID_DEVINTERFACE_INTERDRIVER_EXPORT,
    0x3a44f2d1, 0x7acb, 0x4b49, 0x91, 0xd5, 0x73, 0xac, 0x92, 0xd4, 0x27, 0x11);

#define INTERDRIVER_EXPORT_DEVICE_TYPE 0x8124

#define IOCTL_INTERDRIVER_EXPORT_GET_INFO \
    CTL_CODE(INTERDRIVER_EXPORT_DEVICE_TYPE, 0x800, METHOD_BUFFERED, FILE_READ_ACCESS)
#define IOCTL_INTERDRIVER_EXPORT_TRANSFER \
    CTL_CODE(INTERDRIVER_EXPORT_DEVICE_TYPE, 0x801, METHOD_BUFFERED, FILE_READ_ACCESS | FILE_WRITE_ACCESS)
#define IOCTL_INTERDRIVER_EXPORT_ABORT_ENDPOINT \
    CTL_CODE(INTERDRIVER_EXPORT_DEVICE_TYPE, 0x802, METHOD_BUFFERED, FILE_WRITE_ACCESS)

#define INTERDRIVER_EXPORT_PROTOCOL_VERSION 1
#define INTERDRIVER_EXPORT_MAX_TRANSFER (1024 * 1024)

#define INTERDRIVER_EXPORT_SPEED_FULL  1
#define INTERDRIVER_EXPORT_SPEED_HIGH  2
#define INTERDRIVER_EXPORT_SPEED_SUPER 3

typedef struct _INTERDRIVER_EXPORT_INFO {
    unsigned long Size;
    unsigned long Version;
    unsigned long Speed;
    unsigned long Flags;
    unsigned char DeviceDescriptor[18];
    unsigned char Reserved[2];
} INTERDRIVER_EXPORT_INFO, *PINTERDRIVER_EXPORT_INFO;

/*
 * The transfer buffer is used in both directions. DataLength bytes follow the
 * fixed header for host-to-device requests; TransferLength bytes of capacity
 * follow it for device-to-host requests. EndpointAddress is the complete USB
 * endpoint address, including the direction bit. Endpoint zero uses Setup.
 */
typedef struct _INTERDRIVER_EXPORT_TRANSFER {
    unsigned long Size;
    unsigned long EndpointAddress;
    unsigned long TransferLength;
    unsigned long TimeoutMs;
    long Status;
    unsigned long ActualLength;
    unsigned char Setup[8];
    unsigned long Reserved;
} INTERDRIVER_EXPORT_TRANSFER, *PINTERDRIVER_EXPORT_TRANSFER;

typedef struct _INTERDRIVER_EXPORT_ABORT {
    unsigned long Size;
    unsigned long EndpointNumber;
} INTERDRIVER_EXPORT_ABORT, *PINTERDRIVER_EXPORT_ABORT;

#endif
