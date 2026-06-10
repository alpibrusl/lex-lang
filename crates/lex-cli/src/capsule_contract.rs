//! The capsule **capability contract** — lex-lang's emitter/verifier for the
//! signed contract that `lex-os capsule install` consumes (the registry leg of
//! lex-os#36).
//!
//! This is a faithful, byte-compatible reimplementation of the versioned
//! `lex.os.capsule.contract.v1` format defined in `lex-os-capsule`. lex-lang
//! and lex-os are deliberately separate repos with a one-way dependency
//! (lex-os → lex-lang), so the contract type cannot simply be imported here.
//! Instead we reproduce the *versioned* wire format exactly; the one non-trivial
//! shared type — [`lex_types::trust::Grant`] — really is the same crate (lex-lang
//! owns it, lex-os imports it), so its serialization cannot drift. Only the thin
//! wrapper structs and the canonical-byte construction are mirrored here, and
//! `tests/pkg_contract.rs::canonical_form_is_pinned` locks those bytes against a
//! golden vector so a divergence fails CI rather than silently breaking install.
//!
//! What lex-lang produces (`lex pkg publish --sign`) is the same `SignedContract`
//! JSON `lex-os capsule install --contract` already verifies, signed over the
//! same domain-separated payload. The interop is proven end-to-end by the
//! cross-binary test that feeds a lex-lang-signed contract to `lex-os capsule
//! verify`/`install`.

use lex_types::trust::Grant;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Domain separator mixed into the signed payload and the content id, so a
/// contract signature can never be confused with a signature over any other
/// lex-os structure. Must match `lex-os-capsule`'s `CONTRACT_DOMAIN` exactly.
pub const CONTRACT_DOMAIN: &[u8] = b"lex.os.capsule.contract.v1";

/// A handle to the distributable bits: a `lex pkg` artifact identified by name,
/// version, and the content hash of its published archive. The hash is
/// authoritative — name/version are for humans and discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub name: String,
    pub version: String,
    /// Hex SHA-256 of the published package archive. The signature binds the
    /// contract to *these* bytes.
    pub content_hash: String,
}

/// The capability envelope an artifact declares it needs to run as intended:
/// the trust [`Grant`] and the network egress allowlist. A *declared
/// requirement* bound to a specific artifact, never a grant of authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityContract {
    pub artifact: ArtifactRef,
    /// The grant the artifact needs. Same `lex_types::trust::Grant` lex-os
    /// uses, so the on-disk JSON (`{"filesystem":"ReadOnly",…}`) is identical.
    pub requires: Grant,
    /// Network hosts the artifact must reach. Subset of the consumer's egress
    /// at install time.
    #[serde(default)]
    pub egress: Vec<String>,
}

/// A capability contract plus the publisher's Ed25519 signature over its
/// canonical bytes — the unit that travels with (or alongside) a published
/// artifact. Field order matches `lex-os-capsule::SignedContract` so the JSON
/// round-trips between the two.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedContract {
    pub contract: CapabilityContract,
    /// Hex-encoded Ed25519 public key (32 bytes) of the signer.
    pub signer: String,
    /// Hex-encoded Ed25519 signature (64 bytes) over [`signing_payload`].
    ///
    /// [`signing_payload`]: CapabilityContract::signing_payload
    pub signature: String,
}

impl CapabilityContract {
    /// The canonical JSON used for signing and content-addressing. Mirrors
    /// `lex-os-capsule::CapabilityContract::canonical_json` byte-for-byte: a
    /// dedicated `#[derive(Serialize)]` struct (never `serde_json::Value`, whose
    /// key order depends on a build flag), grant levels as integer **ranks**,
    /// and egress sorted.
    pub fn canonical_json(&self) -> String {
        #[derive(Serialize)]
        struct CanonArtifact<'a> {
            name: &'a str,
            version: &'a str,
            content_hash: &'a str,
        }
        #[derive(Serialize)]
        struct CanonRequires {
            filesystem: u8,
            network: u8,
            exec: u8,
        }
        #[derive(Serialize)]
        struct Canon<'a> {
            artifact: CanonArtifact<'a>,
            requires: CanonRequires,
            egress: Vec<String>,
        }
        let mut egress = self.egress.clone();
        egress.sort();
        let canon = Canon {
            artifact: CanonArtifact {
                name: &self.artifact.name,
                version: &self.artifact.version,
                content_hash: &self.artifact.content_hash,
            },
            requires: CanonRequires {
                filesystem: self.requires.filesystem.rank(),
                network: self.requires.network.rank(),
                exec: self.requires.exec.rank(),
            },
            egress,
        };
        serde_json::to_string(&canon).expect("contract canonical json is always serializable")
    }

    /// The exact bytes that get signed and verified: the domain separator, a
    /// null byte, then the canonical JSON.
    pub fn signing_payload(&self) -> Vec<u8> {
        let mut payload = Vec::with_capacity(CONTRACT_DOMAIN.len() + 1 + self.canonical_json().len());
        payload.extend_from_slice(CONTRACT_DOMAIN);
        payload.push(b'\0');
        payload.extend_from_slice(self.canonical_json().as_bytes());
        payload
    }

    /// Content address of the contract — SHA-256 over the domain separator and
    /// the canonical form (independent of who signed it). Matches
    /// `lex-os-capsule::CapabilityContract::content_id`.
    pub fn content_id(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(CONTRACT_DOMAIN);
        hasher.update(self.canonical_json().as_bytes());
        hex_lower(&hasher.finalize())
    }

    /// Sign this contract with `key`, producing a [`SignedContract`] the
    /// consumer verifies with the corresponding public key.
    pub fn sign(self, key: &lex_vcs::Keypair) -> SignedContract {
        let signature = key.sign_message(&self.signing_payload());
        SignedContract {
            signer: key.public_hex(),
            signature,
            contract: self,
        }
    }
}

impl SignedContract {
    /// Verify the signature binds *this* contract to the declared signer key.
    /// Returns the signer's hex public key on success; trusting that *key* is a
    /// separate decision (see the keyring check in `lex pkg verify`).
    pub fn verify(&self) -> Result<String, String> {
        lex_vcs::verify_message(
            &self.signer,
            &self.contract.signing_payload(),
            &self.signature,
        )
        .map_err(|e| format!("signature does not verify for signer {}: {e}", self.signer))?;
        Ok(self.signer.clone())
    }

    /// Check that `bytes` are the artifact this contract was signed over: their
    /// SHA-256 must equal `artifact.content_hash`. The signature only *promises*
    /// the declared hash; this closes the loop on the bytes you actually have.
    pub fn matches_artifact(&self, bytes: &[u8]) -> Result<(), String> {
        let actual = hash_artifact_bytes(bytes);
        if actual.eq_ignore_ascii_case(&self.contract.artifact.content_hash) {
            Ok(())
        } else {
            Err(format!(
                "artifact bytes do not match the contract: content_hash is {} but the supplied bytes hash to {actual}",
                self.contract.artifact.content_hash
            ))
        }
    }
}

/// SHA-256 of an artifact archive as lowercase hex — the value
/// [`ArtifactRef::content_hash`] holds. Matches
/// `lex-os-capsule::CapabilityContract::hash_artifact_bytes`.
pub fn hash_artifact_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_lower(&hasher.finalize())
}

/// Lowercase-hex of a byte slice (lex-cli has no `hex` crate; this mirrors
/// `lex-vcs::canonical::hash_bytes`'s encoding).
fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// A trusted-signer keyring: `{"trusted":[<hex pubkey>, …]}`. Same shape as
/// `lex-os-capsule::Keyring` and what `lex producer-trust keyring` emits, so a
/// keyring earned from track record gates `lex pkg verify` too.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Keyring {
    #[serde(default)]
    pub trusted: Vec<String>,
}

impl Keyring {
    /// Whether `signer` (hex public key) is in the trusted set (case-insensitive).
    pub fn trusts(&self, signer: &str) -> bool {
        self.trusted.iter().any(|k| k.eq_ignore_ascii_case(signer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lex_types::trust::{Grant, Level};

    fn sample() -> CapabilityContract {
        CapabilityContract {
            artifact: ArtifactRef {
                name: "pdf-extract".into(),
                version: "2.0.0".into(),
                content_hash: "ff".repeat(32),
            },
            requires: Grant {
                filesystem: Level::ReadOnly,
                network: Level::Allowlist,
                exec: Level::None,
            },
            // Intentionally unsorted: canonical form must sort it.
            egress: vec!["b.example".into(), "a.example".into()],
        }
    }

    #[test]
    fn canonical_form_is_pinned() {
        // Locks the `lex.os.capsule.contract.v1` wire bytes lex-os signs and
        // verifies over: grant levels as integer ranks, egress sorted, fixed
        // field order. If this assertion changes, every `lex-os capsule install`
        // of a lex-pkg-published contract silently breaks — so it must only
        // change in lock-step with lex-os-capsule's `canonical_form_is_pinned`.
        let expected = format!(
            "{{\"artifact\":{{\"name\":\"pdf-extract\",\"version\":\"2.0.0\",\"content_hash\":\"{}\"}},\"requires\":{{\"filesystem\":1,\"network\":2,\"exec\":0}},\"egress\":[\"a.example\",\"b.example\"]}}",
            "ff".repeat(32)
        );
        assert_eq!(sample().canonical_json(), expected);

        let payload = sample().signing_payload();
        assert!(payload.starts_with(CONTRACT_DOMAIN));
        assert_eq!(payload[CONTRACT_DOMAIN.len()], 0, "domain is NUL-separated");
        assert_eq!(&payload[CONTRACT_DOMAIN.len() + 1..], expected.as_bytes());

        let id = sample().content_id();
        assert_eq!(id.len(), 64);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sign_then_verify_roundtrips_and_detects_tamper() {
        let kp = lex_vcs::Keypair::from_seed(&[9u8; 32]);
        let signed = sample().sign(&kp);
        assert_eq!(signed.signer, kp.public_hex());
        assert_eq!(signed.verify().unwrap(), kp.public_hex());

        // Tampering any signed field (here the declared hash) breaks the bind.
        let mut bad = signed.clone();
        bad.contract.artifact.content_hash = "00".repeat(32);
        assert!(bad.verify().is_err(), "tampered contract must not verify");
    }

    #[test]
    fn matches_artifact_checks_the_actual_bytes() {
        let bytes = b"the real archive bytes";
        let mut c = sample();
        c.artifact.content_hash = hash_artifact_bytes(bytes);
        let signed = c.sign(&lex_vcs::Keypair::from_seed(&[3u8; 32]));
        assert!(signed.matches_artifact(bytes).is_ok());
        assert!(signed.matches_artifact(b"different bytes").is_err());
    }

    #[test]
    fn json_roundtrips_with_grant_as_enum_names() {
        // The on-disk SignedContract must use Grant enum names (not ranks) so
        // lex-os deserializes it into the identical lex_types::Grant.
        let signed = sample().sign(&lex_vcs::Keypair::from_seed(&[1u8; 32]));
        let json = serde_json::to_string(&signed).unwrap();
        assert!(json.contains("\"filesystem\":\"ReadOnly\""));
        assert!(json.contains("\"network\":\"Allowlist\""));
        let back: SignedContract = serde_json::from_str(&json).unwrap();
        assert_eq!(back, signed);
    }
}
