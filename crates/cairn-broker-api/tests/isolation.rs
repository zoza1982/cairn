//! Enforces the secret-isolation invariant from RFC-0008 / LLD §9.6: the untrusted layers
//! (`cairn-ai`, `cairn-plugin`) must NOT have `cairn-vault` (or `cairn-secrets`) anywhere in their
//! **normal** dependency closure. They may reach the credential store only through the secret-free
//! `cairn-broker-api`. This turns the boundary from a convention into a CI-enforced guarantee — a
//! future careless `cairn-broker = { workspace = true }` on `cairn-ai` would fail here.

use cargo_metadata::{DependencyKind, MetadataCommand, Package};
use std::collections::{HashMap, HashSet};

/// Crates that must never appear in an untrusted crate's normal-dependency closure.
/// `cairn-broker` exposes `Broker::resolve -> SecretString` (the execution layer); `cairn-vault` and
/// `cairn-secrets` hold plaintext secret material. Forbidding all three states the contract directly
/// rather than relying on `cairn-broker` being caught only transitively via the vault.
const FORBIDDEN: &[&str] = &["cairn-broker", "cairn-vault", "cairn-secrets"];
/// The untrusted crates whose closures we audit.
const UNTRUSTED: &[&str] = &["cairn-ai", "cairn-plugin"];

#[test]
fn untrusted_crates_cannot_reach_the_vault() {
    // We walk manifest-declared dependencies (`pkg.dependencies` below), which include optional,
    // feature-gated, and platform-gated edges regardless of activation — a conservative superset
    // that is feature-agnostic by construction, so no feature resolution is needed here.
    let metadata = MetadataCommand::new().exec().expect("cargo metadata");

    let by_name: HashMap<&str, &Package> = metadata
        .packages
        .iter()
        .map(|p| (p.name.as_str(), p))
        .collect();

    for &root in UNTRUSTED {
        let Some(pkg) = by_name.get(root) else {
            panic!("workspace crate `{root}` not found in cargo metadata");
        };
        let closure = normal_dep_closure(pkg, &by_name);
        for &forbidden in FORBIDDEN {
            assert!(
                !closure.contains(forbidden),
                "SECRET-ISOLATION VIOLATION: `{root}` has `{forbidden}` in its normal dependency \
                 closure. The AI/plugin layers must reach credentials only via `cairn-broker-api` \
                 (RFC-0008). Path was introduced by a normal (non-dev) dependency edge."
            );
        }
    }
}

/// The set of crate names reachable from `root` following only **normal** (non-dev, non-build)
/// dependency edges.
fn normal_dep_closure<'a>(
    root: &'a Package,
    by_name: &HashMap<&'a str, &'a Package>,
) -> HashSet<String> {
    let mut seen = HashSet::new();
    let mut stack = vec![root];
    while let Some(pkg) = stack.pop() {
        for dep in &pkg.dependencies {
            // Fail closed: only dev/build deps are excluded (they don't ship in the runtime). Any
            // other kind — including a future/unknown one — counts toward the closure.
            if matches!(
                dep.kind,
                DependencyKind::Development | DependencyKind::Build
            ) {
                continue;
            }
            if seen.insert(dep.name.clone()) {
                if let Some(next) = by_name.get(dep.name.as_str()) {
                    stack.push(next);
                }
            }
        }
    }
    seen
}
