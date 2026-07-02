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
use russh::keys::known_hosts::{check_known_hosts_path, learn_known_hosts_path};
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
                // Unknown host key: record it (best-effort) and accept this first connection.
                Ok(false) => {
                    let _ = learn_known_hosts_path(host, port, key, known_hosts);
                    true
                }
                // A *changed* key is always rejected (fail-safe).
                Err(_) => false,
            })
        }
    }
}

/// Create the `known_hosts` file (and parent directory) if absent, so a first connection's
/// "unknown key" check returns `Ok(false)` rather than an io error. `create(true).append(true)` is
/// idempotent, so no `exists()` pre-check (which would be a TOCTOU race) is needed.
fn ensure_known_hosts(path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    Ok(())
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
    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(15)),
        keepalive_max: 3,
        ..client::Config::default()
    });
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

/// Public-key auth via the platform SSH agent (`$SSH_AUTH_SOCK` on Unix, Pageant/named-pipe on
/// Windows) — `connect_env` handles the platform lookup. No key material is held by Cairn.
async fn authenticate_agent(
    handle: &mut Handle<CairnHandler>,
    user: &str,
) -> Result<bool, VfsError> {
    use russh::keys::agent::client::AgentClient;
    use russh::keys::agent::AgentIdentity;
    let mut agent = AgentClient::connect_env()
        .await
        .map_err(|_| VfsError::Auth)?;
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
}
