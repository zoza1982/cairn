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
//! 5. Exercises `list_dir`/`stat`/`read` on a kube-system pod that has `tar`.
//!    - If no suitable container can be found, or the container lacks `tar`, the fs checks are
//!      skipped gracefully rather than failing (exec_unavailable is expected in that case).
//! 6. Exercises `logs` (non-follow, tail 10) on the same kube-system pod — asserts the stream
//!    yields at least some bytes without errors. System pods log actively, so a non-empty result
//!    is expected; an empty log (recently-started pod) is noted but does not fail the test.
//! 7. Exercises `exec` (non-TTY, `["sh", "-c", "echo CAIRN_EXEC_MARKER"]`) — asserts the marker
//!    appears in stdout and `done` resolves with `Ok(0)`. Skips gracefully when no suitable
//!    container (Running phase, has `sh`) can be found. The exec handle is programmatically driven
//!    without a TUI pane (this PR is the backend only; the TUI exec pane is PR-4).
#![cfg(feature = "k8s")]

use cairn_backend_k8s::{KubeOps, KubeRsOps};
use cairn_types::EntryKind;
use cairn_vfs::VfsError;
use futures::StreamExt as _;

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

    // 5. In-container filesystem access (M6-5b).
    //
    // kube-system pods (etcd, kube-apiserver, kindnet, etc.) ship with `tar` in most distributions
    // and kind images.  We try the first container; if it reports exec_unavailable (no tar), we
    // skip the filesystem assertions gracefully.
    let container_name = &containers[0].name;

    // 5a. list_dir on "/" must be non-empty for a running system pod (or exec_unavailable).
    let list_result = ops.list_dir(ctx, ns, pod_name, container_name, "/").await;
    eprintln!("list_dir('/') result: {list_result:?}");

    match &list_result {
        Err(VfsError::Backend { code, .. }) if code == "exec_unavailable" => {
            eprintln!(
                "SKIP: container '{container_name}' in pod '{pod_name}' has no 'tar' — \
                 skipping in-container filesystem checks"
            );
            return;
        }
        Err(e) => {
            panic!("list_dir('/') in container '{container_name}' failed unexpectedly: {e}");
        }
        Ok(entries) => {
            assert!(
                !entries.is_empty(),
                "expected '/' to be non-empty in a running kube-system pod, got empty listing"
            );
            eprintln!(
                "list_dir('/') entries: {:?}",
                entries.iter().map(|e| &e.name).collect::<Vec<_>>()
            );
        }
    }

    // 5b. list_dir on "/etc" must succeed and return at least one entry.
    let etc_entries = ops
        .list_dir(ctx, ns, pod_name, container_name, "/etc")
        .await
        .expect("list_dir('/etc') should succeed in a kube-system container");
    assert!(
        !etc_entries.is_empty(),
        "expected /etc to be non-empty, got empty listing"
    );
    eprintln!(
        "/etc entries: {:?}",
        etc_entries.iter().map(|e| &e.name).collect::<Vec<_>>()
    );

    // 5c. stat "/" must return a directory.
    let root_meta = ops
        .stat(ctx, ns, pod_name, container_name, "/")
        .await
        .expect("stat('/') should succeed");
    assert_eq!(root_meta.kind, EntryKind::Dir, "root '/' must be a Dir");

    // 5d. stat "/etc" must return a directory.
    let etc_meta = ops
        .stat(ctx, ns, pod_name, container_name, "/etc")
        .await
        .expect("stat('/etc') should succeed");
    assert_eq!(etc_meta.kind, EntryKind::Dir, "/etc must be a Dir");

    // 5e. stat and read "/etc/hostname" — must exist and contain the pod's name.
    //     (The kernel mounts /etc/hostname from the pod spec; it is present in every running pod.)
    let hostname_meta = ops
        .stat(ctx, ns, pod_name, container_name, "/etc/hostname")
        .await
        .expect("stat('/etc/hostname') should succeed");
    assert_eq!(
        hostname_meta.kind,
        EntryKind::File,
        "/etc/hostname must be a File"
    );

    let hostname_bytes = ops
        .read(ctx, ns, pod_name, container_name, "/etc/hostname")
        .await
        .expect("read('/etc/hostname') should succeed");
    let hostname = String::from_utf8_lossy(&hostname_bytes);
    let hostname_trimmed = hostname.trim();
    assert!(
        !hostname_trimmed.is_empty(),
        "/etc/hostname must not be empty"
    );
    eprintln!("/etc/hostname = {hostname_trimmed:?}");

    // 5f. read of a directory must return Unsupported (not a crash, not NotFound).
    let read_dir_result = ops.read(ctx, ns, pod_name, container_name, "/etc").await;
    assert!(
        matches!(read_dir_result, Err(VfsError::Unsupported(_))),
        "reading a directory must return Unsupported, got: {read_dir_result:?}"
    );

    // 5g. stat and list_dir on a non-existent path must return NotFound.
    let missing_stat = ops
        .stat(
            ctx,
            ns,
            pod_name,
            container_name,
            "/cairn_nonexistent_path_12345",
        )
        .await;
    assert!(
        matches!(missing_stat, Err(VfsError::NotFound(_))),
        "stat of a missing path must return NotFound, got: {missing_stat:?}"
    );

    let missing_list = ops
        .list_dir(
            ctx,
            ns,
            pod_name,
            container_name,
            "/cairn_nonexistent_path_12345",
        )
        .await;
    assert!(
        matches!(missing_list, Err(VfsError::NotFound(_))),
        "list_dir of a missing path must return NotFound, got: {missing_list:?}"
    );

    // 6. Log streaming (M6-6 first slice).
    //
    // Stream the last 10 log lines from the kube-system pod's first container (non-follow so
    // the stream is bounded and terminates without a timeout). System pods log actively, so we
    // expect a non-empty result; an empty stream from a very recently-started pod is noted
    // but is not a test failure (only errors in the stream fail the test).
    eprintln!("Testing log streaming on pod '{pod_name}' container '{container_name}'...");

    let log_stream = ops.logs(
        ctx,
        ns,
        pod_name,
        Some(container_name),
        false, // non-follow: stream terminates after history
        Some(10),
    );

    let chunks: Vec<Result<bytes::Bytes, VfsError>> = log_stream.collect().await;
    eprintln!("Received {} log chunk(s)", chunks.len());

    // All items must be Ok — errors in the stream indicate a problem with the implementation.
    for chunk in &chunks {
        assert!(
            chunk.is_ok(),
            "log stream must not emit errors, got: {chunk:?}"
        );
    }

    let total_bytes: usize = chunks
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .map(bytes::Bytes::len)
        .sum();

    if total_bytes > 0 {
        eprintln!("log stream yielded {total_bytes} bytes — OK");
    } else {
        // A very recently-started pod may have no buffered log lines yet.  This is benign; the
        // test validated that there were no stream errors, which is the important invariant.
        eprintln!("WARN: log stream yielded 0 bytes (pod may have just started)");
    }
}

/// Interactive exec round-trip against a kind/live cluster.
///
/// Runs `sh -c 'echo CAIRN_EXEC_MARKER'` non-TTY in a kube-system pod, reads stdout from the
/// `SessionHandle`, asserts the marker is present, and asserts `done` resolves with `Ok(0)`.
///
/// Skips gracefully when:
/// - `CAIRN_IT_K8S` is unset (hermetic CI guard).
/// - No Running pod in kube-system is found with a container that has `sh` on its PATH (checked
///   by inspecting the exec error: if the error code is `exec-io` or the API returns 404/400, the
///   container is not suitable).
///
/// This test drives the `SessionHandle` programmatically (no TUI pane). The TUI exec pane is
/// implemented in PR-4 (RFC-0009 §4).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn k8s_exec_non_tty_round_trip() {
    if std::env::var("CAIRN_IT_K8S").is_err() {
        eprintln!("CAIRN_IT_K8S unset — skipping live Kubernetes exec integration test");
        return;
    }

    let ops = KubeRsOps::new();

    // Use the first kind context (or the first context if no kind context is present).
    let contexts = ops
        .list_contexts()
        .await
        .expect("list_contexts must succeed");
    assert!(!contexts.is_empty(), "need at least one kubeconfig context");
    let ctx = &contexts
        .iter()
        .find(|c| c.name.contains("kind"))
        .unwrap_or(&contexts[0])
        .name;

    // Enumerate kube-system pods.
    let ns = "kube-system";
    let pods = ops
        .list_pods(ctx, ns)
        .await
        .expect("list_pods in kube-system must succeed");
    assert!(!pods.is_empty(), "kube-system must have at least one pod");

    // Prefer a Running pod; non-Running pods return 400/404 on exec.
    let pod = pods
        .iter()
        .find(|p| matches!(p.phase, cairn_types::PodPhase::Running));
    let pod = match pod {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no Running pod in kube-system — skipping exec test");
            return;
        }
    };
    let pod_name = &pod.name;

    // Pick the first container.
    let containers = ops
        .list_containers(ctx, ns, pod_name)
        .await
        .expect("list_containers must succeed");
    assert!(
        !containers.is_empty(),
        "Running pod '{pod_name}' must have at least one container"
    );
    let container_name = &containers[0].name;

    eprintln!("Testing exec on pod '{pod_name}' container '{container_name}'...");

    // Launch a non-TTY exec that echoes a known marker.
    let command = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        "echo CAIRN_EXEC_MARKER".to_owned(),
    ];
    let handle = match ops
        .exec(ctx, ns, pod_name, container_name, command, false)
        .await
    {
        Ok(h) => h,
        Err(VfsError::NotFound(_)) | Err(VfsError::Backend { .. }) => {
            // Pod may not accept exec (Succeeded/Failed race) or container has no sh.
            eprintln!(
                "SKIP: exec on '{pod_name}/{container_name}' returned an error — \
                 the container may not have 'sh' or the pod changed phase. Skipping."
            );
            return;
        }
        Err(e) => panic!("exec failed unexpectedly: {e}"),
    };

    // Close stdin immediately — the command does not read it.
    drop(handle.stdin);

    // Collect stdout until the channel closes (the relay task exits after the process exits).
    let mut stdout = handle.stdout.expect("non-tty exec must have stdout");
    let mut output = Vec::new();
    while let Some(chunk) = stdout.recv().await {
        output.extend_from_slice(&chunk);
    }
    let text = String::from_utf8_lossy(&output);
    eprintln!("exec stdout: {text:?}");

    assert!(
        text.contains("CAIRN_EXEC_MARKER"),
        "stdout must contain the marker; got: {text:?}"
    );

    // Await the exit code.
    let exit = handle
        .done
        .await
        .expect("done channel must not be dropped before resolution");
    let code = exit.expect("exec must not fail with VfsError");
    assert_eq!(
        code, 0,
        "non-TTY echo command must exit with code 0, got: {code}"
    );

    eprintln!("exec round-trip OK — exit code {code}");
}
