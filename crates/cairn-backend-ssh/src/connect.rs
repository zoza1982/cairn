//! The live SSH connection layer (gated behind the `ssh` feature).
//!
//! Establishes a `russh` client session — TCP connect → host-key verification → authentication →
//! open the `sftp` subsystem — and hands the resulting `russh_sftp` session to [`RealSftp`] →
//! [`SftpVfs`]. Credentials come from the broker as a [`CredentialSecret`]; this connector is a plain
//! `async fn` with no broker dependency (the broker boundary wraps the call in the binary's effect
//! runner). Connection pooling/keepalive, jump hosts, and `~/.ssh/config` parsing are follow-ups
//! (RFC-0003); this lands the core connect+auth+SFTP path.

use crate::{RealSftp, SftpVfs};
use cairn_types::ConnectionId;
use cairn_vault::{CredentialSecret, ExposeSecret, SshCredential};
use cairn_vfs::VfsError;
use russh::client::{self, Handle};
use russh::keys::known_hosts::{
    check_known_hosts_path, known_host_keys_path, learn_known_hosts_path,
};
use russh::keys::ssh_key::PublicKey;
use russh::keys::{decode_secret_key, Algorithm, HashAlg, PrivateKeyWithHashAlg};
use russh_sftp::client::SftpSession;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// How the server's host key is verified.
#[derive(Clone, Debug)]
pub enum HostKeyPolicy {
    /// The key must already be present in `known_hosts` and match. An unknown or changed key is
    /// rejected. (No "accept anything" mode exists — that would disable host-key verification.)
    Strict {
        /// Path to the `known_hosts` file.
        known_hosts: PathBuf,
    },
    /// Trust-on-first-use: accept and record an unknown key; a *changed* key is always rejected.
    AcceptNew {
        /// Path to the `known_hosts` file (created on first use).
        known_hosts: PathBuf,
    },
}

/// Parameters for establishing an SSH/SFTP connection. Credentials are passed separately.
#[derive(Clone, Debug)]
pub struct SshConnectParams {
    /// Hostname or IP to connect to.
    pub host: String,
    /// TCP port (usually 22).
    pub port: u16,
    /// Remote username.
    pub user: String,
    /// Host-key verification policy.
    pub host_key: HostKeyPolicy,
    /// Timeout for the TCP connect + SSH handshake.
    pub connect_timeout: Duration,
    /// Timeout for the authentication phase.
    pub auth_timeout: Duration,
}

impl SshConnectParams {
    /// Construct with default timeouts (10s connect, 30s auth).
    #[must_use]
    pub fn new(
        host: impl Into<String>,
        port: u16,
        user: impl Into<String>,
        host_key: HostKeyPolicy,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            user: user.into(),
            host_key,
            connect_timeout: Duration::from_secs(10),
            auth_timeout: Duration::from_secs(30),
        }
    }
}

/// Decide whether to accept a server host key under `policy`. Factored out (and pure but for the
/// `known_hosts` file) so the security-critical logic is unit-testable without a live server.
///
/// Returns `Err` only for an infrastructure failure (e.g. `~/.ssh` is unwritable under `AcceptNew`),
/// so the caller can surface that as a connection error rather than a misleading host-key rejection.
/// A *changed* key always returns `Ok(false)` (reject), never `Err`.
fn verify_host_key(
    policy: &HostKeyPolicy,
    host: &str,
    port: u16,
    key: &PublicKey,
) -> Result<bool, std::io::Error> {
    match policy {
        HostKeyPolicy::Strict { known_hosts } => {
            // Only a recorded, matching key is accepted; unknown / changed / missing-file → reject.
            // A revocation outranks a match: the same key may appear on both a plain and an
            // `@revoked` line, and russh's matcher does not look at markers at all.
            if host_pin(host, port, known_hosts) == HostPin::Revoked {
                return Ok(false);
            }
            Ok(matches!(
                check_known_hosts_path(host, port, key, known_hosts),
                Ok(true)
            ))
        }
        HostKeyPolicy::AcceptNew { known_hosts } => {
            // Ensure the file exists so "unknown" reads as Ok(false), not an io Err (which we can't
            // distinguish from a changed key). Propagate a real io failure so it isn't mistaken for
            // a host-key rejection.
            ensure_known_hosts(known_hosts)?;
            Ok(match check_known_hosts_path(host, port, key, known_hosts) {
                Ok(true) => true,
                // `Ok(false)` means "no matching entry" — which covers BOTH a host we have never
                // seen AND a host we have pinned under a *different key algorithm* (russh only
                // reports `Err(KeyChanged)` for a same-algorithm mismatch). Learning on the second
                // case would let anyone who can answer for the host swap ed25519 for RSA and be
                // trusted, which is precisely the attack `known_hosts` exists to stop. "Accept new"
                // means a host we have no key for at all, so ask that question directly.
                Ok(false) => match host_pin(host, port, known_hosts) {
                    HostPin::Unseen => {
                        // Genuine first contact: pin it. A failed write means we would silently
                        // re-prompt forever, so it is not something to swallow — but there is no
                        // logging in this crate, so surface it as an infrastructure error.
                        learn_known_hosts_path(host, port, key, known_hosts).map_err(|e| {
                            std::io::Error::other(format!("cannot record host key: {e}"))
                        })?;
                        true
                    }
                    HostPin::Pinned | HostPin::Revoked => false,
                },
                // A *changed* key is always rejected (fail-safe).
                Err(_) => false,
            })
        }
    }
}

/// What `known_hosts` already says about a host — the question "accept new" actually asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostPin {
    /// No entry: a genuine first contact, safe to learn.
    Unseen,
    /// At least one entry exists. Whatever key we were just offered did not match one, so do not
    /// learn — this is the algorithm-swap case, or an unreadable file we must not gamble on.
    Pinned,
    /// An `@revoked` line names this host. Never connect, never learn: the user said no.
    Revoked,
}

/// Classify a host against `known_hosts`.
///
/// russh's matcher is not sufficient on its own: it compares field 0 of each line for exact string
/// equality (or the `|1|` HMAC), so it silently fails to match a `@revoked` / `@cert-authority`
/// marker line, a `host1,host2` list, a `*.example.com` pattern, or a differently-cased hostname.
/// Every one of those would read as "never seen" and let an impostor's key be learned *and appended
/// to the user's real `known_hosts`* — strictly worse than the bug this guard exists to close. So we
/// scan the file ourselves for the forms russh cannot see, and still call russh for the hashed
/// (`|1|`) entries we deliberately do not reimplement. Either source saying "known" wins.
fn host_pin(host: &str, port: u16, known_hosts: &Path) -> HostPin {
    let text = match std::fs::read_to_string(known_hosts) {
        Ok(t) => t,
        // Absent: genuine first contact — russh reports the same, and cannot tell us apart from…
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HostPin::Unseen,
        // …unreadable, which we must not treat as "no pin". Refusing to learn costs a connection;
        // learning here would trust an impostor because we could not read the user's own file.
        Err(_) => return HostPin::Pinned,
    };

    let mut seen = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let Some(first) = fields.next() else { continue };
        // A line may open with `@revoked` / `@cert-authority`; the patterns are then the next field.
        let (marker, patterns) = if let Some(m) = first.strip_prefix('@') {
            match fields.next() {
                Some(p) => (Some(m), p),
                None => continue,
            }
        } else {
            (None, first)
        };
        if !host_matches_patterns(host, port, patterns) {
            continue;
        }
        // A revocation is the strongest statement in the file and outranks any other line.
        if marker == Some("revoked") {
            return HostPin::Revoked;
        }
        seen = true;
    }
    if seen {
        return HostPin::Pinned;
    }
    // Hashed (`|1|salt|hash`) entries are opaque to the scan above; russh does that matching.
    match known_host_keys_path(host, port, known_hosts) {
        Ok(keys) if keys.is_empty() => HostPin::Unseen,
        Ok(_) => HostPin::Pinned,
        Err(_) => HostPin::Pinned,
    }
}

/// Whether `host`(:`port`) matches a `known_hosts` pattern list (`a.example,*.b.example,!c.b.example`).
///
/// Follows `sshd(8)`'s rules: comma-separated patterns, `*`/`?` globs, a leading `!` negation that
/// vetoes the whole line, and the `[host]:port` form for a non-default port. Hostnames are compared
/// case-insensitively, as OpenSSH does.
fn host_matches_patterns(host: &str, port: u16, patterns: &str) -> bool {
    let host = host.to_ascii_lowercase();
    // OpenSSH writes `[host]:port` for anything other than 22, and plain `host` for 22.
    let candidate = if port == 22 {
        host.clone()
    } else {
        format!("[{host}]:{port}")
    };
    let mut matched = false;
    for pat in patterns.split(',') {
        let (negated, pat) = match pat.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, pat),
        };
        if glob_matches(&pat.to_ascii_lowercase(), &candidate) {
            if negated {
                return false; // an explicit exclusion vetoes the line
            }
            matched = true;
        }
    }
    matched
}

/// `*` (any run) and `?` (one char) matching, iterative so a hostile pattern cannot blow the stack.
fn glob_matches(pat: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pat.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0usize, 0usize);
    // Where to resume if the current `*` turns out to have consumed too little.
    let (mut star, mut resume) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            resume = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            resume += 1;
            ti = resume;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Move the algorithms already recorded for this host to the front of the client's host-key
/// preference list, leaving the rest of russh's order intact behind them.
fn prefer_pinned_algorithms(
    config: &mut client::Config,
    host: &str,
    port: u16,
    policy: &HostKeyPolicy,
) {
    let known_hosts = match policy {
        HostKeyPolicy::Strict { known_hosts } | HostKeyPolicy::AcceptNew { known_hosts } => {
            known_hosts
        }
    };
    let Ok(pinned) = known_host_keys_path(host, port, known_hosts) else {
        return;
    };
    if pinned.is_empty() {
        return;
    }
    let pinned: Vec<_> = pinned.iter().map(|(_, k)| k.algorithm()).collect();
    let mut order: Vec<_> = pinned.clone();
    order.extend(
        config
            .preferred
            .key
            .iter()
            .filter(|a| !pinned.contains(a))
            .cloned(),
    );
    config.preferred.key = std::borrow::Cow::Owned(order);
}

/// Create the `known_hosts` file (and parent directory) if absent, so a first connection's
/// "unknown key" check returns `Ok(false)` rather than an io error. `create(true).append(true)` is
/// idempotent, so no `exists()` pre-check (which would be a TOCTOU race) is needed.
fn ensure_known_hosts(path: &Path) -> std::io::Result<()> {
    let mut dir_opts = std::fs::DirBuilder::new();
    dir_opts.recursive(true);
    let mut file_opts = std::fs::OpenOptions::new();
    file_opts.create(true).append(true);
    // We may be creating `~/.ssh` and its contents. OpenSSH refuses to use a group- or
    // world-writable `~/.ssh`, and the umask alone does not guarantee that — so set the modes we
    // want rather than inheriting whatever the process happens to have. Only applied on creation;
    // an existing file's permissions are the user's business, not ours.
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        dir_opts.mode(0o700);
        file_opts.mode(0o600);
    }
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            dir_opts.create(dir)?;
        }
    }
    // Create-only. Opening for append aborted the connection whenever `known_hosts` existed but was
    // not writable by us — a read-only shared file such as `/etc/ssh/ssh_known_hosts` made every
    // `accept-new` connection fail with an opaque I/O error, even when the pinned key matched.
    match file_opts.create_new(true).open(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// The russh client callback: routes host-key verification through [`verify_host_key`].
struct CairnHandler {
    host: String,
    port: u16,
    policy: HostKeyPolicy,
}

// russh's `Handler` uses RPITIT (`-> impl Future + Send`), not `#[async_trait]`; implement with a
// plain `async fn`, which is compatible.
impl client::Handler for CairnHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, russh::Error> {
        // An io failure (e.g. unwritable known_hosts under AcceptNew) surfaces as a connection error,
        // not a silent host-key rejection.
        verify_host_key(&self.policy, &self.host, self.port, server_public_key)
            .map_err(russh::Error::IO)
    }
}

/// Establish an SSH/SFTP connection and return a ready [`SftpVfs`].
///
/// # Errors
/// [`VfsError::Timeout`] on connect/auth timeout, [`VfsError::Connection`] for transport or host-key
/// failures, [`VfsError::Auth`] if authentication is rejected or the credential family is not SSH.
#[must_use = "the established SftpVfs must be used or the connection is dropped"]
pub async fn ssh_connect(
    conn: ConnectionId,
    params: &SshConnectParams,
    cred: &CredentialSecret,
) -> Result<SftpVfs<RealSftp>, VfsError> {
    let ssh = match cred {
        CredentialSecret::Ssh(s) => s,
        // A non-SSH credential was referenced for an SSH connection.
        _ => return Err(VfsError::Auth),
    };

    // Keepalives give a transport-level backstop so a dead/half-open peer is detected on an
    // established session (no explicit per-op timeout in the SFTP adapter yet).
    let mut config = client::Config {
        keepalive_interval: Some(Duration::from_secs(15)),
        keepalive_max: 3,
        ..client::Config::default()
    };
    // Ask for the algorithms this host is already pinned under, first. russh offers a fixed
    // preference list (Ed25519 ahead of RSA) and never reorders it by `known_hosts`, the way
    // OpenSSH's `order_hostkeyalgs()` does. Without this, a host pinned only under RSA whose server
    // also offers Ed25519 would present the Ed25519 key, match nothing, and — now that an unmatched
    // key for a pinned host is refused rather than learned — fail to connect at all. Preferring the
    // pinned algorithm makes the server send the key we hold, which also means an impostor cannot
    // pick an algorithm to dodge the pin.
    prefer_pinned_algorithms(&mut config, &params.host, params.port, &params.host_key);
    let config = Arc::new(config);
    let handler = CairnHandler {
        host: params.host.clone(),
        port: params.port,
        policy: params.host_key.clone(),
    };

    let mut handle = tokio::time::timeout(
        params.connect_timeout,
        client::connect(config, (params.host.as_str(), params.port), handler),
    )
    .await
    .map_err(|_| VfsError::Timeout(params.connect_timeout))?
    .map_err(connection_error)?;

    let authed = tokio::time::timeout(
        params.auth_timeout,
        authenticate(&mut handle, &params.user, ssh),
    )
    .await
    .map_err(|_| VfsError::Timeout(params.auth_timeout))??;
    if !authed {
        return Err(VfsError::Auth);
    }

    // Open the SFTP subsystem and wrap the stream in a session. Bounded by a timeout: a server that
    // accepts auth then stalls the channel/subsystem/SFTP-version exchange must not hang the task
    // (russh keepalives only detect total silence, not a stalled-but-alive peer).
    let session = tokio::time::timeout(params.connect_timeout, async {
        let channel = handle
            .channel_open_session()
            .await
            .map_err(connection_error)?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(connection_error)?;
        SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| VfsError::Connection(Box::new(e)))
    })
    .await
    .map_err(|_| VfsError::Timeout(params.connect_timeout))??;

    Ok(SftpVfs::new(conn, RealSftp::new(session)))
}

/// Run the credential's auth method against the open handle. Returns whether auth succeeded;
/// transport errors map to [`VfsError`].
async fn authenticate(
    handle: &mut Handle<CairnHandler>,
    user: &str,
    cred: &SshCredential,
) -> Result<bool, VfsError> {
    match cred {
        SshCredential::Password(p) => Ok(handle
            .authenticate_password(user, p.expose_secret())
            .await
            .map_err(connection_error)?
            .success()),
        SshCredential::PrivateKey {
            key_pem,
            passphrase,
        } => {
            // Discard the decode error detail deliberately: it could distinguish "bad passphrase"
            // from "bad key format" — an oracle. `VfsError::Auth` carries no such signal.
            let key = decode_secret_key(
                key_pem.expose_secret(),
                passphrase.as_ref().map(ExposeSecret::expose_secret),
            )
            .map_err(|_| VfsError::Auth)?;
            let hash = rsa_hash(handle, key.algorithm()).await;
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
            Ok(handle
                .authenticate_publickey(user, key)
                .await
                .map_err(connection_error)?
                .success())
        }
        SshCredential::PrivateKeyFile { path, passphrase } => {
            // Read the key file at connect time. The path is non-secret (stored in the vault
            // as a reference); the bytes are transient — never stored in Cairn's vault.
            // Reading at connect time means key rotation on disk is reflected immediately.
            //
            // Wrap in `Zeroizing` so the PEM bytes are wiped on drop — CLAUDE.md §9 requires
            // secrets to be zeroized after use. A plain `String` would leave key material on
            // the heap until the allocator reuses that memory (without overwrite).
            let pem: zeroize::Zeroizing<String> = tokio::fs::read_to_string(path)
                .await
                // Discard the I/O error detail: path names must not appear in error messages.
                .map(zeroize::Zeroizing::new)
                .map_err(|_| VfsError::Auth)?;
            // Same oracle-avoidance as PrivateKey: discard decode error detail so we can't
            // distinguish "bad passphrase" from "bad key format".
            let key = decode_secret_key(&pem, passphrase.as_ref().map(ExposeSecret::expose_secret))
                .map_err(|_| VfsError::Auth)?;
            let hash = rsa_hash(handle, key.algorithm()).await;
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
            Ok(handle
                .authenticate_publickey(user, key)
                .await
                .map_err(connection_error)?
                .success())
        }
        SshCredential::Agent => authenticate_agent(handle, user).await,
        // A future SSH auth variant this build doesn't yet handle.
        _ => Err(VfsError::Auth),
    }
}

/// Public-key auth via the platform SSH agent. The agent connection is platform-specific — russh's
/// `connect_env` (`$SSH_AUTH_SOCK`) is Unix-only — so we branch on target here and hand the concrete
/// agent client to the generic [`agent_publickey_auth`]. No key material is held by Cairn.
#[cfg(unix)]
async fn authenticate_agent(
    handle: &mut Handle<CairnHandler>,
    user: &str,
) -> Result<bool, VfsError> {
    use russh::keys::agent::client::AgentClient;
    let agent = AgentClient::connect_env()
        .await
        .map_err(|_| VfsError::Auth)?;
    agent_publickey_auth(handle, user, agent).await
}

/// Windows counterpart: connect to the OpenSSH named-pipe agent (Windows 10+'s built-in
/// `ssh-agent` service at `\\.\pipe\openssh-ssh-agent`), falling back to Pageant (PuTTY). russh
/// exposes these as separate constructors returning distinct stream types, so each arm dispatches
/// to the same generic [`agent_publickey_auth`].
#[cfg(windows)]
async fn authenticate_agent(
    handle: &mut Handle<CairnHandler>,
    user: &str,
) -> Result<bool, VfsError> {
    use russh::keys::agent::client::AgentClient;
    // OpenSSH's agent is the common case on modern Windows; try it first. Only a real success stops
    // here: if the pipe is reachable but no key authenticates (`Ok(false)`) — or it errors mid-auth —
    // fall through to Pageant, where the user's key may actually live. The OpenSSH agent service is
    // often present but empty (auto-started by other tools), so returning on mere reachability would
    // strand a Pageant-held key.
    if let Ok(agent) = AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await {
        if let Ok(true) = agent_publickey_auth(handle, user, agent).await {
            return Ok(true);
        }
    }
    // Pageant (PuTTY) as the fallback / terminal path. If no agent was reachable at all this surfaces
    // as `VfsError::Auth`, matching the Unix arm when `$SSH_AUTH_SOCK` is unset.
    let agent = AgentClient::connect_pageant()
        .await
        .map_err(|_| VfsError::Auth)?;
    agent_publickey_auth(handle, user, agent).await
}

/// Try every agent-held public key against the server, generic over the agent's transport stream
/// (Unix socket, Windows named pipe, or Pageant) so the platform arms above share one implementation.
/// The bound mirrors russh's `Signer for AgentClient<S>` impl (`AsyncRead + AsyncWrite + Unpin +
/// Send`), which is also enough for `request_identities`.
async fn agent_publickey_auth<S>(
    handle: &mut Handle<CairnHandler>,
    user: &str,
    mut agent: russh::keys::agent::client::AgentClient<S>,
) -> Result<bool, VfsError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    use russh::keys::agent::AgentIdentity;
    let identities = agent
        .request_identities()
        .await
        .map_err(|_| VfsError::Auth)?;
    for ident in identities {
        // Standard agent keys; agent-held certificates are a follow-up.
        let AgentIdentity::PublicKey { key, .. } = ident else {
            continue;
        };
        // Choose the right RSA hash so agent-held RSA keys work against modern servers.
        let hash = rsa_hash(handle, key.algorithm()).await;
        // `authenticate_publickey_with` returns the signer's (agent's) error type, not russh::Error.
        if handle
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
            .map_err(|_| VfsError::Auth)?
            .success()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Pick the signature hash for a key. OpenSSH ≥8.8 rejects the legacy `ssh-rsa`/SHA-1, so an RSA key
/// must use rsa-sha2-256/512 — prefer the server's advertised best, else SHA-512. For Ed25519/ECDSA
/// the algorithm is fixed and the hash must be `None`.
async fn rsa_hash(handle: &Handle<CairnHandler>, algorithm: Algorithm) -> Option<HashAlg> {
    if algorithm.is_rsa() {
        handle
            .best_supported_rsa_hash()
            .await
            .ok()
            .flatten()
            .flatten()
            .or(Some(HashAlg::Sha512))
    } else {
        None
    }
}

/// Map a russh transport error to a [`VfsError::Connection`], preserving the error chain. Never
/// carries secret material (russh errors describe protocol/transport state, incl. host-key rejection).
fn connection_error(e: russh::Error) -> VfsError {
    VfsError::Connection(Box::new(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    // No comment: a server's wire host key has none, and `known_hosts` round-trips only the key
    // blob, so a commented key would spuriously compare unequal on re-read.
    const KEY1: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAfxRdr5RspdOM74m7aAk/bBnLazyU6TxXgHM/TT5jNA";
    const KEY2: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIwfUWs3P5Y44bfN7pkbRzDS3duf9lQk3qIKPeMtUsJY";
    /// A *different algorithm* for the same host — the case `check_known_hosts_path` reports as
    /// "no entry" rather than "changed", which is what made the swap acceptable.
    const KEY_OTHER_ALGO: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBFcILe/T/S9jSYaUWpzZZ92pRq3LI8GIbLZHwT8SGoRMQm6szceUsk1PLIWQrcI0p0CjDtDgNyZSRpInX/CfvhI=";

    fn pubkey(s: &str) -> PublicKey {
        PublicKey::from_openssh(s).unwrap()
    }

    #[test]
    fn strict_rejects_unknown_and_accepts_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let kh = dir.path().join("known_hosts");
        let key = pubkey(KEY1);
        let strict = HostKeyPolicy::Strict {
            known_hosts: kh.clone(),
        };
        // Unknown host (no known_hosts entry) is rejected.
        assert!(!verify_host_key(&strict, "h", 22, &key).unwrap());
        // Once recorded, the same key is accepted.
        learn_known_hosts_path("h", 22, &key, &kh).unwrap();
        assert!(verify_host_key(&strict, "h", 22, &key).unwrap());
    }

    #[test]
    fn accept_new_learns_then_rejects_a_changed_key() {
        let dir = tempfile::tempdir().unwrap();
        let kh = dir.path().join("known_hosts");
        let accept = HostKeyPolicy::AcceptNew {
            known_hosts: kh.clone(),
        };
        // First use: unknown key is accepted and recorded (TOFU).
        assert!(verify_host_key(&accept, "h", 22, &pubkey(KEY1)).unwrap());
        // The recorded key is still accepted...
        assert!(verify_host_key(&accept, "h", 22, &pubkey(KEY1)).unwrap());
        // ...but a *different* key for the same host is rejected, even under AcceptNew.
        assert!(!verify_host_key(&accept, "h", 22, &pubkey(KEY2)).unwrap());
        // ...and Strict now also accepts the learned key.
        let strict = HostKeyPolicy::Strict { known_hosts: kh };
        assert!(verify_host_key(&strict, "h", 22, &pubkey(KEY1)).unwrap());
    }

    /// Regression: a host pinned under one algorithm must not be silently re-pinned under another.
    /// `check_known_hosts_path` answers "is there a matching entry", and reports a *different
    /// algorithm* as `Ok(false)` — indistinguishable from a host never seen — so `AcceptNew` learned
    /// the impostor's key and trusted it. Anyone able to answer for the host could swap ed25519 for
    /// ECDSA and defeat the pin entirely.
    #[test]
    fn accept_new_rejects_a_key_of_a_different_algorithm_for_a_known_host() {
        let dir = tempfile::tempdir().unwrap();
        let kh = dir.path().join("known_hosts");
        let accept = HostKeyPolicy::AcceptNew {
            known_hosts: kh.clone(),
        };
        assert!(verify_host_key(&accept, "h", 22, &pubkey(KEY1)).unwrap());

        // The impostor presents a different algorithm for the same host.
        assert!(
            !verify_host_key(&accept, "h", 22, &pubkey(KEY_OTHER_ALGO)).unwrap(),
            "an algorithm swap defeated the host-key pin"
        );
        // …and it was not written to known_hosts, so a later connection is not poisoned either.
        assert!(!verify_host_key(
            &HostKeyPolicy::Strict {
                known_hosts: kh.clone()
            },
            "h",
            22,
            &pubkey(KEY_OTHER_ALGO)
        )
        .unwrap());
        // The genuine key still works.
        assert!(verify_host_key(&accept, "h", 22, &pubkey(KEY1)).unwrap());
    }

    /// `~/.ssh` and `known_hosts` are created with restrictive modes rather than whatever the
    /// process umask happens to be: OpenSSH refuses a group/world-writable `~/.ssh`, and a
    /// world-readable one leaks which hosts the user connects to.
    #[cfg(unix)]
    #[test]
    fn known_hosts_and_its_directory_are_created_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ssh_dir = dir.path().join(".ssh");
        let kh = ssh_dir.join("known_hosts");
        ensure_known_hosts(&kh).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode(&ssh_dir),
            0o700,
            "~/.ssh must not be group/world accessible"
        );
        assert_eq!(mode(&kh), 0o600);
    }

    /// The entry forms russh's matcher cannot see. Each one used to read as "never seen", so the
    /// impostor's key was accepted **and appended to the user's real `known_hosts`** — worse than
    /// the bug the guard closes. Found by the security review of the first version of this fix.
    #[test]
    fn entry_forms_russh_cannot_match_still_count_as_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            ("marker", "@cert-authority h ", true),
            ("comma", "other.example,h ", true),
            ("wildcard", "*.example.com ", false),
            ("case", "H ", true),
        ];
        for (name, prefix, exact_host) in cases {
            let kh = dir.path().join(name);
            let host = if exact_host { "h" } else { "a.example.com" };
            std::fs::write(&kh, format!("{prefix}{KEY1}\n")).unwrap();
            let accept = HostKeyPolicy::AcceptNew {
                known_hosts: kh.clone(),
            };
            assert!(
                !verify_host_key(&accept, host, 22, &pubkey(KEY_OTHER_ALGO)).unwrap(),
                "{name}: an algorithm swap was accepted"
            );
            let after = std::fs::read_to_string(&kh).unwrap();
            assert_eq!(
                after.lines().count(),
                1,
                "{name}: a trusting line was appended to the user's known_hosts"
            );
        }
    }

    /// A revoked key must never be accepted, under either policy — and `@revoked` outranks a plain
    /// entry for the same host, since russh's matcher does not look at markers at all.
    #[test]
    fn a_revoked_host_key_is_refused_under_both_policies() {
        let dir = tempfile::tempdir().unwrap();
        let kh = dir.path().join("known_hosts");
        std::fs::write(&kh, format!("@revoked h {KEY1}\n")).unwrap();
        for policy in [
            HostKeyPolicy::AcceptNew {
                known_hosts: kh.clone(),
            },
            HostKeyPolicy::Strict {
                known_hosts: kh.clone(),
            },
        ] {
            assert!(!verify_host_key(&policy, "h", 22, &pubkey(KEY1)).unwrap());
            assert!(!verify_host_key(&policy, "h", 22, &pubkey(KEY_OTHER_ALGO)).unwrap());
        }
        assert_eq!(std::fs::read_to_string(&kh).unwrap().lines().count(), 1);
    }

    /// An unreadable `known_hosts` must not read as "no pin". Absent means first contact; anything
    /// else means we cannot see the user's pins and must not gamble on there being none.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_known_hosts_is_not_treated_as_unpinned() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let kh = dir.path().join("known_hosts");
        std::fs::write(&kh, format!("h {KEY1}\n")).unwrap();
        std::fs::set_permissions(&kh, std::fs::Permissions::from_mode(0o200)).unwrap();

        let accept = HostKeyPolicy::AcceptNew {
            known_hosts: kh.clone(),
        };
        assert!(!verify_host_key(&accept, "h", 22, &pubkey(KEY_OTHER_ALGO)).unwrap());
        // Restore so the tempdir cleans up.
        std::fs::set_permissions(&kh, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    /// A `known_hosts` that exists but is not writable by us must not abort the connection: a
    /// read-only shared file (`/etc/ssh/ssh_known_hosts`) made every accept-new connection fail with
    /// an opaque I/O error, even when the pinned key matched.
    #[cfg(unix)]
    #[test]
    fn a_read_only_known_hosts_still_verifies() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let kh = dir.path().join("known_hosts");
        std::fs::write(&kh, format!("h {KEY1}\n")).unwrap();
        std::fs::set_permissions(&kh, std::fs::Permissions::from_mode(0o444)).unwrap();

        let accept = HostKeyPolicy::AcceptNew {
            known_hosts: kh.clone(),
        };
        assert!(verify_host_key(&accept, "h", 22, &pubkey(KEY1)).unwrap());
        assert!(!verify_host_key(&accept, "h", 22, &pubkey(KEY_OTHER_ALGO)).unwrap());
        std::fs::set_permissions(&kh, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    /// Non-default ports use OpenSSH's `[host]:port` form, and the guard must follow it — otherwise
    /// a host pinned on :2222 reads as unseen.
    #[test]
    fn the_guard_follows_the_bracketed_port_form() {
        let dir = tempfile::tempdir().unwrap();
        let kh = dir.path().join("known_hosts");
        let accept = HostKeyPolicy::AcceptNew {
            known_hosts: kh.clone(),
        };
        assert!(verify_host_key(&accept, "h", 2222, &pubkey(KEY1)).unwrap());
        assert!(!verify_host_key(&accept, "h", 2222, &pubkey(KEY_OTHER_ALGO)).unwrap());
        // A different port on the same host is a different pin, and is still learnable.
        assert!(verify_host_key(&accept, "h", 2200, &pubkey(KEY_OTHER_ALGO)).unwrap());
    }

    /// The pinned algorithm is offered first, so a server that has since added a stronger host key
    /// still presents the one we hold. Without this, refusing an unmatched key for a pinned host —
    /// which is the point of this fix — would break every host pinned under an older algorithm.
    #[test]
    fn pinned_algorithms_are_preferred_in_the_negotiation() {
        let dir = tempfile::tempdir().unwrap();
        let kh = dir.path().join("known_hosts");
        let ecdsa = pubkey(KEY_OTHER_ALGO);
        learn_known_hosts_path("h", 22, &ecdsa, &kh).unwrap();

        let mut config = client::Config::default();
        let default_order = config.preferred.key.to_vec();
        assert_ne!(
            default_order.first(),
            Some(&ecdsa.algorithm()),
            "fixture assumes ECDSA is not already russh's first choice"
        );

        prefer_pinned_algorithms(
            &mut config,
            "h",
            22,
            &HostKeyPolicy::AcceptNew {
                known_hosts: kh.clone(),
            },
        );
        assert_eq!(config.preferred.key.first(), Some(&ecdsa.algorithm()));
        // The rest of russh's order survives behind it, with no duplicates.
        let after = config.preferred.key.to_vec();
        assert_eq!(after.len(), default_order.len());
        for a in &default_order {
            assert!(
                after.contains(a),
                "{a:?} was dropped from the preference list"
            );
        }
    }

    /// An unpinned host leaves russh's own preference order untouched.
    #[test]
    fn an_unpinned_host_leaves_the_negotiation_order_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = client::Config::default();
        let before = config.preferred.key.to_vec();
        prefer_pinned_algorithms(
            &mut config,
            "never-seen",
            22,
            &HostKeyPolicy::AcceptNew {
                known_hosts: dir.path().join("known_hosts"),
            },
        );
        assert_eq!(config.preferred.key.to_vec(), before);
    }

    #[test]
    fn glob_and_pattern_matching_follows_sshd_rules() {
        assert!(host_matches_patterns("a.example.com", 22, "*.example.com"));
        assert!(host_matches_patterns("A.Example.COM", 22, "*.example.com"));
        assert!(host_matches_patterns("h", 22, "other,h,third"));
        assert!(host_matches_patterns("host1", 22, "host?"));
        // `*` in known_hosts spans dots, unlike a shell glob.
        assert!(host_matches_patterns(
            "a.b.example.com",
            22,
            "*.example.com"
        ));
        // A negation vetoes the whole line even when another pattern on it matched.
        assert!(!host_matches_patterns(
            "bad.example.com",
            22,
            "*.example.com,!bad.example.com"
        ));
        assert!(!host_matches_patterns("other.net", 22, "*.example.com"));
        // Bracketed form for a non-default port.
        assert!(host_matches_patterns("h", 2222, "[h]:2222"));
        assert!(!host_matches_patterns("h", 2222, "h"));
    }

    /// A host we have never seen is still learned — the swap guard must not break TOFU for a
    /// genuinely new host that happens to use a different algorithm from some *other* host.
    #[test]
    fn accept_new_still_learns_an_unseen_host_of_any_algorithm() {
        let dir = tempfile::tempdir().unwrap();
        let kh = dir.path().join("known_hosts");
        let accept = HostKeyPolicy::AcceptNew {
            known_hosts: kh.clone(),
        };
        assert!(verify_host_key(&accept, "one", 22, &pubkey(KEY1)).unwrap());
        assert!(verify_host_key(&accept, "two", 22, &pubkey(KEY_OTHER_ALGO)).unwrap());
        // Both are now pinned under Strict.
        let strict = HostKeyPolicy::Strict { known_hosts: kh };
        assert!(verify_host_key(&strict, "one", 22, &pubkey(KEY1)).unwrap());
        assert!(verify_host_key(&strict, "two", 22, &pubkey(KEY_OTHER_ALGO)).unwrap());
    }
}
