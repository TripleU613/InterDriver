//! A mutually authenticated TLS connection the USB/IP engine can drive from
//! several threads: one reading, any number writing, any one shutting down.
//!
//! rustls's own `StreamOwned` is single-threaded, and the engine needs a
//! reader blocked on the wire while writers push completions. The connection
//! state therefore sits behind a mutex and the socket is cloned per handle:
//!
//! - the reader blocks on the socket *without* the lock, then takes the lock
//!   only to feed the bytes through rustls and pull plaintext out;
//! - a writer takes the lock only to encrypt into a private buffer, releases
//!   it, then puts the ciphertext on the wire under a separate write lock that
//!   keeps TLS records in order.
//!
//! No thread ever blocks on the socket while holding the connection lock, so
//! two peers saturating both directions at once cannot deadlock each other.

use interdriver_transport::Transport;
use rustls::pki_types::ServerName;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

const READ_CHUNK: usize = 32 * 1024;

pub struct Duplex {
    connection: Arc<Mutex<rustls::Connection>>,
    write_lock: Arc<Mutex<()>>,
    socket: TcpStream,
    /// Ciphertext read off the socket, awaiting decryption.
    incoming: Vec<u8>,
    /// Decrypted plaintext drained out of rustls, awaiting the caller. rustls
    /// caps how much decrypted plaintext it will hold internally, so under a
    /// two-way flood it can refuse fresh ciphertext ("received plaintext buffer
    /// full"). Draining it into this buffer after every feed keeps rustls near
    /// empty; a per-handle buffer bounds our own memory to one chunk.
    plaintext: Vec<u8>,
    plaintext_read: usize,
}

/// Dials nothing: completes a client handshake on an already connected socket,
/// presenting the client identity and requiring the server to be `server_name`
/// with a CA-signed certificate.
pub fn connect_duplex(
    config: Arc<rustls::ClientConfig>,
    server_name: &str,
    tcp: TcpStream,
) -> io::Result<Duplex> {
    let name = ServerName::try_from(server_name.to_owned())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let connection = rustls::ClientConnection::new(config, name).map_err(io::Error::other)?;
    Duplex::handshake(rustls::Connection::Client(connection), tcp)
}

/// Completes a server handshake on an accepted socket, requiring a CA-signed
/// client certificate.
pub fn accept_duplex(config: Arc<rustls::ServerConfig>, tcp: TcpStream) -> io::Result<Duplex> {
    let connection = rustls::ServerConnection::new(config).map_err(io::Error::other)?;
    Duplex::handshake(rustls::Connection::Server(connection), tcp)
}

impl Duplex {
    fn handshake(mut connection: rustls::Connection, mut tcp: TcpStream) -> io::Result<Self> {
        tcp.set_nodelay(true)?;
        while connection.is_handshaking() {
            connection.complete_io(&mut tcp)?;
        }
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            write_lock: Arc::new(Mutex::new(())),
            socket: tcp,
            incoming: Vec::new(),
            plaintext: Vec::new(),
            plaintext_read: 0,
        })
    }

    fn lock(&self) -> io::Result<MutexGuard<'_, rustls::Connection>> {
        self.connection
            .lock()
            .map_err(|_| io::Error::other("TLS connection lock poisoned"))
    }

    /// Encrypts whatever rustls has queued and puts it on the wire, in record
    /// order with respect to every other handle.
    fn flush_tls(&self) -> io::Result<()> {
        let _ordered = self
            .write_lock
            .lock()
            .map_err(|_| io::Error::other("TLS write lock poisoned"))?;
        let mut ciphertext = Vec::new();
        {
            let mut connection = self.lock()?;
            while connection.wants_write() {
                connection.write_tls(&mut ciphertext)?;
            }
        }
        if !ciphertext.is_empty() {
            (&self.socket).write_all(&ciphertext)?;
        }
        Ok(())
    }
}

impl Duplex {
    /// Number of staged plaintext bytes not yet handed to the caller.
    fn staged(&self) -> usize {
        self.plaintext.len() - self.plaintext_read
    }

    /// Copies staged plaintext into the caller's buffer, compacting when drained.
    fn take_staged(&mut self, buffer: &mut [u8]) -> usize {
        let available = &self.plaintext[self.plaintext_read..];
        let count = available.len().min(buffer.len());
        buffer[..count].copy_from_slice(&available[..count]);
        self.plaintext_read += count;
        if self.plaintext_read == self.plaintext.len() {
            self.plaintext.clear();
            self.plaintext_read = 0;
        }
        count
    }
}

impl Read for Duplex {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.staged() > 0 {
                return Ok(self.take_staged(buffer));
            }
            if self.incoming.len() < READ_CHUNK {
                self.incoming.resize(READ_CHUNK, 0);
            }
            let count = self.socket.read(&mut self.incoming)?;
            if count == 0 {
                // TCP closed under the TLS session. The peer either shut down
                // without a close_notify or vanished; either way there is
                // nothing more to read.
                return Ok(0);
            }
            let respond;
            let mut drained = Vec::new();
            {
                let mut connection = self.lock()?;
                let mut bytes = &self.incoming[..count];
                let mut chunk = [0_u8; READ_CHUNK];
                while !bytes.is_empty() {
                    connection.read_tls(&mut bytes)?;
                    connection
                        .process_new_packets()
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                    // Drain decrypted plaintext out of rustls immediately so its
                    // capped internal buffer never fills, which would otherwise
                    // make the next read_tls fail under a two-way flood.
                    loop {
                        match connection.reader().read(&mut chunk) {
                            Ok(0) => break,
                            Ok(read) => drained.extend_from_slice(&chunk[..read]),
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                            Err(error) => return Err(error),
                        }
                    }
                }
                respond = connection.wants_write();
            }
            self.plaintext.extend_from_slice(&drained);
            if respond {
                // A key update or alert queued in response to what we read.
                self.flush_tls()?;
            }
            if self.staged() > 0 {
                return Ok(self.take_staged(buffer));
            }
            // The chunk carried only handshake or alert records and produced no
            // application data; go back for more ciphertext.
        }
    }
}

impl Write for Duplex {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let count = self.lock()?.writer().write(buffer)?;
        self.flush_tls()?;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_tls()?;
        (&self.socket).flush()
    }
}

impl Transport for Duplex {
    fn split(&self) -> io::Result<Box<dyn Transport>> {
        Ok(Box::new(Self {
            connection: Arc::clone(&self.connection),
            write_lock: Arc::clone(&self.write_lock),
            socket: self.socket.try_clone()?,
            incoming: Vec::new(),
            plaintext: Vec::new(),
            plaintext_read: 0,
        }))
    }

    fn shutdown(&self) -> io::Result<()> {
        if let Ok(mut connection) = self.connection.lock() {
            connection.send_close_notify();
        }
        // Best effort: the peer may already be gone, and the socket shutdown
        // below is what actually unblocks our own reader.
        let _ = self.flush_tls();
        Transport::shutdown(&self.socket)
    }

    fn set_timeouts(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.socket.set_timeouts(timeout)
    }

    fn peer_addr(&self) -> Option<SocketAddr> {
        self.socket.peer_addr().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Authority, client_config, server_config};
    use std::net::TcpListener;
    use std::thread;

    fn pair() -> (Duplex, Duplex) {
        let ca = Authority::generate("CA").unwrap();
        let server_cfg = server_config(&ca.issue("importer").unwrap(), &ca.certificate()).unwrap();
        let client_cfg = client_config(&ca.issue("exporter").unwrap(), &ca.certificate()).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            accept_duplex(server_cfg, tcp).unwrap()
        });
        let client = connect_duplex(client_cfg, "importer", TcpStream::connect(address).unwrap()).unwrap();
        (client, server.join().unwrap())
    }

    #[test]
    fn reads_and_writes_from_split_handles() {
        let (mut client, mut server) = pair();
        let mut client_writer = client.split().unwrap();
        let echo = thread::spawn(move || {
            let mut buffer = [0_u8; 11];
            server.read_exact(&mut buffer).unwrap();
            server.write_all(&buffer).unwrap();
            server.flush().unwrap();
        });
        client_writer.write_all(b"usb payload").unwrap();
        let mut reply = [0_u8; 11];
        client.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"usb payload");
        echo.join().unwrap();
    }

    #[test]
    fn both_directions_saturated_at_once_do_not_deadlock() {
        // Each side writes 8 MiB while reading the other's 8 MiB, on separate
        // threads, exactly the pattern that deadlocks a lock-held socket write.
        const TOTAL: usize = 8 * 1024 * 1024;
        let (client, server) = pair();
        let run = |mut stream: Duplex| {
            let mut writer = stream.split().unwrap();
            let writer = thread::spawn(move || {
                let chunk = vec![0x5a_u8; 64 * 1024];
                let mut sent = 0;
                while sent < TOTAL {
                    let take = chunk.len().min(TOTAL - sent);
                    writer.write_all(&chunk[..take]).unwrap();
                    sent += take;
                }
                writer.flush().unwrap();
            });
            let mut received = 0;
            let mut buffer = vec![0_u8; 64 * 1024];
            while received < TOTAL {
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0, "stream closed early");
                assert!(buffer[..count].iter().all(|byte| *byte == 0x5a));
                received += count;
            }
            writer.join().unwrap();
            received
        };
        let client_side = thread::spawn(move || run(client));
        let server_side = thread::spawn(move || run(server));
        assert_eq!(client_side.join().unwrap(), TOTAL);
        assert_eq!(server_side.join().unwrap(), TOTAL);
    }

    #[test]
    fn shutdown_from_another_handle_ends_a_blocked_read() {
        let (mut client, server) = pair();
        let closer = client.split().unwrap();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            closer.shutdown().unwrap();
        });
        let mut buffer = [0_u8; 1];
        let result = client.read(&mut buffer);
        assert!(matches!(result, Ok(0) | Err(_)), "shutdown must end the read");
        drop(server);
    }
}
