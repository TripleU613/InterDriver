/*
 * Shared contract between the kernel shim and the user-mode Rust service.
 * Kept free of WDK types so the Rust side can mirror it exactly.
 */

#ifndef INTERDRIVER_UDE_IOCTL_H
#define INTERDRIVER_UDE_IOCTL_H

// {A1C0FFEE-5E11-4C0D-9F3A-1B2C3D4E5F60}
DEFINE_GUID(GUID_DEVINTERFACE_INTERDRIVER_UDE,
    0xa1c0ffee, 0x5e11, 0x4c0d, 0x9f, 0x3a, 0x1b, 0x2c, 0x3d, 0x4e, 0x5f, 0x60);

#define INTERDRIVER_UDE_DEVICE_TYPE 0x8123

#define IOCTL_INTERDRIVER_UDE_PLUG_IN \
    CTL_CODE(INTERDRIVER_UDE_DEVICE_TYPE, 0x800, METHOD_BUFFERED, FILE_WRITE_ACCESS)
#define IOCTL_INTERDRIVER_UDE_UNPLUG \
    CTL_CODE(INTERDRIVER_UDE_DEVICE_TYPE, 0x801, METHOD_BUFFERED, FILE_WRITE_ACCESS)

/* Speed values mirror UDECX_USB_DEVICE_SPEED. */
#define INTERDRIVER_UDE_SPEED_FULL  1
#define INTERDRIVER_UDE_SPEED_HIGH  2
#define INTERDRIVER_UDE_SPEED_SUPER 3

/*
 * Followed in the same buffer by DeviceDescriptorLength bytes of device
 * descriptor and then ConfigDescriptorLength bytes of configuration
 * descriptor, both taken verbatim from the real remote device.
 */
typedef struct _INTERDRIVER_UDE_PLUG_IN_REQUEST {
    unsigned long Size;
    unsigned long Speed;
    unsigned long DeviceDescriptorLength;
    unsigned long ConfigDescriptorLength;
} INTERDRIVER_UDE_PLUG_IN_REQUEST, *PINTERDRIVER_UDE_PLUG_IN_REQUEST;

#define IOCTL_INTERDRIVER_UDE_FETCH_URB \
    CTL_CODE(INTERDRIVER_UDE_DEVICE_TYPE, 0x802, METHOD_BUFFERED, FILE_READ_ACCESS)
#define IOCTL_INTERDRIVER_UDE_COMPLETE_URB \
    CTL_CODE(INTERDRIVER_UDE_DEVICE_TYPE, 0x803, METHOD_BUFFERED, FILE_WRITE_ACCESS)

#define INTERDRIVER_UDE_MAX_DESCRIPTOR 4096
#define INTERDRIVER_UDE_MAX_TRANSFER   65536
#define INTERDRIVER_UDE_MAX_PENDING    32

/*
 * One endpoint-0 transfer handed up to the user-mode service. For a
 * host-to-device request DataLength bytes of payload follow the header.
 */
typedef struct _INTERDRIVER_UDE_URB {
    unsigned long Id;
    unsigned long TransferLength;
    unsigned long DataLength;
    unsigned char Setup[8];
} INTERDRIVER_UDE_URB, *PINTERDRIVER_UDE_URB;

/*
 * The answer from the real device. DataLength bytes of payload follow for a
 * device-to-host request.
 */
typedef struct _INTERDRIVER_UDE_URB_RESULT {
    unsigned long Id;
    long Status;
    unsigned long DataLength;
} INTERDRIVER_UDE_URB_RESULT, *PINTERDRIVER_UDE_URB_RESULT;

#endif /* INTERDRIVER_UDE_IOCTL_H */
