//! Shared helpers for tests that run the real `kagemusha` binary.

#![cfg(unix)]
#![allow(dead_code)] // not every target uses every helper

use std::time::{Duration, Instant};

pub const AGENT: &str = env!("CARGO_BIN_EXE_kagemusha");

/// Stops and reaps the child on drop — a failed test must leave neither
/// the agent nor its app running. Owns NO fixture dir: cleanup belongs
/// to `TestDir`, so a spawn retry can drop a failed AgentProc without
/// taking shared fixtures down with it.
pub struct AgentProc {
    pub child: std::process::Child,
    /// File receiving the child's stdout (the JSON log) and stderr (a
    /// startup error returned from `main`). The agent's own "hook
    /// listener up" line in it is the process-specific readiness proof
    /// for `spawn_until_port` — a squatter socket can't fake it. The app
    /// inherits both streams too, so the file may hold arbitrary
    /// non-UTF-8 app output — `log_announces_port` reads it lossily.
    pub log: std::path::PathBuf,
}

impl AgentProc {
    /// Spawn `command` with stdout and stderr sharing one handle on
    /// `log` (truncated first), so a failed start leaves its cause in
    /// the file `spawn_until_port` quotes when it gives up.
    pub fn spawn(command: &mut std::process::Command, log: std::path::PathBuf) -> Self {
        let stdout = std::fs::File::create(&log).expect("create agent log");
        let stderr = stdout.try_clone().expect("share agent log with stderr");
        let child = command
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .expect("spawn kagemusha");
        Self { child, log }
    }
}

impl Drop for AgentProc {
    fn drop(&mut self) {
        // SIGTERM first: the agent forwards it to the app's process group
        // and exits once the app is gone. A bare SIGKILL would leave the
        // app (e.g. `sleep 60`) running as an orphan.
        if let Ok(None) = self.child.try_wait() {
            unsafe {
                libc::kill(self.child.id() as i32, libc::SIGTERM);
            }
            wait_exit(&mut self.child, Duration::from_secs(2));
        }
        // Still running: the app ignored the forwarded SIGTERM and holds
        // the agent. SIGKILL its group while the agent still parents it —
        // killing the agent alone would orphan the whole group. Unreaped,
        // the pid is still ours, so `pgrep -P` can't hit a reused one.
        if let Ok(None) = self.child.try_wait() {
            kill_child_groups(self.child.id());
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// SIGKILL the process group of each direct child of `parent` — the app
/// and any in-flight hooks.d script each lead their own. `pgrep -P`
/// exists on macOS and Linux; without it this is a no-op. A child in the
/// test's own group is killed alone, never the group.
fn kill_child_groups(parent: u32) {
    let Ok(out) = std::process::Command::new("pgrep")
        .args(["-P", &parent.to_string()])
        .output()
    else {
        return;
    };
    let own = unsafe { libc::getpgrp() };
    for pid in String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|p| p.parse::<i32>().ok())
    {
        let target = match unsafe { libc::getpgid(pid) } {
            pgid if pgid <= 0 => continue, // already gone
            pgid if pgid != own => -pgid,
            _ => pid,
        };
        unsafe {
            libc::kill(target, libc::SIGKILL);
        }
    }
}

/// Owns a per-test fixture dir and removes it on drop.
pub struct TestDir(pub std::path::PathBuf);

impl Drop for TestDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

impl std::ops::Deref for TestDir {
    type Target = std::path::PathBuf;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Fresh per-test dir — leftovers from an interrupted run must not let
/// fs assertions pass without this run having done the work. The
/// sequence number keeps concurrent tests apart: `SystemTime` on macOS
/// has only microsecond resolution, so two tests with the same tag got
/// the same dir, and one test's cleanup removed the other's files.
pub fn fresh_dir(tag: &str) -> TestDir {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "kagemusha-{tag}-{}-{}-{seq}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    TestDir(dir)
}

/// Listener on an ephemeral port, bound to the agent's own address
/// (`0.0.0.0`, see `hooks::serve`). On macOS a socket gets `FD_CLOEXEC`
/// only after socket(2) returns, so a child that another test thread
/// spawns in that window inherits the probe, and the probe keeps
/// listening after we drop it. An exact-address probe makes the agent's
/// bind fail on such a leak (EADDRINUSE), and `spawn_until_port` retries.
/// A `127.0.0.1` probe let the agent's wildcard bind succeed beside the
/// leak, and the leak took the test's 127.0.0.1 connections, then reset
/// them.
pub fn port_probe() -> std::net::TcpListener {
    std::net::TcpListener::bind("0.0.0.0:0").unwrap()
}

/// A port no socket holds right now — see `port_probe`.
pub fn free_port() -> u16 {
    port_probe().local_addr().unwrap().port()
}

/// Strip every env var the agent's config reads — ambient host
/// `KAGEMUSHA_*`/`OTEL_*` values must never leak into a spawned agent.
/// Driven by `config::KNOWN_ENV_VARS`, which a config.rs unit test keeps
/// in step with the `ENV_*` constants `apply_env` reads. `RUST_LOG` is
/// pinned to `info`: `spawn_until_port` waits for an INFO line that an
/// ambient `RUST_LOG=warn` would filter out.
pub fn strip_agent_env(command: &mut std::process::Command) {
    for k in kagemusha::config::KNOWN_ENV_VARS {
        command.env_remove(k);
    }
    command.env("RUST_LOG", "info");
}

/// Write `body` to `<dir>/<name>` and mark it executable — the hooks.d
/// fixture every suite needs. `dir` must exist.
pub fn write_script(dir: &std::path::Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    let mut perm = std::fs::metadata(&p).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&p, perm).unwrap();
}

/// Exit code or 128+signal — a signal death must not look like a timeout.
pub fn status_code(st: std::process::ExitStatus) -> i32 {
    if let Some(c) = st.code() {
        return c;
    }
    use std::os::unix::process::ExitStatusExt;
    if let Some(s) = st.signal() {
        return 128 + s;
    }
    1
}

pub fn wait_exit(child: &mut std::process::Child, timeout: Duration) -> Option<i32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(Some(st)) = child.try_wait() {
            return Some(status_code(st));
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Poll until `path` exists (app-installed readiness marker).
pub fn wait_file(path: &std::path::Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        assert!(Instant::now() < deadline, "timeout waiting for {path:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Wait until the file contains `needle` — guards against reading the
/// target file mid-write (exists() fires while env is still writing).
pub fn wait_file_contains(path: &std::path::Path, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(s) = std::fs::read_to_string(path)
            && s.contains(needle)
        {
            return s;
        }
        assert!(
            Instant::now() < deadline,
            "timeout waiting for {needle:?} in {path:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Poll until `pid` has exited. A zombie counts as exited: with no
/// reaper above the test (a container whose PID 1 never calls `wait`),
/// a SIGKILLed orphan stays a zombie and `kill(pid, 0)` still succeeds.
pub fn wait_gone(pid: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if unsafe { libc::kill(pid, 0) } != 0 || is_zombie(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Poll until `pid` no longer exists — its parent has reaped it. Unlike
/// `wait_gone`, a zombie still counts as present: for the agent's app
/// leader this proves the agent collected the exit status.
pub fn wait_reaped(pid: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if unsafe { libc::kill(pid, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(target_os = "linux")]
fn is_zombie(pid: i32) -> bool {
    // State is the first field after the parenthesised comm, which may
    // itself contain ')'.
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            s.rsplit_once(')')
                .map(|(_, rest)| rest.trim_start().starts_with('Z'))
        })
        .unwrap_or(false)
}

/// No `/proc` to ask: rely on init (launchd on macOS) reaping orphans.
#[cfg(not(target_os = "linux"))]
fn is_zombie(_pid: i32) -> bool {
    false
}

/// Last lines of an agent log, for a spawn failure's panic message —
/// the fixture dir holding the log is removed during the unwind.
fn log_tail(log: &std::path::Path) -> String {
    let bytes = std::fs::read(log).unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(20)..].join("\n")
}

/// True once the child's own log records the hook listener bound to
/// `port`. Emitted by this process only, so a squatter's socket — even
/// one answering HTTP — can't fake it.
fn log_announces_port(log: &std::path::Path, port: u16) -> bool {
    // The app shares this stdout and may write non-UTF-8 bytes — read
    // lossily so app junk can't blind the readiness check.
    let Ok(s) = std::fs::read(log) else {
        return false;
    };
    let needle = format!(":{port}");
    String::from_utf8_lossy(&s)
        .lines()
        .any(|l| l.contains("hook listener up") && l.contains(&needle))
}

/// Wait for the agent's hook port — retrying the whole spawn when the
/// port TOCTOU loses the race (bind→drop→squatter, including a probe
/// leaked into another test's child — see `port_probe`). `spawn` must
/// produce a fresh child bound to a fresh port each call.
pub fn spawn_until_port<F>(mut spawn: F) -> (AgentProc, u16)
where
    F: FnMut(u16) -> AgentProc,
{
    let mut last_log = String::new();
    for _ in 0..3 {
        let port = free_port();
        let mut agent = spawn(port);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if agent.child.try_wait().unwrap().is_some() {
                break; // died — retry with a new port
            }
            if log_announces_port(&agent.log, port) {
                // Announced by *this* child; if it died right after
                // binding, the attempt is lost — retry on a fresh port
                // instead of burning the deadline.
                if agent.child.try_wait().unwrap().is_none() {
                    return (agent, port);
                }
                break;
            }
            if Instant::now() >= deadline {
                panic!(
                    "agent did not start listening within 5s; log tail:\n{}",
                    log_tail(&agent.log)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // A retry may truncate this same log path — keep this attempt's.
        last_log = log_tail(&agent.log);
    }
    panic!("agent failed to start after 3 attempts; last log tail:\n{last_log}");
}
