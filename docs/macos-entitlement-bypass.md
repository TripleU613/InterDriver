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
Request drafted in `docs/apple-entitlement-request.md` (needs the Team ID).

So: path A unblocks development and internal InterDriver use now; path B is required
for the shipped product. They are not mutually exclusive — do A to build/test
today, pursue B in parallel for release.
