//! Long-lived device identity: a self-signed ECDSA P-256 certificate.
//!
//! The SHA-256 of the certificate is the device fingerprint; the host ID
//! shown to users is derived from it, so an ID can't be claimed without the
//! key.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

pub type Fingerprint = [u8; 32];

pub struct Identity {
    pub cert: CertificateDer<'static>,
    key_pkcs8: Vec<u8>,
    pub fingerprint: Fingerprint,
}

impl Clone for Identity {
    fn clone(&self) -> Self {
        Self {
            cert: self.cert.clone(),
            key_pkcs8: self.key_pkcs8.clone(),
            fingerprint: self.fingerprint,
        }
    }
}

pub fn fingerprint(cert_der: &[u8]) -> Fingerprint {
    Sha256::digest(cert_der).into()
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `qr-` and the first 64 bits of the fingerprint in lowercase base32,
/// grouped 4-4-5: `qr-abcd-efgh-ijklm`.
pub fn host_id(fp: &Fingerprint) -> String {
    let s = data_encoding::BASE32_NOPAD.encode(&fp[..8]).to_lowercase();
    format!("qr-{}-{}-{}", &s[..4], &s[4..8], &s[8..])
}

/// Default config directory: `~/.config/quickremote/<role>`.
pub fn default_dir(role: &str) -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("quickremote")
        .join(role)
}

impl Identity {
    /// Generates a fresh identity in memory.
    pub fn generate(name: &str) -> Result<Self> {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
        let mut params = rcgen::CertificateParams::new(vec!["quickremote".to_string()])?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        params.not_before = rcgen::date_time_ymd(2025, 1, 1);
        params.not_after = rcgen::date_time_ymd(2125, 1, 1);
        let cert = params.self_signed(&key)?;
        let cert = CertificateDer::from(cert.der().to_vec());
        let fingerprint = fingerprint(&cert);
        Ok(Self {
            cert,
            key_pkcs8: key.serialize_der(),
            fingerprint,
        })
    }

    /// Loads `identity.der` + `identity.key` from `dir`, creating them on
    /// first use (key file mode 0600).
    pub fn load_or_create(dir: &Path, name: &str) -> Result<Self> {
        let cert_path = dir.join("identity.der");
        let key_path = dir.join("identity.key");
        if cert_path.exists() && key_path.exists() {
            let cert = std::fs::read(&cert_path)
                .with_context(|| format!("reading {}", cert_path.display()))?;
            let key = std::fs::read(&key_path)
                .with_context(|| format!("reading {}", key_path.display()))?;
            let fingerprint = fingerprint(&cert);
            return Ok(Self {
                cert: CertificateDer::from(cert),
                key_pkcs8: key,
                fingerprint,
            });
        }
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let id = Self::generate(name)?;
        write_private(&key_path, &id.key_pkcs8)?;
        std::fs::write(&cert_path, &id.cert)?;
        Ok(id)
    }

    pub fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_pkcs8.clone()))
    }

    pub fn host_id(&self) -> String {
        host_id(&self.fingerprint)
    }

    /// ECDSA P-256 SHA-256 signature (ASN.1 DER) over `msg`.
    pub fn sign(&self, msg: &[u8]) -> Result<Vec<u8>> {
        let rng = ring::rand::SystemRandom::new();
        let kp = ring::signature::EcdsaKeyPair::from_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING,
            &self.key_pkcs8,
            &rng,
        )
        .map_err(|e| anyhow::anyhow!("identity key: {e}"))?;
        Ok(kp
            .sign(&rng, msg)
            .map_err(|e| anyhow::anyhow!("signing: {e}"))?
            .as_ref()
            .to_vec())
    }
}

/// Checks an ECDSA P-256 signature made by [`Identity::sign`] against the
/// public key in `cert_der`.
pub fn verify(cert_der: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    let Some(spki) = public_key(cert_der) else {
        return false;
    };
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_ASN1, spki)
        .verify(msg, sig)
        .is_ok()
}

/// The uncompressed P-256 point in a certificate's SubjectPublicKeyInfo.
/// Our certificates come from rcgen, so a search for the fixed SPKI prefix
/// (id-ecPublicKey + prime256v1, then the BIT STRING) is enough.
fn public_key(cert_der: &[u8]) -> Option<&[u8]> {
    const PREFIX: &[u8] = &[
        0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08,
        0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
    ];
    let at = cert_der.windows(PREFIX.len()).position(|w| w == PREFIX)? + PREFIX.len();
    cert_der.get(at..at + 65)
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut f = opts
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_format_and_signatures() {
        let id = Identity::generate("t").unwrap();
        let hid = id.host_id();
        assert!(
            hid.starts_with("qr-") && hid.len() == 3 + 4 + 1 + 4 + 1 + 5,
            "{hid}"
        );
        let sig = id.sign(b"hello").unwrap();
        assert!(verify(&id.cert, b"hello", &sig));
        assert!(!verify(&id.cert, b"hellp", &sig));
        let other = Identity::generate("u").unwrap();
        assert!(!verify(&other.cert, b"hello", &sig));
    }

    #[test]
    fn persists() {
        let dir = std::env::temp_dir().join(format!("qr-id-{}", std::process::id()));
        let a = Identity::load_or_create(&dir, "x").unwrap();
        let b = Identity::load_or_create(&dir, "x").unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
