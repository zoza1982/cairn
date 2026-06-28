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
}

impl BinaryStepExecutor {
    pub(crate) fn new(registry: VfsRegistry) -> Self {
        Self { registry }
    }

    /// Resolve a `conn:N` handle to a backend.
    async fn vfs(&self, conn: &str) -> Result<Arc<dyn Vfs>, String> {
        let id = parse_conn(conn)?;
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

/// Parse a `"conn:N"` handle into a [`ConnectionId`].
fn parse_conn(s: &str) -> Result<ConnectionId, String> {
    s.strip_prefix("conn:")
        .and_then(|n| n.parse::<u64>().ok())
        .map(ConnectionId)
        .ok_or_else(|| format!("malformed connection ref '{s}'"))
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
                let items = [(vpath(&i.src.path)?, vpath(&i.dst.path)?)];
                let spec = TransferSpec {
                    op: if step.tool == "move" {
                        TransferOp::Move
                    } else {
                        TransferOp::Copy
                    },
                    conflict: ConflictPolicy::Overwrite,
                    verify: VerifyPolicy::Size,
                };
                let cancel = CancellationToken::new();
                run_transfer(&src, &dst, &items, spec, &cancel, &mut |_| {})
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string())
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
            // These need live transport / the `Vfs::invoke` path-routing change (RFC-0007 Gap 1).
            "exec" | "logs" | "port_forward" | "open_connection" => Err(format!(
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
        }
    }

    async fn registry_with(dir: &std::path::Path) -> VfsRegistry {
        let reg = VfsRegistry::new();
        reg.insert(
            ConnectionId(1),
            Arc::new(LocalVfs::new(ConnectionId(1), dir)),
        )
        .await;
        reg
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
        let exec = BinaryStepExecutor::new(registry_with(dir.path()).await);

        // list succeeds.
        exec.execute(&step(
            "list",
            serde_json::json!({"conn":"conn:1","path":"/"}),
        ))
        .await
        .unwrap();
        // delete removes the file.
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
        let exec = BinaryStepExecutor::new(registry_with(dir.path()).await);
        exec.execute(&step(
            "copy",
            serde_json::json!({
                "src": {"conn":"conn:1","path":"/a.txt"},
                "dst": {"conn":"conn:1","path":"/b.txt"}
            }),
        ))
        .await
        .unwrap();
        assert_eq!(std::fs::read(dir.path().join("b.txt")).unwrap(), b"hello");
    }

    #[tokio::test]
    async fn deferred_tools_and_bad_refs_error_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let exec = BinaryStepExecutor::new(registry_with(dir.path()).await);
        // A deferred action tool reports not-yet-available, not a panic.
        let e = exec
            .execute(&step(
                "exec",
                serde_json::json!({"conn":"conn:1","path":"/x","argv":[]}),
            ))
            .await
            .unwrap_err();
        assert!(e.contains("not yet available"));
        // An unknown connection is a clean error.
        let e = exec
            .execute(&step(
                "list",
                serde_json::json!({"conn":"conn:9","path":"/"}),
            ))
            .await
            .unwrap_err();
        assert!(e.contains("unknown connection"));
    }
}
