use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::{Notify, Semaphore};

use crate::config::Config;
use crate::telemetry::Telemetry;

/// `Instant::now() + d` with a pathological-clock fallback: expire
/// immediately rather than panic (panic = "abort" kills PID 1, and the
/// hook simply answers fail-open instead).
pub(crate) fn deadline_after(d: Duration) -> Instant {
    Instant::now().checked_add(d).unwrap_or_else(Instant::now)
}

/// Time left until `deadline` (zero when already past).
pub(crate) fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// Process-wide bound on blocking filesystem work sent to Tokio's
/// blocking pool. A wedged filesystem keeps each `spawn_blocking`
/// closure alive forever — a dropped `JoinHandle` does NOT stop it —
/// so without a cap, repeated hooks on a hung fs (forged `/run` and
/// `/suspend` reach these paths freely) would leak pool threads until
/// every blocking op stalls. Each call site holds its permit *inside*
/// the closure, so even a timed-out op still occupies a slot until
/// the syscall actually returns.
pub(crate) static FS_PERMITS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(8)));

/// How long a caller queues for an fs-op slot before failing open —
/// healthy operations are millisecond-fast, so a wait this long means
/// the filesystem (not the pool) is the problem and the fail-open
/// answer is the right one.
pub(crate) const FS_PERMIT_WAIT: Duration = Duration::from_millis(250);

/// Upper bound on one fs op's join wait. Generous for real files;
/// only reached when the filesystem itself is wedged — after which
/// the dropped JoinHandle still holds its permit, bounding the leak.
pub(crate) const FS_OP_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard cap on unclaimed statuses: orphans' entries are never taken, so
/// without a bound a fork-bombing workload could grow this map forever.
/// Eviction may drop a status a waiter still wants — that waiter then
/// falls back to `try_wait` and eventually its own timeout, so the damage
/// stays bounded (a mislabelled script outcome, never a hang). The
/// supervised app's status is exempt: it lives in its own slot.
const MAX_STATUSES: usize = 1024;

/// Centralized exit-status store. The agent runs as PID 1, so a single
/// `waitpid(-1, WNOHANG)` drain in the supervisor reaps every dead child —
/// including orphans reparented to us. Polling a shared map keeps all
/// reaping in one place: no competing waiters can steal each other's
/// statuses (a second `waitpid` caller would turn another module's
/// `wait()` into an ECHILD error).
#[derive(Default)]
pub(crate) struct Reaper {
    /// pid → (raw waitpid status, drain instant) for every reaped
    /// process except the app. Entries are removed by `take_since`;
    /// statuses for unmanaged orphans stay recorded until `MAX_STATUSES`
    /// evicts an arbitrary entry. The instant is the drain's start (≤ the
    /// waitpid that collected the zombie) so a waiter can tell a stale
    /// recycled-pid entry from a status that belongs to its own child.
    statuses: Mutex<HashMap<u32, (i32, Instant)>>,
    /// The supervised app's pid, registered once after spawn (0 = unset).
    app_pid: AtomicU32,
    /// Set once the app's exit status lands in `app_status` — after
    /// that the pid may be reused, and later reaps of the same pid go
    /// to the bounded map, not back into the app slot.
    app_recorded: AtomicBool,
    /// The app's own exit status — outside the capped map so eviction can
    /// never drop it and blind the supervisor to the app's death.
    app_status: Mutex<Option<i32>>,
}

impl Reaper {
    /// Register the supervised pid (called once by the supervisor right
    /// after spawning the app — before any drain can record it).
    pub(crate) fn supervise(&self, pid: u32) {
        self.app_pid.store(pid, Ordering::Release);
    }

    /// Record a reaped process (called only from the supervisor drain).
    /// `at` must be the instant the drain started, not `Instant::now()`
    /// at insert: a waitpid executed *before* a spawn returned its pid
    /// can only have collected the previous generation (the kernel never
    /// recycles an unreaped zombie), while a waitpid after can only be
    /// the new child — the drain-start instant preserves that ordering
    /// even when the record lands after the spawn.
    /// The app slot is written once only: after `take_app_status`
    /// consumes it, the kernel may hand the app's pid to a hooks.d
    /// script, and that later reap must not overwrite (or refill) the
    /// app slot — it falls through to the bounded map instead.
    /// First record wins per pid: a reaped pid may be recycled and a
    /// later reap of the same pid must not overwrite a status its
    /// waiter hasn't claimed — a bounded "status lost" outcome beats a
    /// wrong-generation one.
    pub(crate) fn record(&self, pid: u32, status: i32, at: Instant) {
        if pid != 0
            && pid == self.app_pid.load(Ordering::Acquire)
            && !self.app_recorded.load(Ordering::Acquire)
        {
            // Write the status BEFORE setting the flag so a concurrent
            // take_app_status can never observe claimed-but-empty.
            *self.app_status.lock().unwrap() = Some(status);
            self.app_recorded.store(true, Ordering::Release);
            return;
        }
        let mut statuses = self.statuses.lock().unwrap();
        if statuses.len() >= MAX_STATUSES
            && !statuses.contains_key(&pid)
            && let Some(&victim) = statuses.keys().next()
        {
            statuses.remove(&victim);
        }
        statuses.entry(pid).or_insert((status, at));
    }

    /// Claim a reaped *non-app* process's raw waitpid status, but only a
    /// record at or after `since`. A waiter passes its own post-spawn
    /// instant: the kernel can't recycle a pid while its previous owner
    /// is still an unreaped zombie, so a drain that started *after* our
    /// spawn could only have reaped our child — never the previous
    /// generation. A drain that started before our spawn but reaped our
    /// child anyway is rejected (false negative → "status lost" outcome,
    /// bounded), never the dangerous direction of accepting a stale
    /// entry for a live child. Never routes to the app slot: pid reuse
    /// must not let a hooks.d waiter steal the app's exit status (the
    /// supervisor claims it exclusively via `take_app_status`).
    pub(crate) fn take_since(&self, pid: u32, since: Instant) -> Option<i32> {
        let mut statuses = self.statuses.lock().unwrap();
        if let Some(&(_, at)) = statuses.get(&pid)
            && at >= since
        {
            return statuses.remove(&pid).map(|(s, _)| s);
        }
        None
    }

    /// Claim the supervised app's exit status. A dedicated slot so the
    /// invariant holds by API shape, not by the timing of drain vs take.
    pub(crate) fn take_app_status(&self) -> Option<i32> {
        self.app_status.lock().unwrap().take()
    }
}

/// Shared state handed to every hook request. Fields read by the
/// test-suite (`telemetry`, `app_spawned`, `terminating`, `microvm_id`,
/// `otlp_headers`) stay public; the rest are crate-internal.
pub struct AgentCtx {
    pub(crate) cfg: Config,
    pub(crate) client: reqwest::Client,
    pub telemetry: Telemetry,
    /// Agent start time (uptime bookkeeping for usage records).
    pub(crate) start: Instant,
    /// Wall-clock anchor captured with `start` — OTLP `startTimeUnixNano`
    /// must be bit-stable across exports; recomputing
    /// `now() - start.elapsed()` each flush drifts by µs per call.
    pub(crate) start_wall: std::time::SystemTime,
    /// Set once `/terminate` has been served; the supervisor watches it.
    pub terminating: AtomicBool,
    /// Wakes the supervisor the moment `terminating` flips — latency
    /// matters at VM shutdown.
    pub(crate) terminate_notify: Notify,
    /// Set once the child app process has been spawned (readiness signal).
    pub app_spawned: AtomicBool,
    /// Centralized child reaping (PID-1 duty), filled by the supervisor.
    pub(crate) reaper: Reaper,
    /// Set on the first claiming `/run`, under `run_pipeline_lock` (a
    /// `/run` that times out waiting for the lock claims nothing) —
    /// identity repair and the microvm_id store are single-shot under
    /// this flag so a duplicate (or forged-second) `/run` cannot
    /// re-poison state. The OTLP header
    /// file is the exception: it retries per `/run` until
    /// `headers_loaded` flips (a first-read failure must not disable
    /// OTLP auth permanently).
    pub(crate) run_seen: AtomicBool,
    /// Set once the `/run` pipeline (hooks.d + app relay) has fired —
    /// by a claiming `/run` or the first unclaimed one (fail-open runs
    /// it once for format-change resilience). Forged POST floods from
    /// the untrusted app can then trigger privileged scripts at most
    /// once before the real `/run` claims.
    pub(crate) run_pipeline_fired: AtomicBool,
    /// Serializes the check-fire-execute sequence of the `/run` pipeline:
    /// without it, a forged POST landing while a claiming `/run` is still
    /// inside identity repair could fire scripts+relay itself — and the
    /// claiming run would relay afterwards, delivering `/run` to the app
    /// twice. The flag alone can't express "in progress"; the mutex can.
    pub(crate) run_pipeline_lock: tokio::sync::Mutex<()>,
    /// `microvmId` delivered by the first `/run` — identity repair derives
    /// the machine-id from it and hooks.d sees it as `KAGEMUSHA_MICROVM_ID`.
    /// Only stored when the value is env-safe (see server.rs).
    pub microvm_id: OnceLock<String>,
    /// Headers loaded from `cfg.otlp_headers_file` inside `/run`. When
    /// populated, telemetry export must use these instead of
    /// `cfg.otlp_headers` — the file is the per-VM credential channel.
    pub otlp_headers: Mutex<Vec<(String, String)>>,
    /// Set once `otlp_headers_file` yielded a non-empty header set — the
    /// /run loader retries until this flips, then forged /runs can't
    /// trigger a reload.
    pub(crate) headers_loaded: AtomicBool,
    /// Set when the supervisor begins shutdown teardown: the accept loop
    /// drops new connections immediately so no fresh hook can start while
    /// `wait_hooks_idle`/`final_flush` are racing toward `process::exit`.
    pub(crate) stop_accepting: AtomicBool,
    /// Latest cgroup/uptime sample — written by the meter task and by
    /// `sample_fresh` inside flushes.
    pub(crate) meter: crate::meter::UsageMeter,
    /// Hook requests currently being served (dispatch → response write).
    /// The supervisor waits for this to drain before exiting on app death,
    /// so `process::exit` can't cut an in-flight `/suspend` mid-flush.
    pub(crate) hooks_in_flight: AtomicUsize,
    /// Wakes the supervisor when `hooks_in_flight` reaches zero.
    pub(crate) hooks_idle: Notify,
}

impl AgentCtx {
    pub fn new(mut cfg: Config) -> anyhow::Result<Self> {
        // Config::load clamps already; re-clamp for hand-built configs
        // (tests, embedding) so every path gets bounded budgets.
        cfg.clamp();
        let client = reqwest::Client::builder()
            // Hook calls and OTLP posts share one client; per-request
            // deadlines are enforced by the caller with explicit timeouts.
            .connect_timeout(std::time::Duration::from_secs(5))
            // Never follow redirects: the app is untrusted, and a 307 would
            // re-POST the hook body (which may carry secrets) cross-origin.
            // Real 3xx answers pass through to the platform verbatim instead.
            .redirect(reqwest::redirect::Policy::none())
            // HTTP(S)_PROXY/ALL_PROXY in the image are meant for the app.
            // reqwest would honour them without exempting loopback, routing
            // hook bodies and OTLP headers through that proxy.
            .no_proxy()
            .build()?;
        Ok(Self {
            cfg,
            client,
            telemetry: Telemetry::default(),
            start: Instant::now(),
            start_wall: std::time::SystemTime::now(),
            terminating: AtomicBool::new(false),
            terminate_notify: Notify::new(),
            app_spawned: AtomicBool::new(false),
            reaper: Reaper::default(),
            run_seen: AtomicBool::new(false),
            run_pipeline_fired: AtomicBool::new(false),
            run_pipeline_lock: tokio::sync::Mutex::new(()),
            microvm_id: OnceLock::new(),
            otlp_headers: Mutex::new(Vec::new()),
            headers_loaded: AtomicBool::new(false),
            stop_accepting: AtomicBool::new(false),
            meter: crate::meter::UsageMeter::default(),
            hooks_in_flight: AtomicUsize::new(0),
            hooks_idle: Notify::new(),
        })
    }

    /// Mark one hook request as in flight until the returned guard drops
    /// (held across dispatch *and* the response write by the connection
    /// task, so the supervisor can drain hooks before exiting).
    ///
    /// SeqCst: this increment participates in the same total order as
    /// the `stop_accepting` flag and the drain waiter's final count
    /// read — an acceptor that observed flag==false must have its
    /// increment visible to the waiter (Dekker/SB litmus).
    pub(crate) fn track_hook(self: &Arc<Self>) -> HookGuard {
        self.hooks_in_flight.fetch_add(1, Ordering::SeqCst);
        HookGuard(self.clone())
    }
}

/// Decrements `hooks_in_flight` and wakes drain waiters at zero.
pub(crate) struct HookGuard(Arc<AgentCtx>);

impl Drop for HookGuard {
    fn drop(&mut self) {
        if self.0.hooks_in_flight.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.hooks_idle.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: a fork-bombing workload must not evict the supervised
    /// app's exit status — losing it would blind the supervisor forever.
    #[test]
    fn app_status_survives_full_map_eviction() {
        let r = Reaper::default();
        r.supervise(7);
        let now = Instant::now();
        // Fill the capped map, then keep recording orphans — each record
        // evicts an arbitrary entry.
        for pid in 100..(100 + MAX_STATUSES + 64) {
            r.record(pid as u32, 0x100, now);
        }
        r.record(7, 0x2a00, now);
        // More orphans after the app status: eviction must never touch it.
        for pid in 5000..5064 {
            r.record(pid, 0x300, now);
        }
        assert_eq!(r.take_app_status(), Some(0x2a00));
        assert_eq!(r.take_app_status(), None);
        // A reused-pid script status claimed via `take_since` can't
        // reach the app slot.
        assert_eq!(r.take_since(7, now), None);
    }

    #[test]
    fn ordinary_statuses_flow_through_map() {
        let r = Reaper::default();
        let now = Instant::now();
        r.record(42, 0xdead, now);
        assert_eq!(r.take_since(42, now), Some(0xdead));
        assert_eq!(r.take_since(42, now), None);
        assert_eq!(r.take_since(9999, now), None);
    }

    /// The generation gate: a status recorded before the waiter's spawn
    /// instant belongs to a recycled previous owner — rejected, never
    /// accepted as the live child's exit (the dangerous direction).
    #[test]
    fn take_since_rejects_stale_generation() {
        let r = Reaper::default();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(1);
        r.record(42, 0x111, t0);
        // Recorded before `since` → not claimable by the newer generation.
        assert_eq!(r.take_since(42, t1), None);
        assert_eq!(r.take_since(42, t0), Some(0x111));
    }

    /// A reaped pid may be recycled before its waiter claims the status;
    /// a later reap of the same pid must not overwrite the unclaimed
    /// record — the new generation gets a bounded "lost", not a wrong
    /// status, and the original waiter's claim stays intact.
    #[test]
    fn first_record_wins_on_pid_reuse() {
        let r = Reaper::default();
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(1);
        r.record(42, 0x111, t0);
        r.record(42, 0x222, t1); // recycled pid's reap — must not overwrite
        // The old generation's waiter still gets its own status.
        assert_eq!(r.take_since(42, t0), Some(0x111));
        // Once consumed, the slot is gone — the recycled record was
        // dropped at insert, so nothing stale remains to confuse.
        assert_eq!(r.take_since(42, t0), None);
    }
}
