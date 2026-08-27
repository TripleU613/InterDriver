//! Control plane for one-device-per-session USB/IP rendezvous.
//!
//! The control plane never carries USB payloads. It authenticates the two
//! peers, matches exactly one exporter to one importer for one physical device,
//! issues a short-lived single-use ticket, enforces one active consumer per
//! device, and revokes sessions so both ends release the device.
//!
//! Time is injected as unix seconds so the logic is deterministic and testable;
//! there is no ambient clock.

mod token;

pub use token::Token;

use std::collections::HashMap;
use std::fmt;

/// Stable identity of a physical device, independent of its changing USB
/// attachment. Prefer the serial; fall back to a port path in recovery states
/// where the serial is unavailable.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DeviceKey(pub String);

/// Identity of an authenticated peer (an edge exporter or an importer host).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PeerId(pub String);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionState {
    /// Ticket issued, not yet redeemed by the importer.
    Issued,
    /// Importer redeemed the ticket; the data plane may connect.
    Active,
    /// Ended normally by either peer.
    Released,
    /// Force-revoked; both ends must drop the device.
    Revoked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The device already has a live session (Issued or Active).
    DeviceBusy,
    /// No session matches the presented token.
    UnknownToken,
    /// The ticket's time-to-live elapsed before it was redeemed.
    Expired,
    /// The ticket was already redeemed; tickets are single-use.
    AlreadyRedeemed,
    /// A different importer than the one the ticket was issued to.
    WrongImporter,
    /// The session was revoked or released and cannot be used.
    NotLive,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DeviceBusy => "device already has a live session",
            Self::UnknownToken => "no session matches the presented ticket",
            Self::Expired => "ticket expired before it was redeemed",
            Self::AlreadyRedeemed => "ticket was already redeemed",
            Self::WrongImporter => "ticket was issued to a different importer",
            Self::NotLive => "session is no longer live",
        })
    }
}

impl std::error::Error for Error {}

struct Session {
    device: DeviceKey,
    exporter: PeerId,
    importer: PeerId,
    expires_at: u64,
    state: SessionState,
}

/// What the importer receives when it redeems a valid ticket: enough to open
/// the data-plane connection to the matched exporter. The token is not echoed
/// back — the importer already holds it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grant {
    pub device: DeviceKey,
    pub exporter: PeerId,
}

/// The ticket handed to both peers out of band. `token` is the secret; the rest
/// is non-secret routing the peers use to find each other.
pub struct Ticket {
    pub token: Token,
    pub device: DeviceKey,
    pub exporter: PeerId,
    pub importer: PeerId,
    pub expires_at: u64,
}

#[derive(Default)]
pub struct Broker {
    sessions: HashMap<String, Session>,
}

impl Broker {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Issues a ticket matching one exporter to one importer for one device.
    /// Fails with `DeviceBusy` if the device already has an Issued or Active
    /// session, enforcing one consumer per physical device.
    pub fn issue(
        &mut self,
        device: DeviceKey,
        exporter: PeerId,
        importer: PeerId,
        now: u64,
        ttl_seconds: u64,
    ) -> Result<Ticket, Error> {
        self.expire_stale(now);
        if self.device_is_live(&device) {
            return Err(Error::DeviceBusy);
        }
        let token = Token::generate().map_err(|_| Error::DeviceBusy)?;
        let expires_at = now.saturating_add(ttl_seconds);
        self.sessions.insert(
            token.to_hex(),
            Session {
                device: device.clone(),
                exporter: exporter.clone(),
                importer: importer.clone(),
                expires_at,
                state: SessionState::Issued,
            },
        );
        Ok(Ticket {
            token,
            device,
            exporter,
            importer,
            expires_at,
        })
    }

    /// Redeems a ticket exactly once. The importer must be the one the ticket
    /// was issued to. On success the session becomes Active and the data plane
    /// may connect.
    pub fn redeem(&mut self, token: &Token, importer: &PeerId, now: u64) -> Result<Grant, Error> {
        let key = token.to_hex();
        let session = self.sessions.get_mut(&key).ok_or(Error::UnknownToken)?;
        match session.state {
            SessionState::Issued => {}
            SessionState::Active => return Err(Error::AlreadyRedeemed),
            SessionState::Released | SessionState::Revoked => return Err(Error::NotLive),
        }
        if now >= session.expires_at {
            session.state = SessionState::Released;
            return Err(Error::Expired);
        }
        if session.importer != *importer {
            return Err(Error::WrongImporter);
        }
        session.state = SessionState::Active;
        Ok(Grant {
            device: session.device.clone(),
            exporter: session.exporter.clone(),
        })
    }

    /// Force-revokes a session. Returns the device that must be released, or
    /// `UnknownToken`. Idempotent for an already-revoked session.
    pub fn revoke(&mut self, token: &Token) -> Result<DeviceKey, Error> {
        let session = self
            .sessions
            .get_mut(&token.to_hex())
            .ok_or(Error::UnknownToken)?;
        session.state = SessionState::Revoked;
        Ok(session.device.clone())
    }

    /// Ends a session normally, freeing the device for a new ticket.
    pub fn release(&mut self, token: &Token) -> Result<DeviceKey, Error> {
        let session = self
            .sessions
            .get_mut(&token.to_hex())
            .ok_or(Error::UnknownToken)?;
        session.state = SessionState::Released;
        Ok(session.device.clone())
    }

    #[must_use]
    pub fn state(&self, token: &Token) -> Option<SessionState> {
        self.sessions.get(&token.to_hex()).map(|s| s.state)
    }

    fn device_is_live(&self, device: &DeviceKey) -> bool {
        self.sessions.values().any(|session| {
            session.device == *device
                && matches!(session.state, SessionState::Issued | SessionState::Active)
        })
    }

    /// Lazily expires Issued sessions whose TTL elapsed, so a dead ticket never
    /// keeps a device busy. Active sessions are not auto-expired: a live data
    /// connection ends by release or revoke, not by the clock.
    fn expire_stale(&mut self, now: u64) {
        for session in self.sessions.values_mut() {
            if session.state == SessionState::Issued && now >= session.expires_at {
                session.state = SessionState::Released;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn broker() -> Broker {
        Broker::new()
    }

    fn dev() -> DeviceKey {
        DeviceKey("4a6fbe059b44f8f206f8ca96b9319078cf2c36a9".into())
    }

    fn edge() -> PeerId {
        PeerId("edge-linux".into())
    }

    fn mac() -> PeerId {
        PeerId("mac-importer".into())
    }

    #[test]
    fn issue_then_redeem_activates_the_session() {
        let mut b = broker();
        let ticket = b.issue(dev(), edge(), mac(), 1_000, 60).unwrap();
        let grant = b.redeem(&ticket.token, &mac(), 1_010).unwrap();
        assert_eq!(grant.device, dev());
        assert_eq!(grant.exporter, edge());
        assert_eq!(b.state(&ticket.token), Some(SessionState::Active));
    }

    #[test]
    fn ticket_is_single_use() {
        let mut b = broker();
        let ticket = b.issue(dev(), edge(), mac(), 0, 60).unwrap();
        b.redeem(&ticket.token, &mac(), 1).unwrap();
        assert_eq!(
            b.redeem(&ticket.token, &mac(), 2),
            Err(Error::AlreadyRedeemed)
        );
    }

    #[test]
    fn one_consumer_per_device() {
        let mut b = broker();
        let _first = b.issue(dev(), edge(), mac(), 0, 60).unwrap();
        let second = b.issue(dev(), edge(), PeerId("other-mac".into()), 0, 60);
        assert_eq!(second.err(), Some(Error::DeviceBusy));
    }

    #[test]
    fn expired_ticket_cannot_be_redeemed_and_frees_the_device() {
        let mut b = broker();
        let ticket = b.issue(dev(), edge(), mac(), 0, 60).unwrap();
        assert_eq!(b.redeem(&ticket.token, &mac(), 61), Err(Error::Expired));
        // device is free again after expiry
        assert!(b.issue(dev(), edge(), mac(), 100, 60).is_ok());
    }

    #[test]
    fn wrong_importer_is_rejected() {
        let mut b = broker();
        let ticket = b.issue(dev(), edge(), mac(), 0, 60).unwrap();
        assert_eq!(
            b.redeem(&ticket.token, &PeerId("attacker".into()), 1),
            Err(Error::WrongImporter)
        );
        // the legitimate importer can still redeem afterwards
        assert!(b.redeem(&ticket.token, &mac(), 2).is_ok());
    }

    #[test]
    fn revoke_frees_the_device_and_blocks_reuse() {
        let mut b = broker();
        let ticket = b.issue(dev(), edge(), mac(), 0, 60).unwrap();
        b.redeem(&ticket.token, &mac(), 1).unwrap();
        assert_eq!(b.revoke(&ticket.token), Ok(dev()));
        assert_eq!(b.state(&ticket.token), Some(SessionState::Revoked));
        assert_eq!(b.redeem(&ticket.token, &mac(), 2), Err(Error::NotLive));
        // a fresh session can claim the device after revoke
        assert!(b.issue(dev(), edge(), mac(), 3, 60).is_ok());
    }

    #[test]
    fn unknown_token_is_rejected() {
        let mut b = broker();
        let stray = Token::from_bytes([7; 32]);
        assert_eq!(b.redeem(&stray, &mac(), 0), Err(Error::UnknownToken));
        assert_eq!(b.revoke(&stray), Err(Error::UnknownToken));
    }

    #[test]
    fn release_frees_the_device() {
        let mut b = broker();
        let ticket = b.issue(dev(), edge(), mac(), 0, 60).unwrap();
        b.redeem(&ticket.token, &mac(), 1).unwrap();
        assert_eq!(b.release(&ticket.token), Ok(dev()));
        assert!(b.issue(dev(), edge(), mac(), 2, 60).is_ok());
    }
}
