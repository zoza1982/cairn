//! Kubeconfig-level Kubernetes discovery for RFC-0011 P3.
//!
//! Checks for Kubernetes connectivity sources **without** making any network connections or
//! running exec-credential plugins. The caller (the `KubeconfigProvider` in the `cairn` binary)
//! converts the results into `ConnectionDescriptor`s.
//!
//! ## What is checked
//!
//! - **Kubeconfig**: `Kubeconfig::read` (which follows `$KUBECONFIG` or `~/.kube/config`) is
//!   called in a `spawn_blocking` task. Any parse error means the kubeconfig is unusable.
//!
//! - **In-cluster**: `KUBERNETES_SERVICE_HOST` must be set and non-empty, AND the standard
//!   service-account token file must exist. No file contents are read; existence is sufficient.
//!
//! Parse errors are logged as warnings (at level `warn`; the kube error Display exposes only path
//! and YAML-syntax information — no credentials or secret material). A missing or unreadable
//! kubeconfig simply sets `has_kubeconfig = false`.

use kube::config::Kubeconfig;
use std::time::Duration;

/// Standard path for the pod's service-account token when running inside Kubernetes.
const IN_CLUSTER_TOKEN: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";

/// Timeout for the `spawn_blocking` kubeconfig read task.
///
/// Protects against a `~/.kube/config` file on a stalled network filesystem. The blocking OS
/// thread cannot be cancelled (it leaks until the FS eventually responds), but the async caller
/// unblocks after this deadline so the TUI can draw. 2 s is generous for a local filesystem and
/// still short enough to not noticeably delay startup on a slow networked home directory.
const KUBECONFIG_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Result of a kubeconfig/in-cluster discovery probe.
///
/// Both fields default to `false`; either may be `true` independently.
#[derive(Debug, Clone, Default)]
pub struct KubeconfigStatus {
    /// A kubeconfig was found and parsed without errors at
    /// `$KUBECONFIG` / `~/.kube/config`.
    pub has_kubeconfig: bool,
    /// `KUBERNETES_SERVICE_HOST` is set and the SA token file exists.
    /// When true, in-cluster credentials are available.
    pub has_incluster: bool,
}

/// Probe for Kubernetes connectivity sources without making any network calls.
///
/// Blocking FS/YAML I/O is offloaded to `spawn_blocking` and time-bounded by
/// `KUBECONFIG_READ_TIMEOUT`. If the kubeconfig read fails, times out, or the spawned task
/// panics, `has_kubeconfig` is set to `false`. Parse errors are logged at `warn` level — the
/// kube error Display exposes only path/syntax information, never credential material.
pub async fn probe_kubeconfig() -> KubeconfigStatus {
    // Kubeconfig: reading and parsing the YAML file is blocking CPU + I/O, potentially on a
    // network filesystem. Wrap in a timeout so a stalled FS can't block the TUI from drawing.
    // The blocking thread itself cannot be cancelled — it leaks until the FS responds — but
    // the async task returns immediately after the deadline.
    let kubeconfig_ok = match tokio::time::timeout(
        KUBECONFIG_READ_TIMEOUT,
        tokio::task::spawn_blocking(Kubeconfig::read),
    )
    .await
    {
        Ok(Ok(Ok(_cfg))) => true,
        Ok(Ok(Err(e))) => {
            // kube's error Display shows path + YAML parse info only — no secrets.
            tracing::warn!(error = %e, "kubeconfig unreadable; skipping k8s discovery");
            false
        }
        Ok(Err(_join_err)) => {
            // spawn_blocking task panicked — very unlikely, but recoverable.
            false
        }
        Err(_timeout) => {
            // The FS stalled; the blocking thread leaks but the UI unblocks.
            tracing::warn!(
                timeout_secs = KUBECONFIG_READ_TIMEOUT.as_secs(),
                "kubeconfig read timed out; skipping k8s discovery"
            );
            false
        }
    };

    // In-cluster env var: non-blocking env lookup.
    let incluster_env = std::env::var("KUBERNETES_SERVICE_HOST")
        .map(|v| !v.is_empty())
        .unwrap_or(false);

    // SA token existence: `Path::exists()` is a blocking syscall; offload it.
    let incluster_token = if incluster_env {
        tokio::task::spawn_blocking(|| std::path::Path::new(IN_CLUSTER_TOKEN).exists())
            .await
            .unwrap_or(false)
    } else {
        false
    };

    KubeconfigStatus {
        has_kubeconfig: kubeconfig_ok,
        has_incluster: incluster_env && incluster_token,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `probe_kubeconfig()` must not panic and must complete without network access.
    ///
    /// In CI / a developer machine without `~/.kube/config`, `has_kubeconfig` will be `false`;
    /// that is a valid result. The test only asserts the function returns (no panic, no deadlock).
    // `current_thread` ensures the env-var mutation below doesn't race with parallel test tasks.
    #[tokio::test(flavor = "current_thread")]
    async fn probe_kubeconfig_is_hermetic_and_non_panicking() {
        // May return false on both fields in an environment without k8s — that's correct.
        let _ = probe_kubeconfig().await;
    }

    /// When `KUBERNETES_SERVICE_HOST` is not set, `has_incluster` must be `false`.
    ///
    /// Uses `current_thread` flavor to avoid races: process-global env mutation is not
    /// safe across concurrently-running tokio test tasks in the default multi-thread runtime.
    #[tokio::test(flavor = "current_thread")]
    async fn has_incluster_false_when_env_not_set() {
        let was_set = std::env::var_os("KUBERNETES_SERVICE_HOST");
        // SAFETY (env): this test runs single-threaded so no concurrent env readers exist.
        std::env::remove_var("KUBERNETES_SERVICE_HOST");
        let status = probe_kubeconfig().await;
        // Restore the variable so other test modules are unaffected.
        if let Some(val) = was_set {
            std::env::set_var("KUBERNETES_SERVICE_HOST", val);
        }
        assert!(
            !status.has_incluster,
            "has_incluster must be false when KUBERNETES_SERVICE_HOST is unset"
        );
    }
}
