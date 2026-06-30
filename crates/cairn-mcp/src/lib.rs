#![forbid(unsafe_code)]
//! # cairn-mcp — Model Context Protocol client
//!
//! This crate is Cairn's **client** for the [Model Context Protocol](https://modelcontextprotocol.io)
//! (MCP). It is the **transport/protocol layer only**: a thin, well-documented wrapper over the
//! official [`rmcp`] SDK that knows how to
//!
//! 1. connect to an MCP server (over **stdio** — spawning a child process — or **streamable HTTP**),
//! 2. perform the MCP `initialize` handshake,
//! 3. list the server's tools, and
//! 4. call a single tool and read back its content.
//!
//! ## Scope and boundaries
//!
//! This is a **foundation** crate. It deliberately does **not**:
//!
//! - depend on `cairn-ai`, `cairn-broker`, `cairn-vault`, or `cairn-secrets`, and
//! - wire MCP tools into the agent's tool surface.
//!
//! Cairn's AI agent (`cairn-ai`) exposes a deliberately **closed, capability-gated** tool set behind
//! the broker and the plan→confirm→execute safety model. Arbitrary tools discovered from an external
//! MCP server are, by contrast, **untrusted and open-ended**. How (and whether) such tools are allowed
//! to interact with that closed set — registration, capability mapping, confirmation, prompt-injection
//! defense — is a **separate follow-up RFC** (`RFC-mcp-client`) and is intentionally out of scope here.
//! Treat anything returned by [`McpClient`] as untrusted input.
//!
//! ## Errors and secrets
//!
//! All fallible operations return [`McpError`]. The secret-free guarantee applies to the
//! [`Display`](std::fmt::Display) of `McpError` (e.g. `format!("{err}")`): those messages are fixed
//! strings — Cairn never embeds connection URLs, commands, or tool arguments (which may carry secrets)
//! in its own message text. The underlying transport/protocol error is attached as the error
//! [`source`](std::error::Error::source) for debugging, and **that chain (and the `Debug` output) may
//! carry sensitive transport detail** — a reqwest error can contain the request URL, and a server's
//! JSON-RPC error can echo arguments. Callers that log the source chain or `Debug` should redact, or
//! log only the top-level `Display`.
//!
//! ## Timeouts
//!
//! Because a tool may talk to an arbitrary external process or endpoint, every operation is bounded by
//! a timeout so an unresponsive server can never block the caller forever (CLAUDE.md §9). The
//! constructors apply [`McpClient::DEFAULT_TIMEOUT`] to the handshake; the per-request timeout for
//! [`list_tools`](McpClient::list_tools)/[`call_tool`](McpClient::call_tool) can be changed with
//! [`with_timeout`](McpClient::with_timeout). A timed-out operation returns [`McpError::Timeout`].
//!
//! ## Example
//!
//! ```no_run
//! use cairn_mcp::McpClient;
//! use serde_json::json;
//!
//! # async fn run() -> Result<(), cairn_mcp::McpError> {
//! // Spawn a stdio MCP server (e.g. a filesystem server) as a child process.
//! let client = McpClient::connect_stdio("uvx", ["mcp-server-fetch"]).await?;
//!
//! for tool in client.list_tools().await? {
//!     println!("{}: {}", tool.name, tool.description.unwrap_or_default());
//! }
//!
//! let result = client.call_tool("fetch", json!({ "url": "https://example.com" })).await?;
//! println!("{}", result.text);
//! # Ok(())
//! # }
//! ```

use std::ffi::OsStr;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResult, Tool};
use rmcp::service::{ClientInitializeError, RunningService};
use rmcp::transport::{IntoTransport, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceError, ServiceExt};
use serde_json::Value;

/// A connected MCP client session.
///
/// Construct one with [`McpClient::connect_stdio`] (spawn a server as a child process and talk to it
/// over its stdin/stdout) or [`McpClient::connect_http`] (talk to a server over streamable HTTP). The
/// MCP `initialize` handshake is completed before the constructor returns, so a returned `McpClient`
/// is ready for [`list_tools`](McpClient::list_tools) and [`call_tool`](McpClient::call_tool).
///
/// The underlying connection is closed when the `McpClient` is dropped. For the stdio transport rmcp
/// also terminates the child process on drop, provided a Tokio runtime is still active (the kill is
/// spawned as a task); a hard parent exit can leave the child orphaned.
pub struct McpClient {
    /// The running rmcp client service. `()` is the no-op client handler: this crate consumes a server
    /// but exposes no client-side capabilities (sampling, roots, …) of its own.
    service: RunningService<RoleClient, ()>,
    /// Per-request timeout for [`list_tools`](Self::list_tools) and [`call_tool`](Self::call_tool).
    timeout: Duration,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl McpClient {
    /// The default timeout applied to the connect handshake and to each request, unless overridden via
    /// [`with_timeout`](Self::with_timeout).
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

    /// Connect to an MCP server spawned as a **child process**, speaking MCP over its stdin/stdout.
    ///
    /// `program` is the executable to run and `args` its arguments (e.g. `"npx"` with
    /// `["-y", "@modelcontextprotocol/server-everything"]`). The child is spawned with piped stdin/stdout
    /// and its **stderr is discarded** (so a chatty or hostile server cannot scribble on Cairn's
    /// terminal); the `initialize` handshake is performed, bounded by [`Self::DEFAULT_TIMEOUT`], before
    /// this returns.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::Spawn`] if the process cannot be started, [`McpError::Initialize`] if the
    /// MCP handshake fails, or [`McpError::Timeout`] if it does not complete in time.
    pub async fn connect_stdio<P, I, S>(program: P, args: I) -> Result<Self, McpError>
    where
        P: AsRef<OsStr>,
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = tokio::process::Command::new(program);
        command.args(args);
        // Don't inherit Cairn's stderr: an untrusted server's stderr must not reach the terminal.
        let (transport, _stderr) = TokioChildProcess::builder(command)
            .stderr(Stdio::null())
            .spawn()
            .map_err(McpError::Spawn)?;
        Self::connect_transport(transport, Self::DEFAULT_TIMEOUT).await
    }

    /// Connect to an MCP server over **streamable HTTP** at the given URL.
    ///
    /// The transport uses **rustls** for TLS (no OpenSSL dependency). The `initialize` handshake is
    /// performed, bounded by [`Self::DEFAULT_TIMEOUT`], before this returns.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::Initialize`] if the MCP handshake fails (including the initial connection),
    /// or [`McpError::Timeout`] if it does not complete in time.
    ///
    /// # Panics
    ///
    /// Constructing the underlying HTTP client can, in principle, panic if the process has no usable
    /// rustls crypto provider — an upstream `rmcp`/`reqwest` behavior. Cairn's pinned rustls (ring /
    /// aws-lc-rs) makes this practically unreachable.
    pub async fn connect_http(url: impl Into<Arc<str>>) -> Result<Self, McpError> {
        let transport = StreamableHttpClientTransport::from_uri(url);
        Self::connect_transport(transport, Self::DEFAULT_TIMEOUT).await
    }

    /// Set the per-request timeout for subsequent [`list_tools`](Self::list_tools) and
    /// [`call_tool`](Self::call_tool) calls. Defaults to [`Self::DEFAULT_TIMEOUT`].
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Drive the `initialize` handshake over an arbitrary rmcp transport, bounded by `timeout`.
    ///
    /// Private: the public surface is the two concrete `connect_*` constructors. Keeping the transport
    /// generic here lets tests connect over an in-memory duplex stream without exposing rmcp's transport
    /// types in Cairn's public API.
    async fn connect_transport<T, E, A>(transport: T, timeout: Duration) -> Result<Self, McpError>
    where
        T: IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let service = tokio::time::timeout(timeout, ().serve(transport))
            .await
            .map_err(|_| McpError::Timeout)?
            .map_err(|e| McpError::Initialize(Box::new(e)))?;
        Ok(Self { service, timeout })
    }

    /// List every tool advertised by the connected server.
    ///
    /// Pagination is handled internally: this returns the full set. The whole operation (including all
    /// pages) is bounded by the client's timeout, so a server that paginates without end cannot hang the
    /// caller — though a hard per-page/total cap is left to the safety RFC (`RFC-mcp-client`).
    ///
    /// # Errors
    ///
    /// Returns [`McpError::Request`] if the `tools/list` request fails, or [`McpError::Timeout`] if it
    /// does not complete within the client's timeout.
    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>, McpError> {
        let tools = tokio::time::timeout(self.timeout, self.service.peer().list_all_tools())
            .await
            .map_err(|_| McpError::Timeout)?
            .map_err(McpError::Request)?;
        Ok(tools.into_iter().map(McpToolInfo::from).collect())
    }

    /// Call a tool by `name`, passing `args` as its arguments.
    ///
    /// `args` must be a JSON **object** (matching the tool's input schema) or [`Value::Null`] for a
    /// tool that takes no arguments. Any other JSON type is rejected with
    /// [`McpError::InvalidArguments`] without ever contacting the server.
    ///
    /// A returned [`McpToolResult`] with [`is_error`](McpToolResult::is_error) set to `true` represents
    /// a *tool-level* failure reported by the server (the tool ran and decided it failed); that is
    /// **not** an [`McpError`]. Protocol/transport failures are [`McpError::Request`].
    ///
    /// # Errors
    ///
    /// Returns [`McpError::InvalidArguments`] if `args` is neither an object nor null,
    /// [`McpError::Request`] if the `tools/call` request fails, or [`McpError::Timeout`] if it does not
    /// complete within the client's timeout.
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<McpToolResult, McpError> {
        let arguments = match args {
            Value::Null => None,
            Value::Object(map) => Some(map),
            other => return Err(McpError::InvalidArguments(json_kind(&other))),
        };

        let mut param = CallToolRequestParams::new(name.to_owned());
        param.arguments = arguments;

        let result = tokio::time::timeout(self.timeout, self.service.peer().call_tool(param))
            .await
            .map_err(|_| McpError::Timeout)?
            .map_err(McpError::Request)?;
        Ok(McpToolResult::from(result))
    }
}

/// A tool advertised by an MCP server.
///
/// This is a plain, owned view of rmcp's `Tool` carrying just the fields Cairn needs at this layer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct McpToolInfo {
    /// The tool's unique name, used as the `name` argument to [`McpClient::call_tool`].
    pub name: String,
    /// A human-readable description of what the tool does, if the server provided one.
    ///
    /// rmcp's `Tool` also carries an optional display `title`; it is not preserved at this transport
    /// layer (name + description + schema is the surface this layer needs). A later AI-integration layer
    /// that wants the richer title can read it from a direct rmcp handle.
    pub description: Option<String>,
    /// The tool's input JSON Schema (a JSON object) describing its accepted arguments.
    pub input_schema: Value,
}

impl From<Tool> for McpToolInfo {
    fn from(tool: Tool) -> Self {
        // `Tool::schema_as_json_value` clones the inner schema map into an owned `Value::Object`.
        let input_schema = tool.schema_as_json_value();
        Self {
            name: tool.name.into_owned(),
            description: tool.description.map(|d| d.into_owned()),
            input_schema,
        }
    }
}

/// The outcome of an [`McpClient::call_tool`] invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct McpToolResult {
    /// The concatenation of all text content blocks returned by the tool, joined by newlines.
    ///
    /// Non-text content (images, audio, embedded resources) is not represented here; see
    /// [`structured`](McpToolResult::structured) for a tool's structured output.
    pub text: String,
    /// Whether the server reported this as a tool-level error (the tool ran but failed).
    pub is_error: bool,
    /// The tool's structured JSON result, if it returned one.
    pub structured: Option<Value>,
}

impl From<CallToolResult> for McpToolResult {
    fn from(result: CallToolResult) -> Self {
        let text = result
            .content
            .iter()
            .filter_map(|block| block.as_text().map(|t| t.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        Self {
            text,
            is_error: result.is_error.unwrap_or(false),
            structured: result.structured_content,
        }
    }
}

/// Errors returned by [`McpClient`].
///
/// Top-level messages are fixed and **secret-free**; the originating transport/protocol error is
/// attached as the error `source` for debugging.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum McpError {
    /// The MCP server child process could not be spawned (stdio transport).
    #[error("failed to spawn MCP server process")]
    Spawn(#[source] std::io::Error),

    /// The MCP `initialize` handshake failed (bad connection, protocol mismatch, server error).
    ///
    /// Boxed because `ClientInitializeError` is large relative to the other variants.
    #[error("failed to initialize MCP session")]
    Initialize(#[source] Box<ClientInitializeError>),

    /// An MCP request (`tools/list`, `tools/call`, …) failed at the protocol/transport level.
    #[error("MCP request failed")]
    Request(#[source] ServiceError),

    /// `call_tool` was given arguments that were neither a JSON object nor null.
    ///
    /// The offending value's JSON type is reported, but never its contents (which may be secret).
    #[error("invalid tool arguments: expected a JSON object or null, got {0}")]
    InvalidArguments(&'static str),

    /// The operation (handshake, `tools/list`, or `tools/call`) did not complete within the timeout.
    #[error("MCP operation timed out")]
    Timeout,
}

/// The JSON type name of `value`, for secret-free error messages (the value itself is never included).
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{
        CallToolRequestParams, CallToolResult, ContentBlock, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    };
    use rmcp::service::RequestContext;
    use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
    use serde_json::json;

    /// A trivial in-process MCP server exposing a single `echo` tool that returns its `message` arg.
    #[derive(Clone)]
    struct EchoServer;

    impl ServerHandler for EchoServer {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
        }

        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, ErrorData> {
            let schema = json!({
                "type": "object",
                "properties": { "message": { "type": "string" } },
                "required": ["message"],
            });
            let schema = schema.as_object().cloned().unwrap_or_default();
            let tool = Tool::new(
                "echo",
                "Echoes back the `message` argument",
                Arc::new(schema),
            );
            Ok(ListToolsResult::with_all_items(vec![tool]))
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<CallToolResult, ErrorData> {
            if request.name != "echo" {
                return Err(ErrorData::invalid_params("unknown tool", None));
            }
            let message = request
                .arguments
                .as_ref()
                .and_then(|args| args.get("message"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            Ok(CallToolResult::success(vec![ContentBlock::text(message)]))
        }
    }

    /// A server whose `call_tool` never returns — used to test client-side timeouts.
    #[derive(Clone)]
    struct HangServer;

    impl ServerHandler for HangServer {
        fn get_info(&self) -> ServerInfo {
            ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
        }

        async fn call_tool(
            &self,
            _request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<CallToolResult, ErrorData> {
            std::future::pending().await
        }
    }

    /// Connect an `McpClient` to an in-process server over an in-memory duplex stream.
    ///
    /// The `initialize` handshake is a request/response exchange, so both sides must make progress
    /// concurrently — `tokio::join!` drives both `serve` futures together. Returns the live server
    /// handle (keep it alive for the test's duration) and the connected client.
    async fn connect_in_memory<H>(handler: H) -> (RunningService<RoleServer, H>, McpClient)
    where
        H: ServerHandler,
    {
        let (client_io, server_io) = tokio::io::duplex(8 * 1024);
        let (server, client) = tokio::join!(
            handler.serve(server_io),
            McpClient::connect_transport(client_io, McpClient::DEFAULT_TIMEOUT),
        );
        (
            server.expect("server initialize"),
            client.expect("client initialize"),
        )
    }

    /// Hermetic round-trip: connect → initialize → list_tools → call_tool, entirely in-process over an
    /// in-memory duplex stream. No child process, no socket, no network — runs in the default offline
    /// `cargo test`.
    #[tokio::test]
    async fn stdio_in_memory_round_trip() {
        let (server, client) = connect_in_memory(EchoServer).await;

        // list_tools maps the rmcp Tool into McpToolInfo.
        let tools = client.list_tools().await.expect("list_tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert_eq!(
            tools[0].description.as_deref(),
            Some("Echoes back the `message` argument")
        );
        assert_eq!(tools[0].input_schema["type"], json!("object"));

        // call_tool round-trips the argument back as text content.
        let result = client
            .call_tool("echo", json!({ "message": "hello cairn" }))
            .await
            .expect("call_tool");
        assert!(!result.is_error);
        assert_eq!(result.text, "hello cairn");

        server.cancel().await.expect("server shutdown");
    }

    #[tokio::test]
    async fn call_tool_with_null_args_sends_no_arguments() {
        let (server, client) = connect_in_memory(EchoServer).await;

        // The echo server falls back to an empty string when `message` is absent.
        let result = client
            .call_tool("echo", Value::Null)
            .await
            .expect("call_tool with null args");
        assert!(!result.is_error);
        assert_eq!(result.text, "");

        server.cancel().await.expect("server shutdown");
    }

    #[tokio::test]
    async fn call_tool_rejects_non_object_arguments() {
        let (server, client) = connect_in_memory(EchoServer).await;

        let err = client
            .call_tool("echo", json!([1, 2, 3]))
            .await
            .expect_err("array arguments must be rejected");
        assert!(matches!(err, McpError::InvalidArguments("array")));

        server.cancel().await.expect("server shutdown");
    }

    #[tokio::test]
    async fn call_tool_times_out_on_unresponsive_server() {
        let (server, client) = connect_in_memory(HangServer).await;
        let client = client.with_timeout(Duration::from_millis(100));

        let err = client
            .call_tool("anything", json!({}))
            .await
            .expect_err("an unresponsive server must time out");
        assert!(matches!(err, McpError::Timeout));

        // The server task is parked in the hung handler; cancelling it is best-effort here.
        let _ = server.cancel().await;
    }

    #[test]
    fn tool_result_collects_text_and_error_flag() {
        // `error(..)` sets `is_error = Some(true)`; `structured_content` is then set directly.
        let mut result =
            CallToolResult::error(vec![ContentBlock::text("a"), ContentBlock::text("b")]);
        result.structured_content = Some(json!({ "ok": true }));

        let mapped = McpToolResult::from(result);
        assert_eq!(mapped.text, "a\nb");
        assert!(mapped.is_error);
        assert_eq!(mapped.structured, Some(json!({ "ok": true })));
    }

    #[test]
    fn json_kind_names_types_without_leaking_values() {
        assert_eq!(json_kind(&json!("secret-token")), "string");
        assert_eq!(json_kind(&json!(42)), "number");
        assert_eq!(json_kind(&json!(true)), "boolean");
        assert_eq!(json_kind(&json!([])), "array");
        assert_eq!(json_kind(&json!({})), "object");
        assert_eq!(json_kind(&Value::Null), "null");
    }
}
