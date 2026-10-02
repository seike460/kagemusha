use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tracing::warn;

use crate::ctx::Reaper;
use crate::types::HookKind;

/// Directory-scan bound — a hostile hooks.d cannot pin unbounded memory.
const MAX_SCAN: usize = 1024;
/// At most this many scripts (alphabetically first) run per hook.
const MAX_SCRIPTS: usize = 64;
/// Per-script budget; also capped by whatever time the hook has left.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(10);
/// Don't bother spawning a script when less than this remains.
const MIN_LEFT: Duration = Duration::from_millis(50);
/// `PATH` for hooks.d scripts when the agent itself has none.
const FALLBACK_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
/// Poll granularity while waiting for a script's exit status.
const WAIT_POLL: Duration = Duration::from_millis(5);
/// How long to wait for the reaper after a group kill before giving up.
const REAP_WAIT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub struct ScriptOutcome {
    pub path: PathBuf,
    pub ok: bool,
    pub detail: String,
}

/// Kills the child's whole process group when dropped — covers the
/// cancellation path: dropping a std `Child` kills nothing, let alone its
/// grandchildren. Must be `disarm()`ed once the child has exited
/// normally, otherwise a recycled pgid could take the hit.
struct GroupKillGuard {
    pid: Option<u32>,
}

impl GroupKillGuard {
    fn new(pid: u32) -> Self {
        Self { pid: Some(pid) }
    }

    fn disarm(&mut self) {
        self.pid = None;
    }

    /// Signal the group WITHOUT disarming: only `disarm()` clears the
    /// pid. Drop may retry the kill — members forked into the group
    /// after the first SIGKILL must not outlive the hook.
    fn kill_group(&self) {
        if let Some(pid) = self.pid {
            // The script is a process-group leader (pgid == pid); kill the
            // whole group so background jobs can't outlive the hook.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}

impl Drop for GroupKillGuard {
    fn drop(&mut self) {
        self.kill_group();
    }
}

/// Run every executable in `<hooks_dir>/<hook>/` in sorted filename order,
/// inside one shared `budget`. Spent time is subtracted per script; when the
/// budget is gone, remaining scripts are skipped so the pipeline always fits
/// inside the platform's deadline.
///
/// Script stdout/stderr go to the agent's own stdout/stderr, which lands in
/// the MicroVM's log stream — nothing is buffered into agent memory.
/// Scripts run in their own process group with a clean environment; a
/// timed-out script is SIGKILLed together with its whole group, so no
/// background job outlives suspend/terminate.
///
/// Exit statuses come from the central `Reaper` (the agent is PID 1): the
/// supervisor drains `waitpid(-1)` and every waiter polls the map. A
/// direct `try_wait` on the child is used as a fallback so this function
/// also works without a running supervisor (unit tests).
///
/// Fail-open by contract: errors are collected and returned, never thrown —
/// a broken user script must not wedge the lifecycle hook (ADR-002).
pub async fn run_dir(
    hooks_dir: &Path,
    hook: HookKind,
    budget: Duration,
    env_extra: &[(String, String)],
    reaper: &Reaper,
) -> Vec<ScriptOutcome> {
    // Charge directory scanning to the same budget — the hook deadline
    // must cover everything this call does. Per-script reap waits must
    // stay inside it too so they cannot eat the caller's reserved flush
    // time.
    let budget_end = crate::ctx::deadline_after(budget);
    let dir = hooks_dir.join(hook.path_segment());
    let paths = match scan_bounded(&dir, budget_end, crate::ctx::FS_PERMITS.clone()).await {
        Scan::Scripts(v) => v,
        Scan::Missing => return Vec::new(),
        Scan::Failed(detail) => {
            return vec![ScriptOutcome {
                path: dir,
                ok: false,
                detail,
            }];
        }
    };

    let mut out = Vec::new();
    for path in paths {
        let left = crate::ctx::remaining(budget_end);
        if left < MIN_LEFT {
            break;
        }
        let per_script = left.min(SCRIPT_TIMEOUT);
        let mut cmd = std::process::Command::new(&path);
        // Own process group: a timeout must kill grandchildren too, not
        // just the script's shell.
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        // Allowlisted env only: the startup scrub drops the agent's own
        // credential vars, but operator-set secrets (`AWS_*`, …) stay in
        // its env and must never reach user scripts.
        cmd.env_clear()
            .env(
                "PATH",
                std::env::var("PATH").unwrap_or_else(|_| FALLBACK_PATH.to_string()),
            )
            .env("KAGEMUSHA_HOOK", hook.path_segment())
            .env("KAGEMUSHA_HOOK_DIR", &dir)
            .envs(env_extra.iter().cloned())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                out.push(ScriptOutcome {
                    ok: false,
                    detail: format!("spawn: {e}"),
                    path,
                });
                continue;
            }
        };
        let pid = child.id();
        // Declared after `child` so it drops first on cancellation: group
        // kill, then the leader's zombie is left to the central reaper.
        let mut guard = GroupKillGuard::new(pid);
        // Anything recorded before this instant belongs to a previous
        // owner's recycled pid — `take_since` keeps it from masquerading
        // as this script's exit, while a fresh record (the drain may
        // already have reaped an instant-exit) stays claimable.
        let spawned_at = Instant::now();

        let outcome = loop {
            match wait_status_once(reaper, pid, spawned_at, &mut child) {
                WaitHit::Status(raw) => break Ok(Some(raw)),
                // ECHILD + empty map: either the supervisor is mid-drain
                // (waitpid consumed the zombie, `record` lands a moment
                // later) or a Lost-for-good. One poll interval separates
                // the two — a take then returns Some for the mid-drain.
                WaitHit::Lost => {
                    tokio::time::sleep(WAIT_POLL).await;
                    break Ok(reaper.take_since(pid, spawned_at));
                }
                WaitHit::Pending => {}
            }
            if spawned_at.elapsed() >= per_script {
                break Err(());
            }
            tokio::time::sleep(WAIT_POLL).await;
        };

        out.push(match outcome {
            Err(()) => {
                guard.kill_group();
                // Bounded wait for the kill to be reaped; a D-state zombie
                // must not stall the pipeline. Bounded by REAP_WAIT *and*
                // the shared budget's end so this cannot eat the time the
                // caller reserved for the telemetry flush.
                let reap_end = crate::ctx::deadline_after(REAP_WAIT).min(budget_end);
                let hit = loop {
                    match wait_status_once(reaper, pid, spawned_at, &mut child) {
                        WaitHit::Pending => {
                            let left = crate::ctx::remaining(reap_end);
                            if left.is_zero() {
                                break WaitHit::Pending;
                            }
                            tokio::time::sleep(left.min(WAIT_POLL)).await;
                        }
                        h => break h,
                    }
                };
                // Status observed or Lost: the child is dead and its pgid
                // is freed — disarm so Drop's kill can't hit a recycled
                // pgid. Still Pending = maybe-alive → stay armed so Drop
                // retries the group kill.
                if matches!(hit, WaitHit::Status(_) | WaitHit::Lost) {
                    guard.disarm();
                }
                ScriptOutcome {
                    ok: false,
                    detail: format!("timeout after {}ms", per_script.as_millis()),
                    path,
                }
            }
            Ok(raw) => {
                // Clean exit (or lost status — the child is dead either
                // way): disarm so a recycled pgid can't take a hit.
                // Detached grandchildren of a *successful* script are left
                // alone — only timeouts/cancellation kill groups.
                guard.disarm();
                match raw {
                    Some(raw) => {
                        let (success, code) = decode_status(raw);
                        ScriptOutcome {
                            ok: success,
                            detail: match code {
                                Some(c) => format!("exit {c}"),
                                None => "signalled".to_string(),
                            },
                            path,
                        }
                    }
                    None => ScriptOutcome {
                        ok: false,
                        detail: "exit status lost (reaped before wait)".to_string(),
                        path,
                    },
                }
            }
        });
    }
    out
}

/// Result of the blocking-pool directory scan.
enum Scan {
    /// Executable script paths, sorted; already capped at `MAX_SCRIPTS`.
    Scripts(Vec<PathBuf>),
    /// The hook dir doesn't exist — normal, no scripts.
    Missing,
    /// `read_dir` failed or the dir exceeded `MAX_SCAN`.
    Failed(String),
}

/// `scan` on the blocking pool under `permits` — a wedged filesystem
/// keeps each `spawn_blocking` closure alive forever (a dropped
/// JoinHandle does NOT stop it), so without the permit a hung fs would
/// leak one pool thread per forged POST until every blocking op stalls.
/// The permit is held *inside* the closure: even a timed-out scan still
/// occupies a slot until the syscall actually returns. Healthy scans
/// are ms-fast — wait briefly for a slot rather than failing a burst
/// of concurrent hooks spuriously.
async fn scan_bounded(dir: &Path, budget_end: Instant, permits: Arc<Semaphore>) -> Scan {
    let permit_wait = crate::ctx::remaining(budget_end).min(crate::ctx::FS_PERMIT_WAIT);
    let permit = match tokio::time::timeout(permit_wait, permits.acquire_owned()).await {
        Ok(Ok(p)) => p,
        _ => {
            warn!(dir = %dir.display(), "hooks.d scan slots exhausted; skipped");
            return Scan::Failed("scan slots exhausted".to_string());
        }
    };
    let scan_dir = dir.to_path_buf();
    match tokio::time::timeout(
        crate::ctx::remaining(budget_end),
        tokio::task::spawn_blocking(move || {
            let _permit = permit; // held until the syscalls actually return
            scan(&scan_dir)
        }),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => Scan::Failed(format!("scan task: {e}")),
        Err(_) => Scan::Failed("scan exceeded hook budget".to_string()),
    }
}

/// Sorted, executable-only listing of `dir` — the blocking half of
/// `run_dir`'s scan, run via `spawn_blocking` so a wedged fs can't stall
/// the executor. Fail-open: errors become a single `Failed` outcome.
fn scan(dir: &Path) -> Scan {
    let mut entries = match std::fs::read_dir(dir) {
        // Bound the scan itself: a giant hooks.d must not pin memory.
        // MAX_SCAN + 1 is a truncation sentinel so we can tell "exactly
        // at the cap" from "cut off".
        Ok(e) => e
            .filter_map(|e| e.ok())
            .take(MAX_SCAN + 1)
            .collect::<Vec<_>>(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Scan::Missing,
        Err(e) => return Scan::Failed(format!("read_dir: {e}")),
    };
    if entries.len() > MAX_SCAN {
        // Truncating before sort would pick an arbitrary subset of a huge
        // dir — the contract is "alphabetically first N". Refuse instead.
        warn!(dir = %dir.display(), "hooks.d exceeds {MAX_SCAN} entries; skipping entirely");
        return Scan::Failed(format!("directory exceeds {MAX_SCAN} entries; skipped"));
    }
    entries.sort_by_key(|e| e.file_name());
    // Filter executables BEFORE the run cap so non-executable files
    // don't burn slots and drop runnable scripts past position 64.
    let mut paths: Vec<PathBuf> = entries
        .iter()
        .map(|e| e.path())
        .filter(|p| is_executable(p))
        .collect();
    if paths.len() > MAX_SCRIPTS {
        warn!(
            dir = %dir.display(),
            runnable = paths.len(),
            "hooks.d has too many scripts; only the first {MAX_SCRIPTS} run"
        );
        paths.truncate(MAX_SCRIPTS);
    }
    Scan::Scripts(paths)
}

/// Result of one non-blocking status check.
enum WaitHit {
    /// Got the raw waitpid status.
    Status(i32),
    /// Still running (or not yet reaped).
    Pending,
    /// Another waiter reaped the child and its status is unrecoverable —
    /// waiting any longer can never succeed.
    Lost,
}

/// One non-blocking status check: the central reaper map first (covers the
/// supervisor-reaped case), then a direct `try_wait` (covers the
/// no-supervisor case, e.g. unit tests; an ECHILD means the reaper got
/// there first, so check the map again — a miss there is `Lost`).
/// `since` is the child's spawn instant: `take_since` refuses records
/// older than it, so a stale recycled-pid entry can't answer for a
/// still-running script.
fn wait_status_once(
    reaper: &Reaper,
    pid: u32,
    since: Instant,
    child: &mut std::process::Child,
) -> WaitHit {
    if let Some(raw) = reaper.take_since(pid, since) {
        return WaitHit::Status(raw);
    }
    match child.try_wait() {
        Ok(Some(status)) => WaitHit::Status(status_to_raw(status)),
        Ok(None) => WaitHit::Pending,
        Err(_) => match reaper.take_since(pid, since) {
            Some(raw) => WaitHit::Status(raw),
            None => WaitHit::Lost,
        },
    }
}

fn status_to_raw(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.into_raw()
}

/// Decode a raw waitpid status into `(success, exit_code)` with
/// `ExitStatus` semantics (signal death → `None` code).
fn decode_status(raw: i32) -> (bool, Option<i32>) {
    use std::os::unix::process::ExitStatusExt;
    let status = std::process::ExitStatus::from_raw(raw);
    (status.success(), status.code())
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    fn script(dir: &Path, name: &str, body: &str) {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(&p).unwrap().permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(&p, perm).unwrap();
        }
    }

    #[tokio::test]
    async fn runs_scripts_in_sorted_order() {
        let dir = TempDir::new("hooks");
        let sub = dir.join("suspend");
        std::fs::create_dir_all(&sub).unwrap();
        let log = dir.join("order.txt");
        script(&sub, "20-second", &format!("#!/bin/sh\necho b >> {log:?}"));
        script(&sub, "10-first", &format!("#!/bin/sh\necho a >> {log:?}"));

        let out = run_dir(
            &dir,
            HookKind::Suspend,
            Duration::from_secs(30),
            &[],
            &Reaper::default(),
        )
        .await;
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|o| o.ok));
        let order = std::fs::read_to_string(&log).unwrap();
        assert_eq!(order, "a\nb\n");
    }

    #[tokio::test]
    async fn collects_failures_without_throwing() {
        let dir = TempDir::new("hooks2");
        let sub = dir.join("run");
        std::fs::create_dir_all(&sub).unwrap();
        script(&sub, "bad", "#!/bin/sh\nexit 3");
        script(&sub, "good", "#!/bin/sh\nexit 0");

        let out = run_dir(
            &dir,
            HookKind::Run,
            Duration::from_secs(30),
            &[],
            &Reaper::default(),
        )
        .await;
        assert_eq!(out.len(), 2);
        assert!(!out[0].ok && out[0].detail == "exit 3");
        assert!(out[1].ok);
    }

    #[tokio::test]
    async fn missing_dir_is_empty_not_error() {
        let out = run_dir(
            Path::new("/nonexistent/kagemusha"),
            HookKind::Run,
            Duration::from_secs(1),
            &[],
            &Reaper::default(),
        )
        .await;
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn passes_hook_env() {
        let dir = TempDir::new("hooks3");
        let sub = dir.join("resume");
        std::fs::create_dir_all(&sub).unwrap();
        let log = dir.join("env.txt");
        script(
            &sub,
            "env",
            &format!("#!/bin/sh\necho $KAGEMUSHA_HOOK:$EXTRA > {log:?}"),
        );

        let out = run_dir(
            &dir,
            HookKind::Resume,
            Duration::from_secs(30),
            &[("EXTRA".into(), "hello".into())],
            &Reaper::default(),
        )
        .await;
        assert!(out[0].ok);
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "resume:hello\n");
    }

    /// Regression for the armed-Drop retry: `kill_group` must NOT consume
    /// the pid — group members forked after the first SIGKILL are only
    /// reachable by the armed Drop retry. Verified signal-free: the child
    /// joins OUR group, so no group has pgid == its pid and kill(-pid) is
    /// a guaranteed no-op.
    #[test]
    fn kill_guard_stays_armed_after_kill() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let mut g = GroupKillGuard::new(child.id());
        g.kill_group(); // ESRCH — nothing is actually signalled
        assert!(g.pid.is_some(), "kill_group must not consume the pid");
        g.disarm();
        assert!(g.pid.is_none());
        drop(g); // disarmed — no signal
        let _ = child.kill();
        let _ = child.wait();
    }

    /// The fs-op permit bound is what makes a wedged filesystem leak at
    /// most N blocking threads — a full semaphore must fail the scan
    /// fast, not queue the POST forever.
    #[tokio::test]
    async fn scan_slots_exhaustion_fails_open() {
        let dir = std::env::temp_dir().join(format!("kagemusha-scan0-{}", std::process::id()));
        let end = Instant::now() + Duration::from_secs(30);
        let s = scan_bounded(&dir, end, Arc::new(Semaphore::new(0))).await;
        match s {
            Scan::Failed(d) => assert_eq!(d, "scan slots exhausted"),
            _ => panic!("expected scan-slots-exhausted failure"),
        }
    }

    /// The documented cap: more than MAX_SCRIPTS executables in a hook
    /// dir → only the alphabetically-first 64 run.
    #[tokio::test]
    async fn scan_runs_at_most_max_scripts() {
        let dir = TempDir::new("maxs");
        let sub = dir.join("run");
        std::fs::create_dir_all(&sub).unwrap();
        for i in 0..(MAX_SCRIPTS + 5) {
            script(&sub, &format!("{i:03}"), "#!/bin/sh\nexit 0");
        }
        let out = run_dir(
            &dir,
            HookKind::Run,
            Duration::from_secs(60),
            &[],
            &Reaper::default(),
        )
        .await;
        assert_eq!(out.len(), MAX_SCRIPTS, "only the first {MAX_SCRIPTS} run");
    }

    /// The documented refusal: over MAX_SCAN directory entries → the
    /// whole scan fails (a giant hostile dir can't pin memory and a
    /// truncated sort can't pick an arbitrary subset).
    #[tokio::test]
    async fn scan_refuses_dir_over_max_scan() {
        let dir = TempDir::new("maxi");
        let sub = dir.join("run");
        std::fs::create_dir_all(&sub).unwrap();
        for i in 0..(MAX_SCAN + 1) {
            std::fs::write(sub.join(format!("{i:05}")), "").unwrap();
        }
        let out = run_dir(
            &dir,
            HookKind::Run,
            Duration::from_secs(30),
            &[],
            &Reaper::default(),
        )
        .await;
        assert_eq!(out.len(), 1);
        assert!(!out[0].ok);
        assert!(
            out[0].detail.contains(&MAX_SCAN.to_string()),
            "expected an over-cap detail, got: {}",
            out[0].detail
        );
    }
}
