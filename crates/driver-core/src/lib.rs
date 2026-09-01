use std::fmt;

pub const APPLE_VENDOR_ID: u16 = 0x05ac;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    Linux,
    Macos,
    Windows,
}

impl fmt::Display for Platform {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Linux => "Linux",
            Self::Macos => "macOS",
            Self::Windows => "Windows",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlatformAdapter {
    pub platform: Platform,
    pub exporter: &'static str,
    pub importer: &'static str,
    pub kernel_boundary: &'static str,
}

pub const PLATFORM_ADAPTERS: [PlatformAdapter; 3] = [
    PlatformAdapter {
        platform: Platform::Linux,
        exporter: "Rust service + usbfs (no kernel module)",
        importer: "Rust service + vhci_hcd",
        kernel_boundary: "usbfs to export, in-tree vhci_hcd to import",
    },
    PlatformAdapter {
        platform: Platform::Macos,
        exporter: "Rust service + IOUSBHostDevice capture",
        importer: "Rust IOUSBHostControllerInterface daemon",
        kernel_boundary: "Apple IOUSBHost framework",
    },
    PlatformAdapter {
        platform: Platform::Windows,
        exporter: "Rust service + WinUSB",
        importer: "Rust KMDF UdeCx virtual controller",
        kernel_boundary: "Microsoft KMDF/UdeCx",
    },
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceIdentity {
    pub physical_port: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub serial: Option<String>,
}

impl DeviceIdentity {
    #[must_use]
    pub fn is_apple(&self) -> bool {
        self.vendor_id == APPLE_VENDOR_ID
    }

    #[must_use]
    pub fn same_physical_device(&self, other: &Self) -> bool {
        self.physical_port == other.physical_port && self.vendor_id == other.vendor_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionState {
    Idle,
    Claiming(DeviceIdentity),
    Exported(DeviceIdentity),
    Importing(DeviceIdentity),
    Active(DeviceIdentity),
    AwaitingReenumeration(DeviceIdentity),
    Failed { reason: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEvent {
    CableAttached(DeviceIdentity),
    ExportClaimed,
    ImportRequested,
    ImportReady,
    UsbDisconnected,
    UsbReenumerated(DeviceIdentity),
    Stop,
    Fail(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidTransition {
    pub state: SessionState,
    pub event: SessionEvent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Session {
    state: SessionState,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            state: SessionState::Idle,
        }
    }
}

impl Session {
    #[must_use]
    pub const fn state(&self) -> &SessionState {
        &self.state
    }

    pub fn apply(&mut self, event: SessionEvent) -> Result<&SessionState, Box<InvalidTransition>> {
        if let SessionEvent::Fail(reason) = event {
            self.state = SessionState::Failed { reason };
            return Ok(&self.state);
        }

        let next = match (&self.state, &event) {
            (SessionState::Idle, SessionEvent::CableAttached(device)) if device.is_apple() => {
                SessionState::Claiming(device.clone())
            }
            (SessionState::Claiming(device), SessionEvent::ExportClaimed) => {
                SessionState::Exported(device.clone())
            }
            (SessionState::Exported(device), SessionEvent::ImportRequested) => {
                SessionState::Importing(device.clone())
            }
            (SessionState::Importing(device), SessionEvent::ImportReady) => {
                SessionState::Active(device.clone())
            }
            (SessionState::Active(device), SessionEvent::UsbDisconnected) => {
                SessionState::AwaitingReenumeration(device.clone())
            }
            (
                SessionState::AwaitingReenumeration(previous),
                SessionEvent::UsbReenumerated(current),
            ) if previous.same_physical_device(current) => SessionState::Claiming(current.clone()),
            (_, SessionEvent::Stop) => SessionState::Idle,
            _ => {
                return Err(Box::new(InvalidTransition {
                    state: self.state.clone(),
                    event,
                }));
            }
        };
        self.state = next;
        Ok(&self.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phone(product_id: u16) -> DeviceIdentity {
        DeviceIdentity {
            physical_port: "usb-port-3".into(),
            vendor_id: APPLE_VENDOR_ID,
            product_id,
            serial: None,
        }
    }

    #[test]
    fn erase_reenumeration_preserves_session_by_physical_port() {
        let mut session = Session::default();
        session
            .apply(SessionEvent::CableAttached(phone(0x12a8)))
            .unwrap();
        session.apply(SessionEvent::ExportClaimed).unwrap();
        session.apply(SessionEvent::ImportRequested).unwrap();
        session.apply(SessionEvent::ImportReady).unwrap();
        session.apply(SessionEvent::UsbDisconnected).unwrap();
        session
            .apply(SessionEvent::UsbReenumerated(phone(0x1281)))
            .unwrap();
        assert!(
            matches!(session.state(), SessionState::Claiming(device) if device.product_id == 0x1281)
        );
    }

    #[test]
    fn another_physical_port_cannot_hijack_reenumeration() {
        let mut session = Session::default();
        session
            .apply(SessionEvent::CableAttached(phone(0x12a8)))
            .unwrap();
        session.apply(SessionEvent::ExportClaimed).unwrap();
        session.apply(SessionEvent::ImportRequested).unwrap();
        session.apply(SessionEvent::ImportReady).unwrap();
        session.apply(SessionEvent::UsbDisconnected).unwrap();
        let mut other = phone(0x1281);
        other.physical_port = "usb-port-4".into();
        assert!(session.apply(SessionEvent::UsbReenumerated(other)).is_err());
    }

    #[test]
    fn non_apple_device_is_rejected() {
        let mut device = phone(1);
        device.vendor_id = 0x1234;
        let mut session = Session::default();
        assert!(session.apply(SessionEvent::CableAttached(device)).is_err());
    }
}
