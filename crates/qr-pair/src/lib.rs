//! PIN pairing core, shared by native peers and the web client (WASM):
//! SPAKE2 over Ed25519 plus HMAC confirmation of a transcript that binds
//! both identities and the channel.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity as SpakeId, Password, Spake2};

pub type Fingerprint = [u8; 32];

pub const ID_CLIENT: &[u8] = b"qr-client";
pub const ID_HOST: &[u8] = b"qr-host";

/// What both sides MAC: fingerprints of both identities and a channel
/// binding (TLS exporter for native clients; zeros for browsers, which pin
/// the host certificate by hash instead).
pub struct Transcript {
    pub client_fp: Fingerprint,
    pub host_fp: Fingerprint,
    pub exporter: [u8; 32],
}

impl Transcript {
    fn bytes(&self) -> Vec<u8> {
        let mut v = b"quickremote pairing v1".to_vec();
        v.extend_from_slice(&self.client_fp);
        v.extend_from_slice(&self.host_fp);
        v.extend_from_slice(&self.exporter);
        v
    }
}

pub struct Keys {
    key: Vec<u8>,
    transcript: Vec<u8>,
}

impl Keys {
    fn mac(&self, role: &[u8]) -> [u8; 32] {
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(&self.key).expect("any key length");
        m.update(role);
        m.update(&self.transcript);
        m.finalize().into_bytes().into()
    }

    pub fn client_mac(&self) -> [u8; 32] {
        self.mac(b"client")
    }

    pub fn host_mac(&self) -> [u8; 32] {
        self.mac(b"host")
    }

    /// Constant-time check of the peer's MAC.
    pub fn check(&self, role_is_client: bool, mac: &[u8; 32]) -> bool {
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(&self.key).expect("any key length");
        m.update(if role_is_client { b"client" } else { b"host" });
        m.update(&self.transcript);
        m.verify_slice(mac).is_ok()
    }
}

pub struct Pairing {
    state: Spake2<Ed25519Group>,
}

impl Pairing {
    /// Starts the client side; returns the message to send.
    pub fn client(pin: &str) -> (Self, Vec<u8>) {
        let (state, msg) = Spake2::<Ed25519Group>::start_a(
            &Password::new(pin.as_bytes()),
            &SpakeId::new(ID_CLIENT),
            &SpakeId::new(ID_HOST),
        );
        (Self { state }, msg)
    }

    /// Starts the host side; returns the message to send.
    pub fn host(pin: &str) -> (Self, Vec<u8>) {
        let (state, msg) = Spake2::<Ed25519Group>::start_b(
            &Password::new(pin.as_bytes()),
            &SpakeId::new(ID_CLIENT),
            &SpakeId::new(ID_HOST),
        );
        (Self { state }, msg)
    }

    /// Finishes with the peer's message. A wrong PIN is not detected here
    /// (SPAKE2 yields different keys); it shows as a failed MAC check.
    pub fn finish(self, peer_msg: &[u8], t: &Transcript) -> Option<Keys> {
        let key = self.state.finish(peer_msg).ok()?;
        Some(Keys { key, transcript: t.bytes() })
    }
}
