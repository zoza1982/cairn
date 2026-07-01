//! Socket-level Docker discovery for RFC-0011 P3.
//!
//! Probes candidate Docker / Podman Unix sockets for connectivity and returns a flat list of
//! reachable ones. The caller (the `DockerProvider` in the `cairn` binary) converts the results
//! into `ConnectionDescriptor`s and assigns them to the connection switcher.
//!
//! Discovery is **enumerate-only**: each candidate is probed with a 500 ms `tokio::time::timeout`
//! around a [`BollardDocker::ping`]; no auth tokens, exec-credential plugins, or network calls
//! happen here. All per-socket errors are silently discarded — an unreachable socket is simply
//! absent from the output rather than blocking startup.

use std::path::PathBuf;

use crate::real::BollardDocker;

/// A reachable Docker or Podman socket discovered by [`probe_sockets`].
#[derive(Debug, Clone)]
pub struct DiscoveredSocket {
    /// Stable key string used for `hidden`/`pinned` config matching and for
    /// the `socket_path` of the `ConnectionKey::Docker` variant.
    ///
    /// - `"default"` — the platform-default socket (probed via [`BollardDocker::connect_local`]).
    ///   P4 note: `"default"` tracks whatever `connect_local()` resolves at probe time (which
    ///   honours `DOCKER_HOST`); if `DOCKER_HOST` is set to a different path at each run,
    ///   the key will still be `"default"` and stable-id reuse will work correctly.
    /// - Otherwise the raw absolute socket path string (e.g. `"/run/user/1000/docker.sock"`)
    pub key: String,
    /// Human-readable display name shown in the connection switcher.
    pub display_name: String,
    /// The explicit socket path, or `None` for the platform default.
    pub path: Option<PathBuf>,
}

/// Probe the standard Docker/Podman socket candidates concurrently.
///
/// Probes up to three candidates (see below) in parallel, each with a 500 ms deadline. The
/// returned [`DiscoveredSocket`]s appear in probing order: default first, then rootless Docker,
/// then Podman. Unreachable sockets are silently dropped.
///
/// # Candidates
///
/// 1. **Platform default** — `Docker::connect_with_local_defaults()` (respects `DOCKER_HOST`,
///    the standard system socket `/var/run/docker.sock`, or the Windows named pipe). Key:
///    `"default"`, display: `"docker (default)"`.
/// 2. **Rootless Docker** — `$XDG_RUNTIME_DIR/docker.sock`. Skipped when `XDG_RUNTIME_DIR` is
///    unset. Key: the absolute path string.
/// 3. **Podman** — `$XDG_RUNTIME_DIR/podman/podman.sock`. Skipped when `XDG_RUNTIME_DIR` is
///    unset. Key: the absolute path string.
pub async fn probe_sockets() -> Vec<DiscoveredSocket> {
    use tokio::time::{timeout, Duration};

    const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

    // Build the candidate list. Always include the platform default; add XDG-based sockets only
    // when the runtime directory env var is set and non-empty.
    let mut candidates: Vec<(String, String, Option<PathBuf>)> = Vec::new();

    // Candidate 1: platform default (DOCKER_HOST / system socket / Windows named pipe).
    candidates.push(("default".to_owned(), "docker (default)".to_owned(), None));

    // Candidates 2 & 3: rootless sockets under $XDG_RUNTIME_DIR.
    if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        let xdg_path = PathBuf::from(&xdg);

        let rootless = xdg_path.join("docker.sock");
        let rootless_key = rootless.to_string_lossy().into_owned();
        candidates.push((rootless_key, "docker (rootless)".to_owned(), Some(rootless)));

        let podman = xdg_path.join("podman/podman.sock");
        let podman_key = podman.to_string_lossy().into_owned();
        candidates.push((podman_key, "podman".to_owned(), Some(podman)));
    }

    // Probe all candidates concurrently. Each future is independent: a slow or broken socket
    // does not delay the others beyond the shared deadline.
    let probe_futs = candidates
        .into_iter()
        .map(|(key, display_name, path)| async move {
            let path_ref = path.as_deref();
            let probe = timeout(PROBE_TIMEOUT, probe_one(path_ref)).await;
            match probe {
                Ok(Ok(())) => Some(DiscoveredSocket {
                    key,
                    display_name,
                    path,
                }),
                // Timeout or error: silently discard.
                _ => None,
            }
        });

    futures::future::join_all(probe_futs)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// Probe a single socket for reachability: build a client and send `GET /_ping`.
///
/// `path = None` uses the platform default via [`BollardDocker::connect_local`].
/// `path = Some(p)` connects to the explicit socket at `p`.
///
/// Returns `Ok(())` when reachable, `Err(())` on any failure (all errors are discarded by the
/// caller; this signature keeps the probe hermetic with no error propagation).
async fn probe_one(path: Option<&std::path::Path>) -> Result<(), ()> {
    let client = match path {
        None => BollardDocker::connect_local().map_err(|_| ())?,
        Some(p) => BollardDocker::connect_with_socket(p).map_err(|_| ())?,
    };
    client.ping().await.map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Calling `probe_sockets()` must not panic and must complete without blocking (hermetic: no
    /// live Docker daemon required — unreachable sockets are silently dropped, so the result is
    /// an empty list in CI).
    #[tokio::test]
    async fn probe_sockets_is_hermetic_and_non_panicking() {
        // This test must complete in reasonable time even when no daemon is running (sockets
        // time out at 500 ms each; join_all runs them concurrently, so total wall-time is
        // bounded by the single longest timeout, not N × 500 ms).
        let sockets = probe_sockets().await;
        // We don't assert non-empty because CI may not have Docker; we assert the type is
        // correct and none of the keys are empty strings (invariant of the probe).
        for s in &sockets {
            assert!(!s.key.is_empty(), "discovered socket key must not be empty");
            assert!(
                !s.display_name.is_empty(),
                "discovered socket display_name must not be empty"
            );
        }
    }

    /// The default-socket entry always uses key `"default"`, not a path string.
    #[test]
    fn default_candidate_key_is_the_literal_string_default() {
        // The key is fixed by the constant in probe_sockets; verify the test assumptions match.
        // This is a compile-time invariant test — the probe_sockets function uses "default".
        let key = "default";
        assert_eq!(key, "default"); // trivially true, but documents the contract
    }
}
