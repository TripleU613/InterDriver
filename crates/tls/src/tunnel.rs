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

/// Stop reading a source once its outbound buffer holds this much unsent data,
/// so a slow consumer applies backpressure instead of growing memory without
/// bound during a long transfer (a backup or a large file copy).
const HIGH_WATER: usize = 4 * 1024 * 1024;

/// A directional byte buffer that advances a read cursor instead of draining
/// from the front, so a sustained stream never pays repeated O(n) memmoves.
#[derive(Default)]
struct Pending {
    data: Vec<u8>,
    sent: usize,
}

impl Pending {
    fn len(&self) -> usize {
        self.data.len() - self.sent
    }
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn push(&mut self, bytes: &[u8]) {
        self.data.extend_from_slice(bytes);
    }
    fn advance(&mut self, n: usize) {
        self.sent += n;
        if self.sent == self.data.len() {
            self.data.clear();
            self.sent = 0;
        } else if self.sent > CHUNK {
            self.data.drain(..self.sent);
            self.sent = 0;
        }
    }
    fn slice(&self) -> &[u8] {
        &self.data[self.sent..]
    }
}

/// Shuttles bytes both directions between a TLS stream and a plain stream until
/// either closes. Single thread, non-blocking, with backpressure and a short
/// idle sleep so it never busy-spins and never buffers without bound.
fn pump(tls: TlsStream, plain: TcpStream) -> io::Result<()> {
    tls.set_nonblocking(true)?;
    plain.set_nonblocking(true)?;
    let mut tls = tls;
    let mut plain = plain;
    let mut to_tls = Pending::default();
    let mut to_plain = Pending::default();
    let mut plain_eof = false;
    let mut tls_eof = false;

    loop {
        let mut progressed = false;
        let mut buffer = [0_u8; CHUNK];

        if !plain_eof && to_tls.len() < HIGH_WATER {
            match plain.read(&mut buffer) {
                Ok(0) => plain_eof = true,
                Ok(n) => {
                    to_tls.push(&buffer[..n]);
                    progressed = true;
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        if !tls_eof && to_plain.len() < HIGH_WATER {
            match tls.read(&mut buffer) {
                Ok(0) => tls_eof = true,
                Ok(n) => {
                    to_plain.push(&buffer[..n]);
                    progressed = true;
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        if !to_tls.is_empty() {
            match tls.write(to_tls.slice()) {
                Ok(n) if n > 0 => {
                    to_tls.advance(n);
                    let _ = tls.flush();
                    progressed = true;
                }
                Ok(_) => {}
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
        }
        if !to_plain.is_empty() {
            match plain.write(to_plain.slice()) {
                Ok(n) if n > 0 => {
                    to_plain.advance(n);
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
    fn streams_a_large_payload_byte_correct_through_the_tunnel() {
        use std::time::Instant;
        // sink server behind the tunnel: reads everything, reports the total.
        let sink = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let sink_addr = sink.local_addr().unwrap();
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&counter);
        std::thread::spawn(move || {
            let (mut s, _) = sink.accept().unwrap();
            let mut buf = vec![0_u8; 64 * 1024];
            loop {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        counted.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
        });

        let ca = Authority::generate("CA").unwrap();
        let server_cfg = server_config(&ca.issue("exporter").unwrap(), &ca.certificate()).unwrap();
        let client_cfg = client_config(&ca.issue("importer").unwrap(), &ca.certificate()).unwrap();
        let front = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let front_addr = front.local_addr().unwrap();
        std::thread::spawn(move || {
            let _ = serve(front, server_cfg, sink_addr);
        });
        let local = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let local_addr = local.local_addr().unwrap();
        std::thread::spawn(move || {
            let _ = forward(local, client_cfg, "exporter".into(), front_addr);
        });

        const TOTAL: usize = 32 * 1024 * 1024;
        let mut client = TcpStream::connect(local_addr).unwrap();
        let chunk = vec![0xa5_u8; 256 * 1024];
        let start = Instant::now();
        let mut written = 0;
        while written < TOTAL {
            let n = client.write(&chunk).unwrap();
            written += n;
        }
        client.flush().unwrap();
        drop(client);

        // wait for the sink to drain
        for _ in 0..200 {
            if counter.load(std::sync::atomic::Ordering::Relaxed) >= TOTAL {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let received = counter.load(std::sync::atomic::Ordering::Relaxed);
        let mbps = (received as f64 / 1e6) / start.elapsed().as_secs_f64();
        eprintln!("streamed {received} bytes through mTLS at {mbps:.0} MB/s");
        assert_eq!(received, TOTAL, "all bytes must cross the encrypted tunnel");
    }

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
