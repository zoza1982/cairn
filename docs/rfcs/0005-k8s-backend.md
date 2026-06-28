# RFC-0005: Kubernetes backend

- **Status:** Accepted
- **Author(s):** kube-staff-engineer, rust-staff-engineer (synthesized)
- **Date:** 2026-06-28
- **Tracking item:** M6-4 (RFC), M6-5 (mapping core), M6-6 (actions/integration)

## Summary

`cairn-backend-k8s` presents a Kubernetes cluster as a navigable filesystem:
`/<context>/<namespace>/<pod>/<container>/<path…>`. Following the established transport-seam pattern,
the depth-based path-routing logic (`KubeVfs`) depends only on a `KubeOps` trait and is fully
unit-tested against an in-memory mock; the live `kube-rs` client is the integration step.

## Design

### Tree model (depth → meaning)

| depth | segments | entry / ext | `caps_at` |
|---|---|---|---|
| 0 | `[]` | contexts (Dir) | LIST |
| 1 | `[ctx]` | namespaces (Dir) | LIST |
| 2 | `[ctx, ns]` | pods (Dir, `EntryExt::Pod{phase,ready,node}`) | LIST |
| 3 | `[ctx, ns, pod]` | containers (Dir) | LIST |
| ≥4 | `[ctx, ns, pod, c, …]` | in-container fs (File/Dir) | LIST·READ·RANDOM_READ |

Capabilities **vary by depth** (`CapabilityProvider::caps_at`) — the case that motivated per-path
caps: the navigation tree is list-only, while a container's filesystem (depth ≥ 4) is readable.
Contexts come from kubeconfig (no cluster call). RBAC denials surface as `VfsError::Forbidden`
through the listing stream rather than crashing it.

### `KubeOps` transport seam

`list_contexts` / `list_namespaces` / `list_pods` / `list_containers` for navigation, and
`list_dir` / `stat` / `read` for a container's filesystem (`kubectl cp` semantics — tar over
`exec` — in the real adapter). The bug-prone routing — especially depth-3 pod vs depth-4 container
disambiguation and in-container paths — depends only on this trait and is mock-tested with no
cluster. Containers include init and ephemeral containers (flagged on `ContainerInfo`).

### What ships now vs deferred

- **Now (this crate, hermetic):** `KubeVfs<O: KubeOps>` — list/stat routing across all depths,
  per-depth `caps_at`, ranged container-file reads (via the shared saturating
  `cairn_vfs::apply_byte_range`), and an in-memory `MockKube`. Read-only.
- **Integration step (M6-6):** the live `kube-rs` adapter — per-context `kube::Client` cache,
  `Api::<Namespace>`/`Api::<Pod>` listing, tar-over-`exec` filesystem access, kubeconfig/exec-plugin
  and in-cluster auth resolved **via the broker**, and the action surface: log streaming
  (`ActionKind::Stream`), interactive `exec` (`Interactive`), and port-forward (`Session`). This
  adapter pulls in the `kube`/`k8s-openapi` TLS dependency stack, which is why it lands in the
  dedicated integration job (against `kind`) rather than the default cross-platform build.

## Drawbacks / deferred

- Container **writes**, `delete-pod`, log streaming, `exec`, and port-forward are M6-6 actions —
  not part of this read-only seam yet (adding `ActionOutcome::Session` lands with them).
- Watch-based live refresh, multi-container log merging, and CRD/non-Pod resource types at depth 2
  are post-M6 refinements; listings here are one-shot.

## Rationale & alternatives

- *Implement directly on `kube-rs` with no trait* — the depth routing would be untestable without a
  live cluster; the seam buys hermetic coverage of exactly the part that has bugs.
- *Shell out to `kubectl`* — rejected: unstable output, subprocess-credential audit surface.
- *Logs as a synthetic file* — rejected: a container may legally be named `logs`, and a stream is
  not a seekable file; logs/exec belong to the action model, not the path tree.
- *A separate `KubeVfs` per context* — rejected: context becomes the first path segment, so
  switching contexts is navigation, not reconnection.
- *Bundle the `kube-rs` adapter into this PR* — deferred: its TLS stack (aws-lc-rs/rustls) is a
  cross-platform CI risk under `--all-features`, and the tested value (routing) is fully delivered
  by the seam + mock without it.

## Security & privacy

Kubeconfig path / exec-plugin / in-cluster service-account credentials are resolved by the broker at
connect time — never stored by the backend or written into a connection profile. The exec-plugin
bearer token is consumed inside `kube-rs` and never surfaces in the backend. Errors map HTTP status
to `VfsError` (403→Forbidden, 404→NotFound, 429/503→retryable Backend) without echoing response
bodies; `Authorization` headers and token material are never logged.

## Unresolved questions

- tar-over-`exec` listing/extract format and its behavior across BusyBox vs GNU tar.
- Port-forward local-listener lifecycle ownership (backend vs TUI) and the `SessionHandle` shape.
- Watch strategy for live pod/namespace refresh (`kube::runtime::watcher` vs re-list).
