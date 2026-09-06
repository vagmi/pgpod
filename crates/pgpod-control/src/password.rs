//! Generating the passwords pgpod manages.

use pgpod_core::Secret;
use rand::Rng;

/// Alphanumeric only — no punctuation.
///
/// These passwords travel through `primary_conninfo`, a `.pgpass` file,
/// and SQL literals. Every one of those has its own quoting and escaping
/// rules, and pgpod handles them correctly, but a generated password is
/// the one input we fully control. Removing the characters that need
/// escaping costs a little entropy per character and removes a whole class
/// of failure that would surface as an authentication error nobody could
/// reproduce.
const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

/// 32 characters of the alphabet above is ~190 bits — far past anything
/// that matters for a credential never typed by a human.
const LENGTH: usize = 32;

pub fn generate() -> Secret {
    let mut rng = rand::rng();
    let s: String = (0..LENGTH)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect();
    Secret::new(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_are_long_and_alphanumeric() {
        let p = generate();
        let v = p.expose();
        assert_eq!(v.len(), LENGTH);
        assert!(
            v.chars().all(|c| c.is_ascii_alphanumeric()),
            "a character needing SQL or conninfo escaping got in: {v:?}"
        );
    }

    #[test]
    fn passwords_differ_between_calls() {
        assert_ne!(generate().expose(), generate().expose());
    }

    #[test]
    fn generated_passwords_never_print() {
        let p = generate();
        assert!(!format!("{p:?}").contains(p.expose()));
    }
}
