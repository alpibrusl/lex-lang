//! Ed25519 signing for stage authorship (#227).
//!
//! Lex's auditability story depends on knowing *who* published what.
//! Content addressing answers "what did this stage say"; signing
//! answers "who said it." A consumer can refuse stages that aren't
//! signed (or that aren't signed by a trusted key) and the trail is
//! cryptographic, not policy-only.
//!
//! # What gets signed
//!
//! The bytes of the [`StageId`] string itself — UTF-8 of the
//! lowercase-hex SHA-256 that already content-addresses the stage.
//! Signing the `StageId` instead of the AST means:
//!
//! * Independent of canonical-AST format changes: a future migration
//!   to CBOR for the wire format doesn't invalidate signatures.
//! * Cheap to verify: 64 hex chars in, single Ed25519 verify out.
//! * Cross-tool reproducible: any tool that can compute the StageId
//!   can verify the signature without parsing the AST.
//!
//! This matches Noether's approach so cross-ecosystem verification
//! stays possible.
//!
//! # Hex encoding
//!
//! Public keys are 32 bytes → 64 hex chars. Signatures are 64 bytes
//! → 128 hex chars. Lowercase, no `0x` prefix, matching the
//! existing convention for [`crate::OpId`] / [`crate::StageId`].

use crate::attestation::Signature;
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey, SECRET_KEY_LENGTH};

/// Errors surfaced when keys, signatures, or verification go wrong.
#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    #[error("system entropy unavailable: {0}")]
    Entropy(String),
    #[error("public key must be {expected} hex chars (32 bytes), got {got}")]
    PublicKeyLength { expected: usize, got: usize },
    #[error("secret key must be {expected} hex chars (32 bytes), got {got}")]
    SecretKeyLength { expected: usize, got: usize },
    #[error("signature must be {expected} hex chars (64 bytes), got {got}")]
    SignatureLength { expected: usize, got: usize },
    #[error("invalid hex: {0}")]
    BadHex(String),
    #[error("invalid public key bytes")]
    BadPublicKey,
    #[error("signature did not verify against the given public key and stage id")]
    VerifyFailed,
}

/// An Ed25519 keypair held in memory. The secret key is `[u8; 32]`
/// — the 32-byte seed Ed25519 expands into a signing key. We never
/// keep the expanded scalar around; every sign call regenerates it
/// from the seed.
pub struct Keypair {
    inner: SigningKey,
}

impl std::fmt::Debug for Keypair {
    // The secret seed never appears in Debug output. A panic message
    // or stray `{:?}` should never be the path that leaks the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keypair")
            .field("public_key", &self.public_hex())
            .field("secret_key", &"<redacted>")
            .finish()
    }
}

impl Keypair {
    /// Generate a fresh keypair using the OS CSPRNG. Suitable for
    /// `lex keygen`; production callers wanting deterministic keys
    /// (test vectors, KAT) should use [`Keypair::from_secret_hex`].
    pub fn generate() -> Result<Self, SigningError> {
        let mut seed = [0u8; SECRET_KEY_LENGTH];
        getrandom::getrandom(&mut seed)
            .map_err(|e| SigningError::Entropy(e.to_string()))?;
        Ok(Self::from_seed(&seed))
    }

    /// Build a keypair from a 32-byte seed. Useful for tests where a
    /// known fixture key is needed; production callers should prefer
    /// [`Keypair::generate`] or the hex-deserialise path.
    pub fn from_seed(seed: &[u8; SECRET_KEY_LENGTH]) -> Self {
        Self { inner: SigningKey::from_bytes(seed) }
    }

    /// Parse a hex-encoded 32-byte secret key (the seed).
    pub fn from_secret_hex(hex_str: &str) -> Result<Self, SigningError> {
        const EXPECTED: usize = SECRET_KEY_LENGTH * 2;
        if hex_str.len() != EXPECTED {
            return Err(SigningError::SecretKeyLength {
                expected: EXPECTED, got: hex_str.len()
            });
        }
        let bytes = hex::decode(hex_str)
            .map_err(|e| SigningError::BadHex(e.to_string()))?;
        let arr: [u8; SECRET_KEY_LENGTH] = bytes.try_into()
            .expect("length-checked above");
        Ok(Self::from_seed(&arr))
    }

    /// Lowercase-hex of the 32-byte secret seed. Treat as material —
    /// this is what `lex keygen` prints to stdout exactly once.
    pub fn secret_hex(&self) -> String {
        hex::encode(self.inner.to_bytes())
    }

    /// Lowercase-hex of the 32-byte public key. Safe to publish.
    pub fn public_hex(&self) -> String {
        hex::encode(self.inner.verifying_key().to_bytes())
    }

    /// Sign the UTF-8 bytes of a `StageId` and return the wire-format
    /// [`Signature`] record. The same record verifies via
    /// [`verify_stage_id`].
    pub fn sign_stage_id(&self, stage_id: &str) -> Signature {
        let sig = self.inner.sign(stage_id.as_bytes());
        Signature {
            public_key: self.public_hex(),
            signature: hex::encode(sig.to_bytes()),
        }
    }
}

/// Verify that `signature` is a valid Ed25519 signature over the
/// UTF-8 bytes of `stage_id`, produced by the holder of the private
/// key matching `signature.public_key`.
///
/// Returns `Ok(())` on success. The signature record is otherwise
/// untrusted input: a callsite that wants to enforce *which* key
/// signed has to check `signature.public_key` against an allowlist
/// before or after this call.
pub fn verify_stage_id(stage_id: &str, signature: &Signature) -> Result<(), SigningError> {
    const PK_HEX_LEN: usize = 64;
    const SIG_HEX_LEN: usize = 128;
    if signature.public_key.len() != PK_HEX_LEN {
        return Err(SigningError::PublicKeyLength {
            expected: PK_HEX_LEN, got: signature.public_key.len(),
        });
    }
    if signature.signature.len() != SIG_HEX_LEN {
        return Err(SigningError::SignatureLength {
            expected: SIG_HEX_LEN, got: signature.signature.len(),
        });
    }
    let pk_bytes = hex::decode(&signature.public_key)
        .map_err(|e| SigningError::BadHex(e.to_string()))?;
    let sig_bytes = hex::decode(&signature.signature)
        .map_err(|e| SigningError::BadHex(e.to_string()))?;
    let pk_arr: [u8; 32] = pk_bytes.try_into().expect("length-checked");
    let sig_arr: [u8; 64] = sig_bytes.try_into().expect("length-checked");
    let pk = VerifyingKey::from_bytes(&pk_arr)
        .map_err(|_| SigningError::BadPublicKey)?;
    let sig = ed25519_dalek::Signature::from_bytes(&sig_arr);
    pk.verify(stage_id.as_bytes(), &sig)
        .map_err(|_| SigningError::VerifyFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Keypair {
        // Deterministic seed so test vectors are reproducible.
        Keypair::from_seed(&[7u8; 32])
    }

    #[test]
    fn generate_produces_distinct_keys() {
        let a = Keypair::generate().unwrap();
        let b = Keypair::generate().unwrap();
        assert_ne!(a.public_hex(), b.public_hex(),
            "two keygen calls must not produce identical keys");
        assert_ne!(a.secret_hex(), b.secret_hex());
    }

    #[test]
    fn public_and_secret_hex_lengths_are_canonical() {
        let kp = fixture();
        assert_eq!(kp.public_hex().len(), 64);
        assert_eq!(kp.secret_hex().len(), 64);
        assert!(kp.public_hex().chars().all(|c| c.is_ascii_hexdigit()));
        assert!(kp.secret_hex().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn sign_then_verify_round_trips() {
        let kp = fixture();
        let stage_id = "deadbeefcafef00d";
        let sig = kp.sign_stage_id(stage_id);
        assert_eq!(sig.public_key, kp.public_hex());
        assert_eq!(sig.signature.len(), 128);
        verify_stage_id(stage_id, &sig).expect("signature must verify");
    }

    #[test]
    fn signing_is_deterministic_for_same_input() {
        // Ed25519 is deterministic by RFC 8032: same key + same
        // message ⇒ same signature. The agent-attestation story
        // benefits from this — two harnesses signing the same
        // StageId with the same key produce byte-identical evidence.
        let kp = fixture();
        let s1 = kp.sign_stage_id("stage-abc");
        let s2 = kp.sign_stage_id("stage-abc");
        assert_eq!(s1, s2);
    }

    #[test]
    fn different_stage_ids_produce_different_signatures() {
        let kp = fixture();
        let a = kp.sign_stage_id("stage-A");
        let b = kp.sign_stage_id("stage-B");
        assert_ne!(a.signature, b.signature);
    }

    #[test]
    fn verification_rejects_tampered_stage_id() {
        let kp = fixture();
        let sig = kp.sign_stage_id("real-stage-id");
        let err = verify_stage_id("forged-stage-id", &sig).unwrap_err();
        assert!(matches!(err, SigningError::VerifyFailed),
            "tampered stage_id must fail verification, got {err:?}");
    }

    #[test]
    fn verification_rejects_wrong_public_key() {
        // The signature was made by `kp_a` but is presented under
        // `kp_b`'s public key — a classic substitution attempt.
        let kp_a = Keypair::from_seed(&[1u8; 32]);
        let kp_b = Keypair::from_seed(&[2u8; 32]);
        let mut sig = kp_a.sign_stage_id("stage-id");
        sig.public_key = kp_b.public_hex();
        let err = verify_stage_id("stage-id", &sig).unwrap_err();
        assert!(matches!(err, SigningError::VerifyFailed));
    }

    #[test]
    fn from_secret_hex_round_trips() {
        let original = Keypair::from_seed(&[42u8; 32]);
        let hex_secret = original.secret_hex();
        let parsed = Keypair::from_secret_hex(&hex_secret).unwrap();
        assert_eq!(original.public_hex(), parsed.public_hex());
        // And signatures are bit-identical given Ed25519 determinism.
        assert_eq!(
            original.sign_stage_id("x"),
            parsed.sign_stage_id("x"),
        );
    }

    #[test]
    fn from_secret_hex_rejects_wrong_length() {
        let err = Keypair::from_secret_hex("deadbeef").unwrap_err();
        assert!(matches!(err, SigningError::SecretKeyLength { .. }));
    }

    #[test]
    fn from_secret_hex_rejects_invalid_hex() {
        let bad = "z".repeat(64);
        let err = Keypair::from_secret_hex(&bad).unwrap_err();
        assert!(matches!(err, SigningError::BadHex(_)));
    }

    #[test]
    fn verify_rejects_malformed_signature_lengths() {
        let mut sig = fixture().sign_stage_id("x");
        sig.signature = "deadbeef".into();
        let err = verify_stage_id("x", &sig).unwrap_err();
        assert!(matches!(err, SigningError::SignatureLength { .. }));
    }

    #[test]
    fn verify_rejects_malformed_public_key_lengths() {
        let mut sig = fixture().sign_stage_id("x");
        sig.public_key = "deadbeef".into();
        let err = verify_stage_id("x", &sig).unwrap_err();
        assert!(matches!(err, SigningError::PublicKeyLength { .. }));
    }
}
