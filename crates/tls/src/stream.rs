use rustls::pki_types::ServerName;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

/// A synchronous TLS stream over a TCP connection. Wraps rustls so the USB/IP
/// and control-plane code can read and write bytes without seeing the crypto.
pub struct TlsStream {
    inner: Inner,
}

enum Inner {
    Client(rustls::StreamOwned<rustls::ClientConnection, TcpStream>),
    Server(rustls::StreamOwned<rustls::ServerConnection, TcpStream>),
}

/// Completes a client handshake, presenting the client identity and verifying
/// the server is `server_name` and CA-signed.
pub fn connect(
    config: Arc<rustls::ClientConfig>,
    server_name: &str,
    tcp: TcpStream,
) -> io::Result<TlsStream> {
    let name = ServerName::try_from(server_name.to_owned())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let mut connection = rustls::ClientConnection::new(config, name)
        .map_err(|error| io::Error::new(io::ErrorKind::Other, error))?;
    let mut tcp = tcp;
    connection.complete_io(&mut tcp)?;
    Ok(TlsStream {
        inner: Inner::Client(rustls::StreamOwned::new(connection, tcp)),
    })
}

/// Completes a server handshake, requiring a CA-signed client certificate.
pub fn accept(config: Arc<rustls::ServerConfig>, tcp: TcpStream) -> io::Result<TlsStream> {
    let mut connection = rustls::ServerConnection::new(config)
        .map_err(|error| io::Error::new(io::ErrorKind::Other, error))?;
    let mut tcp = tcp;
    connection.complete_io(&mut tcp)?;
    Ok(TlsStream {
        inner: Inner::Server(rustls::StreamOwned::new(connection, tcp)),
    })
}

impl TlsStream {
    /// Puts the underlying socket into non-blocking mode for the duplex pump.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match &self.inner {
            Inner::Client(stream) => stream.sock.set_nonblocking(nonblocking),
            Inner::Server(stream) => stream.sock.set_nonblocking(nonblocking),
        }
    }
}

impl Read for TlsStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match &mut self.inner {
            Inner::Client(stream) => stream.read(buffer),
            Inner::Server(stream) => stream.read(buffer),
        }
    }
}

impl Write for TlsStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match &mut self.inner {
            Inner::Client(stream) => stream.write(buffer),
            Inner::Server(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.inner {
            Inner::Client(stream) => stream.flush(),
            Inner::Server(stream) => stream.flush(),
        }
    }
}
