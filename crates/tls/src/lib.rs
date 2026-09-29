//! TLS for the device channel.
//!
//! The controller is its own certificate authority. On first start it creates the CA and a
//! server certificate the CA signs, and serves both, so the agent receives the CA with every
//! handshake. The agent pins that CA: the first controller it reaches is trusted on first use
//! and recorded, and from then on it accepts only a server certificate that chains to the
//! pinned CA. Host names aren't checked: devices reach the router by whatever address it has
//! (the default gateway, an IP), and the pin, not the name, is what identifies the
//! controller.
//!
//! Everything runs on `ring` (rcgen, rustls, webpki), which builds for every OpenWrt target
//! with the SDK's C compiler; aws-lc-rs would need cmake and clang.

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{PrivateKeyDer, UnixTime};
use rustls::{
    ClientConfig, DigitallySignedStruct, Error as TlsError, ServerConfig, SignatureScheme,
};
use std::fmt;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub use rustls::pki_types::{CertificateDer, ServerName};
pub use tokio_rustls::{TlsAcceptor, TlsConnector};

#[derive(Debug)]
pub enum Error {
    Io(PathBuf, std::io::Error),
    Cert(String),
    Tls(TlsError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(p, e) => write!(f, "{}: {e}", p.display()),
            Error::Cert(m) => write!(f, "certificate: {m}"),
            Error::Tls(e) => write!(f, "tls: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<rcgen::Error> for Error {
    fn from(e: rcgen::Error) -> Self {
        Error::Cert(e.to_string())
    }
}

impl From<TlsError> for Error {
    fn from(e: TlsError) -> Self {
        Error::Tls(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The SHA-256 fingerprint of a certificate, as colon-separated hex (what people compare).
pub fn fingerprint(cert: &CertificateDer<'_>) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, cert.as_ref());
    d.as_ref()
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn read(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).map_err(|e| Error::Io(path.into(), e))
}

/// Writes `data` to `path` via a temporary file, readable by the owner only when `private`.
fn write(path: &Path, data: &[u8], private: bool) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let io = |e| Error::Io(tmp.clone(), e);
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(if private { 0o600 } else { 0o644 })
        .open(&tmp)
        .map_err(io)?;
    f.write_all(data).map_err(io)?;
    f.sync_all().map_err(io)?;
    fs::rename(&tmp, path).map_err(|e| Error::Io(path.into(), e))
}

fn certs_from_pem(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let pem = read(path)?;
    CertificateDer::pem_slice_iter(&pem)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::Cert(format!("{}: {e}", path.display())))
}

/// The controller's certificate authority and its server certificate.
pub struct ControllerIdentity {
    pub ca: CertificateDer<'static>,
    pub server: CertificateDer<'static>,
    server_key: PrivateKeyDer<'static>,
    /// True when this start created them.
    pub created: bool,
}

const CA_CERT: &str = "ca.pem";
const CA_KEY: &str = "ca.key";
const SERVER_CERT: &str = "server.pem";
const SERVER_KEY: &str = "server.key";

impl ControllerIdentity {
    /// Loads the identity from `dir`, creating it on first start. The keys are written
    /// readable by their owner only.
    pub fn load_or_create(dir: &Path) -> Result<ControllerIdentity> {
        let files = [CA_CERT, CA_KEY, SERVER_CERT, SERVER_KEY].map(|f| dir.join(f));
        let present = files.iter().filter(|p| p.exists()).count();
        if present == 0 {
            fs::create_dir_all(dir).map_err(|e| Error::Io(dir.into(), e))?;
            Self::create(dir)?;
        } else if present < files.len() {
            return Err(Error::Cert(format!(
                "{} holds part of the controller's identity; restore the rest, or remove {} to start over \
                 (every agent then has to be re-pinned)",
                dir.display(),
                files
                    .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                    .join(", ")
            )));
        }
        let ca = certs_from_pem(&files[0])?
            .into_iter()
            .next()
            .ok_or(Error::Cert("no CA certificate".into()))?;
        let server = certs_from_pem(&files[2])?
            .into_iter()
            .next()
            .ok_or(Error::Cert("no server certificate".into()))?;
        let server_key = PrivateKeyDer::from_pem_slice(&read(&files[3])?)
            .map_err(|e| Error::Cert(format!("{}: {e}", files[3].display())))?;
        Ok(ControllerIdentity {
            ca,
            server,
            server_key,
            created: present == 0,
        })
    }

    fn create(dir: &Path) -> Result<()> {
        // The validity starts well in the past: an OpenWrt device without an RTC boots with
        // its image's build date until NTP answers.
        let (from, until) = (
            rcgen::date_time_ymd(2020, 1, 1),
            rcgen::date_time_ymd(2099, 12, 31),
        );
        let ca_key = KeyPair::generate()?;
        let mut ca = CertificateParams::default();
        ca.distinguished_name = DistinguishedName::new();
        ca.distinguished_name
            .push(DnType::CommonName, "Steward controller CA");
        ca.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        ca.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        ca.not_before = from;
        ca.not_after = until;
        let ca_cert = ca.self_signed(&ca_key)?;

        let server_key = KeyPair::generate()?;
        let mut server = CertificateParams::new(vec!["steward-controller".to_string()])?;
        server
            .distinguished_name
            .push(DnType::CommonName, "steward-controller");
        server.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        server.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        server.not_before = from;
        server.not_after = until;
        let issuer = Issuer::new(ca, &ca_key);
        let server_cert = server.signed_by(&server_key, &issuer)?;

        write(&dir.join(CA_KEY), ca_key.serialize_pem().as_bytes(), true)?;
        write(&dir.join(CA_CERT), ca_cert.pem().as_bytes(), false)?;
        write(
            &dir.join(SERVER_KEY),
            server_key.serialize_pem().as_bytes(),
            true,
        )?;
        write(&dir.join(SERVER_CERT), server_cert.pem().as_bytes(), false)
    }

    /// The TLS configuration the device channel serves: the server certificate and the CA.
    pub fn server_config(&self) -> Result<Arc<ServerConfig>> {
        let config = ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(
                vec![self.server.clone(), self.ca.clone()],
                self.server_key.clone_key(),
            )?;
        Ok(Arc::new(config))
    }
}

/// Where the agent keeps the CA it pinned.
pub struct PinFile(pub PathBuf);

impl PinFile {
    /// The pinned CA, if there is one. Only a missing file means there's none: a pin file that
    /// can't be read, or holds no certificate (emptied, say), is an error, because reading it
    /// as no pin would trust whichever controller answers next.
    pub fn load(&self) -> Result<Option<CertificateDer<'static>>> {
        match fs::metadata(&self.0) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::Io(self.0.clone(), e)),
            Ok(_) => {}
        }
        let ca = certs_from_pem(&self.0)?.into_iter().next().ok_or_else(|| {
            Error::Cert(format!(
                "{} holds no certificate: put the controller's CA back, or remove the file to \
                 trust the next controller on first use",
                self.0.display()
            ))
        })?;
        Ok(Some(ca))
    }

    pub fn save(&self, ca: &CertificateDer<'_>) -> Result<()> {
        if let Some(dir) = self.0.parent() {
            fs::create_dir_all(dir).map_err(|e| Error::Io(dir.into(), e))?;
        }
        let pem = pem_encode("CERTIFICATE", ca.as_ref());
        write(&self.0, pem.as_bytes(), false)
    }
}

fn pem_encode(label: &str, der: &[u8]) -> String {
    const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut b = String::new();
    for chunk in der.chunks(3) {
        let n = chunk.iter().fold(0u32, |a, &x| a << 8 | u32::from(x)) << (8 * (3 - chunk.len()));
        for i in 0..4 {
            b.push(if i <= chunk.len() {
                B64[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    let lines: Vec<&str> = b
        .as_bytes()
        .chunks(64)
        .map(|l| std::str::from_utf8(l).unwrap())
        .collect();
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        lines.join("\n")
    )
}

/// The agent's check of the controller: the server certificate must chain to the pinned CA.
/// Without a pin, the CA the controller sends is trusted on first use: the chain must still
/// be valid under it, and [`PinnedCa::first_use`] then hands it over to be pinned.
#[derive(Debug)]
pub struct PinnedCa {
    pin: Option<CertificateDer<'static>>,
    seen: Mutex<Option<CertificateDer<'static>>>,
    provider: Arc<CryptoProvider>,
}

impl PinnedCa {
    pub fn new(pin: Option<CertificateDer<'static>>) -> Arc<PinnedCa> {
        Arc::new(PinnedCa {
            pin,
            seen: Mutex::new(None),
            provider: provider(),
        })
    }

    /// The CA trusted on first use by the last handshake, when there was no pin.
    pub fn first_use(&self) -> Option<CertificateDer<'static>> {
        self.seen.lock().unwrap().take()
    }

    pub fn client_config(self: &Arc<Self>) -> Result<Arc<ClientConfig>> {
        let config = ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(self.clone())
            .with_no_client_auth();
        Ok(Arc::new(config))
    }
}

impl ServerCertVerifier for PinnedCa {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, TlsError> {
        let ca = match &self.pin {
            Some(pin) => pin.clone(),
            None => intermediates
                .last()
                .ok_or_else(|| TlsError::General("the controller sent no CA certificate".into()))?
                .clone()
                .into_owned(),
        };
        let anchor = webpki::anchor_from_trusted_cert(&ca)
            .map_err(|e| TlsError::General(format!("the controller's CA is unusable: {e}")))?;
        let cert = webpki::EndEntityCert::try_from(end_entity).map_err(|e| {
            TlsError::General(format!("the controller's certificate is unusable: {e}"))
        })?;
        cert.verify_for_usage(
            self.provider.signature_verification_algorithms.all,
            &[anchor],
            intermediates,
            now,
            webpki::KeyUsage::server_auth(),
            None,
            None,
        )
        .map_err(|e| match self.pin {
            Some(_) => TlsError::General(format!(
                "the controller's certificate doesn't chain to the pinned CA {} ({e}): not the controller this \
                 device was pinned to",
                fingerprint(&ca)
            )),
            None => TlsError::General(format!("the controller's certificate chain is invalid: {e}")),
        })?;
        if self.pin.is_none() {
            *self.seen.lock().unwrap() = Some(ca);
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn tempdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("steward-tls-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    /// One TLS handshake over an in-memory pipe, then a byte each way.
    async fn handshake(server: &ControllerIdentity, verifier: &Arc<PinnedCa>) -> Result<()> {
        let (c, s) = tokio::io::duplex(64 * 1024);
        let acceptor = TlsAcceptor::from(server.server_config()?);
        let connector = TlsConnector::from(verifier.client_config()?);
        let srv = tokio::spawn(async move {
            let mut s = acceptor.accept(s).await?;
            let mut b = [0u8; 1];
            s.read_exact(&mut b).await?;
            s.write_all(&b).await?;
            s.flush().await
        });
        let name = ServerName::try_from("192.0.2.1").unwrap();
        let mut c = connector
            .connect(name, c)
            .await
            .map_err(|e| Error::Io("client".into(), e))?;
        c.write_all(b"x")
            .await
            .map_err(|e| Error::Io("client".into(), e))?;
        let mut b = [0u8; 1];
        c.read_exact(&mut b)
            .await
            .map_err(|e| Error::Io("client".into(), e))?;
        srv.await
            .unwrap()
            .map_err(|e| Error::Io("server".into(), e))?;
        Ok(())
    }

    #[test]
    fn the_identity_is_created_once_and_its_keys_are_private() {
        let dir = tempdir("create");
        let first = ControllerIdentity::load_or_create(&dir).unwrap();
        assert!(first.created);
        let again = ControllerIdentity::load_or_create(&dir).unwrap();
        assert!(!again.created);
        assert_eq!(first.ca, again.ca);
        for key in [CA_KEY, SERVER_KEY] {
            let mode = fs::metadata(dir.join(key)).unwrap().permissions();
            assert_eq!(
                std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
                0o600,
                "{key}"
            );
        }
        fs::remove_file(dir.join(SERVER_KEY)).unwrap();
        assert!(matches!(
            ControllerIdentity::load_or_create(&dir),
            Err(Error::Cert(_))
        ));
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn first_use_pins_the_ca_and_the_pin_holds() {
        let dir = tempdir("pin");
        let controller = ControllerIdentity::load_or_create(&dir.join("controller")).unwrap();

        // First contact: no pin, the CA the controller sends is trusted and handed over.
        let tofu = PinnedCa::new(None);
        handshake(&controller, &tofu).await.unwrap();
        let ca = tofu.first_use().expect("the CA to pin");
        assert_eq!(ca, controller.ca);
        let pin = PinFile(dir.join("agent/controller-ca.pem"));
        pin.save(&ca).unwrap();
        assert_eq!(pin.load().unwrap().as_ref(), Some(&controller.ca));

        // Later: the pinned controller is accepted, and nothing new is handed over.
        let pinned = PinnedCa::new(pin.load().unwrap());
        handshake(&controller, &pinned).await.unwrap();
        assert!(pinned.first_use().is_none());

        // Another controller (a new CA) is refused.
        let impostor = ControllerIdentity::load_or_create(&dir.join("impostor")).unwrap();
        let err = handshake(&impostor, &pinned).await.unwrap_err().to_string();
        assert!(err.contains("pinned CA"), "{err}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_pin_file_without_a_certificate_fails_closed() {
        let dir = tempdir("badpin");
        fs::create_dir_all(&dir).unwrap();
        let pin = PinFile(dir.join("controller-ca.pem"));
        assert!(pin.load().unwrap().is_none(), "no file: nothing pinned yet");
        // Emptied, or text that isn't PEM: an error that names the file, not "no pin" (which
        // would trust the next controller on first use).
        for content in ["", "not a certificate\n"] {
            fs::write(&pin.0, content).unwrap();
            let err = pin.load().expect_err(content).to_string();
            assert!(
                err.contains("controller-ca.pem holds no certificate"),
                "{err}"
            );
        }
        // Cut short, or not a file at all.
        fs::write(&pin.0, "-----BEGIN CERTIFICATE-----\nMIIB\n").unwrap();
        assert!(pin.load().is_err());
        fs::remove_file(&pin.0).unwrap();
        fs::create_dir(&pin.0).unwrap();
        assert!(pin.load().is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fingerprints_are_colon_separated_sha256() {
        let fp = fingerprint(&CertificateDer::from(vec![1, 2, 3]));
        assert_eq!(fp.len(), 32 * 3 - 1);
        assert!(fp.starts_with("03:90:58:C6"));
    }

    #[test]
    fn pem_round_trips() {
        let der = CertificateDer::from((0u8..=200).collect::<Vec<_>>());
        let pem = pem_encode("CERTIFICATE", der.as_ref());
        let back = CertificateDer::pem_slice_iter(pem.as_bytes())
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(back, der);
    }
}
