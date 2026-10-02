use std::fmt;

use serde::Deserialize;

pub const REDACTED: &str = "[REDACTED]";

/// Credentials must be explicitly exposed for use, never through formatting.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

/// Replace secrets in one pass so overlapping values and the replacement text
/// cannot interfere with one another. Empty credentials must not match everything.
pub fn redact(text: &str, secrets: &[&Secret]) -> String {
    let mut redacted = String::with_capacity(text.len());
    let mut remaining = text;
    while !remaining.is_empty() {
        let matched = secrets
            .iter()
            .map(|secret| secret.expose())
            .filter(|secret| !secret.is_empty() && remaining.starts_with(secret))
            .max_by_key(|secret| secret.len());
        if let Some(secret) = matched {
            redacted.push_str(REDACTED);
            remaining = &remaining[secret.len()..];
        } else {
            let character = remaining.chars().next().unwrap();
            redacted.push(character);
            remaining = &remaining[character.len_utf8()..];
        }
    }
    redacted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_never_exposes_credentials() {
        let secret = Secret::from("test-password".to_string());
        assert_eq!(format!("{secret:?}"), REDACTED);
        assert_eq!(format!("{secret}"), REDACTED);
        assert_eq!(secret.expose(), "test-password");
    }

    #[test]
    fn deserialization_preserves_the_credential() {
        let secret: Secret = serde_json::from_str("\"test-password\"").unwrap();
        assert_eq!(secret.expose(), "test-password");
    }

    #[test]
    fn redaction_handles_repeated_overlapping_and_empty_secrets() {
        let secrets = ["", "abc", "abcdef", "REDACTED"].map(|s| Secret::from(s.to_string()));
        let refs = secrets.iter().collect::<Vec<_>>();
        assert_eq!(
            redact("Error: abc / abcdef / abc / REDACTED — retry", &refs),
            "Error: [REDACTED] / [REDACTED] / [REDACTED] / [REDACTED] — retry"
        );
        assert_eq!(redact("", &refs), "");
        assert_eq!(redact("unchanged", &refs), "unchanged");
    }
}
