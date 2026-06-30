//! Live Kubernetes round-trip against a real cluster (kind or any reachable kubeconfig).
//!
//! **Env-guarded:** this test is a no-op unless `CAIRN_IT_K8S` is set, so the default
//! `cargo test` (and the hermetic CI in `ci.yml`) never touch a real API server. The dedicated
//! integration job spins up a `kind` cluster, sets `CAIRN_IT_K8S=1`, and runs this test with
//! `--features k8s`. See ADR-0006.
//!
//! The test:
//! 1. Lists kubeconfig contexts — asserts at least one exists.
//! 2. Picks the first context and lists namespaces — asserts `kube-system` or `default` appears.
//! 3. Lists pods in `kube-system` — asserts at least one pod exists (kube-system always has pods).
//! 4. Lists containers of the first pod — asserts at least one container is present.
//! 5. Verifies that `list_dir` inside a container returns `VfsError::Unsupported` (deferred M6-5b).
#![cfg(feature = "k8s")]

use cairn_backend_k8s::{KubeOps, KubeRsOps};
use cairn_vfs::VfsError;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn k8s_cluster_tree_round_trip() {
    if std::env::var("CAIRN_IT_K8S").is_err() {
        eprintln!("CAIRN_IT_K8S unset — skipping live Kubernetes integration test");
        return;
    }

    let ops = KubeRsOps::new();

    // 1. List contexts — must be non-empty.
    let contexts = ops
        .list_contexts()
        .await
        .expect("list_contexts should succeed with a valid kubeconfig");
    assert!(
        !contexts.is_empty(),
        "expected at least one kubeconfig context, got none"
    );
    eprintln!(
        "contexts: {:?}",
        contexts.iter().map(|c| &c.name).collect::<Vec<_>>()
    );

    // Prefer a kind context (its name/cluster starts with `kind-`) so a runner with residual
    // kubeconfig entries from a previous job can't silently target the wrong cluster; fall back to
    // the first context otherwise (e.g. when pointed at a non-kind cluster).
    let ctx = &contexts
        .iter()
        .find(|c| c.name.contains("kind"))
        .unwrap_or(&contexts[0])
        .name;

    // 2. List namespaces — must contain kube-system or default.
    let namespaces = ops
        .list_namespaces(ctx)
        .await
        .expect("list_namespaces should succeed");
    assert!(
        !namespaces.is_empty(),
        "expected at least one namespace in context '{ctx}', got none"
    );
    let has_known = namespaces
        .iter()
        .any(|ns| ns == "kube-system" || ns == "default");
    assert!(
        has_known,
        "expected 'kube-system' or 'default' in namespaces, got: {namespaces:?}"
    );
    eprintln!("namespaces in '{ctx}': {namespaces:?}");

    // 3. List pods in kube-system — must be non-empty; kube-system always has system pods.
    let ns = "kube-system";
    let pods = ops
        .list_pods(ctx, ns)
        .await
        .expect("list_pods in kube-system should succeed");
    assert!(
        !pods.is_empty(),
        "expected at least one pod in '{ns}', got none (is the cluster healthy?)"
    );
    eprintln!(
        "pods in '{ns}': {:?}",
        pods.iter().map(|p| &p.name).collect::<Vec<_>>()
    );

    // 4. List containers of the first pod — must be non-empty.
    let pod_name = &pods[0].name;
    let containers = ops
        .list_containers(ctx, ns, pod_name)
        .await
        .expect("list_containers should succeed for a running pod");
    assert!(
        !containers.is_empty(),
        "expected at least one container in pod '{pod_name}', got none"
    );
    eprintln!(
        "containers in '{pod_name}': {:?}",
        containers.iter().map(|c| &c.name).collect::<Vec<_>>()
    );

    // 5. In-container fs is deferred (M6-5b): list_dir lists EMPTY (navigable, per the trait
    //    contract + caps_at), while read of a path stays Unsupported.
    let container_name = &containers[0].name;
    let list_dir_result = ops.list_dir(ctx, ns, pod_name, container_name, "/").await;
    assert!(
        matches!(&list_dir_result, Ok(v) if v.is_empty()),
        "deferred list_dir must return an empty listing, got: {list_dir_result:?}"
    );
    let read_result = ops
        .read(ctx, ns, pod_name, container_name, "/etc/hostname")
        .await;
    assert!(
        matches!(read_result, Err(VfsError::Unsupported(_))),
        "deferred read must be Unsupported until M6-5b, got: {read_result:?}"
    );
}
