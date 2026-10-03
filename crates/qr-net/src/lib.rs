//! Networking for QuickRemote: identities, TLS, QUIC endpoints, pairing,
//! the session handshake, clock sync and LAN discovery.

pub mod clock;
pub mod discovery;
pub mod endpoint;
pub mod identity;
pub mod keystore;
pub mod pairing;
pub mod session;
pub mod tls;
pub mod trace;

pub use identity::{Fingerprint, Identity};
pub use keystore::KeyStore;

/// Installs the ring crypto provider as the process default (idempotent).
pub fn init_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
