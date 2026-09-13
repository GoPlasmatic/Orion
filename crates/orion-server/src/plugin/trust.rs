//! `[plugins.trust]`: detached Ed25519 signatures over a component digest.
//!
//! Optional hardening on top of admin auth. The trust root for installing a
//! plugin is the admin credential — the one that already reads and writes
//! connector secrets — so a signature adds no new principal; what it adds is
//! a check that survives the upload. The signed message is the digest string
//! exactly as the server computes it (`sha256:<64 hex>`), so a release
//! pipeline signs the identity a generation, a trace and a package already
//! name, and never needs the bytes in memory to do it. Keys and signatures
//! travel as standard base64.
//!
//! Verified twice: when an upload arrives (`services::plugins::prepare`),
//! and again by every node that loads the version (`loader::load_one`), so a
//! row that reached the database by any other path — an import on a node
//! with no keys, a peer's activation — is checked by the node that runs it.
//! A node with no keys configured checks nothing and stores what it was sent.
//!
//! The check itself is [`crate::crypto::ed25519`], shared with `[models.trust]`
//! — the same message shape, the same key encoding, one implementation. This
//! module is the plugin-facing spelling of it and adds nothing.

pub use crate::crypto::ed25519::{KEY_LEN, SIGNATURE_LEN, SigningKey, parse_public_key, verify};
