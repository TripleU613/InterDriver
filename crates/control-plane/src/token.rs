use core::fmt;
use std::fs::File;
use std::io::{self, Read};

/// A 256-bit opaque capability token. The token value is the secret: holding it
/// is authorization. Never logged; compared in constant time.
#[derive(Clone)]
pub struct Token([u8; 32]);

impl Token {
    /// Draws a fresh token from the OS CSPRNG. Linux and macOS both expose
    /// `/dev/urandom`; the control plane runs on the Linux rendezvous host.
    pub fn generate() -> io::Result<Self> {
        let mut bytes = [0_u8; 32];
        File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        Ok(Self(bytes))
    }

    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn as_bytes(&self) -> [u8; 32] {
        self.0
    }

    #[must_use]
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in self.0 {
            use fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    pub fn from_hex(text: &str) -> Option<Self> {
        if text.len() != 64 {
            return None;
        }
        let mut bytes = [0_u8; 32];
        for (index, chunk) in text.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            let hi = hex_value(chunk[0])?;
            let lo = hex_value(chunk[1])?;
            bytes[index] = (hi << 4) | lo;
        }
        Some(Self(bytes))
    }
}

/// Constant-time equality so token comparison cannot be timed. Never derive
/// `PartialEq`, which would short-circuit on the first differing byte.
impl PartialEq for Token {
    fn eq(&self, other: &Self) -> bool {
        let mut diff = 0_u8;
        for (left, right) in self.0.iter().zip(other.0.iter()) {
            diff |= left ^ right;
        }
        diff == 0
    }
}

impl Eq for Token {}

/// Redaction: the debug form never reveals the secret.
impl fmt::Debug for Token {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Token(redacted)")
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let token = Token::from_bytes([0xab; 32]);
        assert_eq!(token.to_hex().len(), 64);
        assert_eq!(Token::from_hex(&token.to_hex()), Some(token));
    }

    #[test]
    fn rejects_malformed_hex() {
        assert_eq!(Token::from_hex("zz"), None);
        assert_eq!(Token::from_hex(&"a".repeat(63)), None);
        assert_eq!(Token::from_hex(&"g".repeat(64)), None);
    }

    #[test]
    fn distinct_tokens_are_unequal() {
        assert_ne!(Token::from_bytes([1; 32]), Token::from_bytes([2; 32]));
        assert_eq!(Token::from_bytes([1; 32]), Token::from_bytes([1; 32]));
    }

    #[test]
    fn urandom_tokens_differ() {
        let a = Token::generate().unwrap();
        let b = Token::generate().unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn debug_does_not_leak_the_secret() {
        let token = Token::from_bytes([0xff; 32]);
        assert_eq!(format!("{token:?}"), "Token(redacted)");
    }
}
