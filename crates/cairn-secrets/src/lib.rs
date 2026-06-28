//! Secret-handling primitives shared across Cairn.
//!
//! Re-exports the zeroizing secret types from [`secrecy`] (which do not implement `Debug`/`Display`/
//! `Serialize` and wipe on drop) and provides a [`redact`] helper that scrubs common secret patterns
//! from strings before they are logged. See `docs/LLD.md` §9.5.

pub use secrecy::{ExposeSecret, SecretBox, SecretString};
pub use zeroize::Zeroizing;

use regex::Regex;
use std::sync::OnceLock;

/// The placeholder substituted for redacted spans.
pub const REDACTED: &str = "[REDACTED]";

fn patterns() -> &'static [Regex] {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            // AWS access key id.
            r"AKIA[0-9A-Z]{16}",
            // Bearer tokens.
            r"(?i)bearer\s+[A-Za-z0-9._\-]+",
            // Common signed-URL / SAS signature parameters.
            r"(?i)[?&](X-Amz-Signature|X-Goog-Signature|sig)=[^&\s]+",
            // PEM private-key blocks.
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
            // JWT-ish triple-segment base64url tokens.
            r"eyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+",
        ]
        .into_iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect()
    })
}

/// Scrub known secret patterns (AWS keys, bearer tokens, signed-URL signatures, PEM blocks, JWTs)
/// from a string, replacing each match with [`REDACTED`].
///
/// This is a best-effort defense for log/diagnostic output, not a substitute for keeping secrets in
/// the typed [`SecretString`]/[`SecretBox`] containers in the first place.
#[must_use]
pub fn redact(input: &str) -> String {
    let mut out = input.to_owned();
    for re in patterns() {
        out = re.replace_all(&out, REDACTED).into_owned();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_aws_key() {
        let s = "id=AKIAIOSFODNN7EXAMPLE rest";
        let r = redact(s);
        assert!(!r.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(r.contains(REDACTED));
        assert!(r.contains("rest"));
    }

    #[test]
    fn redacts_bearer_token() {
        let r = redact("Authorization: Bearer abc.def-ghi");
        assert!(!r.contains("abc.def-ghi"));
    }

    #[test]
    fn redacts_signed_url_signature() {
        let r = redact("https://x/y?X-Amz-Signature=deadbeef&z=1");
        assert!(!r.contains("deadbeef"));
        assert!(r.contains("z=1"));
    }

    #[test]
    fn leaves_innocuous_text_untouched() {
        let s = "just a normal path /etc/hosts";
        assert_eq!(redact(s), s);
    }

    #[test]
    fn secret_string_does_not_leak_in_debug() {
        let s = SecretString::from("hunter2".to_owned());
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("hunter2"));
        assert_eq!(s.expose_secret(), "hunter2");
    }
}
