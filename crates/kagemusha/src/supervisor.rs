//! Process supervision — the agent is the container entrypoint (PID 1).
//!
//! - Spawns the app in its own process group so signals reach the whole
//!   app tree, not just the launcher.
//! - Forwards catchable signals to the app's group.
//! - Reaps every dead child centrally (`waitpid(-1, WNOHANG)`): the app's
//!   own exit plus orphans reparented to us (detached hooks.d jobs).
//! - Drives the `/terminate` shutdown sequence: SIGTERM → grace → SIGKILL,
//!   then a final synchronous telemetry flush.
//! - Exits with the app's exit status (128+signal on signal death).

use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use tokio::sync::Notify;

use crate::ctx::AgentCtx;

/// Delay between the `/terminate` flag and the app shutdown — lets the
/// hook's HTTP response reach the platform before we start dismantling.
const TERMINATE_RESPONSE_DELAY: Duration = Duration::from_millis(300);
/// How often the zombie sweep runs as a backstop to SIGCHLD delivery.
const REAP_TICK: Duration = Duration::from_millis(250);
/// Poll granularity while waiting for the app during shutdown.
const SHUTDOWN_POLL: Duration = Duration::from_millis(50);
/// Slack on top of a connection's worst-case lifetime for the drain cap.
const DRAIN_SLACK: Duration = Duration::from_secs(5);

/// Env names carrying agent credentials — scrubbed from the app's
/// inherited environment AND from the agent's own environ at startup
/// (the app is untrusted; a same-uid app could read `/proc/1/environ`).
/// Headers hold tokens; endpoint URLs may embed userinfo credentials.
/// Uses the config.rs env-name constants, so the names can't drift, but
/// the list is kept by hand: a credential-bearing var added to
/// `apply_env` must be added here too.
/// A denylist can't catch operator-injected secrets (AWS_*, DATABASE_URL,
/// …) — real isolation also needs `KAGEMUSHA_APP_UID`/`GID`.
const SECRET_ENV_KEYS: &[&str] = &[
    crate::config::ENV_OTLP_HEADERS,
    crate::config::ENV_OTLP_HEADERS_FALLBACK,
    crate::config::ENV_OTLP_HEADERS_FILE,
    crate::config::ENV_OTLP_ENDPOINT,
    crate::config::ENV_OTLP_ENDPOINT_FALLBACK,
    // App-facing URLs can embed userinfo credentials too.
    crate::config::ENV_APP_HOOK_BASE,
    crate::config::ENV_APP_READY_URL,
];

/// Remove the credential-bearing vars from the AGENT's environment.
///
/// `remove_var` alone is NOT enough on Linux: `/proc/<pid>/environ` shows
/// the *initial* stack's environment region, which `setenv`/`remove_var`
/// never touch — a same-uid app could still read the original values
/// back. So we first overwrite the matching strings in place, then drop
/// the pointers.
///
/// # Safety
///
/// The process must still be single-threaded — call it in `main` before
/// the Tokio runtime spawns threads. Mutating `environ` while another
/// thread may read it is undefined behavior.
pub unsafe fn scrub_secret_env() {
    // SAFETY: the caller guarantees a single-threaded process.
    unsafe {
        wipe_environ_values(SECRET_ENV_KEYS);
    }
    for key in SECRET_ENV_KEYS {
        // SAFETY: as above.
        unsafe { std::env::remove_var(key) };
    }
}

/// Overwrite the VALUE of each matching `environ` entry with NULs, in
/// place — this is the only way to erase what procfs exposes, because
/// `/proc/self/environ` reads the original strings from the initial
/// stack region regardless of later setenv/remove_var calls.
///
/// SAFETY: must run while the process is single-threaded (startup),
/// before any `environ` mutation; only the target entries' bytes are
/// overwritten, other entries are untouched.
unsafe fn wipe_environ_values(keys: &[&str]) {
    #[cfg(target_vendor = "apple")]
    let envp = unsafe { *libc::_NSGetEnviron() };
    #[cfg(not(target_vendor = "apple"))]
    let envp = {
        // POSIX `environ` — libc crate doesn't expose it on Linux.
        unsafe extern "C" {
            static mut environ: *mut *mut libc::c_char;
        }
        unsafe { environ }
    };
    if envp.is_null() {
        return;
    }
    unsafe {
        let mut p = envp;
        while !(*p).is_null() {
            let s = *p;
            // Byte-level match: a non-UTF8 VALUE (obs-text header bytes
            // are legal) must not skip the wipe — only the ASCII key is
            // compared, the value is zeroed regardless of its encoding.
            let bytes = std::ffi::CStr::from_ptr(s).to_bytes();
            if let Some(eq) = bytes.iter().position(|&b| b == b'=') {
                let key = &bytes[..eq];
                let val_len = bytes.len() - eq - 1;
                if keys.iter().any(|k| k.as_bytes() == key) && val_len > 0 {
                    // Zero only the value part — the empty "KEY=" remains
                    // readable, which also documents that scrubbing ran.
                    libc::memset(s.add(eq + 1) as *mut libc::c_void, 0, val_len);
                }
            }
            p = p.add(1);
        }
    }
}

/// Spawn `command` as the supervised app and run the supervisor loop.
/// Returns the exit code the agent should itself exit with.
pub async fn run(ctx: Arc<AgentCtx>, command: &[std::ffi::OsString]) -> anyhow::Result<i32> {
    let Some(prog) = command.first() else {
        anyhow::bail!("empty app command");
    };
    let mut cmd = std::process::Command::new(prog);
    cmd.args(&command[1..]);
    // The app is untrusted: never let it inherit any credential-bearing
    // var in SECRET_ENV_KEYS.
    for key in SECRET_ENV_KEYS {
        cmd.env_remove(key);
    }
    {
        use std::os::unix::process::CommandExt;
        // Own process group (pgid == pid): signal forwarding targets the
        // whole app tree.
        cmd.process_group(0);
        // Drop privileges when configured: keeps root-owned files
        // (hooks.d, the OTLP headers file) out of the app's reach.
        // Setting only uid OR gid leaves the other at 0 — warn.
        match (ctx.cfg.app_uid, ctx.cfg.app_gid) {
            (Some(_), None) => {
                warn!("KAGEMUSHA_APP_UID set without KAGEMUSHA_APP_GID — app keeps egid=0")
            }
            (None, Some(_)) => {
                warn!("KAGEMUSHA_APP_GID set without KAGEMUSHA_APP_UID — app keeps euid=0")
            }
            (None, None) => {
                // The app is untrusted — running it with the agent's
                // (typically root) privileges leaves root-owned files
                // like hooks.d and the OTLP headers file in its reach.
                warn!(
                    "KAGEMUSHA_APP_UID/GID unset — app runs with the agent's privileges; \
                     set them to isolate the untrusted app"
                )
            }
            (Some(_), Some(_)) => {}
        }
        if ctx.cfg.app_uid.is_some() || ctx.cfg.app_gid.is_some() {
            let uid = ctx.cfg.app_uid;
            let gid = ctx.cfg.app_gid;
            // Do NOT use Command::uid/gid: std's uid branch tolerates an
            // EPERM from its internal setgroups(0,NULL) and continues —
            // in a user-ns without CAP_SETGID the app would keep PID 1's
            // supplementary groups. Doing the full drop inside pre_exec
            // (while euid is still 0) lets us fail the spawn on ANY
            // error — fail-closed: no app beats an app with root groups.
            // SAFETY: runs post-fork/pre-exec; all three calls are
            // async-signal-safe; error aborts the exec and surfaces as a
            // spawn error.
            unsafe {
                cmd.pre_exec(move || {
                    if libc::setgroups(0, std::ptr::null()) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if let Some(g) = gid
                        && libc::setgid(g) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    if let Some(u) = uid
                        && libc::setuid(u) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
    }
    // Listen BEFORE the app exists: from spawn on, a signal meant for the
    // app is queued for forwarding (one landing before the loop's first
    // poll is kept) instead of meeting the default disposition, which
    // kills a non-PID-1 agent and orphans the app (PID 1 drops it).
    let signals = Signals::register()?;
    let mut child = cmd.spawn()?;
    let app_pid = child.id();
    // Register before any drain can record the pid: the app's status lives
    // in its own slot, immune to the capped map's eviction.
    ctx.reaper.supervise(app_pid);
    ctx.app_spawned.store(true, Ordering::Release);
    // Log the program name only — argv can carry tokens/passwords.
    let prog_name = std::path::Path::new(prog)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| prog.to_string_lossy().into_owned());
    info!(pid = app_pid, prog = %prog_name, "app spawned");

    let code = run_unix(&ctx, &mut child, app_pid, signals).await?;

    final_flush(&ctx).await;
    Ok(code)
}

/// The streams the supervisor loop selects on: the six signals it
/// forwards to the app group, plus SIGCHLD for reaping.
struct Signals {
    term: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
    quit: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
    usr1: tokio::signal::unix::Signal,
    usr2: tokio::signal::unix::Signal,
    chld: tokio::signal::unix::Signal,
}

impl Signals {
    fn register() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
            quit: signal(SignalKind::quit())?,
            hup: signal(SignalKind::hangup())?,
            usr1: signal(SignalKind::user_defined1())?,
            usr2: signal(SignalKind::user_defined2())?,
            chld: signal(SignalKind::child())?,
        })
    }
}

async fn run_unix(
    ctx: &Arc<AgentCtx>,
    child: &mut std::process::Child,
    app_pid: u32,
    mut sig: Signals,
) -> anyhow::Result<i32> {
    let pgid = -(app_pid as i32);

    let mut reap_tick = tokio::time::interval(REAP_TICK);
    reap_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Eager first tick so pending zombies are cleared at startup too.
    reap_tick.tick().await;

    let app_status = loop {
        tokio::select! {
            _ = sig.chld.recv() => {
                if let Some(s) = reap_app(ctx) { break s; }
            }
            _ = reap_tick.tick() => {
                if let Some(s) = reap_app(ctx) { break s; }
            }
            _ = sig.term.recv() => forward(pgid, libc::SIGTERM),
            _ = sig.int.recv() => forward(pgid, libc::SIGINT),
            _ = sig.quit.recv() => forward(pgid, libc::SIGQUIT),
            _ = sig.hup.recv() => forward(pgid, libc::SIGHUP),
            _ = sig.usr1.recv() => forward(pgid, libc::SIGUSR1),
            _ = sig.usr2.recv() => forward(pgid, libc::SIGUSR2),
            _ = terminate_wait(&ctx.terminate_notify, &ctx.terminating) => {
                return Ok(shutdown_and_drain(ctx, child, pgid).await);
            }
        }
        // The flag may have flipped between select arms — check directly.
        if ctx.terminating.load(Ordering::Acquire) {
            return Ok(shutdown_and_drain(ctx, child, pgid).await);
        }
    };

    // The app is gone. If /terminate raced its death (the flag flipped
    // between the select arm and this break), `shutdown_seq` never ran —
    // so detached app-group members may still be alive. Put the group
    // down like the sequence's escalation would.
    if ctx.terminating.load(Ordering::Acquire) {
        forward(pgid, libc::SIGKILL);
    }
    // But a `/suspend` or `/terminate` may still be running its hooks.d
    // scripts or OTLP flush, and `process::exit` in main would cut them
    // (and the platform's response) mid-flight. Drain before we leave;
    // bounded by the per-connection budget.
    wait_hooks_idle(ctx).await;
    // A /terminate accepted during the drain flipped the flag after the
    // check above — take the app group down before exiting (same
    // documented pgid-reuse residual as `forward`).
    if ctx.terminating.load(Ordering::Acquire) {
        forward(pgid, libc::SIGKILL);
    }
    info!(?app_status, "app exited");
    Ok(exit_code(app_status))
}

/// SIGCHLD (or the backstop tick) woke us: reap everything pending,
/// then report the app's own status if it landed.
fn reap_app(ctx: &Arc<AgentCtx>) -> Option<ExitStatus> {
    drain(&ctx.reaper);
    ctx.reaper.take_app_status().map(decode_status)
}

/// The shared teardown tail: run the terminate sequence, then drain
/// in-flight hooks (a forged /suspend mid-flush, this /terminate's own
/// tail) so `process::exit` can't cut a response mid-write.
async fn shutdown_and_drain(
    ctx: &Arc<AgentCtx>,
    child: &mut std::process::Child,
    pgid: i32,
) -> i32 {
    let code = Box::pin(shutdown_seq(ctx, child, pgid)).await;
    wait_hooks_idle(ctx).await;
    code
}

/// Wait until no hook request is being served anymore. Bounded by the
/// per-connection budget (header read + hook budget + write allowance)
/// plus slack, so a wedged client can never hold the exit forever.
async fn wait_hooks_idle(ctx: &AgentCtx) {
    // The cap shares the server's per-connection deadline formula — a
    // server constant that changes must not let a wedged client outlive
    // the drain.
    let cap = ctx
        .cfg
        .hook_budget
        .saturating_add(crate::hooks::CONN_OVERHEAD)
        .saturating_add(DRAIN_SLACK);
    let deadline = crate::ctx::deadline_after(cap);
    loop {
        // Phase 1: drain what's in flight while the port still accepts —
        // a late platform /suspend after app death is legitimate work
        // worth answering 200 (fail-open), not resetting.
        while ctx.hooks_in_flight.load(Ordering::SeqCst) > 0 {
            let left = crate::ctx::remaining(deadline);
            if left.is_zero() {
                warn!(
                    in_flight = ctx.hooks_in_flight.load(Ordering::SeqCst),
                    "hooks still in flight at drain deadline; exiting anyway"
                );
                return;
            }
            tokio::select! {
                _ = ctx.hooks_idle.notified() => {}
                _ = tokio::time::sleep(left.min(SHUTDOWN_POLL)) => {}
            }
        }
        // Phase 2: refuse new connections, then re-check. The flag
        // store, the fence, this count read, and the acceptor's
        // track-then-load-flag are ALL SeqCst — required, because
        // weaker orderings permit a store-buffered flag to lag behind
        // the count read on x86/ARM (Dekker/SB litmus). In the single
        // SeqCst total order, any connection that read flag==false and
        // tracked itself is guaranteed visible to this load, and is
        // drained by the loop.
        ctx.stop_accepting.store(true, Ordering::SeqCst);
        std::sync::atomic::fence(Ordering::SeqCst);
        if ctx.hooks_in_flight.load(Ordering::SeqCst) == 0 {
            return;
        }
    }
}

/// Completes when `terminating` is set, even if the notify fired before
/// we started listening. A notification landing between the flag check
/// and `notified()` registration would be lost — the select-loop's
/// post-poll flag re-check and the 250 ms reap tick bound that window,
/// so this can never hang.
async fn terminate_wait(notify: &Notify, flag: &std::sync::atomic::AtomicBool) {
    if flag.load(Ordering::Acquire) {
        return;
    }
    notify.notified().await;
}

/// `/terminate` served: give the response a moment on the wire, then
/// SIGTERM the app group, wait `shutdown_grace`, SIGKILL, and flush.
async fn shutdown_seq(ctx: &Arc<AgentCtx>, child: &mut std::process::Child, pgid: i32) -> i32 {
    tokio::time::sleep(TERMINATE_RESPONSE_DELAY).await;
    info!("terminate: signalling app group");
    forward(pgid, libc::SIGTERM);

    let deadline = crate::ctx::deadline_after(ctx.cfg.shutdown_grace);
    let mut status = wait_for_app(ctx, deadline).await;
    if status.is_none() {
        warn!("app ignored SIGTERM during grace; SIGKILL");
        forward(pgid, libc::SIGKILL);
        // Bounded wait: reap what the kill produced, then move on.
        status = wait_for_app(ctx, crate::ctx::deadline_after(Duration::from_secs(2))).await;
    }
    match status {
        Some(s) => {
            info!(?s, "app exited at terminate");
            exit_code(s)
        }
        None => {
            // Still no status — kill the leader directly and give up
            // waiting; the orphan queue is irrelevant since we're exiting.
            // 137 is the honest answer: the app died to our SIGKILL.
            // `Child::kill` is kill(2) PLUS a blocking waitpid — on a
            // D-state child it could pin this worker forever, so signal
            // directly instead.
            // (The pid may have been recycled since spawn — see `forward`;
            // the blast radius is bounded because we are exiting anyway.)
            unsafe {
                libc::kill(child.id() as i32, libc::SIGKILL);
            }
            137
        }
    }
}

/// Poll the reaper for the app's exit status until `deadline`.
async fn wait_for_app(ctx: &AgentCtx, deadline: Instant) -> Option<ExitStatus> {
    loop {
        drain(&ctx.reaper);
        if let Some(raw) = ctx.reaper.take_app_status() {
            return Some(decode_status(raw));
        }
        let left = crate::ctx::remaining(deadline);
        if left.is_zero() {
            return None;
        }
        tokio::time::sleep(left.min(SHUTDOWN_POLL)).await;
    }
}

/// Reap every dead child into `reaper.statuses`. Unmanaged orphans are
/// recorded too — their entries are simply never taken. Every status
/// carries this drain's start instant: a waitpid executed before a
/// spawn returned its pid can only have collected the previous
/// generation, so drain-start ordering is what lets waiters separate
/// generations (see `Reaper::record`).
fn drain(reaper: &crate::ctx::Reaper) {
    let drained_at = Instant::now();
    loop {
        let mut status: libc::c_int = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid <= 0 {
            break;
        }
        let pid = pid as u32;
        debug!(pid, status, "reaped child");
        reaper.record(pid, status, drained_at);
    }
}

/// Best-effort signal to a process group. Residual hazard: once the app
/// is reaped its pid — and thus this pgid — can be recycled by a hooks.d
/// script (`process_group(0)` makes pgid == its own pid). The blast
/// radius is a mis-targeted script kill → a fail-open warn, not data
/// corruption; there is no cheap way to tell a recycled pgid from a
/// live leftover group.
fn forward(pgid: i32, sig: libc::c_int) {
    let rc = unsafe { libc::kill(pgid, sig) };
    if rc != 0 {
        debug!(pgid, sig, "signal forward failed (group gone?)");
    }
}

fn decode_status(raw: i32) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus::from_raw(raw)
}

/// Shell convention: exit code, or 128+signal for signal death.
fn exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(c), _) => c,
        (None, Some(s)) => 128 + s,
        (None, None) => 1,
    }
}

async fn final_flush(ctx: &AgentCtx) {
    let n = ctx.telemetry.flush(ctx, ctx.cfg.flush_timeout).await;
    debug!(flush_count = n, "final telemetry flush");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn test_ctx() -> Arc<AgentCtx> {
        Arc::new(AgentCtx::new(Config::default()).unwrap())
    }

    /// Spawn the drain and return once it waits in phase 1. The caller
    /// must hold a hook guard so the drain can't finish. On the test's
    /// single thread, the sleep is what lets the spawned waiter run.
    async fn drain_in_phase_one(ctx: &Arc<AgentCtx>) -> tokio::task::JoinHandle<()> {
        let c = ctx.clone();
        let waiter = tokio::spawn(async move { wait_hooks_idle(&c).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !waiter.is_finished(),
            "drain returned with a hook in flight"
        );
        assert!(!ctx.stop_accepting.load(Ordering::SeqCst));
        waiter
    }

    /// Regression: a hook tracked while the drain is already running —
    /// the acceptor observed stop_accepting==false before phase 2 —
    /// must still be waited for after the earlier hooks have finished.
    #[tokio::test]
    async fn drain_waits_for_late_tracked_hook() {
        let ctx = test_ctx();
        let early = ctx.track_hook();
        let waiter = drain_in_phase_one(&ctx).await;

        let late = ctx.track_hook();
        drop(early);
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(
            !waiter.is_finished(),
            "drain returned while a late-tracked hook was in flight"
        );
        drop(late);
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("drain hung after last guard dropped")
            .unwrap();
    }

    /// The refused path tracks, reads the flag, and drops its guard at
    /// once. A running drain left waiting on that transient guard alone
    /// must return when it drops.
    #[tokio::test]
    async fn drain_survives_refused_track() {
        let ctx = test_ctx();
        let early = ctx.track_hook();
        let waiter = drain_in_phase_one(&ctx).await;

        // New connections are refused from here on (phase 2 has run).
        ctx.stop_accepting.store(true, Ordering::SeqCst);
        let refused = ctx.track_hook();
        drop(early);
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(refused);
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("drain hung on a refused connection")
            .unwrap();
    }
}
