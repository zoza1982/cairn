//! The agent step executor: maps an approved [`cairn_ai::PlanStep`] to a real backend operation,
//! per RFC-0007. Connections are referenced by the opaque `conn:N` handles the model gets from the
//! secret-free `WorldSnapshot`; this layer resolves them against the [`VfsRegistry`] and runs the
//! op through the VFS / transfer engine.
//!
//! The safe/local tools (`list`/`stat`/`read`/`copy`/`move`/`delete`) execute against any registered
//! backend. The container/cluster action tools (`exec`/`logs`/`port_forward`) and `open_connection`
//! need live transport / the `Vfs::invoke` path-routing change (RFC-0007 Gap 1) and return a clear
//! "not yet available" error until that integration lands.

use async_trait::async_trait;
use cairn_ai::{PlanStep, StepExecutor};
use cairn_transfer::{run_transfer, ConflictPolicy, TransferOp, TransferSpec, VerifyPolicy};
use cairn_types::{ConnectionId, VfsPath};
use cairn_vfs::{ListOpts, Recurse, Vfs, VfsRegistry};
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

#[async_trait]
impl StepExecutor for BinaryStepExecutor {
    async fn execute(&self, step: &PlanStep) -> Result<(), String> {
        match step.tool.as_str() {
            "list" => {
                // Drain the listing to confirm it succeeds; the entries are not surfaced until step
                // output is a first-class channel (RFC-0007 Gap 1), so this is a validate-only probe.
                let i: ConnPath = parse_input(step)?;
                let vfs = self.vfs(&i.conn).await?;
                let dir = vpath(&i.path)?;
                let mut stream = vfs.list(&dir, ListOpts::default());
                while let Some(page) = stream.next().await {
                    page.map_err(|e| e.redacted().to_string())?;
                }
                Ok(())
            }
            "stat" => {
                let i: ConnPath = parse_input(step)?;
                let vfs = self.vfs(&i.conn).await?;
                vfs.stat(&vpath(&i.path)?)
                    .await
                    .map(|_| ())
                    .map_err(|e| e.redacted().to_string())
            }
            "read" => {
                // Validate-only until step output exists (RFC-0007 Gap 1): opening confirms the file
                // is reachable; the contents are not streamed anywhere yet.
                let i: ConnPath = parse_input(step)?;
                let vfs = self.vfs(&i.conn).await?;
                vfs.open_read(&vpath(&i.path)?, None)
                    .await
                    .map(|_| ())
                    .map_err(|e| e.redacted().to_string())
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
                run_transfer(&src, &dst, &items, spec, &cancel, &mut |_| {})
                    .await
                    .map(|_| ())
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
                for p in &i.paths {
                    vfs.remove(&vpath(p)?, recurse)
                        .await
                        .map_err(|e| e.redacted().to_string())?;
                }
                Ok(())
            }
            // In the closed set but needing live transport / the `Vfs::invoke` path-routing change
            // (RFC-0007 Gap 1). `logs`/`port_forward` are not yet in the tool set, so they route to
            // the `unroutable` arm below until they are added.
            "exec" | "open_connection" => Err(format!(
                "'{}' is not yet available (needs live transport)",
                step.tool
            )),
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
        exec.execute(&step(
            "list",
            serde_json::json!({"conn":"conn:1","path":"/"}),
        ))
        .await
        .unwrap();
        exec.execute(&step(
            "delete",
            serde_json::json!({"conn":"conn:1","paths":["/doomed.txt"]}),
        ))
        .await
        .unwrap();
        assert!(!dir.path().join("doomed.txt").exists());
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
    async fn deferred_tools_and_bad_refs_error_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let exec = exec_for(dir.path()).await;
        let e = exec
            .execute(&step(
                "exec",
                serde_json::json!({"conn":"conn:1","path":"/x","argv":[]}),
            ))
            .await
            .unwrap_err();
        assert!(e.contains("not yet available"));
        // An unknown (but allowed-range) connection is a clean error — and carries no path.
        let dir2 = tempfile::tempdir().unwrap();
        let reg = VfsRegistry::new();
        let exec = BinaryStepExecutor::new(reg, vec![ConnectionId(1)]);
        let _ = dir2;
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
