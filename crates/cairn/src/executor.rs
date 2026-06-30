//! The agent step executor: maps an approved [`cairn_ai::PlanStep`] to a real backend operation,
//! per RFC-0007. Connections are referenced by the opaque `conn:N` handles the model gets from the
//! secret-free `WorldSnapshot`; this layer resolves them against the [`VfsRegistry`] and runs the
//! op through the VFS / transfer engine.
//!
//! The safe/local tools (`list`/`stat`/`read`/`copy`/`move`/`delete`) execute against any registered
//! backend. The `exec` action tool routes through [`cairn_vfs::Vfs::invoke`] (RFC-0007 Gap 1), so it
//! reaches whichever backend the connection resolves to — local backends report Unsupported and the
//! container/cluster backends report `not_implemented` until their live transport lands, but the
//! routing itself is real. The broker-backed connection opener now exists (see `crate::connect`) and
//! drives the binary's own (user-initiated) connection flow, but the assistant's `open_connection`
//! tool stays deferred: letting the model open a vault-credentialed connection requires the M7
//! authorize→confirm mediation, so it still returns a clear "not yet available" error here.

use async_trait::async_trait;
use cairn_ai::{PlanStep, StepExecutor};
use cairn_transfer::{run_transfer, ConflictPolicy, TransferOp, TransferSpec, VerifyPolicy};
use cairn_types::{ConnectionId, VfsPath};
use cairn_vfs::{
    action_ids, ActionCtx, ActionId, ActionOutcome, ListOpts, Recurse, Vfs, VfsRegistry,
};
use futures::StreamExt;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Executes approved plan steps against the registered backends.
pub(crate) struct BinaryStepExecutor {
    registry: VfsRegistry,
    /// The connections the model is allowed to act on — exactly those exposed in its `WorldSnapshot`.
    /// A step referencing any other registered connection is refused, so the AI cannot widen its
    /// blast radius beyond what it can see (e.g. a switcher backend rooted at `/`).
    allowed: Vec<ConnectionId>,
}

impl BinaryStepExecutor {
    pub(crate) fn new(registry: VfsRegistry, allowed: Vec<ConnectionId>) -> Self {
        Self { registry, allowed }
    }

    /// Resolve a `conn:N` handle to a backend, enforcing the allow-list.
    async fn vfs(&self, conn: &str) -> Result<Arc<dyn Vfs>, String> {
        let id = parse_conn(conn)?;
        if !self.allowed.contains(&id) {
            return Err(format!(
                "connection '{conn}' is not available to the assistant"
            ));
        }
        self.registry
            .get(id)
            .await
            .ok_or_else(|| format!("unknown connection '{conn}'"))
    }
}

/// A `{conn, path}` reference (RFC-0007).
#[derive(Deserialize)]
struct ConnPath {
    conn: String,
    path: String,
}

#[derive(Deserialize)]
struct CopyMoveInput {
    src: ConnPath,
    dst: ConnPath,
}

#[derive(Deserialize)]
struct DeleteInput {
    conn: String,
    paths: Vec<String>,
    #[serde(default)]
    recursive: bool,
}

#[derive(Deserialize)]
struct ExecInput {
    conn: String,
    path: String,
    #[serde(default)]
    argv: Vec<String>,
    #[serde(default)]
    tty: bool,
}

/// Parse a `"conn:N"` handle into a [`ConnectionId`] (the inverse of its `Display`).
fn parse_conn(s: &str) -> Result<ConnectionId, String> {
    s.parse::<ConnectionId>()
        .map_err(|()| format!("malformed connection ref '{s}'"))
}

/// Parse a step's `input` JSON into the tool's typed shape.
fn parse_input<T: DeserializeOwned>(step: &PlanStep) -> Result<T, String> {
    serde_json::from_value(step.input.clone())
        .map_err(|e| format!("bad input for '{}': {e}", step.tool))
}

fn vpath(s: &str) -> Result<VfsPath, String> {
    VfsPath::parse(s).map_err(|e| format!("bad path '{s}': {e}"))
}

/// A compact byte-size summary (`512 B`, `1.2 KiB`) for a step's output line.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    // Guard the boundary: `{:.1}` could round e.g. 1023.97 KiB up to "1024.0 KiB"; bump a unit.
    if unit < UNITS.len() - 1 && (v * 10.0).round() >= 10240.0 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

#[async_trait]
impl StepExecutor for BinaryStepExecutor {
    async fn execute(&self, step: &PlanStep) -> Result<Option<String>, String> {
        match step.tool.as_str() {
            "list" => {
                // Drain the listing and report its entry count (RFC-0007 Gap 1: a short, secret-free
                // summary surfaced to the user — the entries themselves are never fed to the model).
                let i: ConnPath = parse_input(step)?;
                let vfs = self.vfs(&i.conn).await?;
                let dir = vpath(&i.path)?;
                let mut stream = vfs.list(&dir, ListOpts::default());
                let mut entries = 0usize;
                while let Some(page) = stream.next().await {
                    entries += page.map_err(|e| e.redacted().to_string())?.entries.len();
                }
                Ok(Some(format!("{entries} entries")))
            }
            "stat" => {
                let i: ConnPath = parse_input(step)?;
                let vfs = self.vfs(&i.conn).await?;
                let e = vfs
                    .stat(&vpath(&i.path)?)
                    .await
                    .map_err(|err| err.redacted().to_string())?;
                Ok(Some(if e.is_dir() {
                    "directory".to_owned()
                } else {
                    match e.size {
                        Some(n) => format!("file, {}", human_size(n)),
                        None => "file".to_owned(),
                    }
                }))
            }
            "read" => {
                // Opening confirms reachability and reports the size; contents are never streamed
                // anywhere (and never to the model) — a byte count is the secret-free summary.
                let i: ConnPath = parse_input(step)?;
                let vfs = self.vfs(&i.conn).await?;
                let handle = vfs
                    .open_read(&vpath(&i.path)?, None)
                    .await
                    .map_err(|e| e.redacted().to_string())?;
                Ok(Some(match handle.len_hint() {
                    Some(n) => format!("read {}", human_size(n)),
                    None => "readable".to_owned(),
                }))
            }
            "copy" | "move" => {
                let i: CopyMoveInput = parse_input(step)?;
                let src = self.vfs(&i.src.conn).await?;
                let dst = self.vfs(&i.dst.conn).await?;
                let (src_path, dst_path) = (vpath(&i.src.path)?, vpath(&i.dst.path)?);
                // Refuse to clobber an existing destination. `ConflictPolicy::Prompt` covers the
                // stream path, but a same-connection `move` takes the engine's rename fast-path which
                // ignores the policy and would overwrite — so we check existence up front for both.
                if dst.stat(&dst_path).await.is_ok() {
                    return Err("destination already exists".to_owned());
                }
                let items = [(src_path, dst_path)];
                let spec = TransferSpec {
                    op: if step.tool == "move" {
                        TransferOp::Move
                    } else {
                        TransferOp::Copy
                    },
                    // AI-driven transfers must never silently overwrite: a copy/move onto an existing
                    // destination errors (the conflict is surfaced) rather than destroying data. This
                    // keeps `copy`'s `Safe`/bulk-approvable classification honest.
                    conflict: ConflictPolicy::Prompt,
                    verify: VerifyPolicy::Size,
                };
                let cancel = CancellationToken::new();
                // AI-driven transfers are not pausable from the UI, so feed a never-paused channel.
                let (_pause_tx, paused) = tokio::sync::watch::channel(false);
                run_transfer(&src, &dst, &items, spec, &cancel, &paused, &mut |_| {})
                    .await
                    .map(|_| None)
                    .map_err(|e| e.redacted())
            }
            "delete" => {
                let i: DeleteInput = parse_input(step)?;
                let vfs = self.vfs(&i.conn).await?;
                let recurse = if i.recursive {
                    Recurse::Yes
                } else {
                    Recurse::No
                };
                let mut removed = 0usize;
                for p in &i.paths {
                    vfs.remove(&vpath(p)?, recurse)
                        .await
                        .map_err(|e| e.redacted().to_string())?;
                    removed += 1;
                }
                Ok(Some(format!("removed {removed}")))
            }
            "exec" => {
                // Route through the backend's `Vfs::invoke` (RFC-0007 Gap 1). The local backends
                // return Unsupported and the container backends return `not_implemented` until the
                // live exec streams land; either way the executor delegates rather than hardcoding.
                let i: ExecInput = parse_input(step)?;
                let vfs = self.vfs(&i.conn).await?;
                let ctx = ActionCtx::Exec {
                    argv: i.argv,
                    tty: i.tty,
                };
                match vfs
                    .invoke(&vpath(&i.path)?, ActionId::new(action_ids::EXEC), ctx)
                    .await
                {
                    Ok(ActionOutcome::Done) => Ok(None),
                    // SECURITY: surface only a bounded size summary, never the raw command output —
                    // exec stdout is arbitrary (could contain secrets) and `PlanStep.output` is
                    // contractually a short, secret-free summary. (Dormant today: no backend yet
                    // returns `Text`.)
                    Ok(ActionOutcome::Text(t)) => Ok(Some(format!("{} bytes output", t.len()))),
                    // `Stream`/`Session` outcomes carry live I/O (follow-mode output, an interactive
                    // TTY, a port-forward) that this fire-and-forget executor has no channel to
                    // surface — dropping the handle would cancel the session and falsely report
                    // success. Fail loudly until step output is first-class (RFC-0007 Gap 1).
                    Ok(_) => Err(
                        "exec produced a live session/stream the assistant cannot yet surface"
                            .to_owned(),
                    ),
                    Err(e) => Err(e.redacted().to_string()),
                }
            }
            // The connection opener exists (`crate::connect`) and powers the user-initiated connect
            // flow, but the *assistant*-initiated path is gated on the M7 authorize→confirm
            // mediation, so this tool stays deferred.
            "open_connection" => Err("'open_connection' is not yet available".to_owned()),
            other => Err(format!("unroutable tool '{other}'")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cairn_ai::{capability_for, PlanStep, StepStatus};
    use cairn_backend_local::LocalVfs;

    fn step(tool: &str, input: serde_json::Value) -> PlanStep {
        PlanStep {
            tool: tool.to_owned(),
            input,
            description: String::new(),
            capability: capability_for(tool).unwrap(),
            status: StepStatus::Approved,
            error: None,
            output: None,
        }
    }

    /// An executor over a registry with conn:1 (the allowed dir) and conn:2 (registered but NOT
    /// exposed to the assistant), so scope enforcement can be tested.
    async fn exec_for(dir: &std::path::Path) -> BinaryStepExecutor {
        let reg = VfsRegistry::new();
        reg.insert(
            ConnectionId(1),
            Arc::new(LocalVfs::new(ConnectionId(1), dir)),
        )
        .await;
        reg.insert(
            ConnectionId(2),
            Arc::new(LocalVfs::new(ConnectionId(2), dir)),
        )
        .await;
        BinaryStepExecutor::new(reg, vec![ConnectionId(1)])
    }

    #[test]
    fn human_size_scales_and_guards_the_boundary() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(human_size(1_048_575), "1.0 MiB"); // not "1024.0 KiB"
    }

    #[test]
    fn parse_conn_handles() {
        assert_eq!(parse_conn("conn:7"), Ok(ConnectionId(7)));
        assert!(parse_conn("7").is_err());
        assert!(parse_conn("conn:x").is_err());
    }

    #[tokio::test]
    async fn executes_list_and_delete_against_local() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("doomed.txt"), b"x").unwrap();
        let exec = exec_for(dir.path()).await;
        // `list` surfaces an entry-count summary (RFC-0007 Gap 1).
        let out = exec
            .execute(&step(
                "list",
                serde_json::json!({"conn":"conn:1","path":"/"}),
            ))
            .await
            .unwrap();
        assert_eq!(out.as_deref(), Some("1 entries"));
        let out = exec
            .execute(&step(
                "delete",
                serde_json::json!({"conn":"conn:1","paths":["/doomed.txt"]}),
            ))
            .await
            .unwrap();
        assert_eq!(out.as_deref(), Some("removed 1"));
        assert!(!dir.path().join("doomed.txt").exists());
    }

    #[tokio::test]
    async fn stat_and_read_surface_summaries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap(); // 5 bytes
        let exec = exec_for(dir.path()).await;
        let out = exec
            .execute(&step(
                "stat",
                serde_json::json!({"conn":"conn:1","path":"/a.txt"}),
            ))
            .await
            .unwrap();
        assert_eq!(out.as_deref(), Some("file, 5 B"));
        let out = exec
            .execute(&step(
                "read",
                serde_json::json!({"conn":"conn:1","path":"/a.txt"}),
            ))
            .await
            .unwrap();
        assert_eq!(out.as_deref(), Some("read 5 B"));
    }

    #[tokio::test]
    async fn executes_copy_against_local() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let exec = exec_for(dir.path()).await;
        exec.execute(&step(
            "copy",
            serde_json::json!({"src":{"conn":"conn:1","path":"/a.txt"},
                               "dst":{"conn":"conn:1","path":"/b.txt"}}),
        ))
        .await
        .unwrap();
        assert_eq!(std::fs::read(dir.path().join("b.txt")).unwrap(), b"hello");
    }

    #[tokio::test]
    async fn copy_and_move_refuse_to_overwrite_an_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"new").unwrap();
        std::fs::write(dir.path().join("keep.txt"), b"precious").unwrap();
        let exec = exec_for(dir.path()).await;
        for tool in ["copy", "move"] {
            let e = exec
                .execute(&step(
                    tool,
                    serde_json::json!({"src":{"conn":"conn:1","path":"/a.txt"},
                                       "dst":{"conn":"conn:1","path":"/keep.txt"}}),
                ))
                .await
                .unwrap_err();
            assert!(e.contains("already exists"), "{tool}: {e}");
            // The existing destination is untouched (incl. the same-conn move rename fast-path).
            assert_eq!(
                std::fs::read(dir.path().join("keep.txt")).unwrap(),
                b"precious"
            );
        }
    }

    #[tokio::test]
    async fn refuses_a_connection_not_exposed_to_the_assistant() {
        let dir = tempfile::tempdir().unwrap();
        let exec = exec_for(dir.path()).await; // allows only conn:1
        let e = exec
            .execute(&step(
                "list",
                serde_json::json!({"conn":"conn:2","path":"/"}),
            ))
            .await
            .unwrap_err();
        assert!(e.contains("not available to the assistant"), "{e}");
    }

    #[tokio::test]
    async fn exec_routes_through_invoke_and_errors_on_a_non_action_backend() {
        // `exec` now delegates to `Vfs::invoke`; the local backend has no action surface, so it
        // returns an error (not a panic, not a silent success) — proving the routing is live.
        let dir = tempfile::tempdir().unwrap();
        let exec = exec_for(dir.path()).await;
        let e = exec
            .execute(&step(
                "exec",
                serde_json::json!({"conn":"conn:1","path":"/x","argv":["sh"]}),
            ))
            .await
            .unwrap_err();
        // Assert on the backend's redacted `Unsupported` message, not just `is_err()`, so a failure
        // *before* reaching `invoke` (bad conn/path) can't masquerade as a passing routing test.
        assert!(e.contains("operation not supported"), "unexpected: {e}");
    }

    #[tokio::test]
    async fn open_connection_is_deferred_and_bad_refs_error_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let exec = exec_for(dir.path()).await;
        let e = exec
            .execute(&step(
                "open_connection",
                serde_json::json!({"profile": "prod"}),
            ))
            .await
            .unwrap_err();
        assert!(e.contains("not yet available"));
        // An unknown connection is a clean error — and carries no path.
        let reg = VfsRegistry::new();
        let exec = BinaryStepExecutor::new(reg, vec![ConnectionId(1)]);
        let e = exec
            .execute(&step(
                "stat",
                serde_json::json!({"conn":"conn:1","path":"/secret"}),
            ))
            .await
            .unwrap_err();
        assert!(e.contains("unknown connection"));
    }
}
