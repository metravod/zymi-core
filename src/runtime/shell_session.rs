//! Persistent shell session pool (ADR-0015).
//!
//! Provides a [`ShellSessionPool`] that maintains one long-lived `bash` process
//! per `stream_id`. Shell state (cwd, env vars, functions) persists across
//! calls within the same session. Each command's output boundary is detected
//! using per-call UUID sentinels so that a previous command's output can never
//! be mistaken for a boundary marker.
//!
//! **Slice 1** — standalone primitive + tests.
//! **Slice 2** — wired into [`super::Runtime`] / [`super::CatalogActionExecutor`],
//! emits [`crate::events::EventKind::ShellSessionStarted`] /
//! [`crate::events::EventKind::ShellSessionClosed`], idle reaper task.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::config::ShellConfig;
use crate::events::bus::EventBus;
use crate::events::{Event, EventKind};

/// Sentinel prefix baked into stdout/stderr boundary markers.
const SENTINEL_PREFIX: &str = "__ZYMI_END_";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Output captured from a single command execution in a persistent shell.
#[derive(Debug)]
pub struct ShellOutput {
    /// Standard output of the command (sentinel lines stripped).
    pub stdout: String,
    /// Standard error output of the command (sentinel lines stripped).
    pub stderr: String,
    /// Exit code of the command. `124` when the command was killed by timeout.
    pub exit_code: i32,
    /// `true` when the command was killed because it exceeded the timeout.
    pub timed_out: bool,
}

/// Errors from shell session operations.
#[derive(Debug, Error)]
pub enum ShellError {
    #[error("failed to spawn shell: {0}")]
    Spawn(std::io::Error),

    #[error("failed to write to shell stdin: {0}")]
    Stdin(std::io::Error),

    #[error("shell session died unexpectedly")]
    SessionDead,

    #[error("command timed out after {0:?}")]
    Timeout(Duration),

    #[error("sentinel parse error: {0}")]
    SentinelParse(String),
}

// ---------------------------------------------------------------------------
// ShellSession — one long-lived bash process
// ---------------------------------------------------------------------------

/// A persistent shell backed by a long-lived `bash` process.
///
/// State (cwd, env vars, shell functions) persists across [`Self::execute`]
/// calls. Created lazily by [`ShellSessionPool`] on first use per `stream_id`.
struct ShellSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr: BufReader<ChildStderr>,
    #[allow(dead_code)]
    stream_id: String,
    last_used: Instant,
    #[allow(dead_code)]
    cwd_at_creation: PathBuf,
}

impl ShellSession {
    /// Spawn a new shell with piped stdio.
    ///
    /// When `interactive` is true the shell is started with `-i`; otherwise
    /// `--norc --noprofile` is used for a lean, predictable environment.
    fn spawn(
        stream_id: &str,
        project_root: &Path,
        shell_path: &str,
        interactive: bool,
    ) -> Result<Self, ShellError> {
        let mut cmd = tokio::process::Command::new(shell_path);
        if interactive {
            cmd.arg("-i");
        } else {
            // --norc / --noprofile are bash/zsh-specific; passing them to
            // dash or other POSIX shells causes an immediate exit.
            let shell_name = Path::new(shell_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if matches!(shell_name, "bash" | "zsh") {
                cmd.args(["--norc", "--noprofile"]);
            }
        }
        let mut child = cmd
            .current_dir(project_root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(ShellError::Spawn)?;

        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr: BufReader::new(stderr),
            stream_id: stream_id.to_string(),
            last_used: Instant::now(),
            cwd_at_creation: project_root.to_path_buf(),
        })
    }

    /// `true` if the underlying bash process has not exited.
    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Run `command` in this session and return captured output.
    async fn execute(
        &mut self,
        command: &str,
        timeout: Duration,
    ) -> Result<ShellOutput, ShellError> {
        let pid = self.child.id();
        let result = run_in_session(
            &mut self.stdin,
            &mut self.stdout,
            &mut self.stderr,
            pid,
            command,
            timeout,
        )
        .await;

        // If recovery after timeout failed, kill the session so the pool
        // can respawn on the next call.
        if matches!(&result, Err(ShellError::Timeout(_))) {
            let _ = self.child.kill().await;
        }

        result
    }
}

impl Drop for ShellSession {
    fn drop(&mut self) {
        // Safety net for panics — explicit `close_session` is the contract.
        let _ = self.child.start_kill();
    }
}

// ---------------------------------------------------------------------------
// ShellSessionPool
// ---------------------------------------------------------------------------

/// Pool of persistent shell sessions, keyed by `stream_id`.
///
/// Each `stream_id` gets at most one session. Sessions are created lazily on
/// the first [`Self::run_command`] call and recycled until they die, are
/// explicitly closed, or are reaped for idleness.
///
/// When an [`EventBus`] is attached (via [`Self::with_bus`]), the pool emits
/// [`EventKind::ShellSessionStarted`] on session creation and
/// [`EventKind::ShellSessionClosed`] on close / idle reap.
pub struct ShellSessionPool {
    sessions: Mutex<HashMap<String, ShellSession>>,
    idle_timeout: Duration,
    max_sessions: usize,
    shell_path: String,
    interactive: bool,
    bus: Option<Arc<EventBus>>,
}

impl ShellSessionPool {
    /// Create a pool with the given idle-reap timeout and no event bus.
    ///
    /// Uses default config values for max_sessions, shell_path, and
    /// interactive. Prefer [`Self::from_config`] for production use.
    pub fn new(idle_timeout: Duration) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            idle_timeout,
            max_sessions: 32,
            shell_path: "/bin/bash".to_string(),
            interactive: false,
            bus: None,
        }
    }

    /// Create a pool from a [`ShellConfig`].
    pub fn from_config(config: &ShellConfig) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            idle_timeout: Duration::from_secs(config.idle_timeout_seconds),
            max_sessions: config.max_sessions,
            shell_path: config.shell_path.clone(),
            interactive: config.interactive,
            bus: None,
        }
    }

    /// Attach an [`EventBus`] so the pool emits lifecycle events.
    pub fn with_bus(mut self, bus: Arc<EventBus>) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Spawn a background task that calls [`Self::reap_idle`] every 60 s.
    ///
    /// Uses a [`std::sync::Weak`] reference: the reaper exits automatically
    /// when all `Arc<ShellSessionPool>` handles are dropped.
    pub fn start_reaper(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                match weak.upgrade() {
                    Some(pool) => pool.reap_idle().await,
                    None => break,
                }
            }
        });
    }

    /// Run `command` in the persistent session for `stream_id`.
    ///
    /// If no session exists (or the previous one died), a fresh shell is
    /// spawned with `cwd = project_root`. When the pool is at capacity
    /// (`max_sessions`), the least-recently-used session is evicted first.
    pub async fn run_command(
        &self,
        stream_id: &str,
        command: &str,
        timeout: Duration,
        project_root: &Path,
    ) -> Result<ShellOutput, ShellError> {
        // Take the session out so we don't hold the lock during execution.
        let existing = self.sessions.lock().await.remove(stream_id);

        let (mut session, spawned) = match existing {
            Some(mut s) => {
                if s.is_alive() {
                    (s, false)
                } else {
                    (
                        ShellSession::spawn(
                            stream_id,
                            project_root,
                            &self.shell_path,
                            self.interactive,
                        )?,
                        true,
                    )
                }
            }
            None => (
                ShellSession::spawn(
                    stream_id,
                    project_root,
                    &self.shell_path,
                    self.interactive,
                )?,
                true,
            ),
        };

        if spawned {
            // Evict LRU session if we're at capacity.
            self.evict_lru_if_full().await;
            self.emit_started(stream_id, &session).await;
        }

        let result = session.execute(command, timeout).await;

        // Return the session to the pool if it survived.
        if session.is_alive() {
            session.last_used = Instant::now();
            self.sessions
                .lock()
                .await
                .insert(stream_id.to_string(), session);
        }

        result
    }

    /// Explicitly close (kill) the session for `stream_id`.
    pub async fn close_session(&self, stream_id: &str, reason: &str) {
        if let Some(mut session) = self.sessions.lock().await.remove(stream_id) {
            let _ = session.child.kill().await;
            self.emit_closed(stream_id, reason).await;
        }
    }

    /// Kill sessions that have been idle longer than `idle_timeout`.
    pub async fn reap_idle(&self) {
        let expired = {
            let now = Instant::now();
            let mut sessions = self.sessions.lock().await;
            let keys: Vec<String> = sessions
                .iter()
                .filter(|(_, s)| now.duration_since(s.last_used) > self.idle_timeout)
                .map(|(k, _)| k.clone())
                .collect();
            for key in &keys {
                if let Some(mut s) = sessions.remove(key) {
                    let _ = s.child.start_kill();
                }
            }
            keys
        };
        // Lock released — emit events outside the critical section.
        for key in &expired {
            self.emit_closed(key, "idle").await;
        }
    }

    // -- capacity management -------------------------------------------------

    /// If the pool is at `max_sessions`, evict the least-recently-used
    /// session to make room for a new one.
    async fn evict_lru_if_full(&self) {
        let evicted_key = {
            let mut sessions = self.sessions.lock().await;
            if sessions.len() < self.max_sessions {
                return;
            }
            // Find the LRU session.
            let lru_key = sessions
                .iter()
                .min_by_key(|(_, s)| s.last_used)
                .map(|(k, _)| k.clone());
            if let Some(key) = &lru_key {
                if let Some(mut s) = sessions.remove(key) {
                    let _ = s.child.start_kill();
                }
            }
            lru_key
        };
        if let Some(key) = evicted_key {
            self.emit_closed(&key, "lru_evict").await;
        }
    }

    // -- event helpers --------------------------------------------------------

    async fn emit_started(&self, stream_id: &str, session: &ShellSession) {
        if let Some(bus) = &self.bus {
            let pid = session.child.id().unwrap_or(0);
            let event = Event::new(
                stream_id.to_string(),
                EventKind::ShellSessionStarted {
                    stream_id: stream_id.to_string(),
                    pid,
                    shell_path: self.shell_path.clone(),
                },
                "shell_pool".into(),
            );
            let _ = bus.publish(event).await;
        }
    }

    async fn emit_closed(&self, stream_id: &str, reason: &str) {
        if let Some(bus) = &self.bus {
            let event = Event::new(
                stream_id.to_string(),
                EventKind::ShellSessionClosed {
                    stream_id: stream_id.to_string(),
                    reason: reason.to_string(),
                },
                "shell_pool".into(),
            );
            let _ = bus.publish(event).await;
        }
    }
}

impl Drop for ShellSessionPool {
    fn drop(&mut self) {
        // `get_mut()` — we own the Mutex, no async lock needed.
        for session in self.sessions.get_mut().values_mut() {
            let _ = session.child.start_kill();
        }
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Build the wrapped command string with stdout + stderr sentinels.
///
/// After `{ command ; }` finishes, `$?` holds its exit code. Two `printf`s
/// emit boundary markers: one to stdout (carrying the exit code) and one to
/// stderr (marker only). The per-call `run_id` ensures a previous command's
/// output can never spoof a future boundary.
fn build_wrapped_command(command: &str, run_id: &str) -> String {
    format!(
        "{{ {command} ; }}; printf '\\n{SENTINEL_PREFIX}{run_id}__%d\\n' \"$?\"; \
         printf '\\n{SENTINEL_PREFIX}{run_id}__\\n' >&2\n"
    )
}

/// Core execution: write the wrapped command, read output with timeout,
/// recover on timeout by killing the foreground process.
async fn run_in_session(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
    stderr: &mut BufReader<ChildStderr>,
    bash_pid: Option<u32>,
    command: &str,
    timeout: Duration,
) -> Result<ShellOutput, ShellError> {
    let run_id = Uuid::new_v4().to_string();
    let wrapped = build_wrapped_command(command, &run_id);

    stdin
        .write_all(wrapped.as_bytes())
        .await
        .map_err(ShellError::Stdin)?;
    stdin.flush().await.map_err(ShellError::Stdin)?;

    // --- Phase 1: read within the caller's timeout --------------------------
    match tokio::time::timeout(timeout, read_output(stdout, stderr, &run_id)).await {
        Ok(result) => return result,
        Err(_) => { /* fall through to recovery */ }
    }

    // --- Phase 2: timeout — kill the foreground command ----------------------
    if let Some(pid) = bash_pid {
        kill_children_of(pid).await;
    }

    // The shell should now run the sentinel printfs.  Give it a short grace
    // period to flush the markers.
    match tokio::time::timeout(Duration::from_secs(5), read_output(stdout, stderr, &run_id)).await
    {
        Ok(result) => {
            let mut output = result?;
            output.exit_code = 124;
            output.timed_out = true;
            Ok(output)
        }
        Err(_) => Err(ShellError::Timeout(timeout)),
    }
}

/// Read stdout until its sentinel, then stderr until its sentinel.
///
/// Sequential order matches the ADR specification.  A deadlock is possible if
/// stderr fills the OS pipe buffer (~64 KiB) before stdout is fully consumed;
/// this is acceptable for Slice 1 and documented in ADR-0015 §2.
async fn read_output(
    stdout: &mut BufReader<ChildStdout>,
    stderr: &mut BufReader<ChildStderr>,
    run_id: &str,
) -> Result<ShellOutput, ShellError> {
    let (stdout_text, exit_suffix) = read_until_sentinel(stdout, run_id).await?;
    let (stderr_text, _) = read_until_sentinel(stderr, run_id).await?;
    let exit_code = parse_exit_code(&exit_suffix)?;
    Ok(ShellOutput {
        stdout: stdout_text,
        stderr: stderr_text,
        exit_code,
        timed_out: false,
    })
}

/// Read lines from `reader` until a sentinel for `run_id` appears.
///
/// Returns `(collected_output, sentinel_suffix)`.  On stdout the suffix is
/// the decimal exit code; on stderr it is empty.
async fn read_until_sentinel(
    reader: &mut (impl AsyncBufRead + Unpin),
    run_id: &str,
) -> Result<(String, String), ShellError> {
    let sentinel = format!("{SENTINEL_PREFIX}{run_id}__");
    let mut output = String::new();
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|_| ShellError::SessionDead)?;
        if n == 0 {
            return Err(ShellError::SessionDead);
        }
        let trimmed = line.trim();
        if let Some(suffix) = trimmed.strip_prefix(&sentinel) {
            return Ok((output.trim_end().to_string(), suffix.to_string()));
        }
        output.push_str(&line);
    }
}

fn parse_exit_code(suffix: &str) -> Result<i32, ShellError> {
    if suffix.is_empty() {
        return Ok(0);
    }
    suffix
        .parse::<i32>()
        .map_err(|e| ShellError::SentinelParse(format!("bad exit code '{suffix}': {e}")))
}

/// Kill direct children of `parent_pid` so that a timed-out command dies
/// without killing the persistent shell itself.
async fn kill_children_of(parent_pid: u32) {
    let _ = tokio::process::Command::new("pkill")
        .args(["-TERM", "-P", &parent_pid.to_string()])
        .status()
        .await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn project_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    // -- cwd persistence -----------------------------------------------------

    #[tokio::test]
    async fn cwd_persists_across_commands() {
        let pool = ShellSessionPool::new(Duration::from_secs(60));
        let root = project_root();

        pool.run_command("cwd", "cd /tmp", Duration::from_secs(5), &root)
            .await
            .unwrap();

        let out = pool
            .run_command("cwd", "pwd", Duration::from_secs(5), &root)
            .await
            .unwrap();
        assert_eq!(out.stdout.trim(), "/tmp", "cwd did not persist");
        assert_eq!(out.exit_code, 0);
    }

    // -- env persistence ------------------------------------------------------

    #[tokio::test]
    async fn env_persists_across_commands() {
        let pool = ShellSessionPool::new(Duration::from_secs(60));
        let root = project_root();

        pool.run_command("env", "export ZYMI_T=hello42", Duration::from_secs(5), &root)
            .await
            .unwrap();

        let out = pool
            .run_command("env", "echo $ZYMI_T", Duration::from_secs(5), &root)
            .await
            .unwrap();
        assert_eq!(out.stdout.trim(), "hello42");
    }

    // -- exit codes -----------------------------------------------------------

    #[tokio::test]
    async fn exit_code_success() {
        let pool = ShellSessionPool::new(Duration::from_secs(60));
        let out = pool
            .run_command("ec0", "true", Duration::from_secs(5), &project_root())
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0);
    }

    #[tokio::test]
    async fn exit_code_failure() {
        let pool = ShellSessionPool::new(Duration::from_secs(60));
        let out = pool
            .run_command("ec1", "false", Duration::from_secs(5), &project_root())
            .await
            .unwrap();
        assert_eq!(out.exit_code, 1);
    }

    #[tokio::test]
    async fn exit_code_specific() {
        let pool = ShellSessionPool::new(Duration::from_secs(60));
        let out = pool
            .run_command(
                "ec42",
                "bash -c 'exit 42'",
                Duration::from_secs(5),
                &project_root(),
            )
            .await
            .unwrap();
        assert_eq!(out.exit_code, 42);
    }

    // -- stderr ---------------------------------------------------------------

    #[tokio::test]
    async fn stderr_captured_separately() {
        let pool = ShellSessionPool::new(Duration::from_secs(60));
        let out = pool
            .run_command(
                "stderr",
                "echo err_msg >&2",
                Duration::from_secs(5),
                &project_root(),
            )
            .await
            .unwrap();
        assert!(out.stderr.contains("err_msg"), "stderr not captured");
        assert!(out.stdout.is_empty(), "stdout should be empty");
        assert_eq!(out.exit_code, 0);
    }

    // -- timeout kills command, not session -----------------------------------

    #[tokio::test]
    async fn timeout_kills_command_not_session() {
        let pool = ShellSessionPool::new(Duration::from_secs(60));
        let root = project_root();

        let out = pool
            .run_command("tmout", "sleep 60", Duration::from_secs(2), &root)
            .await;

        match out {
            Ok(o) => assert!(o.timed_out, "expected timed_out flag"),
            Err(ShellError::Timeout(_)) => { /* unrecoverable — acceptable */ }
            Err(e) => panic!("unexpected error: {e}"),
        }

        // Session should still work for new commands.
        let out = pool
            .run_command("tmout", "echo alive", Duration::from_secs(5), &root)
            .await
            .unwrap();
        assert_eq!(out.stdout.trim(), "alive");
    }

    // -- sentinel spoofing ----------------------------------------------------

    #[tokio::test]
    async fn sentinel_spoofing_defeated() {
        let pool = ShellSessionPool::new(Duration::from_secs(60));
        let root = project_root();

        let out = pool
            .run_command(
                "spoof",
                "echo '__ZYMI_END_fake-uuid__0'",
                Duration::from_secs(5),
                &root,
            )
            .await
            .unwrap();

        assert!(
            out.stdout.contains("__ZYMI_END_fake-uuid__0"),
            "fake sentinel should appear in output, not be consumed"
        );
        assert_eq!(out.exit_code, 0);

        // Session still works correctly after the spoofed line.
        let out = pool
            .run_command("spoof", "echo ok", Duration::from_secs(5), &root)
            .await
            .unwrap();
        assert_eq!(out.stdout.trim(), "ok");
    }

    // -- stream isolation -----------------------------------------------------

    #[tokio::test]
    async fn different_streams_are_independent() {
        let pool = ShellSessionPool::new(Duration::from_secs(60));
        let root = project_root();

        pool.run_command("s1", "export X=one", Duration::from_secs(5), &root)
            .await
            .unwrap();
        pool.run_command("s2", "export X=two", Duration::from_secs(5), &root)
            .await
            .unwrap();

        let o1 = pool
            .run_command("s1", "echo $X", Duration::from_secs(5), &root)
            .await
            .unwrap();
        let o2 = pool
            .run_command("s2", "echo $X", Duration::from_secs(5), &root)
            .await
            .unwrap();

        assert_eq!(o1.stdout.trim(), "one");
        assert_eq!(o2.stdout.trim(), "two");
    }

    // -- close and respawn ----------------------------------------------------

    #[tokio::test]
    async fn close_session_and_respawn() {
        let pool = ShellSessionPool::new(Duration::from_secs(60));
        let root = project_root();

        pool.run_command("close", "export M=before", Duration::from_secs(5), &root)
            .await
            .unwrap();
        pool.close_session("close", "workflow_end").await;

        // New session — previous state is gone.
        let out = pool
            .run_command("close", "echo ${M:-empty}", Duration::from_secs(5), &root)
            .await
            .unwrap();
        assert_eq!(out.stdout.trim(), "empty");
    }

    // -- idle reaping ---------------------------------------------------------

    #[tokio::test]
    async fn reap_idle_removes_old_sessions() {
        let pool = ShellSessionPool::new(Duration::from_millis(50));
        let root = project_root();

        pool.run_command("reap", "export MARKER=yes", Duration::from_secs(5), &root)
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;
        pool.reap_idle().await;

        // Session was reaped — state is gone.
        let out = pool
            .run_command("reap", "echo ${MARKER:-gone}", Duration::from_secs(5), &root)
            .await
            .unwrap();
        assert_eq!(out.stdout.trim(), "gone");
    }

    // -- multi-line output ----------------------------------------------------

    #[tokio::test]
    async fn multi_line_output() {
        let pool = ShellSessionPool::new(Duration::from_secs(60));
        let out = pool
            .run_command(
                "multi",
                "echo line1; echo line2; echo line3",
                Duration::from_secs(5),
                &project_root(),
            )
            .await
            .unwrap();
        let lines: Vec<&str> = out.stdout.lines().collect();
        assert_eq!(lines, vec!["line1", "line2", "line3"]);
    }

    // -- no output command ----------------------------------------------------

    #[tokio::test]
    async fn no_output_command() {
        let pool = ShellSessionPool::new(Duration::from_secs(60));
        let out = pool
            .run_command("noop", "true", Duration::from_secs(5), &project_root())
            .await
            .unwrap();
        assert!(out.stdout.is_empty());
        assert_eq!(out.exit_code, 0);
    }

    // -- from_config ----------------------------------------------------------

    #[tokio::test]
    async fn from_config_respects_settings() {
        use crate::config::ShellConfig;

        let config = ShellConfig {
            idle_timeout_seconds: 30,
            max_sessions: 4,
            shell_path: "/bin/bash".to_string(),
            interactive: false,
        };
        let pool = ShellSessionPool::from_config(&config);
        assert_eq!(pool.idle_timeout, Duration::from_secs(30));
        assert_eq!(pool.max_sessions, 4);
        assert_eq!(pool.shell_path, "/bin/bash");

        // Pool should work normally.
        let out = pool
            .run_command("cfg", "echo ok", Duration::from_secs(5), &project_root())
            .await
            .unwrap();
        assert_eq!(out.stdout.trim(), "ok");
    }

    // -- LRU eviction ---------------------------------------------------------

    #[tokio::test]
    async fn lru_eviction_when_at_max_sessions() {
        // Pool with max_sessions = 2.
        let pool = ShellSessionPool {
            sessions: Mutex::new(HashMap::new()),
            idle_timeout: Duration::from_secs(60),
            max_sessions: 2,
            shell_path: "/bin/bash".to_string(),
            interactive: false,
            bus: None,
        };
        let root = project_root();

        // Fill both slots.
        pool.run_command("s1", "export V=one", Duration::from_secs(5), &root)
            .await
            .unwrap();
        pool.run_command("s2", "export V=two", Duration::from_secs(5), &root)
            .await
            .unwrap();

        // Pool = {s1, s2}. s1 is LRU. Adding s3 should evict s1.
        pool.run_command("s3", "export V=three", Duration::from_secs(5), &root)
            .await
            .unwrap();

        // Pool = {s2, s3}. s1 was evicted — fresh session, no state.
        let o1 = pool
            .run_command("s1", "echo ${V:-evicted}", Duration::from_secs(5), &root)
            .await
            .unwrap();
        assert_eq!(o1.stdout.trim(), "evicted");
        // Note: spawning s1 evicted s2 (LRU of {s2,s3}), so pool = {s3, s1}.

        // s3 should still have its state (it was the most recent at eviction time).
        let o3 = pool
            .run_command("s3", "echo $V", Duration::from_secs(5), &root)
            .await
            .unwrap();
        assert_eq!(o3.stdout.trim(), "three");
    }

    // -- custom shell_path ----------------------------------------------------

    #[tokio::test]
    async fn custom_shell_path_sh() {
        let pool = ShellSessionPool {
            sessions: Mutex::new(HashMap::new()),
            idle_timeout: Duration::from_secs(60),
            max_sessions: 32,
            shell_path: "/bin/sh".to_string(),
            interactive: false,
            bus: None,
        };
        let out = pool
            .run_command("sh", "echo using_sh", Duration::from_secs(5), &project_root())
            .await
            .unwrap();
        assert_eq!(out.stdout.trim(), "using_sh");
    }
}
