//! True end-to-end tests that drive the real `cairn` binary inside a `tmux` pane.
//!
//! **Why tmux:** the reducer is covered by pure unit tests and every screen by headless
//! `TestBackend` snapshots (`cairn-tui::scenarios`) — but neither can exercise the genuine
//! raw-mode / alternate-screen / TTY handoff, because a `TestBackend` is an in-memory buffer, not a
//! terminal. tmux gives us a real PTY: we launch `cairn` in a detached session, `send-keys`, and
//! `capture-pane` the result as plain text we can assert on. The highest-value case here is the
//! external-`$EDITOR` suspend/resume round-trip (RFC-0012 P2 / ADR-0011): it drives an interactive
//! editor that *reads stdin*, which only works if the editor shares Cairn's foreground process group
//! (the SIGTTIN fix) — a property no unit test can reach.
//!
//! **Env-guarded** (like the `CAIRN_IT_*` backend integration tests): these are a no-op unless
//! `CAIRN_IT_TMUX=1` is set *and* `tmux` is on `PATH`, so the default `cargo test` stays hermetic and
//! offline. CI runs them in a dedicated workflow (`.github/workflows/tui-e2e.yml`). Each test owns a
//! full-screen pane; they pass in parallel, but prefer `--test-threads=1` locally (as CI does) to
//! avoid pane contention on a busy or resource-constrained machine.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Absolute path to the freshly built `cairn` binary (cargo sets this for integration tests).
const CAIRN_BIN: &str = env!("CARGO_BIN_EXE_cairn");

/// How long to wait for an expected string to appear in the pane before failing.
const WAIT_TIMEOUT: Duration = Duration::from_secs(15);
/// Poll interval while waiting — short enough to feel instant, long enough not to spin.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Whether the suite should run at all: opt-in via `CAIRN_IT_TMUX` and `tmux` must be present.
fn enabled() -> bool {
    if std::env::var_os("CAIRN_IT_TMUX").is_none() {
        eprintln!("skipping tmux e2e: set CAIRN_IT_TMUX=1 to run");
        return false;
    }
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("skipping tmux e2e: `tmux` not found on PATH");
        return false;
    }
    true
}

/// Monotonic counter so parallel test threads get distinct session names.
static SESSION_SEQ: AtomicU32 = AtomicU32::new(0);

/// A dedicated tmux socket so the suite is fully hermetic w.r.t. any tmux the developer is running
/// on the default socket (and immune to their `~/.tmux.conf` global-key rebindings).
const TMUX_SOCKET: &str = "cairn-e2e";

/// A `tmux` command already targeting the dedicated [`TMUX_SOCKET`] (the `-L` must precede the
/// subcommand).
fn tmux() -> Command {
    let mut c = Command::new("tmux");
    c.args(["-L", TMUX_SOCKET]);
    c
}

/// A detached tmux session running one command, torn down on drop.
struct TmuxSession {
    name: String,
}

impl TmuxSession {
    /// Start `program` (with `env` overrides) in a detached 80×24 session whose start directory is
    /// `cwd`. The session name is unique per process + call so parallel tests never collide.
    fn start(cwd: &Path, program: &Path, env: &[(&str, &str)]) -> Self {
        let seq = SESSION_SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!("cairn-e2e-{}-{seq}", std::process::id());

        // Build `env K=V … <program>` so the child sees exactly the overrides we set (plus tmux's
        // own TERM). `env` is used rather than tmux's `set-environment` so it applies to the very
        // first process, deterministically.
        let mut shell = String::from("env");
        for (k, v) in env {
            shell.push_str(&format!(" {k}={}", shell_quote(v)));
        }
        shell.push(' ');
        shell.push_str(&shell_quote(&program.to_string_lossy()));

        let status = tmux()
            .args([
                "new-session",
                "-d",
                "-s",
                &name,
                "-x",
                "80",
                "-y",
                "24",
                "-c",
            ])
            .arg(cwd)
            .arg(&shell)
            .status()
            .expect("failed to spawn tmux new-session");
        assert!(status.success(), "tmux new-session failed for {name}");
        Self { name }
    }

    /// Send tmux key names (e.g. `["Down", "Enter"]`, `["F4"]`, `["q"]`).
    fn send_keys(&self, keys: &[&str]) {
        let mut cmd = tmux();
        cmd.args(["send-keys", "-t", &self.name]);
        cmd.args(keys);
        let status = cmd.status().expect("tmux send-keys failed to run");
        assert!(status.success(), "tmux send-keys failed");
    }

    /// Send a run of literal text (no key-name interpretation), e.g. a line typed into an editor.
    fn send_literal(&self, text: &str) {
        let status = tmux()
            .args(["send-keys", "-t", &self.name, "-l", text])
            .status()
            .expect("tmux send-keys -l failed to run");
        assert!(status.success(), "tmux send-keys -l failed");
    }

    /// Capture the current pane contents as plain text (trailing blank lines trimmed).
    fn capture(&self) -> String {
        let out = tmux()
            .args(["capture-pane", "-p", "-t", &self.name])
            .output()
            .expect("tmux capture-pane failed to run");
        String::from_utf8_lossy(&out.stdout).trim_end().to_string()
    }

    /// Poll the pane until `needle` appears, or fail with the last captured frame for debugging.
    /// Fast-fails if the session dies while waiting (a crash on launch) rather than blocking for the
    /// full timeout on an empty pane — the common real failure, and otherwise a useless diagnostic.
    fn wait_for(&self, needle: &str) {
        let start = Instant::now();
        let mut last = String::new();
        while start.elapsed() < WAIT_TIMEOUT {
            last = self.capture();
            if last.contains(needle) {
                return;
            }
            if !self.is_alive() {
                panic!("session exited before {needle:?} appeared; last frame:\n{last}");
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        panic!("timed out waiting for {needle:?} in pane; last frame:\n{last}");
    }

    /// Poll until `needle` is *no longer* on screen (e.g. an overlay was dismissed), or fail. The
    /// counterpart to [`wait_for`](Self::wait_for) — use a string exclusive to the state that must
    /// go away (an overlay's own titled border), never one that also appears in the base UI.
    fn wait_for_absence(&self, needle: &str) {
        let start = Instant::now();
        let mut last = String::new();
        while start.elapsed() < WAIT_TIMEOUT {
            last = self.capture();
            if !last.contains(needle) {
                return;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        panic!("timed out waiting for {needle:?} to disappear; last frame:\n{last}");
    }

    /// Whether the tmux session still exists (i.e. the program has not exited).
    fn is_alive(&self) -> bool {
        tmux()
            .args(["has-session", "-t", &self.name])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Poll until the session is gone (the program exited), or fail.
    fn wait_until_exited(&self) {
        let start = Instant::now();
        while start.elapsed() < WAIT_TIMEOUT {
            if !self.is_alive() {
                return;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        panic!("program did not exit; last frame:\n{}", self.capture());
    }
}

impl Drop for TmuxSession {
    fn drop(&mut self) {
        // Best-effort teardown so a failed assertion never leaks a live session. Silence stdout/
        // stderr: the session has usually already exited (a clean `q` quit), and tmux's "no such
        // session" complaint is just noise in the test log.
        let _ = tmux()
            .args(["kill-session", "-t", &self.name])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Minimal single-quote shell escaping for the `env …` command string we hand to tmux.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A throwaway working directory that also serves as a private `$HOME` (so no real vault/config
/// interferes), cleaned up on drop.
struct Sandbox {
    dir: PathBuf,
    home: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        // `fetch_add` (not `load`) so the suffix is genuinely unique even if two sandboxes share a
        // `tag` — isolation must not silently rest on callers always passing distinct tags.
        let seq = SESSION_SEQ.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("cairn-e2e-{}-{tag}-{seq}", std::process::id()));
        let dir = root.join("work");
        let home = root.join("home");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        Self { dir, home }
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let p = self.dir.join(name);
        std::fs::write(&p, contents).unwrap();
        p
    }

    fn env(&self) -> Vec<(&str, String)> {
        vec![
            ("HOME", self.home.to_string_lossy().into_owned()),
            ("TERM", "xterm-256color".to_owned()),
        ]
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Some(root) = self.dir.parent() {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

/// Launch `cairn` in `sandbox.dir` with `extra` env on top of the sandbox defaults.
fn launch_cairn(sandbox: &Sandbox, extra: &[(&str, &str)]) -> TmuxSession {
    let base = sandbox.env();
    let mut env: Vec<(&str, &str)> = base.iter().map(|(k, v)| (*k, v.as_str())).collect();
    env.extend_from_slice(extra);
    TmuxSession::start(&sandbox.dir, Path::new(CAIRN_BIN), &env)
}

#[test]
fn startup_renders_two_panes_and_quits_cleanly() {
    if !enabled() {
        return;
    }
    let sb = Sandbox::new("startup");
    sb.write("hello.txt", "hi\n");
    let session = launch_cairn(&sb, &[]);

    // The two-pane browser is up: the entry and the status-bar hints are on screen.
    session.wait_for("hello.txt");
    session.wait_for("quit"); // the `q quit` status-bar hint

    // `q` quits, restoring the terminal — the session (and thus the process) must exit.
    session.send_keys(&["q"]);
    session.wait_until_exited();
}

#[test]
fn navigate_into_subdirectory() {
    if !enabled() {
        return;
    }
    let sb = Sandbox::new("nav");
    std::fs::create_dir(sb.dir.join("subdir")).unwrap();
    std::fs::write(sb.dir.join("subdir").join("inner.txt"), "x\n").unwrap();
    let session = launch_cairn(&sb, &[]);

    session.wait_for("subdir");
    // Cursor starts on `..` (row 0); one Down lands on `subdir/`, Enter descends into it.
    session.send_keys(&["Down", "Enter"]);
    // The pane border shows the new cwd, and the child file is now listed.
    session.wait_for("inner.txt");

    session.send_keys(&["q"]);
    session.wait_until_exited();
}

#[test]
fn f3_pager_opens_and_closes_over_a_real_pty() {
    if !enabled() {
        return;
    }
    let sb = Sandbox::new("pager");
    sb.write("notes.txt", "alpha beta gamma\nsecond line\n");
    let session = launch_cairn(&sb, &[]);

    session.wait_for("notes.txt");
    // Cursor on `..`; Down to notes.txt, F3 opens the read-only pager overlay.
    session.send_keys(&["Down", "F3"]);
    session.wait_for("notes.txt — view"); // the pager's titled border
    session.wait_for("alpha beta gamma"); // file contents rendered in the overlay

    // `Esc` (Cancel) dismisses the overlay without quitting the app — the browser status bar
    // returns. (`q` is global Quit and would exit the whole app even from an overlay.)
    session.send_keys(&["Escape"]);
    // Assert on the pager's own titled border disappearing — `notes.txt — view` appears ONLY in the
    // open pager, so its absence proves the overlay actually closed. (The status-bar hint "F3 view"
    // is drawn on the bottom row even while the pager is open, so it would be a vacuous check.)
    session.wait_for_absence("notes.txt — view");
    assert!(
        session.is_alive(),
        "closing the pager must not quit the app"
    );

    session.send_keys(&["q"]);
    session.wait_until_exited();
}

/// The crown-jewel e2e: press F4 on a file → Cairn suspends the TUI, hands the real TTY to an
/// interactive `$EDITOR` that *reads a line from stdin*, then resumes. This exercises the entire
/// RFC-0012 P2 / ADR-0011 machinery over a real PTY — including the SIGTTIN foreground-process-group
/// fix, since the editor's blocking `read` only succeeds if it is in the terminal's foreground group.
#[test]
fn external_editor_suspend_resume_roundtrip() {
    if !enabled() {
        return;
    }
    let sb = Sandbox::new("editor");
    let target = sb.write("edit-me.txt", "original\n");

    // A fake `$EDITOR`: announce readiness, block reading one line from the inherited TTY, append it
    // to the file (the last argv element, after Cairn's `--` terminator), then exit. The blocking
    // `read` is the SIGTTIN probe: if the editor were in a background process group it would be
    // stopped here instead of reading.
    let editor = sb.dir.join("fake-editor.sh");
    {
        let mut f = std::fs::File::create(&editor).unwrap();
        writeln!(
            f,
            "#!/bin/sh\n\
             f=\"\"\n\
             for a in \"$@\"; do f=\"$a\"; done\n\
             printf 'FAKE-EDITOR-READY\\n'\n\
             IFS= read -r line\n\
             printf '%s\\n' \"$line\" >> \"$f\"\n"
        )
        .unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let editor_str = editor.to_string_lossy().into_owned();
    let session = launch_cairn(&sb, &[("EDITOR", editor_str.as_str()), ("VISUAL", "")]);

    session.wait_for("edit-me.txt");
    // Down onto edit-me.txt, F4 to edit it unconditionally.
    session.send_keys(&["Down", "F4"]);

    // The TUI has suspended and the editor now owns the terminal — it printed its readiness marker.
    session.wait_for("FAKE-EDITOR-READY");

    // Type a line and press Enter: the editor's `read` consumes it (proving it is the foreground
    // group and not SIGTTIN-stopped), appends it, and exits.
    let sentinel = "cairn-e2e-edited-line";
    session.send_literal(sentinel);
    session.send_keys(&["Enter"]);

    // Cairn resumes: it re-enters the alternate screen, repaints, and reports the edit — proving the
    // terminal was restored (not left cooked/torn) and the reducer ran EditFinished.
    session.wait_for("edited edit-me.txt");

    // The editor actually ran against the right path (argv `--` terminator + absolute path) and its
    // stdin read succeeded.
    let contents = std::fs::read_to_string(&target).unwrap();
    assert!(
        contents.contains(sentinel),
        "editor should have appended the typed line; file was:\n{contents}"
    );

    // The app is still fully alive and responsive after the round-trip: `q` quits cleanly, which it
    // could only do if raw mode was re-enabled and the input reader unparked.
    session.send_keys(&["q"]);
    session.wait_until_exited();
}
