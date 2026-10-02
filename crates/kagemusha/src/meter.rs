//! Usage meter — samples cgroup v2 CPU/memory counters and agent uptime.
//!
//! ADR-004: we ship *facts* (counters and gauges exactly as the kernel
//! reports them), never derived rates — the collector computes deltas.
//! `throttled_usec` is included because it is the direct evidence of
//! CPU bursting beyond the baseline the VM was sized for.
//!
//! cgroup layout (kernel cgroup-v2 docs):
//! - `cpu.stat`    — `usage_usec`, `user_usec`, `system_usec` always;
//!   `nr_throttled`, `throttled_usec` when the bandwidth controller
//!   is enabled.
//! - `memory.current` / `memory.peak` / `memory.max` — bytes,
//!   `memory.max` may literally read `max` (no limit).
//!
//! The cgroup root defaults to `/sys/fs/cgroup` and is configurable for
//! tests and hosts that mount it elsewhere.

use std::path::Path;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tracing::debug;

use crate::ctx::AgentCtx;
use std::sync::Arc;

/// One point-in-time usage sample. `Option` fields are `None` when the
/// file is missing or unparsable — the kernel may not expose every
/// interface (non-cgroup runtimes, partial controllers).
#[derive(Debug, Clone, Default)]
pub struct UsageSample {
    /// Wall-clock time of the sample (unix epoch millis).
    pub unix_ms: u64,
    /// Milliseconds since agent start — the VM's alive-time proxy.
    pub uptime_ms: u64,
    /// `cpu.stat:usage_usec` — total cgroup CPU (user+system).
    pub cpu_usage_usec: Option<u64>,
    /// `cpu.stat:user_usec`.
    pub cpu_user_usec: Option<u64>,
    /// `cpu.stat:system_usec`.
    pub cpu_system_usec: Option<u64>,
    /// `cpu.stat:throttled_usec` — cumulative throttle time.
    pub cpu_throttled_usec: Option<u64>,
    /// `cpu.stat:nr_throttled` — number of throttle periods.
    pub cpu_nr_throttled: Option<u64>,
    /// `memory.current` — live usage in bytes.
    pub memory_current_bytes: Option<u64>,
    /// `memory.peak` — high-water mark in bytes.
    pub memory_peak_bytes: Option<u64>,
    /// `memory.max` — hard limit in bytes (`None` when the file says
    /// `max`, i.e. unlimited).
    pub memory_max_bytes: Option<u64>,
}

/// Latest-sample store. The background task and `sample_fresh` write it
/// with `record()`. A flush exports the sample `sample_fresh` returns and
/// reads `latest()` only as a fallback when no fresh sample was taken.
#[derive(Default)]
pub struct UsageMeter {
    latest: Mutex<Option<UsageSample>>,
}

impl UsageMeter {
    /// Store the sample only if it is at least as fresh as the current
    /// one — a periodic sample that finished late must not overwrite the
    /// fresher value a flush just took (suspend must not ship stale
    /// counters). Note: a backward wall-clock step leaves `latest` frozen
    /// at the old max until the clock passes it again — bounded impact
    /// since flushes export their own freshly-taken sample regardless.
    pub(crate) fn record(&self, s: UsageSample) {
        let mut latest = self.latest.lock().unwrap();
        if latest.as_ref().is_none_or(|cur| s.unix_ms >= cur.unix_ms) {
            *latest = Some(s);
        }
    }

    pub(crate) fn latest(&self) -> Option<UsageSample> {
        self.latest.lock().unwrap().clone()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A degraded sample — uptime still flows when the cgroup tree can't
/// be read (wedged fs, exhausted fs-op slots), so the `kagemusha.uptime`
/// series survives.
fn degraded_sample(start: Instant) -> UsageSample {
    UsageSample {
        unix_ms: now_ms(),
        uptime_ms: start.elapsed().as_millis() as u64,
        ..UsageSample::default()
    }
}

/// Read every cgroup file once and assemble a sample. All file errors
/// degrade to `None` fields — a missing cgroup must never break metering.
///
/// Reads run on the blocking pool under the shared fs-op permit bound:
/// the flush path calls this on every drain hook, so a wedged cgroup
/// mount would otherwise leak one blocking-pool thread per forged
/// `/suspend` (a timeout frees the future, not the parked thread).
async fn sample_now(cgroup_root: &Path, start: Instant) -> UsageSample {
    let permit = match tokio::time::timeout(
        crate::ctx::FS_PERMIT_WAIT,
        crate::ctx::FS_PERMITS.clone().acquire_owned(),
    )
    .await
    {
        Ok(Ok(p)) => p,
        _ => {
            debug!("fs-op slots exhausted; cgroup sample degraded");
            return degraded_sample(start);
        }
    };
    let root = cgroup_root.to_path_buf();
    let join = tokio::task::spawn_blocking(move || {
        let _permit = permit; // held until the syscalls actually return
        sample_now_sync(&root, start)
    });
    match tokio::time::timeout(crate::ctx::FS_OP_TIMEOUT, join).await {
        Ok(Ok(s)) => s,
        _ => degraded_sample(start),
    }
}

/// The synchronous half of `sample_now` — runs via `spawn_blocking`.
fn sample_now_sync(cgroup_root: &Path, start: Instant) -> UsageSample {
    let cpu = read_cpu_stat(&cgroup_root.join("cpu.stat"));
    UsageSample {
        unix_ms: now_ms(),
        uptime_ms: start.elapsed().as_millis() as u64,
        cpu_usage_usec: cpu.get("usage_usec").copied(),
        cpu_user_usec: cpu.get("user_usec").copied(),
        cpu_system_usec: cpu.get("system_usec").copied(),
        cpu_throttled_usec: cpu.get("throttled_usec").copied(),
        cpu_nr_throttled: cpu.get("nr_throttled").copied(),
        memory_current_bytes: read_u64_file(&cgroup_root.join("memory.current")),
        memory_peak_bytes: read_u64_file(&cgroup_root.join("memory.peak")),
        memory_max_bytes: read_max_file(&cgroup_root.join("memory.max")),
    }
}

/// Background sampler: takes a sample every `meter_interval`. Spawned
/// detached in `main` — it runs until `exit()` after the supervisor
/// returns; the join handle is intentionally dropped.
pub async fn run(ctx: Arc<AgentCtx>) {
    let mut tick = tokio::time::interval(ctx.cfg.meter_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await; // first tick is immediate
        let s = sample_now(&ctx.cfg.cgroup_root, ctx.start).await;
        debug!(
            cpu_usec = ?s.cpu_usage_usec,
            mem_bytes = ?s.memory_current_bytes,
            uptime_ms = s.uptime_ms,
            "usage sample"
        );
        ctx.meter.record(s);
    }
}

/// Fresh sample for the flush path — suspend/terminate wants "now", not
/// the last periodic tick.
pub(crate) async fn sample_fresh(ctx: &AgentCtx) -> UsageSample {
    let s = sample_now(&ctx.cfg.cgroup_root, ctx.start).await;
    ctx.meter.record(s.clone());
    s
}

/// Parse `cpu.stat`'s `key value` lines.
fn parse_cpu_stat(text: &str) -> std::collections::HashMap<String, u64> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(' ')?;
            Some((k.to_string(), v.trim().parse().ok()?))
        })
        .collect()
}

fn read_cpu_stat(path: &Path) -> std::collections::HashMap<String, u64> {
    match std::fs::read_to_string(path) {
        Ok(t) => parse_cpu_stat(&t),
        Err(e) => {
            debug!(path = %path.display(), error = %e, "cpu.stat unreadable");
            Default::default()
        }
    }
}

fn read_u64_file(path: &Path) -> Option<u64> {
    let t = std::fs::read_to_string(path).ok()?;
    t.trim().parse().ok()
}

/// `memory.max` is either bytes or the literal `max`.
fn read_max_file(path: &Path) -> Option<u64> {
    let t = std::fs::read_to_string(path).ok()?;
    let t = t.trim();
    if t == "max" { None } else { t.parse().ok() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn cpu_stat_parses_all_fields() {
        let m = parse_cpu_stat(
            "usage_usec 123456\nuser_usec 100000\nsystem_usec 23456\nnr_periods 5\nnr_throttled 2\nthrottled_usec 999\nnr_bursts 0\nburst_usec 0\n",
        );
        assert_eq!(m["usage_usec"], 123456);
        assert_eq!(m["user_usec"], 100000);
        assert_eq!(m["system_usec"], 23456);
        assert_eq!(m["throttled_usec"], 999);
        assert_eq!(m["nr_throttled"], 2);
    }

    #[test]
    fn cpu_stat_tolerates_garbage_lines() {
        let m = parse_cpu_stat("usage_usec 7\nbad line here\n\nfoo x\n");
        assert_eq!(m["usage_usec"], 7);
        assert_eq!(m.len(), 1);
    }

    #[tokio::test]
    async fn sample_reads_fake_cgroup_tree() {
        let dir = TempDir::new("cg");
        std::fs::write(
            dir.join("cpu.stat"),
            "usage_usec 42\nuser_usec 30\nsystem_usec 12\nthrottled_usec 5\nnr_throttled 1\n",
        )
        .unwrap();
        std::fs::write(dir.join("memory.current"), "1048576\n").unwrap();
        std::fs::write(dir.join("memory.peak"), "2097152\n").unwrap();
        std::fs::write(dir.join("memory.max"), "max\n").unwrap();

        let s = sample_now(&dir, Instant::now()).await;
        assert_eq!(s.cpu_usage_usec, Some(42));
        assert_eq!(s.cpu_user_usec, Some(30));
        assert_eq!(s.cpu_system_usec, Some(12));
        assert_eq!(s.cpu_throttled_usec, Some(5));
        assert_eq!(s.cpu_nr_throttled, Some(1));
        assert_eq!(s.memory_current_bytes, Some(1 << 20));
        assert_eq!(s.memory_peak_bytes, Some(2 << 20));
        assert_eq!(s.memory_max_bytes, None); // "max" = unlimited
    }

    #[tokio::test]
    async fn sample_survives_missing_files() {
        let dir = TempDir::new("cg-empty");
        let s = sample_now(&dir, Instant::now()).await;
        assert!(s.cpu_usage_usec.is_none());
        assert!(s.memory_current_bytes.is_none());
    }

    #[test]
    fn meter_stores_latest() {
        let m = UsageMeter::default();
        assert!(m.latest().is_none());
        m.record(UsageSample {
            unix_ms: 1,
            uptime_ms: 100,
            cpu_usage_usec: Some(5),
            ..Default::default()
        });
        let s = m.latest().unwrap();
        assert_eq!(s.cpu_usage_usec, Some(5));
        assert_eq!(s.uptime_ms, 100);
    }

    /// A periodic sample that finishes after a fresher flush sample must
    /// not overwrite it — suspend would ship stale counters.
    #[test]
    fn meter_ignores_older_sample_recorded_late() {
        let m = UsageMeter::default();
        let at = |unix_ms, cpu| UsageSample {
            unix_ms,
            cpu_usage_usec: Some(cpu),
            ..Default::default()
        };
        m.record(at(2, 20));
        m.record(at(1, 10));
        let s = m.latest().unwrap();
        assert_eq!((s.unix_ms, s.cpu_usage_usec), (2, Some(20)));
        // A sample from the same millisecond is as fresh — it replaces.
        m.record(at(2, 30));
        assert_eq!(m.latest().unwrap().cpu_usage_usec, Some(30));
    }
}
