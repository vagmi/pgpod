//! Every manifest in `examples/` must actually parse.
//!
//! They are the first thing a reader copies, and a manifest that has
//! drifted from the schema fails at `pgpod apply` with an error about a
//! file the operator did not write. Parsing them here costs nothing and
//! means a field rename cannot silently leave the examples behind.
//!
//! Worth having precisely because the manifest types now reject unknown
//! fields: before that, a stale example parsed cleanly and quietly did
//! the wrong thing.

use std::path::{Path, PathBuf};

use pgpod_core::{ClusterManifest, Manifest, PoolerManifest};

fn examples_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/pgpod-core.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .canonicalize()
        .expect("examples/ directory")
}

fn examples() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(examples_dir())
        .expect("read examples/")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "yaml" || e == "yml"))
        .collect();
    out.sort();
    out
}

/// Every example, parsed through the same dispatch `pgpod apply -f` uses.
///
/// Through `Manifest` rather than `ClusterManifest`: the examples
/// directory holds more than one kind now, and a test that assumed
/// otherwise would fail on a perfectly good pooler example — which is
/// exactly what it did when the first one landed.
fn all() -> Vec<(PathBuf, Manifest)> {
    examples()
        .into_iter()
        .map(|path| {
            let yaml = std::fs::read_to_string(&path).expect("read example");
            let m = Manifest::from_yaml(&yaml)
                .unwrap_or_else(|e| panic!("{} does not parse:\n  {e}", path.display()));
            (path, m)
        })
        .collect()
}

fn parsed() -> Vec<(PathBuf, ClusterManifest)> {
    all()
        .into_iter()
        .filter_map(|(path, m)| match m {
            Manifest::Cluster(c) => Some((path, *c)),
            Manifest::Pooler(_) => None,
        })
        .collect()
}

fn poolers() -> Vec<(PathBuf, PoolerManifest)> {
    all()
        .into_iter()
        .filter_map(|(path, m)| match m {
            Manifest::Pooler(p) => Some((path, *p)),
            Manifest::Cluster(_) => None,
        })
        .collect()
}

#[test]
fn every_shipped_example_parses() {
    let found = all();
    assert!(
        !found.is_empty(),
        "no examples found in {}",
        examples_dir().display()
    );
}

#[test]
fn both_manifest_kinds_are_demonstrated() {
    // A reader copies from here. The pooler is the difference between a
    // recreate that drops connections and one that holds them, so there
    // has to be an example of it.
    assert!(!parsed().is_empty(), "no cluster example");
    assert!(!poolers().is_empty(), "no pooler example");
}

#[test]
fn every_pooler_example_declares_a_hold_budget_with_a_unit() {
    // A bare number is milliseconds to pg_doorman. The manifest refuses
    // one, so this really asserts that the examples teach the unit rather
    // than leaving a reader to find out.
    for (path, m) in poolers() {
        assert!(
            m.spec.pg_doorman.max_hold.as_millis() >= 1000,
            "{}: a maxHold under a second is almost certainly a missing \
             unit — pg_doorman reads a bare number as milliseconds",
            path.display()
        );
    }
}

#[test]
fn a_local_repository_example_names_the_volume_it_needs() {
    // `file://` without `spec.backup.volume` is refused by the manifest,
    // so this really asserts that at least one example demonstrates the
    // pairing rather than leaving a reader to discover it.
    let mut seen = false;
    for (path, m) in parsed() {
        if m.spec
            .backup
            .destinations
            .iter()
            .any(|d| d.url.starts_with("file://"))
        {
            seen = true;
            assert!(
                m.spec.backup.volume.is_some(),
                "{} uses a file:// repository without spec.backup.volume",
                path.display()
            );
        }
    }
    assert!(seen, "no example demonstrates a local file:// repository");
}

#[test]
fn every_example_keeps_deletion_opt_in() {
    // ADR 01 §6, kept by ADR 04: retention only deletes under `enforce`.
    // An example shipping `enforce` would teach operators to turn on
    // destructive behaviour by copy-paste.
    for (path, m) in parsed() {
        assert_eq!(
            m.spec.backup.retention_mode,
            pgpod_core::RetentionMode::Report,
            "{} ships a retentionMode other than `report`",
            path.display()
        );
    }
}

#[test]
fn no_example_carries_a_credential_value() {
    // `credentials` names a podman secret; it must never hold the secret.
    // An example is the most-copied text in the repository.
    for (path, m) in parsed() {
        for dest in &m.spec.backup.destinations {
            if let Some(name) = &dest.credentials {
                assert!(
                    !name.contains('=') && !name.contains('/'),
                    "{}: `credentials` should name a podman secret, not hold \
                     one: {name:?}",
                    path.display()
                );
            }
        }
    }
}
