//! The byte stream the USB/IP engine runs on.
//!
//! The exporter, importer and session code never see a socket directly. They
//! see a `Transport`: something that can be read from and written to from two
//! threads at once and can be shut down from a third. A plain `TcpStream` is
//! one implementation; the mutually authenticated TLS stream in `interdriver-tls`
//! is another. Because the engine is written against this trait, the same code
//! carries USB/IP whether the exporter dialled the importer or the other way
//! round, and whether the bytes are encrypted or not.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::time::Duration;

pub trait Transport: Read + Write + Send + 'static {
    /// A second handle on the same connection, so one thread can block in a
    /// read while another writes. Implementations may allow only one thread to
    /// read at a time; the USB/IP engine has exactly one reader per connection.
    fn split(&self) -> io::Result<Box<dyn Transport>>;

    /// Ends the connection for every handle. A thread blocked in `read` sees
    /// end of file, which is how the engine tears a session down from outside.
    fn shutdown(&self) -> io::Result<()>;

    /// Bounds how long a single read or write may block. `None` removes the
    /// bound. Used around the handshake so a silent peer cannot hold a slot.
    fn set_timeouts(&self, timeout: Option<Duration>) -> io::Result<()>;

    fn peer_addr(&self) -> Option<SocketAddr> {
        None
    }
}

impl Transport for TcpStream {
    fn split(&self) -> io::Result<Box<dyn Transport>> {
        Ok(Box::new(self.try_clone()?))
    }

    fn shutdown(&self) -> io::Result<()> {
        match TcpStream::shutdown(self, Shutdown::Both) {
            // Shutting down a socket the peer already closed is not a failure.
            Err(error) if error.kind() == io::ErrorKind::NotConnected => Ok(()),
            other => other,
        }
    }

    fn set_timeouts(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(timeout)?;
        self.set_write_timeout(timeout)
    }

    fn peer_addr(&self) -> Option<SocketAddr> {
        TcpStream::peer_addr(self).ok()
    }
}

impl Transport for Box<dyn Transport> {
    fn split(&self) -> io::Result<Box<dyn Transport>> {
        (**self).split()
    }

    fn shutdown(&self) -> io::Result<()> {
        (**self).shutdown()
    }

    fn set_timeouts(&self, timeout: Option<Duration>) -> io::Result<()> {
        (**self).set_timeouts(timeout)
    }

    fn peer_addr(&self) -> Option<SocketAddr> {
        (**self).peer_addr()
    }
}

/// Turns an accepted or dialled socket into the transport a session runs on:
/// the identity function for plain TCP, a TLS handshake for an encrypted link.
pub type Wrap = std::sync::Arc<dyn Fn(TcpStream) -> io::Result<Box<dyn Transport>> + Send + Sync>;

/// The plain-TCP wrap: the socket is the transport.
#[must_use]
pub fn plain() -> Wrap {
    std::sync::Arc::new(|stream: TcpStream| -> io::Result<Box<dyn Transport>> {
        Ok(Box::new(stream))
    })
}

/// How an importer reaches its exporter. The USB/IP request always flows from
/// importer to exporter, but the TCP connection may be opened by either side:
/// `Dialer` opens it towards the exporter; `Acceptor` waits for an exporter
/// that opens it towards us, which is what a workstation behind a customer
/// firewall has to do.
pub trait Connect: Send {
    /// Returns the next connection, or `WouldBlock` when none is available
    /// yet so the caller can keep servicing other work and try again.
    fn connect(&mut self) -> io::Result<Box<dyn Transport>>;

    /// Whether connections should be served concurrently. A listener may face
    /// several importers at once (`true`); a dialer holds one connection at a
    /// time and redials when it drops (`false`).
    fn concurrent(&self) -> bool;

    fn describe(&self) -> String;
}

pub struct Dialer {
    remote: SocketAddr,
    timeout: Duration,
    wrap: Wrap,
}

impl Dialer {
    pub fn new<A: ToSocketAddrs>(remote: A, timeout: Duration, wrap: Wrap) -> io::Result<Self> {
        let remote = remote.to_socket_addrs()?.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::AddrNotAvailable, "USB/IP host has no usable address")
        })?;
        Ok(Self {
            remote,
            timeout,
            wrap,
        })
    }
}

impl Connect for Dialer {
    fn connect(&mut self) -> io::Result<Box<dyn Transport>> {
        let tcp = match TcpStream::connect_timeout(&self.remote, self.timeout) {
            Ok(tcp) => tcp,
            // The far end (importer or relay) may simply not be listening yet.
            // Treat "not reachable right now" as transient so the caller retries
            // rather than giving up; a genuine misconfiguration still surfaces.
            Err(error) if is_transient(&error) => {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, error));
            }
            Err(error) => return Err(error),
        };
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(self.timeout))?;
        tcp.set_write_timeout(Some(self.timeout))?;
        match (self.wrap)(tcp) {
            Ok(transport) => Ok(transport),
            // A TLS handshake against a peer that dropped is equally transient.
            Err(error) if is_transient(&error) => {
                Err(io::Error::new(io::ErrorKind::WouldBlock, error))
            }
            Err(error) => Err(error),
        }
    }

    fn concurrent(&self) -> bool {
        false
    }

    fn describe(&self) -> String {
        format!("dialling {}", self.remote)
    }
}

fn is_transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::TimedOut
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::NotConnected
            | io::ErrorKind::UnexpectedEof
    )
}

pub struct Acceptor {
    listener: TcpListener,
    handshake_timeout: Duration,
    wrap: Wrap,
}

impl Acceptor {
    /// Listens on `bind`. The listener is non-blocking so `connect` polls
    /// rather than parks; the caller decides how often to poll.
    pub fn bind<A: ToSocketAddrs>(bind: A, handshake_timeout: Duration, wrap: Wrap) -> io::Result<Self> {
        Self::from_listener(TcpListener::bind(bind)?, handshake_timeout, wrap)
    }

    /// Wraps an already bound listener. Used by the classic `serve` entry
    /// points, which are handed a listener the caller opened.
    pub fn from_listener(
        listener: TcpListener,
        handshake_timeout: Duration,
        wrap: Wrap,
    ) -> io::Result<Self> {
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            handshake_timeout,
            wrap,
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}

impl Connect for Acceptor {
    fn connect(&mut self) -> io::Result<Box<dyn Transport>> {
        let (tcp, _) = self.listener.accept()?;
        // Accepted sockets inherit the listener's non-blocking mode on Windows.
        tcp.set_nonblocking(false)?;
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(self.handshake_timeout))?;
        tcp.set_write_timeout(Some(self.handshake_timeout))?;
        (self.wrap)(tcp)
    }

    fn concurrent(&self) -> bool {
        true
    }

    fn describe(&self) -> String {
        match self.listener.local_addr() {
            Ok(address) => format!("listening on {address}"),
            Err(_) => "listening".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn a_split_handle_writes_while_the_original_reads() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let echo = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buffer = [0_u8; 5];
            stream.read_exact(&mut buffer).unwrap();
            stream.write_all(&buffer).unwrap();
        });
        let mut reader: Box<dyn Transport> = Box::new(TcpStream::connect(address).unwrap());
        let mut writer = reader.split().unwrap();
        writer.write_all(b"hello").unwrap();
        let mut reply = [0_u8; 5];
        reader.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"hello");
        echo.join().unwrap();
    }

    #[test]
    fn shutdown_unblocks_a_pending_read() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let _held = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_secs(5));
            drop(stream);
        });
        let mut reader: Box<dyn Transport> = Box::new(TcpStream::connect(address).unwrap());
        let closer = reader.split().unwrap();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            closer.shutdown().unwrap();
        });
        let mut buffer = [0_u8; 1];
        let result = reader.read(&mut buffer);
        assert!(matches!(result, Ok(0) | Err(_)), "shutdown must end the read");
    }
}
