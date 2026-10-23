use super::cert;
use anyhow::{anyhow, bail, Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

/// Generous: `prepare-upload` waits for a person to accept on the other device.
const TIMEOUT: Duration = Duration::from_secs(180);

pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub https: bool,
    pub fingerprint: String,
}

#[derive(Debug)]
struct PinnedFingerprint {
    expected: Vec<u8>,
}

impl PinnedFingerprint {
    fn new(fingerprint: &str) -> Result<Self> {
        let expected = decode_hex(fingerprint)
            .ok_or_else(|| anyhow!("device announced an unreadable fingerprint"))?;
        Ok(Self { expected })
    }
}

impl ServerCertVerifier for PinnedFingerprint {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let actual = ring::digest::digest(&ring::digest::SHA256, end_entity.as_ref());
        if actual.as_ref() == self.expected.as_slice() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "certificate does not match the fingerprint the device announced".into(),
            ))
        }
    }

    // The certificate itself is pinned, so these only confirm the peer holds the matching key.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn waiting_hint(err: std::io::Error) -> anyhow::Error {
    use std::io::ErrorKind::{TimedOut, WouldBlock};
    if matches!(err.kind(), TimedOut | WouldBlock) {
        anyhow!("the device never answered -- it is probably waiting for someone to accept the transfer")
    } else {
        anyhow::Error::new(err).context("reading the device's response")
    }
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    let cleaned: String = value
        .chars()
        .filter(|c| !matches!(c, ':' | ' ' | '-'))
        .collect();
    if !cleaned.len().is_multiple_of(2) {
        return None;
    }
    (0..cleaned.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).ok())
        .collect()
}

fn request_bytes(path: &str, host: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

fn parse_response(raw: &[u8]) -> Result<(u16, Vec<u8>)> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed response from device"))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| anyhow!("device sent no status line"))?;
    let body = &raw[split + 4..];
    // LocalSend answers with Transfer-Encoding: chunked.
    let chunked = head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .any(|(name, value)| {
            name.eq_ignore_ascii_case("transfer-encoding")
                && value.to_lowercase().contains("chunked")
        });
    Ok((
        status,
        if chunked {
            dechunk(body)?
        } else {
            body.to_vec()
        },
    ))
}

fn dechunk(body: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut rest = body;
    // A truncated final chunk is common when a device closes early; whatever arrived is kept.
    while let Some(line_end) = rest.windows(2).position(|w| w == b"\r\n") {
        let header = String::from_utf8_lossy(&rest[..line_end]);
        let size_text = header.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size_text, 16) else {
            bail!("device sent an unreadable chunk size \"{size_text}\"");
        };
        rest = &rest[line_end + 2..];
        if size == 0 {
            break;
        }
        if rest.len() < size {
            out.extend_from_slice(rest);
            break;
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[(size + 2).min(rest.len())..];
    }
    Ok(out)
}

impl Endpoint {
    fn connect(&self) -> Result<TcpStream> {
        let stream = TcpStream::connect((self.host.as_str(), self.port))
            .with_context(|| format!("connecting to {}:{}", self.host, self.port))?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        Ok(stream)
    }

    pub fn post(&self, path: &str, content_type: &str, body: &[u8]) -> Result<Vec<u8>> {
        let request = request_bytes(path, &self.host, content_type, body);
        let raw = if self.https {
            self.exchange_tls(&request)?
        } else {
            self.exchange_plain(&request)?
        };
        let (status, response) = parse_response(&raw)?;
        if !(200..300).contains(&status) {
            let detail = String::from_utf8_lossy(&response).trim().to_string();
            bail!(if detail.is_empty() {
                format!("device answered HTTP {status}")
            } else {
                format!("device answered HTTP {status}: {detail}")
            });
        }
        Ok(response)
    }

    fn exchange_plain(&self, request: &[u8]) -> Result<Vec<u8>> {
        let mut stream = self.connect()?;
        stream.write_all(request)?;
        stream.flush()?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).map_err(waiting_hint)?;
        Ok(raw)
    }

    fn exchange_tls(&self, request: &[u8]) -> Result<Vec<u8>> {
        let verifier = Arc::new(PinnedFingerprint::new(&self.fingerprint)?);
        // The receiver requests a client certificate (TLS 1.3 rejects an empty one), and it must be
        // the one whose fingerprint we announced.
        let identity = cert::shared().context("loading this device's LocalSend certificate")?;
        let config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()?
                .dangerous()
                .with_custom_certificate_verifier(verifier)
                .with_client_auth_cert(identity.chain()?, identity.key()?)
                .context("presenting this device's certificate")?;

        // Self-signed with no useful name: SNI is irrelevant; the fingerprint decides trust.
        let name = ServerName::try_from("localsend")
            .map_err(|_| anyhow!("could not build a server name"))?;
        let connection = ClientConnection::new(Arc::new(config), name)?;
        let mut tls = StreamOwned::new(connection, self.connect()?);
        tls.write_all(request)?;
        tls.flush()?;
        let mut raw = Vec::new();
        // A device closing without a clean TLS shutdown is normal here.
        match tls.read_to_end(&mut raw) {
            Ok(_) => {}
            Err(_) if !raw.is_empty() => {}
            Err(err) => return Err(waiting_hint(err)),
        }
        Ok(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fingerprint_can_be_written_with_or_without_separators() {
        assert_eq!(decode_hex("00ff10").unwrap(), vec![0, 255, 16]);
        assert_eq!(decode_hex("00:FF:10").unwrap(), vec![0, 255, 16]);
    }

    #[test]
    fn an_unusable_fingerprint_is_refused_rather_than_ignored() {
        // A short or non-hex fingerprint must not silently become "no pinning".
        assert!(decode_hex("abc").is_none());
        assert!(decode_hex("zz").is_none());
        assert!(PinnedFingerprint::new("nonsense!").is_err());
    }

    #[test]
    fn a_status_line_and_body_are_split_apart() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        let (status, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"{}");
    }

    #[test]
    fn a_chunked_body_is_unwrapped() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        let (status, body) = parse_response(raw).unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, b"hello");
    }

    #[test]
    fn a_chunked_body_arriving_in_several_chunks_is_joined() {
        let raw = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        let (_, body) = parse_response(raw).unwrap();
        assert_eq!(body, b"abcde");
    }

    #[test]
    fn a_content_length_body_is_left_alone() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let (_, body) = parse_response(raw).unwrap();
        assert_eq!(body, b"hello");
    }

    #[test]
    fn a_response_with_no_header_break_is_an_error() {
        assert!(parse_response(b"HTTP/1.1 200 OK").is_err());
    }
}
