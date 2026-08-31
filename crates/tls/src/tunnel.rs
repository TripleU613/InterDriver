//! Bidirectional TLS tunnel: wraps the plain USB/IP data plane in mutually
//! authenticated TLS without changing the USB/IP code. The exporter runs
//! `serve` in front of its localhost USB/IP server; the importer runs `forward`
//! and points the attach/probe command at the local address it prints.
//!
//! The pump is single-threaded and non-blocking, so one TLS session is never
//! touched from two threads (rustls streams are not splittable). One thread per
//! tunneled connection shuttles bytes both directions until either side closes.

use crate::TlsStream;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

const CHUNK: usize = 64 * 1024;

/// Exporter side: accept mTLS on `listener`, and for each connection open a
/// plain connection to `forward_to` (the localhost USB/IP server) and pump.
pub fn serve<A: ToSocketAddrs + Clone + Send + 'static>(
    listener: TcpListener,
    server_config: Arc<rustls::ServerConfig>,
    forward_to: A,
) -> io::Result<()> {
    for stream in listener.incoming() {
        let stream = stream?;
        let server_config = Arc::clone(&server_config);
        let forward_to = forward_to.clone();
        std::thread::spawn(move || {
            let Ok(tls) = crate::accept(server_config, stream) else {
                return;
            };
            let Ok(plain) = TcpStream::connect(forward_to) else {
                return;
            };
            let _ = pump(tls, plain);
        });
    }
    Ok(())
}

/// Importer side: listen for plain local connections on `local`, and for each
/// open an mTLS connection to `remote` (verified as `server_name`) and pump.
pub fn forward<A: ToSocketAddrs + Clone + Send + 'static>(
    local: TcpListener,
    client_config: Arc<rustls::ClientConfig>,
    server_name: String,
    remote: A,
) -> io::Result<()> {
    for stream in local.incoming() {
        let plain = stream?;
        let client_config = Arc::clone(&client_config);
        let server_name = server_name.clone();
        let remote = remote.clone();
        std::thread::spawn(move || {
            let Ok(tcp) = TcpStream::connect(remote) else {
                return;
            };
            let Ok(tls) = crate::connect(client_config, &server_name, tcp) else {
                return;
            };
            let _ = pump(tls, plain);
        });
    }
    Ok(())
}

/// Shuttles bytes both directions between a TLS stream and a plain stream until
/// either closes. Single thread, non-blocking, with a short idle sleep so it
/// never busy-spins.
fn pump(tls: TlsStream, plain: TcpStream) -> io::Result<()> {
    tls.set_nonblocking(true)?;
    plain.set_nonblocking(true)?;
    let mut tls = tls;
    let mut plain = plain;
    let mut to_tls: Vec<u8> = Vec::new();
    let mut to_plain: Vec<u8> = Vec::new();
    let mut plain_eof = false;
    let mut tls_eof = false;

    loop {
        let mut progressed = false;
        let mut buffer = [0_u8; CHUNK];

        if !plain_eof {
            match plain.read(&mut buffer) {
                Ok(0) => plain_eof = true,
                Ok(n) => {
                    to_tls.extend_from_slice(&buffer[..n]);
                    progressed = true;
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        if !tls_eof {
            match tls.read(&mut buffer) {
                Ok(0) => tls_eof = true,
                Ok(n) => {
                    to_plain.extend_from_slice(&buffer[..n]);
                    progressed = true;
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        if !to_tls.is_empty() {
            match tls.write(&to_tls) {
                Ok(n) if n > 0 => {
                    to_tls.drain(..n);
                    let _ = tls.flush();
                    progressed = true;
                }
                Ok(_) => {}
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        if !to_plain.is_empty() {
            match plain.write(&to_plain) {
                Ok(n) if n > 0 => {
                    to_plain.drain(..n);
                    progressed = true;
                }
                Ok(_) => {}
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }

        if plain_eof && to_tls.is_empty() && tls_eof && to_plain.is_empty() {
            return Ok(());
        }
        if !progressed {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Authority, client_config, server_config};

    #[test]
    fn tunnels_a_plain_echo_service_through_mtls() {
        // plaintext echo "server" behind the tunnel
        let echo = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let echo_addr = echo.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in echo.incoming() {
                let mut s = stream.unwrap();
                std::thread::spawn(move || {
                    let mut buf = [0_u8; 1024];
                    loop {
                        match s.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if s.write_all(&buf[..n]).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });

        let ca = Authority::generate("CA").unwrap();
        let server_cfg = server_config(&ca.issue("exporter").unwrap(), &ca.certificate()).unwrap();
        let client_cfg = client_config(&ca.issue("importer").unwrap(), &ca.certificate()).unwrap();

        // exporter-side TLS front -> echo
        let front = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let front_addr = front.local_addr().unwrap();
        std::thread::spawn(move || {
            let _ = serve(front, server_cfg, echo_addr);
        });

        // importer-side local plain -> TLS -> front
        let local = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let local_addr = local.local_addr().unwrap();
        std::thread::spawn(move || {
            let _ = forward(local, client_cfg, "exporter".into(), front_addr);
        });

        // client speaks plaintext to the local end; bytes cross the encrypted hop
        let mut client = TcpStream::connect(local_addr).unwrap();
        client.write_all(b"encrypted-usb-bytes").unwrap();
        let mut reply = [0_u8; 19];
        client.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"encrypted-usb-bytes");
    }
}
