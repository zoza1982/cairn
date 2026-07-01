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
//! All errors are silently swallowed — a missing or unreadable kubeconfig simply sets
//! `has_kubeconfig = false` rather than propagating an error.

use kube::config::Kubeconfig;

/// Standard path for the pod's service-account token when running inside Kubernetes.
const IN_CLUSTER_TOKEN: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";

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
/// Blocking FS/YAML I/O is offloaded to `spawn_blocking`. If the kubeconfig read fails or the
/// spawned task panics, `has_kubeconfig` is set to `false`. The in-cluster check is a fast,
/// non-blocking env-var lookup plus a non-blocking `std::path::Path::exists()` delegated to a
/// second `spawn_blocking` call.
pub async fn probe_kubeconfig() -> KubeconfigStatus {
    // Kubeconfig: reading and parsing the YAML file is blocking CPU + I/O.
    let kubeconfig_ok = tokio::task::spawn_blocking(|| Kubeconfig::read().is_ok())
        .await
        .unwrap_or(false);

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
    #[tokio::test]
    async fn probe_kubeconfig_is_hermetic_and_non_panicking() {
        // May return false on both fields in an environment without k8s — that's correct.
        let _ = probe_kubeconfig().await;
    }

    /// When `KUBERNETES_SERVICE_HOST` is not set, `has_incluster` must be `false`.
    #[tokio::test]
    async fn has_incluster_false_when_env_not_set() {
        // Remove the env var for the duration of this test. Note: env mutation is process-global;
        // this is acceptable in a single-threaded tokio test.
        let was_set = std::env::var_os("KUBERNETES_SERVICE_HOST");
        std::env::remove_var("KUBERNETES_SERVICE_HOST");
        let status = probe_kubeconfig().await;
        // Restore.
        if let Some(val) = was_set {
            std::env::set_var("KUBERNETES_SERVICE_HOST", val);
        }
        assert!(
            !status.has_incluster,
            "has_incluster must be false when KUBERNETES_SERVICE_HOST is unset"
        );
    }
}
