//! Mutually authenticated TLS transport for the InterDriver USB/IP data and control
//! planes. Both peers present a certificate signed by a shared InterDriver CA and
//! verify the other against that CA, so a session is confidential and both ends
//! are authenticated before any USB payload or ticket is exchanged.
//!
//! The identity material is small and self-contained: `Authority` mints an
//! in-memory CA, `Identity` is a peer's cert+key signed by it. Real deployments
//! load these from disk; tests generate them in-process.

mod identity;
mod stream;

pub use identity::{Authority, Identity};
pub use stream::{TlsStream, accept, connect};
pub use rustls::{ClientConfig, ServerConfig};

use rustls::pki_types::CertificateDer;
use std::io;
use std::sync::Arc;

/// Builds a server config that REQUIRES a client certificate signed by `ca`
/// (mutual TLS). A peer without a CA-signed certificate cannot complete the
/// handshake.
pub fn server_config(identity: &Identity, ca: &CertificateDer<'static>) -> io::Result<Arc<rustls::ServerConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(ca.clone())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(identity.chain(), identity.key())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(Arc::new(config))
}

/// Builds a client config that presents `identity` and only trusts servers with
/// a certificate signed by `ca`.
pub fn client_config(identity: &Identity, ca: &CertificateDer<'static>) -> io::Result<Arc<rustls::ClientConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(ca.clone())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(identity.chain(), identity.key())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    #[test]
    fn mutual_tls_round_trips_between_ca_signed_peers() {
        let ca = Authority::generate("InterDriver Test CA").unwrap();
        let server_id = ca.issue("exporter").unwrap();
        let client_id = ca.issue("importer").unwrap();
        let ca_cert = ca.certificate();

        let server_cfg = server_config(&server_id, &ca_cert).unwrap();
        let client_cfg = client_config(&client_id, &ca_cert).unwrap();

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();

        let server = thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let mut tls = accept(server_cfg, tcp).unwrap();
            let mut buffer = [0_u8; 5];
            tls.read_exact(&mut buffer).unwrap();
            tls.write_all(b"pong").unwrap();
            tls.flush().unwrap();
            buffer
        });

        let tcp = TcpStream::connect(address).unwrap();
        let mut tls = connect(client_cfg, "exporter", tcp).unwrap();
        tls.write_all(b"hello").unwrap();
        tls.flush().unwrap();
        let mut reply = [0_u8; 4];
        tls.read_exact(&mut reply).unwrap();

        assert_eq!(&reply, b"pong");
        assert_eq!(&server.join().unwrap(), b"hello");
    }

    #[test]
    fn a_peer_from_a_different_ca_is_rejected() {
        let ca = Authority::generate("InterDriver Test CA").unwrap();
        let rogue = Authority::generate("Rogue CA").unwrap();
        let server_id = ca.issue("exporter").unwrap();
        let rogue_id = rogue.issue("importer").unwrap();

        let server_cfg = server_config(&server_id, &ca.certificate()).unwrap();
        // rogue client trusts the real CA for the server, but its own cert is
        // signed by the rogue CA, so the server's client-auth must reject it.
        let client_cfg = client_config(&rogue_id, &ca.certificate()).unwrap();

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let mut tls = match accept(server_cfg, tcp) {
                Ok(tls) => tls,
                Err(_) => return true, // handshake rejected as expected
            };
            let mut buffer = [0_u8; 1];
            tls.read_exact(&mut buffer).is_err()
        });

        let tcp = TcpStream::connect(address).unwrap();
        let _ = connect(client_cfg, "exporter", tcp).and_then(|mut tls| {
            tls.write_all(b"x")?;
            tls.flush()
        });
        assert!(server.join().unwrap(), "rogue client must not be accepted");
    }
}
