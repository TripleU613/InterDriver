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
use std::sync::Arc as StdArc;
use std::io::{self, BufRead, BufReader, Read, Write};
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
    handle_stream(stream, broker)
}

/// One request line in, one response line out, over any byte stream (plain TCP
/// or a mutually authenticated TLS stream).
fn handle_stream<S: Read + Write>(mut stream: S, broker: &Mutex<Broker>) -> io::Result<()> {
    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let response = dispatch(line.trim(), broker);
    stream.write_all(response.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

/// Serves the broker over mutually authenticated TLS: every ISSUE/REDEEM peer
/// must present a certificate signed by the shared InterDriver CA.
pub fn serve_tls(
    listener: TcpListener,
    broker: Arc<Mutex<Broker>>,
    server_config: StdArc<interdriver_tls::ServerConfig>,
) -> io::Result<()> {
    for stream in listener.incoming() {
        let stream = stream?;
        let broker = Arc::clone(&broker);
        let server_config = StdArc::clone(&server_config);
        std::thread::spawn(move || {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            if let Ok(tls) = interdriver_tls::accept(server_config, stream) {
                let _ = handle_stream(tls, &broker);
            }
        });
    }
    Ok(())
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

fn connect_tcp<A: ToSocketAddrs>(address: A, timeout: Duration) -> io::Result<TcpStream> {
    let address = address
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "no broker address"))?;
    let stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(stream)
}

fn exchange<S: Read + Write>(mut stream: S, line: &str) -> io::Result<String> {
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut reader = BufReader::new(&mut stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    Ok(response.trim().to_owned())
}

fn request<A: ToSocketAddrs>(address: A, line: &str, timeout: Duration) -> io::Result<String> {
    exchange(connect_tcp(address, timeout)?, line)
}

fn request_tls<A: ToSocketAddrs>(
    address: A,
    server_name: &str,
    client_config: StdArc<interdriver_tls::ClientConfig>,
    line: &str,
    timeout: Duration,
) -> io::Result<String> {
    let tcp = connect_tcp(address, timeout)?;
    let tls = interdriver_tls::connect(client_config, server_name, tcp)?;
    exchange(tls, line)
}

/// Operator side over mTLS.
pub fn issue_tls<A: ToSocketAddrs>(
    address: A,
    server_name: &str,
    client_config: StdArc<interdriver_tls::ClientConfig>,
    device: &str,
    exporter: &str,
    importer: &str,
    ttl: u64,
    timeout: Duration,
) -> io::Result<String> {
    let line = format!("ISSUE {device} {exporter} {importer} {ttl}");
    let response = request_tls(address, server_name, client_config, &line, timeout)?;
    match response.strip_prefix("OK ") {
        Some(rest) => rest
            .split_whitespace()
            .next()
            .map(str::to_owned)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed OK")),
        None => Err(io::Error::other(response)),
    }
}

/// Exporter side over mTLS.
pub fn redeem_tls<A: ToSocketAddrs>(
    address: A,
    server_name: &str,
    client_config: StdArc<interdriver_tls::ClientConfig>,
    token_hex: &str,
    exporter: &str,
    timeout: Duration,
) -> Outcome {
    let line = format!("REDEEM {token_hex} {exporter}");
    match request_tls(address, server_name, client_config, &line, timeout) {
        Ok(response) if response.starts_with("OK") => Outcome::Allow,
        Ok(response) => Outcome::Deny(response),
        Err(error) => Outcome::Unreachable(error),
    }
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
    fn mtls_broker_issue_then_redeem_over_the_wire() {
        use interdriver_tls::Authority;
        let ca = Authority::generate("InterDriver CA").unwrap();
        let server_id = ca.issue("broker").unwrap();
        let operator = ca.issue("operator").unwrap();
        let ca_cert = ca.certificate();

        let server_cfg = interdriver_tls::server_config(&server_id, &ca_cert).unwrap();
        let client_cfg = interdriver_tls::client_config(&operator, &ca_cert).unwrap();

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let broker = Arc::new(Mutex::new(Broker::new()));
        let served = Arc::clone(&broker);
        std::thread::spawn(move || {
            let _ = serve_tls(listener, served, server_cfg);
        });

        let timeout = Duration::from_secs(3);
        let token = issue_tls(&addr, "broker", StdArc::clone(&client_cfg), "dev1", "edge1", "mac1", 60, timeout).unwrap();
        assert_eq!(token.len(), 64);
        assert!(matches!(
            redeem_tls(&addr, "broker", StdArc::clone(&client_cfg), &token, "edge1", timeout),
            Outcome::Allow
        ));
        // single-use holds over mTLS too
        assert!(matches!(
            redeem_tls(&addr, "broker", client_cfg, &token, "edge1", timeout),
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
