use anyhow::{anyhow, Context, Result};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub struct Identity {
    pub certificate_pem: String,
    pub key_pem: String,
    pub fingerprint: String,
}

impl Identity {
    pub fn chain(&self) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
        let der = der_from_pem(&self.certificate_pem)
            .ok_or_else(|| anyhow!("could not read the certificate"))?;
        Ok(vec![rustls::pki_types::CertificateDer::from(der)])
    }

    pub fn key(&self) -> Result<rustls::pki_types::PrivateKeyDer<'static>> {
        let der =
            der_from_pem(&self.key_pem).ok_or_else(|| anyhow!("could not read the private key"))?;
        rustls::pki_types::PrivateKeyDer::try_from(der)
            .map_err(|err| anyhow!("unusable private key: {err}"))
    }
}

static IDENTITY: Mutex<Option<Arc<Identity>>> = Mutex::new(None);

pub fn shared() -> Result<Arc<Identity>> {
    let mut slot = IDENTITY
        .lock()
        .map_err(|_| anyhow!("the identity lock is poisoned"))?;
    if let Some(identity) = slot.as_ref() {
        return Ok(Arc::clone(identity));
    }
    let identity = Arc::new(load_or_create()?);
    *slot = Some(Arc::clone(&identity));
    Ok(identity)
}

fn state_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("epochoxide")
}

pub fn load_or_create() -> Result<Identity> {
    let dir = state_dir();
    let cert_path = dir.join("localsend-cert.pem");
    let key_path = dir.join("localsend-key.pem");

    if let (Ok(certificate_pem), Ok(key_pem)) = (
        fs::read_to_string(&cert_path),
        fs::read_to_string(&key_path),
    ) {
        if let Ok(fingerprint) = fingerprint_of(&certificate_pem) {
            return Ok(Identity {
                certificate_pem,
                key_pem,
                fingerprint,
            });
        }
    }

    let mut params = rcgen::CertificateParams::new(vec!["localsend".to_string()])
        .context("building certificate parameters")?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "LocalSend User");
    let key = rcgen::KeyPair::generate().context("generating a key pair")?;
    let certificate = params
        .self_signed(&key)
        .context("self-signing the certificate")?;

    let certificate_pem = certificate.pem();
    let key_pem = key.serialize_pem();
    let fingerprint = fingerprint_of(&certificate_pem)?;

    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::write(&cert_path, &certificate_pem).context("saving the certificate")?;
    fs::write(&key_path, &key_pem).context("saving the private key")?;
    restrict(&key_path);

    Ok(Identity {
        certificate_pem,
        key_pem,
        fingerprint,
    })
}

/// The private key is a credential; keep it out of other users' reach.
fn restrict(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
}

pub fn fingerprint_of(certificate_pem: &str) -> Result<String> {
    let der = der_from_pem(certificate_pem).context("reading the certificate")?;
    let digest = ring::digest::digest(&ring::digest::SHA256, &der);
    Ok(digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect())
}

pub fn der_from_pem(pem: &str) -> Option<Vec<u8>> {
    let body: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");
    base64_decode(body.trim())
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    for byte in input.bytes() {
        if byte == b'=' || byte.is_ascii_whitespace() {
            continue;
        }
        let value = TABLE.iter().position(|candidate| *candidate == byte)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_a_known_value() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGVsbG8h").unwrap(), b"hello!");
        assert_eq!(base64_decode("aGVs\nbG8=").unwrap(), b"hello");
    }

    #[test]
    fn a_generated_identity_has_a_sha256_fingerprint() {
        let params = rcgen::CertificateParams::new(vec!["localsend".to_string()]).unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let certificate = params.self_signed(&key).unwrap();
        let fingerprint = fingerprint_of(&certificate.pem()).unwrap();
        assert_eq!(fingerprint.len(), 64);
        assert!(fingerprint
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_lowercase()));
    }

    #[test]
    fn the_fingerprint_matches_the_der_the_peer_would_see() {
        let params = rcgen::CertificateParams::new(vec!["localsend".to_string()]).unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let certificate = params.self_signed(&key).unwrap();
        let expected = ring::digest::digest(&ring::digest::SHA256, certificate.der());
        let expected_hex: String = expected
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect();
        assert_eq!(fingerprint_of(&certificate.pem()).unwrap(), expected_hex);
    }
}
