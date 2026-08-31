use rcgen::{CertificateParams, DnType, KeyPair, KeyUsagePurpose};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::io;

/// A InterDriver certificate authority. Signs peer identities; distribute only its
/// certificate (never the key) so peers can verify each other.
pub struct Authority {
    certificate: rcgen::Certificate,
    key: KeyPair,
    cert_der: CertificateDer<'static>,
}

/// A peer's signed identity: its certificate chain (leaf + CA) and private key.
pub struct Identity {
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
}

impl Authority {
    pub fn generate(common_name: &str) -> io::Result<Self> {
        let key = KeyPair::generate().map_err(to_io)?;
        let mut params = CertificateParams::new(Vec::new()).map_err(to_io)?;
        params
            .distinguished_name
            .push(DnType::CommonName, common_name);
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let certificate = params.self_signed(&key).map_err(to_io)?;
        let cert_der = certificate.der().clone();
        Ok(Self {
            certificate,
            key,
            cert_der,
        })
    }

    #[must_use]
    pub fn certificate(&self) -> CertificateDer<'static> {
        self.cert_der.clone()
    }

    /// Issues a peer identity whose subject and SAN are `name`; the client
    /// verifies the server by this name.
    pub fn issue(&self, name: &str) -> io::Result<Identity> {
        let key = KeyPair::generate().map_err(to_io)?;
        let mut params =
            CertificateParams::new(vec![name.to_owned()]).map_err(to_io)?;
        params.distinguished_name.push(DnType::CommonName, name);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.use_authority_key_identifier_extension = true;
        let leaf = params
            .signed_by(&key, &self.certificate, &self.key)
            .map_err(to_io)?;
        let chain = vec![leaf.der().clone(), self.cert_der.clone()];
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        Ok(Identity { chain, key })
    }
}

impl Identity {
    #[must_use]
    pub fn chain(&self) -> Vec<CertificateDer<'static>> {
        self.chain.clone()
    }

    #[must_use]
    pub fn key(&self) -> PrivateKeyDer<'static> {
        self.key.clone_key()
    }

    /// PEM serialization for persisting an identity to disk.
    #[must_use]
    pub fn chain_pem(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        for cert in &self.chain {
            let encoded = pem_block("CERTIFICATE", cert.as_ref());
            let _ = write!(out, "{encoded}");
        }
        out
    }
}

fn to_io<E: std::fmt::Display>(error: E) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn pem_block(label: &str, der: &[u8]) -> String {
    let base64 = base64_encode(der);
    let mut body = String::new();
    for chunk in base64.as_bytes().chunks(64) {
        body.push_str(std::str::from_utf8(chunk).unwrap());
        body.push('\n');
    }
    format!("-----BEGIN {label}-----\n{body}-----END {label}-----\n")
}

fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(CHARS[((n >> 18) & 63) as usize] as char);
        out.push(CHARS[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            CHARS[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            CHARS[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}
