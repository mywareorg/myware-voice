//! Which tier this copy is entitled to.
//!
//! A licence is a short document with an Ed25519 signature, checked against a
//! public key baked in at build time. Nothing is fetched and nothing phones home:
//! the app works offline and keeps its mouth shut, and a licence server would be
//! the first thing to break both. The price of that is a check that is honest
//! rather than enforceable — the free tier is fully useful, and nobody is going
//! to be nagged for having a copy they did not pay for.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

/// The key licences are signed with, from `HIPPO_DICTATE_LICENCE_PUBKEY` at build
/// time.
///
/// Deliberately nothing in the repository: a default build has no key and
/// therefore grants nothing, so a forked or rebuilt binary cannot manufacture
/// licences. A release build sets the variable.
pub const PUBLIC_KEY: Option<&str> = option_env!("HIPPO_DICTATE_LICENCE_PUBKEY");

/// The line between the signed document and its signature.
const SEAM: &str = "---";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    #[default]
    Free,
    Pro,
}

impl Tier {
    pub fn arg(self) -> &'static str {
        match self {
            Tier::Free => "free",
            Tier::Pro => "pro",
        }
    }

    pub fn is_pro(self) -> bool {
        self == Tier::Pro
    }
}

impl core::fmt::Display for Tier {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.arg())
    }
}

/// The signed half of a licence file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    /// Who it was issued to. Printed, never checked.
    pub subject: String,
    pub tier: Tier,
    /// Unix seconds, for a human reading the file.
    #[serde(default)]
    pub issued: Option<u64>,
    /// Unix seconds. Absent means no expiry, which is what a perpetual licence is.
    #[serde(default)]
    pub expires: Option<u64>,
}

/// What is in force, and why it is not Pro when it is not.
#[derive(Debug, Clone)]
pub struct Entitlement {
    pub tier: Tier,
    pub subject: Option<String>,
    pub expires: Option<u64>,
    /// One sentence for `status` and `doctor`. `None` either because this is Pro
    /// or because there is simply no licence installed and nothing to report.
    pub note: Option<String>,
}

impl Entitlement {
    fn free() -> Self {
        Self {
            tier: Tier::Free,
            subject: None,
            expires: None,
            note: None,
        }
    }

    fn free_because(reason: impl Into<String>) -> Self {
        Self {
            tier: Tier::Free,
            subject: None,
            expires: None,
            note: Some(reason.into()),
        }
    }
}

/// The licence file, named outright by `HIPPO_DICTATE_LICENCE` in a test.
pub fn path() -> anyhow::Result<PathBuf> {
    if let Some(path) = crate::compat::var_os("LICENCE") {
        return Ok(PathBuf::from(path));
    }
    crate::dirs::licence_file()
}

/// Read and check the installed licence. Never fails: a problem is a free tier
/// and a sentence explaining it, because nothing about licensing is worth
/// refusing to run over.
pub fn load() -> Entitlement {
    let path = match path() {
        Ok(path) => path,
        Err(error) => return Entitlement::free_because(format!("{error:#}")),
    };
    match std::fs::read_to_string(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Entitlement::free(),
        Err(error) => {
            Entitlement::free_because(format!("could not read {}: {error}", path.display()))
        }
        Ok(text) => match verify(&text, PUBLIC_KEY, now()) {
            Ok(claims) => Entitlement {
                tier: claims.tier,
                subject: Some(claims.subject),
                expires: claims.expires,
                note: None,
            },
            Err(problem) => Entitlement::free_because(problem),
        },
    }
}

/// Check a licence document against a public key.
///
/// The key is an argument rather than the constant so the tests can sign with a
/// key of their own; production passes [`PUBLIC_KEY`], which is `None` unless the
/// build set it.
pub fn verify(text: &str, key: Option<&str>, now: u64) -> Result<Claims, String> {
    let (payload, signature) = split(text)?;
    let Some(key) = key else {
        return Err("this build has no licence key, so no licence can be verified".to_string());
    };
    let key = VerifyingKey::from_bytes(&key_bytes(key)?).map_err(|_| {
        "the built-in licence key is not a valid Ed25519 point; rebuild with the right one"
            .to_string()
    })?;
    let signature = Signature::from_bytes(&signature_bytes(signature)?);
    key.verify_strict(payload.as_bytes(), &signature)
        .map_err(|_| "the signature does not match this document".to_string())?;

    let claims: Claims =
        toml::from_str(payload).map_err(|error| format!("the licence does not parse: {error}"))?;
    if let Some(expires) = claims.expires {
        if now >= expires {
            return Err(format!(
                "the licence expired on {}",
                crate::log::stamp(expires)
            ));
        }
    }
    Ok(claims)
}

/// Sign a licence. Used by `hippo-dictate licence sign`, which is how licences are
/// issued; the key stays on the machine that issues them.
pub fn sign(claims: &Claims, key: &SigningKey) -> String {
    let payload = toml::to_string(claims).expect("a licence payload always serialises");
    let signature = key.sign(payload.as_bytes());
    format!("{payload}{SEAM}\n{}\n", hex::encode(signature.to_bytes()))
}

/// Split a document into the exact bytes that were signed and the signature.
///
/// The payload is taken verbatim rather than re-serialised, so the signature
/// covers what is in the file and a hand-edited field cannot mean something else
/// after a round trip through a parser.
fn split(text: &str) -> Result<(&str, &str), String> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.trim_end() == SEAM {
            return Ok((&text[..offset], &text[offset + line.len()..]));
        }
        offset += line.len();
    }
    Err(format!(
        "the licence has no `{SEAM}` line between the document and its signature"
    ))
}

fn key_bytes(hex_text: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(hex_text.trim())
        .map_err(|_| "the built-in licence key is not hex".to_string())?;
    bytes
        .try_into()
        .map_err(|_| "the built-in licence key is not 32 bytes".to_string())
}

fn signature_bytes(hex_text: &str) -> Result<[u8; 64], String> {
    let bytes = hex::decode(hex_text.trim()).map_err(|_| "the signature is not hex".to_string())?;
    bytes
        .try_into()
        .map_err(|_| "the signature is not 64 bytes".to_string())
}

/// Unix seconds, from the same clock the log stamps.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// A key pair, as the two hex strings an operator has to keep apart.
pub fn keygen() -> (String, String) {
    let key = SigningKey::generate(&mut rand_core::OsRng);
    (
        hex::encode(key.verifying_key().to_bytes()),
        hex::encode(key.to_bytes()),
    )
}

/// Read a signing key from a file holding 64 hex characters.
pub fn read_signing_key(path: &Path) -> anyhow::Result<SigningKey> {
    let text = std::fs::read_to_string(path)?;
    let bytes: [u8; 32] = hex::decode(text.trim())
        .map_err(|_| anyhow::anyhow!("{} is not a hex signing key", path.display()))?
        .try_into()
        .map_err(|_| anyhow::anyhow!("{} is not a 32-byte signing key", path.display()))?;
    Ok(SigningKey::from_bytes(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signing_key() -> SigningKey {
        // Fixed bytes: a test key, never a real one.
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn public_key(key: &SigningKey) -> Option<String> {
        Some(hex::encode(key.verifying_key().to_bytes()))
    }

    fn claims(expires: Option<u64>) -> Claims {
        Claims {
            subject: "someone@example.com".to_string(),
            tier: Tier::Pro,
            issued: Some(1_700_000_000),
            expires,
        }
    }

    #[test]
    fn a_signed_licence_grants_pro() {
        let key = signing_key();
        let text = sign(&claims(None), &key);
        let verified = verify(&text, public_key(&key).as_deref(), 1_800_000_000).expect("verify");
        assert_eq!(verified.tier, Tier::Pro);
        assert_eq!(verified.subject, "someone@example.com");
        assert_eq!(verified.expires, None);
    }

    #[test]
    fn editing_the_document_invalidates_it() {
        let key = signing_key();
        let text = sign(&claims(None), &key).replace("tier = \"pro\"", "tier = \"free\"");
        assert!(
            verify(&text, public_key(&key).as_deref(), 1_800_000_000).is_err(),
            "a hand-edited tier still verified"
        );
    }

    #[test]
    fn an_expired_licence_is_reported_rather_than_honoured() {
        let key = signing_key();
        let text = sign(&claims(Some(1_700_000_000)), &key);
        let error = verify(&text, public_key(&key).as_deref(), 1_700_000_001)
            .expect_err("an expired licence verified");
        assert!(error.contains("expired"), "{error}");
        // And the day before it was fine.
        assert!(verify(&text, public_key(&key).as_deref(), 1_699_999_999).is_ok());
    }

    #[test]
    fn a_build_without_a_key_grants_nothing() {
        let key = signing_key();
        let text = sign(&claims(None), &key);
        let error = verify(&text, None, 1_800_000_000).expect_err("a keyless build verified");
        assert!(error.contains("no licence key"), "{error}");
    }

    #[test]
    fn a_document_without_a_seam_is_not_a_licence() {
        assert!(verify("subject = \"x\"\n", None, 0).is_err());
    }

    #[test]
    fn the_payload_is_taken_verbatim() {
        let key = signing_key();
        let text = sign(&claims(None), &key);
        let (payload, signature) = split(&text).expect("split");
        // Everything before the seam and nothing else: that is what the signature
        // covers, so a parser cannot be talked into a different meaning.
        assert!(payload.starts_with("subject = "));
        assert!(payload.ends_with('\n'));
        assert_eq!(signature.trim().len(), 128);
    }
}
