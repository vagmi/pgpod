//! A duration that always carries its unit.
//!
//! pg_doorman accepts both `"30s"` and a bare `30` in its configuration,
//! and the bare form means **milliseconds** — its own documentation warns
//! that `cache_ttl: 3600` caches for 3.6 seconds rather than an hour. That
//! is exactly the shape of mistake pgpod refuses elsewhere: it parses, it
//! applies, and it is wrong by a factor of a thousand in the direction
//! that looks like it worked.
//!
//! So a pgpod manifest never writes a bare number for a duration. This
//! type parses `500ms`, `30s`, `5m`, `1h`, `2d`, renders back in the
//! canonical form pg_doorman reads, and refuses anything unitless.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A duration written with an explicit unit, e.g. `60s`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HumanDuration {
    millis: u64,
}

impl HumanDuration {
    pub const fn from_millis(millis: u64) -> Self {
        Self { millis }
    }

    pub const fn from_secs(secs: u64) -> Self {
        Self {
            millis: secs * 1000,
        }
    }

    pub const fn as_millis(self) -> u64 {
        self.millis
    }

    pub const fn as_secs(self) -> u64 {
        self.millis / 1000
    }

    pub const fn as_std(self) -> std::time::Duration {
        std::time::Duration::from_millis(self.millis)
    }

    /// The form pg_doorman parses back to the same value.
    ///
    /// Always emitted with a unit, never as a bare integer, so a rendered
    /// config cannot be re-read as milliseconds.
    pub fn render(self) -> String {
        let m = self.millis;
        if m == 0 {
            return "0ms".to_string();
        }
        if m % 3_600_000 == 0 {
            format!("{}h", m / 3_600_000)
        } else if m % 60_000 == 0 {
            format!("{}m", m / 60_000)
        } else if m % 1_000 == 0 {
            format!("{}s", m / 1_000)
        } else {
            format!("{m}ms")
        }
    }
}

impl fmt::Display for HumanDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

impl FromStr for HumanDuration {
    type Err = ParseDurationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let raw = s.trim();
        if raw.is_empty() {
            return Err(ParseDurationError(s.to_string()));
        }
        let digits = raw
            .find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| ParseDurationError(s.to_string()))?;
        if digits == 0 {
            return Err(ParseDurationError(s.to_string()));
        }
        let (value, unit) = raw.split_at(digits);
        let value: u64 = value
            .parse()
            .map_err(|_| ParseDurationError(s.to_string()))?;
        let millis = match unit {
            "ms" => Some(value),
            "s" => value.checked_mul(1_000),
            "m" => value.checked_mul(60_000),
            "h" => value.checked_mul(3_600_000),
            "d" => value.checked_mul(86_400_000),
            _ => return Err(ParseDurationError(s.to_string())),
        }
        .ok_or_else(|| ParseDurationError(s.to_string()))?;
        Ok(Self { millis })
    }
}

impl Serialize for HumanDuration {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.render())
    }
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // A string only. Accepting a number here is the whole bug this
        // type exists to prevent, so the error has to say what to write
        // rather than "invalid type: integer".
        let raw = String::deserialize(d).map_err(|_| {
            serde::de::Error::custom(
                "a duration must be quoted and carry a unit, e.g. \"60s\" — \
                 a bare number is read as milliseconds",
            )
        })?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "{0:?} is not a duration: expected a number followed by ms, s, m, h or d \
     (e.g. \"60s\"). A bare number would be read as milliseconds."
)]
pub struct ParseDurationError(String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_unit() {
        for (input, millis) in [
            ("500ms", 500),
            ("30s", 30_000),
            ("5m", 300_000),
            ("1h", 3_600_000),
            ("2d", 172_800_000),
        ] {
            assert_eq!(
                input.parse::<HumanDuration>().expect(input).as_millis(),
                millis,
                "{input}"
            );
        }
    }

    #[test]
    fn a_bare_number_is_refused() {
        // The entire point. pg_doorman would take `60` as 60 milliseconds
        // and an operator who wrote it meant a minute — a switchover
        // budget a thousand times shorter than intended, discovered only
        // when clients start erroring mid-window.
        for bad in ["60", "0", "3600"] {
            assert!(
                bad.parse::<HumanDuration>().is_err(),
                "{bad} must be refused: it would be read as milliseconds"
            );
        }
    }

    #[test]
    fn nonsense_is_refused() {
        for bad in ["", "s", "60 s", "1 hour", "-5s", "5x", "1.5s"] {
            assert!(bad.parse::<HumanDuration>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn rendering_round_trips_and_always_carries_a_unit() {
        for input in ["500ms", "30s", "5m", "1h", "2d", "90s", "1500ms"] {
            let d: HumanDuration = input.parse().expect(input);
            let rendered = d.render();
            assert!(
                rendered.ends_with(|c: char| c.is_ascii_alphabetic()),
                "{rendered} has no unit"
            );
            assert_eq!(
                rendered.parse::<HumanDuration>().expect(&rendered),
                d,
                "{input} rendered as {rendered} and did not round trip"
            );
        }
    }

    #[test]
    fn deserializing_a_number_explains_itself() {
        // serde's own message would be "invalid type: integer `60`", which
        // does not tell an operator that the fix is a unit. serde_yaml is
        // loose enough to hand `60` over as the string "60", so the
        // message may come from either this type's deserializer or its
        // `FromStr` — both must name the milliseconds trap, because that
        // is the part that is silently wrong rather than merely invalid.
        let err = serde_yaml::from_str::<HumanDuration>("60").unwrap_err();
        assert!(
            err.to_string().contains("millisecond"),
            "unhelpful message: {err}"
        );
    }

    #[test]
    fn deserializes_from_a_quoted_string() {
        let d: HumanDuration = serde_yaml::from_str("\"90s\"").expect("parses");
        assert_eq!(d.as_secs(), 90);
    }
}
