//! A string that must not end up in logs.
//!
//! Superuser and replication passwords flow through config rendering, SQL
//! generation, and error paths. Every one of those is somewhere a
//! `{:?}` could leak them into a log line, a `tracing` span, or an
//! `anyhow` chain that gets printed. Wrapping them in a type whose `Debug`
//! and `Display` refuse to print the value makes the leak a compile-time
//! non-event rather than a review-time catch.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A secret string. `Debug` and `Display` render `<redacted>`; the value
/// comes out only via [`Secret::expose`], which is deliberately
/// awkward to type so it stands out in review.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Yield the underlying value. Call this as late as possible — ideally
    /// at the point of use, never into a variable that then gets logged.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl From<String> for Secret {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for Secret {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_never_reveal_the_value() {
        let s = Secret::new("hunter2");
        assert!(!format!("{s:?}").contains("hunter2"));
        assert!(!format!("{s}").contains("hunter2"));
        assert!(!format!("{s:#?}").contains("hunter2"));
    }

    #[test]
    fn nesting_in_a_derived_debug_stays_redacted() {
        // The realistic leak is not `{:?}` on the secret itself — it is a
        // struct that happens to contain one being logged wholesale.
        // Read only through the derived Debug, which clippy does not count.
        #[allow(dead_code)]
        #[derive(Debug)]
        struct Config {
            user: String,
            password: Secret,
        }
        let c = Config {
            user: "postgres".into(),
            password: Secret::new("hunter2"),
        };
        let rendered = format!("{c:?}");
        assert!(rendered.contains("postgres"));
        assert!(
            !rendered.contains("hunter2"),
            "leaked via derived Debug: {rendered}"
        );
    }

    #[test]
    fn expose_returns_the_real_value() {
        assert_eq!(Secret::new("hunter2").expose(), "hunter2");
    }

    #[test]
    fn serializes_transparently_for_the_registry() {
        // The registry does persist these; redaction is about logs, not
        // storage. A `Secret` must round-trip as a plain JSON string.
        let s = Secret::new("hunter2");
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"hunter2\"");
        assert_eq!(serde_json::from_str::<Secret>(&json).unwrap(), s);
    }
}
