//! Bidirectional TLS tunnel: wraps the plain USB/IP data plane in mutually
//! authenticated TLS without changing the USB/IP code. The exporter runs
//! `serve` in front of its localhost USB/IP server; the importer runs `forward`
//! and points the attach/probe command at the local address it prints.
//!
//! Each tunneled connection is carried by the full-duplex `Duplex` transport and
//! copied with one blocking thread per direction. An earlier single-threaded
//! non-blocking pump over rustls `StreamOwned` stalled a large upload across a
//! high-latency link when the receiving side was the TLS server; the duplex
//! transport is built to be read and written from two threads at once and does
//! not have that failure.

use crate::{Duplex, accept_duplex, connect_duplex};
use interdriver_transport::Transport;
use std::io;
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;

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
            let Ok(tls) = accept_duplex(server_config, stream) else {
                return;
            };
            let Ok(plain) = TcpStream::connect(forward_to) else {
                return;
            };
            let _ = bridge(tls, plain);
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
            let Ok(tls) = connect_duplex(client_config, &server_name, tcp) else {
                return;
            };
            let _ = bridge(tls, plain);
        });
    }
    Ok(())
}

/// Copies bytes both ways between the encrypted `tls` transport and the plain
/// `plain` socket until either side closes, one blocking thread per direction.
/// When one direction ends, both peers are shut down so the other thread also
/// unblocks and the connection tears down cleanly.
fn bridge(tls: Duplex, plain: TcpStream) -> io::Result<()> {
    let tls_read = tls;
    let tls_write = tls_read.split()?;
    let plain_read = plain.try_clone()?;
    let plain_write = plain;

    // plain -> tls (e.g. a large upload)
    let up = {
        let mut source = plain_read;
        let mut sink = tls_write;
        std::thread::spawn(move || {
            let _ = io::copy(&mut source, &mut sink);
            let _ = sink.shutdown();
            let _ = source.shutdown(std::net::Shutdown::Read);
        })
    };
    // tls -> plain (the reply direction)
    {
        let mut source = tls_read;
        let mut sink = plain_write;
        let _ = io::copy(&mut source, &mut sink);
        let _ = sink.shutdown(std::net::Shutdown::Write);
        let _ = source.shutdown();
    }
    let _ = up.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Authority, client_config, server_config};
    use std::io::{Read, Write};
    use std::time::Duration;

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
