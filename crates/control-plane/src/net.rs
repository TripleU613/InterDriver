//! Minimal line-based TCP front end for the broker.
//!
//! Two verbs, newline-terminated, space-separated:
//!   ISSUE <device> <exporter> <importer> <ttl>  -> OK <token-hex> <expires>
//!   REDEEM <token-hex> <exporter>               -> OK <device> | ERR <reason>
//!
//! Operators call ISSUE and hand the token to the importer out of band; the
//! exporter calls REDEEM when a ticket arrives over the data-plane handshake.
//! There is no operator authentication in this layer yet — a real deployment
//! wraps this in mutually authenticated TLS. Do not expose it unauthenticated.

use crate::{Broker, DeviceKey, PeerId, Token};
use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Serves the broker until the listener is closed. Each connection carries one
/// request line and one response line.
pub fn serve(listener: TcpListener, broker: Arc<Mutex<Broker>>) -> io::Result<()> {
    for stream in listener.incoming() {
        let stream = stream?;
        let broker = Arc::clone(&broker);
        std::thread::spawn(move || {
            let _ = handle(stream, &broker);
        });
    }
    Ok(())
}

fn handle(stream: TcpStream, broker: &Mutex<Broker>) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let response = dispatch(line.trim(), broker);
    let mut stream = stream;
    stream.write_all(response.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

fn dispatch(line: &str, broker: &Mutex<Broker>) -> String {
    let parts: Vec<&str> = line.split_whitespace().collect();
    match parts.as_slice() {
        ["ISSUE", device, exporter, importer, ttl] => {
            let Ok(ttl) = ttl.parse::<u64>() else {
                return "ERR bad-ttl".into();
            };
            let mut broker = match broker.lock() {
                Ok(broker) => broker,
                Err(_) => return "ERR broker-poisoned".into(),
            };
            match broker.issue(
                DeviceKey((*device).into()),
                PeerId((*exporter).into()),
                PeerId((*importer).into()),
                now_seconds(),
                ttl,
            ) {
                Ok(ticket) => format!("OK {} {}", ticket.token.to_hex(), ticket.expires_at),
                Err(error) => format!("ERR {error}"),
            }
        }
        ["REDEEM", token, exporter] => {
            let Some(token) = Token::from_hex(token) else {
                return "ERR bad-token".into();
            };
            let mut broker = match broker.lock() {
                Ok(broker) => broker,
                Err(_) => return "ERR broker-poisoned".into(),
            };
            match broker.redeem_for_exporter(&token, &PeerId((*exporter).into()), now_seconds()) {
                Ok(grant) => format!("OK {}", grant.device.0),
                Err(error) => format!("ERR {error}"),
            }
        }
        _ => "ERR bad-request".into(),
    }
}

/// Response of a broker call, distinguishing a definite DENY from a broker we
/// could not reach — the exporter must log and treat these differently.
pub enum Outcome {
    Allow,
    Deny(String),
    Unreachable(io::Error),
}

fn request<A: ToSocketAddrs>(address: A, line: &str, timeout: Duration) -> io::Result<String> {
    let address = address
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "no broker address"))?;
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    Ok(response.trim().to_owned())
}

/// Operator side: ask the broker to issue a ticket, returning the token hex.
pub fn issue<A: ToSocketAddrs>(
    address: A,
    device: &str,
    exporter: &str,
    importer: &str,
    ttl: u64,
    timeout: Duration,
) -> io::Result<String> {
    let line = format!("ISSUE {device} {exporter} {importer} {ttl}");
    let response = request(address, &line, timeout)?;
    match response.strip_prefix("OK ") {
        Some(rest) => rest
            .split_whitespace()
            .next()
            .map(str::to_owned)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed OK")),
        None => Err(io::Error::other(response)),
    }
}

/// Exporter side: redeem a presented ticket. Distinguishes DENY from a broker
/// that could not be reached, so the two never look alike in logs.
pub fn redeem<A: ToSocketAddrs>(
    address: A,
    token_hex: &str,
    exporter: &str,
    timeout: Duration,
) -> Outcome {
    let line = format!("REDEEM {token_hex} {exporter}");
    match request(address, &line, timeout) {
        Ok(response) if response.starts_with("OK") => Outcome::Allow,
        Ok(response) => Outcome::Deny(response),
        Err(error) => Outcome::Unreachable(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn_broker() -> (String, Arc<Mutex<Broker>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let broker = Arc::new(Mutex::new(Broker::new()));
        let served = Arc::clone(&broker);
        std::thread::spawn(move || {
            let _ = serve(listener, served);
        });
        (address, broker)
    }

    #[test]
    fn issue_then_exporter_redeem_is_single_use_over_the_wire() {
        let (addr, _broker) = spawn_broker();
        let timeout = Duration::from_secs(2);
        let token = issue(&addr, "dev1", "edge1", "mac1", 60, timeout).unwrap();
        assert_eq!(token.len(), 64);
        assert!(matches!(
            redeem(&addr, &token, "edge1", timeout),
            Outcome::Allow
        ));
        // single-use survives the network boundary
        assert!(matches!(
            redeem(&addr, &token, "edge1", timeout),
            Outcome::Deny(_)
        ));
    }

    #[test]
    fn unissued_token_is_denied() {
        let (addr, _broker) = spawn_broker();
        let timeout = Duration::from_secs(2);
        let stray = Token::from_bytes([9; 32]).to_hex();
        assert!(matches!(
            redeem(&addr, &stray, "edge1", timeout),
            Outcome::Deny(_)
        ));
    }

    #[test]
    fn wrong_exporter_is_denied_over_the_wire() {
        let (addr, _broker) = spawn_broker();
        let timeout = Duration::from_secs(2);
        let token = issue(&addr, "dev1", "edge1", "mac1", 60, timeout).unwrap();
        assert!(matches!(
            redeem(&addr, &token, "attacker", timeout),
            Outcome::Deny(_)
        ));
    }

    #[test]
    fn unreachable_broker_is_distinct_from_deny() {
        // nothing listening on this port
        let outcome = redeem("127.0.0.1:1", &Token::from_bytes([1; 32]).to_hex(), "edge1", Duration::from_millis(300));
        assert!(matches!(outcome, Outcome::Unreachable(_)));
    }
}
