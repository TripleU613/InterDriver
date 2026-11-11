# macOS Importer Entitlement — How To Unblock Locally

The importer calls `IOUSBHostControllerInterface`, gated by the restricted
entitlement `com.apple.developer.usb.host-controller-interface`.

## What was tested (2026-08-27, MacBook, SIP on)

| Binary signing | Result |
| --- | --- |
| unsigned / signed without the entitlement | runs, API returns `IOUSBHostErrorDomain -536870195 required entitlement` |
| ad-hoc self-signed WITH the entitlement | process **killed by AMFI at launch** (exit 137 = SIGKILL) |

Conclusion: the entitlement is enforced at runtime by AMFI, not merely by the
IOUSBHost API. Root/`sudo` does not help — AMFI is above root. A self-signed
binary that merely claims the entitlement is killed.

## Tested with root on the MacBook (2026-08-31)

Root was available and used, so this is measured rather than assumed:

```
uid 0
SIP: enabled
nvram: WRITABLE
bputil -d:
  Security Mode:               Full
  SIP Status:                  Enabled
  Boot Args Filtering Status:  Enabled
  Kernel CTRR Status:          Enabled
  3rd Party Kexts Status:      Disabled
```

`nvram` accepts writes, so `boot-args=amfi_get_out_of_my_way=0x1` can be set —
but **Boot Args Filtering is Enabled**, so Full Security discards it at boot.
Root therefore cannot relax AMFI on this machine.

Lowering Full Security to Permissive on Apple Silicon requires booting into
Recovery by physically holding the power button; it cannot be done from a
running system or over SSH at any privilege level. There is also no Developer ID
in the keychain (`security find-identity` reports zero identities), so the
provisioning-profile route cannot even be tested here.

Conclusion: the importer entitlement cannot be worked around remotely. The
options are Apple granting it, a physical Recovery-mode step on a dev Mac, or
the gadget board.

## Independent confirmation

`carlossless/usbip-macos` is a separate USB/IP client for macOS built on the
same `IOUSBHostControllerInterface`. Its author reaches the identical
conclusion: it "either needs to be signed with
`com.apple.developer.usb.host-controller-interface` entitlement (which I don't
have) or run as root on a system that has SIP disabled."

VirtualHere, a commercial product, ships a virtual host-controller kernel
extension (`vhhcd.kext`, now a system extension) instead, which needs Apple
approval or Reduced Security.

So there is no fourth door. macOS does not expose an unprivileged way to create
a virtual USB device, and two independent implementations plus this one land in
the same place.

The entitlement is managed but is granted on request: it is absent from the
Xcode capabilities panel only because request volume is low, and requests are
submitted through Feedback Assistant with the Team ID. Reports are that the
approving engineer turns them around quickly.

## One command once SIP is off

`cargo xtask macos enable-import` signs the controller probe with the
entitlement, prints the machine's security posture, runs it, and interprets the
result. Pass a binary path after `enable-import` to test a different build. On a
Full Security machine it reports the AMFI kill and the exact Recovery steps;
with SIP disabled it reports whether the controller was created.

## Two ways forward

### A. Local development / internal use — disable SIP + AMFI on a dev Mac

Do this ONLY on a development Mac. NEVER on the production release runner
`ci-macmini-levis`. It lowers that machine's security.

1. Shut down. Hold the power button until "Loading startup options"; click
   **Options → Continue** to enter Recovery.
2. **Utilities → Terminal**, then: `csrutil disable`
   (authenticate with an admin account; this also sets the boot policy to
   Permissive, required for boot-args on Apple Silicon).
3. Reboot into macOS, then: `sudo nvram boot-args=amfi_get_out_of_my_way=0x1`
4. Reboot again. Verify: `csrutil status` → disabled; `nvram boot-args` shows
   the amfi flag.
5. Self-sign with the entitlement:
   `codesign -s - --entitlements crates/platform-macos/entitlements.plist --force <binary>`

With AMFI disabled the process is no longer killed and the entitlement check
passes, so `IOUSBHostControllerInterface` can be created. This is the standard
pre-approval development path for restricted entitlements.

### B. Shipping to customers — Apple must grant the entitlement

You cannot require customers to disable SIP. A distributed, notarized build
needs Apple to approve `com.apple.developer.usb.host-controller-interface` for
the team; then a provisioning profile carries it and it works under SIP.
Request it through Feedback Assistant (Problem Area: USB) with your Team ID.

So: path A unblocks development and internal InterDriver use now; path B is required
for the shipped product. They are not mutually exclusive — do A to build/test
today, pursue B in parallel for release.
