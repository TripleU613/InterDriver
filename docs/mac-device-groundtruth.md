# Mac-Side Device Ground Truth (acceptance baseline)

Captured 2026-08-27 with the Linux test iPhone physically attached to the Mac
`test-mac`. This is what the virtual device presented through
`IOUSBHostControllerInterface` must reproduce once the entitlement lands.

## cfgutil (acceptance test #1 — PASS on real hardware)

`/Applications/Apple Configurator.app/Contents/MacOS/cfgutil list`:

```
Type: iPhone10,1  ECID: 0xC60CC183B883A
UDID: 4a6fbe059b44f8f206f8ca96b9319078cf2c36a9  Location: 0x2100000  Name: iPhone
```

`cfgutil pair` + Trust tap succeeded. Full profile (acceptance tests 2-4 baseline
— PASS on real hardware):

```
deviceType        iPhone10,1
ECID              0xC60CC183B883A
UDID              4a6fbe059b44f8f206f8ca96b9319078cf2c36a9
serialNumber      C8PW16YHJC6D
isSupervised      yes
isPaired          yes
activationState   Activated
buildVersion      20H392  (iOS firmwareVersion 16.7.16)
```

cfgutil can pair, supervise, and query the device end to end. This is the full
behavior the remote/virtual device must reproduce once the entitlement lands.

## IORegistry device shape (IOUSBHostDevice)

```
idVendor            1452   (0x05AC)
idProduct           4776   (0x12A8)
bcdDevice           4097   (0x1001)
bDeviceClass        0      (per-interface)
bNumConfigurations  5
kUSBCurrentConfig   5      (Apple raises the config once trusted)
Device Speed        2      (high speed)
Serial              4a6fbe059b44f8f206f8ca96b9319078cf2c36a9
```

The importer must present idVendor/idProduct/bcdDevice/serial verbatim and honor
a 5-configuration device that transitions to config 5 — `cfgutil` matches on
ECID/UDID, both of which derive from the serial and device identity above.

## Note

Plugging the iPhone into the Mac does NOT test the remote-import path. That path
is blocked on Apple granting `com.apple.developer.usb.host-controller-interface`;
no cable arrangement changes that. The physical device on the Mac is only useful
for capturing this ground truth and for driving `cfgutil` once paired.
