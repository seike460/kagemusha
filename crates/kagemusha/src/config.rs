use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

// Env names that carry credentials or credential-bearing URLs.
// `supervisor::SECRET_ENV_KEYS` lists these same constants, so a name
// can't drift between the two. The list itself is kept by hand: a
// credential-bearing var added to `apply_env` must be added there too.
pub(crate) const ENV_APP_HOOK_BASE: &str = "KAGEMUSHA_APP_HOOK_BASE";
pub(crate) const ENV_APP_READY_URL: &str = "KAGEMUSHA_APP_READY_URL";
pub(crate) const ENV_OTLP_ENDPOINT: &str = "KAGEMUSHA_OTLP_ENDPOINT";
pub(crate) const ENV_OTLP_ENDPOINT_FALLBACK: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";
pub(crate) const ENV_OTLP_HEADERS: &str = "KAGEMUSHA_OTLP_HEADERS";
pub(crate) const ENV_OTLP_HEADERS_FALLBACK: &str = "OTEL_EXPORTER_OTLP_HEADERS";
pub(crate) const ENV_OTLP_HEADERS_FILE: &str = "KAGEMUSHA_OTLP_HEADERS_FILE";
const ENV_HOOK_PORT: &str = "KAGEMUSHA_HOOK_PORT";
const ENV_HOOKS_DIR: &str = "KAGEMUSHA_HOOKS_DIR";
const ENV_FLUSH_TIMEOUT_MS: &str = "KAGEMUSHA_FLUSH_TIMEOUT_MS";
const ENV_HOOK_BUDGET_MS: &str = "KAGEMUSHA_HOOK_BUDGET_MS";
const ENV_IDENTITY_REPAIR: &str = "KAGEMUSHA_IDENTITY_REPAIR";
const ENV_IDENTITY_ROOT: &str = "KAGEMUSHA_IDENTITY_ROOT";
const ENV_CGROUP_ROOT: &str = "KAGEMUSHA_CGROUP_ROOT";
const ENV_SERVICE_NAME: &str = "KAGEMUSHA_SERVICE_NAME";
const ENV_SERVICE_NAME_FALLBACK: &str = "OTEL_SERVICE_NAME";
const ENV_IMAGE_NAME: &str = "KAGEMUSHA_IMAGE_NAME";
const ENV_METER_INTERVAL_MS: &str = "KAGEMUSHA_METER_INTERVAL_MS";
const ENV_SHUTDOWN_GRACE_MS: &str = "KAGEMUSHA_SHUTDOWN_GRACE_MS";
const ENV_APP_UID: &str = "KAGEMUSHA_APP_UID";
const ENV_APP_GID: &str = "KAGEMUSHA_APP_GID";

/// Every environment variable `apply_env` reads, for callers and tests
/// that must neutralize ambient config. `apply_env` reads the `ENV_*`
/// constants directly, not this list, so a new constant must be added
/// here by hand — a unit test fails until the two match. The
/// credential-bearing subset is `supervisor::SECRET_ENV_KEYS` (scrubbed
/// from the app AND from the agent's own environ).
pub const KNOWN_ENV_VARS: &[&str] = &[
    ENV_HOOK_PORT,
    ENV_APP_HOOK_BASE,
    ENV_HOOKS_DIR,
    ENV_OTLP_ENDPOINT,
    ENV_OTLP_ENDPOINT_FALLBACK,
    ENV_OTLP_HEADERS,
    ENV_OTLP_HEADERS_FALLBACK,
    ENV_OTLP_HEADERS_FILE,
    ENV_FLUSH_TIMEOUT_MS,
    ENV_HOOK_BUDGET_MS,
    ENV_IDENTITY_REPAIR,
    ENV_IDENTITY_ROOT,
    ENV_CGROUP_ROOT,
    ENV_SERVICE_NAME,
    ENV_SERVICE_NAME_FALLBACK,
    ENV_IMAGE_NAME,
    ENV_METER_INTERVAL_MS,
    ENV_APP_READY_URL,
    ENV_SHUTDOWN_GRACE_MS,
    ENV_APP_UID,
    ENV_APP_GID,
];

/// Runtime configuration for the agent.
///
/// Everything can be set through environment variables. `KAGEMUSHA_*` variables
/// win over a JSON file passed with `--config`. The standard
/// `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS` and
/// `OTEL_SERVICE_NAME` variables are honoured as fallbacks; the
/// signal-specific `OTEL_EXPORTER_OTLP_METRICS_*` variables are not read.
#[derive(Debug)]
pub struct Config {
    /// Port the agent listens on for the platform's lifecycle hook POSTs.
    pub hook_port: u16,
    /// Base URL of the application's hook endpoints, e.g. `http://127.0.0.1:8080`.
    /// When unset, application hooks are skipped (agent handles everything itself).
    pub app_hook_base: Option<String>,
    /// Directory containing per-hook executable scripts (`hooks.d` convention).
    pub hooks_dir: PathBuf,
    /// OTLP base endpoint, e.g. `https://otlp.example.com/otlp`. Currently
    /// only `/v1/metrics` is appended (metrics-only exporter, ADR-011).
    pub otlp_endpoint: Option<String>,
    /// Extra OTLP headers parsed from `KAGEMUSHA_OTLP_HEADERS` (`k1=v1,k2=v2`,
    /// values taken literally) or, when that is unset, from
    /// `OTEL_EXPORTER_OTLP_HEADERS` (values percent-decoded, per the spec).
    /// Image-baked static headers for non-secret values — the per-VM
    /// credential channel is `otlp_headers_file`, read at `/run` (retried
    /// per `/run` until it yields a non-empty header set) and preferred
    /// over these.
    pub otlp_headers: Vec<(String, String)>,
    /// File with `k=v` lines whose contents replace `otlp_headers` when `/run`
    /// fires. Only the path is kept at load; the file is read inside the run
    /// hook. This is the supported way to inject credentials that must not be
    /// snapshot into the image.
    pub otlp_headers_file: Option<PathBuf>,
    /// Time budget for a single synchronous telemetry flush.
    pub flush_timeout: Duration,
    /// Overall per-hook budget used to compute the flush deadline. The
    /// platform allows up to 60 s for runtime hooks; the agent keeps a margin.
    pub hook_budget: Duration,
    /// Attempt machine-id/hostname repair inside `/run`.
    pub identity_repair: bool,
    /// Root the identity repair writes under (`{root}run/machine-id`,
    /// `{root}etc/machine-id`, `{root}etc/hostname`). `/` in production;
    /// override in tests/dev so repair never scribbles on the host.
    /// `sethostname(2)` is only attempted when this is `/`.
    pub identity_root: PathBuf,
    /// cgroup v2 mount root sampled by the usage meter. `/sys/fs/cgroup`
    /// in production; override in tests/dev.
    pub cgroup_root: PathBuf,
    /// `service.name` resource attribute.
    pub service_name: String,
    /// Bounded metric attribute identifying the image (safe label value).
    pub image_name: Option<String>,
    /// Periodic cgroup sampling interval for the usage meter.
    pub meter_interval: Duration,
    /// Readiness URL for apps without a hook endpoint (`app_hook_base`
    /// unset): each `/ready` probes it once with a single GET (≤2 s).
    pub app_ready_url: Option<String>,
    /// Grace period between SIGTERM and SIGKILL when the platform terminates
    /// the VM (supervisor shutdown sequence).
    pub shutdown_grace: Duration,
    /// Drop the supervised app to this uid. The app is untrusted —
    /// running it unprivileged keeps root-owned files like
    /// `otlp_headers_file` and `hooks.d` unreadable/unwritable to it.
    pub app_uid: Option<u32>,
    /// Primary gid for the supervised app.
    pub app_gid: Option<u32>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hook_port: 9000,
            app_hook_base: None,
            hooks_dir: PathBuf::from("/etc/kagemusha/hooks.d"),
            otlp_endpoint: None,
            otlp_headers: Vec::new(),
            otlp_headers_file: None,
            flush_timeout: Duration::from_secs(8),
            hook_budget: Duration::from_secs(55),
            identity_repair: true,
            identity_root: PathBuf::from("/"),
            cgroup_root: PathBuf::from("/sys/fs/cgroup"),
            service_name: "kagemusha-app".to_string(),
            image_name: None,
            meter_interval: Duration::from_secs(15),
            app_ready_url: None,
            shutdown_grace: Duration::from_secs(10),
            app_uid: None,
            app_gid: None,
        }
    }
}

/// Optional JSON config file (`--config`). All fields optional; env wins.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    hook_port: Option<u16>,
    app_hook_base: Option<String>,
    hooks_dir: Option<PathBuf>,
    otlp_endpoint: Option<String>,
    otlp_headers_file: Option<PathBuf>,
    flush_timeout_ms: Option<u64>,
    hook_budget_ms: Option<u64>,
    identity_repair: Option<bool>,
    identity_root: Option<PathBuf>,
    cgroup_root: Option<PathBuf>,
    service_name: Option<String>,
    image_name: Option<String>,
    meter_interval_ms: Option<u64>,
    app_ready_url: Option<String>,
    shutdown_grace_ms: Option<u64>,
    app_uid: Option<u32>,
    app_gid: Option<u32>,
}

impl Config {
    pub fn load(file: Option<&std::path::Path>) -> anyhow::Result<Self> {
        let mut cfg = Config::default();
        if let Some(path) = file {
            let text = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("read config {}: {e}", path.display()))?;
            let fc: FileConfig = serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("parse config {}: {e}", path.display()))?;
            cfg.apply_file(fc);
        }
        cfg.apply_env();
        cfg.clamp();
        Ok(cfg)
    }

    /// Bound every time budget to a sane range so `Instant + budget` can
    /// never overflow and a stray env var cannot pin a hook open — or shut
    /// the whole pipeline down — forever. `Config::load` runs this, and
    /// `AgentCtx::new` re-applies it for configs built in code.
    pub(crate) fn clamp(&mut self) {
        const MIN_HOOK_BUDGET: Duration = Duration::from_secs(1);
        const MAX_HOOK_BUDGET: Duration = Duration::from_secs(300);
        const MIN_FLUSH: Duration = Duration::from_millis(200);
        const MAX_FLUSH: Duration = Duration::from_secs(120);
        const MAX_METER: Duration = Duration::from_secs(3600);
        if self.hook_port == 0 {
            // Port 0 binds an ephemeral port the platform can never
            // reach — a typo'd env value would silently unserve the
            // whole lifecycle. Fine for tests, so warn rather than fail.
            tracing::warn!("hook_port is 0 — binding an ephemeral port the platform cannot reach");
        }
        if !(MIN_HOOK_BUDGET..=MAX_HOOK_BUDGET).contains(&self.hook_budget) {
            tracing::warn!(
                ms = self.hook_budget.as_millis(),
                "hook_budget clamped to 1s..300s"
            );
            self.hook_budget = self.hook_budget.clamp(MIN_HOOK_BUDGET, MAX_HOOK_BUDGET);
        }
        if !(MIN_FLUSH..=MAX_FLUSH).contains(&self.flush_timeout) {
            tracing::warn!(
                ms = self.flush_timeout.as_millis(),
                "flush_timeout clamped to 200ms..120s"
            );
            self.flush_timeout = self.flush_timeout.clamp(MIN_FLUSH, MAX_FLUSH);
        }
        // The flush reserve lives inside the hook budget; a flush_timeout
        // that swallows the whole budget would starve hooks.d on every
        // drain hook.
        if self.flush_timeout >= self.hook_budget {
            tracing::warn!(
                flush_ms = self.flush_timeout.as_millis(),
                hook_ms = self.hook_budget.as_millis(),
                "flush_timeout >= hook_budget; capping at hook_budget/4"
            );
            self.flush_timeout = (self.hook_budget / 4).max(MIN_FLUSH);
        }
        const MIN_GRACE: Duration = Duration::from_millis(100);
        const MAX_GRACE: Duration = Duration::from_secs(30);
        if !(MIN_GRACE..=MAX_GRACE).contains(&self.shutdown_grace) {
            tracing::warn!(
                ms = self.shutdown_grace.as_millis(),
                "shutdown_grace clamped to 100ms..30s"
            );
            self.shutdown_grace = self.shutdown_grace.clamp(MIN_GRACE, MAX_GRACE);
        }
        if self.meter_interval < Duration::from_secs(1) || self.meter_interval > MAX_METER {
            tracing::warn!(
                ms = self.meter_interval.as_millis(),
                "meter_interval clamped to 1s..3600s"
            );
            self.meter_interval = self.meter_interval.clamp(Duration::from_secs(1), MAX_METER);
        }
    }

    fn apply_file(&mut self, fc: FileConfig) {
        // Mechanical merge table: a field added to FileConfig but left
        // out here is silently dead config — keep the pairing reviewable
        // in one place rather than trusting a hand-written if-let chain.
        macro_rules! merge {
            (direct { $( $d:ident ),* $(,)? }
             opt { $( $o:ident ),* $(,)? }
             ms { $( $dst:ident = $src:ident ),* $(,)? }) => {{
                $( if let Some(v) = fc.$d { self.$d = v; } )*
                $( if let Some(v) = fc.$o { self.$o = Some(v); } )*
                $( if let Some(v) = fc.$src { self.$dst = Duration::from_millis(v); } )*
            }};
        }
        merge! {
            direct {
                hook_port, hooks_dir, identity_repair, identity_root,
                cgroup_root, service_name
            }
            opt {
                app_hook_base, otlp_endpoint, otlp_headers_file, image_name,
                app_ready_url, app_uid, app_gid
            }
            ms {
                flush_timeout = flush_timeout_ms,
                hook_budget = hook_budget_ms,
                meter_interval = meter_interval_ms,
                shutdown_grace = shutdown_grace_ms
            }
        }
    }

    fn apply_env(&mut self) {
        if let Some(v) = env_parse::<u16>(ENV_HOOK_PORT) {
            self.hook_port = v;
        }
        if let Some(v) = env_string(ENV_APP_HOOK_BASE) {
            self.app_hook_base = Some(v);
        }
        if let Some(v) = env_string(ENV_HOOKS_DIR) {
            self.hooks_dir = PathBuf::from(v);
        }
        if let Some(v) =
            env_string(ENV_OTLP_ENDPOINT).or_else(|| env_string(ENV_OTLP_ENDPOINT_FALLBACK))
        {
            self.otlp_endpoint = Some(v);
        }
        // Only the standard variable is percent-decoded (OTel spec);
        // kagemusha's own keeps raw values, as it always has.
        if let Some(v) = env_string(ENV_OTLP_HEADERS) {
            self.otlp_headers = parse_headers(&v, false);
        } else if let Some(v) = env_string(ENV_OTLP_HEADERS_FALLBACK) {
            self.otlp_headers = parse_headers(&v, true);
        }
        if let Some(v) = env_string(ENV_OTLP_HEADERS_FILE) {
            self.otlp_headers_file = Some(PathBuf::from(v));
        }
        if let Some(v) = env_parse::<u64>(ENV_FLUSH_TIMEOUT_MS) {
            self.flush_timeout = Duration::from_millis(v);
        }
        if let Some(v) = env_parse::<u64>(ENV_HOOK_BUDGET_MS) {
            self.hook_budget = Duration::from_millis(v);
        }
        if let Some(v) = env_bool(ENV_IDENTITY_REPAIR) {
            self.identity_repair = v;
        }
        if let Some(v) = env_string(ENV_IDENTITY_ROOT) {
            self.identity_root = PathBuf::from(v);
        }
        if let Some(v) = env_string(ENV_CGROUP_ROOT) {
            self.cgroup_root = PathBuf::from(v);
        }
        if let Some(v) =
            env_string(ENV_SERVICE_NAME).or_else(|| env_string(ENV_SERVICE_NAME_FALLBACK))
        {
            self.service_name = v;
        }
        if let Some(v) = env_string(ENV_IMAGE_NAME) {
            self.image_name = Some(v);
        }
        if let Some(v) = env_parse::<u64>(ENV_METER_INTERVAL_MS) {
            self.meter_interval = Duration::from_millis(v);
        }
        if let Some(v) = env_string(ENV_APP_READY_URL) {
            self.app_ready_url = Some(v);
        }
        if let Some(v) = env_parse::<u64>(ENV_SHUTDOWN_GRACE_MS) {
            self.shutdown_grace = Duration::from_millis(v);
        }
        if let Some(v) = env_parse::<u64>(ENV_APP_UID) {
            // u32 overflow must not wrap to root.
            if let Ok(v) = u32::try_from(v) {
                self.app_uid = Some(v);
            } else {
                tracing::warn!("KAGEMUSHA_APP_UID exceeds u32; ignoring");
            }
        }
        if let Some(v) = env_parse::<u64>(ENV_APP_GID) {
            if let Ok(v) = u32::try_from(v) {
                self.app_gid = Some(v);
            } else {
                tracing::warn!("KAGEMUSHA_APP_GID exceeds u32; ignoring");
            }
        }
    }

    /// OTLP URL for one signal path (`v1/metrics` etc.).
    pub(crate) fn otlp_url(&self, signal_path: &str) -> Option<String> {
        self.otlp_endpoint
            .as_ref()
            .map(|base| format!("{}/{}", base.trim_end_matches('/'), signal_path))
    }
}

/// Parse `k1=v1,k2=v2` header lists (`OTEL_EXPORTER_OTLP_HEADERS` format).
/// `decode` is set only for `OTEL_EXPORTER_OTLP_HEADERS`, whose values
/// the OpenTelemetry spec defines as percent-encoded (W3C Baggage, e.g.
/// `Basic%20abc`). `KAGEMUSHA_OTLP_HEADERS` values are taken literally.
fn parse_headers(raw: &str, decode: bool) -> Vec<(String, String)> {
    raw.split(',')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            let v = v.trim();
            if decode {
                header_pair(k.trim(), &percent_decode(v))
            } else {
                header_pair(k.trim(), v)
            }
        })
        .collect()
}

/// Parse one `k=v` line of the `otlp_headers_file` (server.rs). Values
/// are taken literally — no percent-decoding.
pub(crate) fn parse_header_pair(pair: &str) -> Option<(String, String)> {
    let (k, v) = pair.split_once('=')?;
    header_pair(k.trim(), v.trim())
}

/// Keep a pair only if the key is non-empty and both halves build a
/// reqwest header — one bad pair would otherwise fail every export's
/// request build, so it is dropped and the rest still reach the wire.
fn header_pair(k: &str, v: &str) -> Option<(String, String)> {
    use reqwest::header::{HeaderName, HeaderValue};
    if k.is_empty() {
        return None;
    }
    if HeaderName::from_bytes(k.as_bytes()).is_err() || HeaderValue::from_str(v).is_err() {
        tracing::warn!(header = %k, "unusable OTLP header pair dropped");
        return None;
    }
    Some((k.to_string(), v.to_string()))
}

/// Decode `%XX` escapes. A value that isn't valid percent-encoding (a
/// stray `%`, or bytes that don't form UTF-8) is kept as written, like
/// the opentelemetry-rust OTLP exporter does.
fn percent_decode(v: &str) -> String {
    fn hex(b: Option<&u8>) -> Option<u8> {
        char::from(*b?).to_digit(16).map(|d| d as u8)
    }
    let bytes = v.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let (Some(hi), Some(lo)) = (hex(bytes.get(i + 1)), hex(bytes.get(i + 2))) else {
                return v.to_string();
            };
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| v.to_string())
}

fn env_string(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    let v = env_string(key)?;
    match v.parse() {
        Ok(n) => Some(n),
        Err(_) => {
            tracing::warn!(key, value = %v, "ignoring unparsable env var");
            None
        }
    }
}

/// `1`/`true`/`yes`/`on` or `0`/`false`/`no`/`off`, any case. Anything
/// else is ignored with a warning like an unparsable number, so a typo
/// can't silently flip the setting.
fn env_bool(key: &str) -> Option<bool> {
    let v = env_string(key)?;
    match v.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => {
            tracing::warn!(key, value = %v, "ignoring unparsable env var");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.hook_port, 9000);
        assert!(c.identity_repair);
        assert_eq!(c.flush_timeout, Duration::from_secs(8));
        assert!(c.otlp_url("v1/metrics").is_none());
    }

    #[test]
    fn parse_headers_splits_pairs() {
        let h = parse_headers("Authorization=Basic abc, X-Scope-OrgID= t1 ,,bad", false);
        assert_eq!(
            h,
            vec![
                ("Authorization".to_string(), "Basic abc".to_string()),
                ("X-Scope-OrgID".to_string(), "t1".to_string())
            ]
        );
    }

    #[test]
    fn parse_headers_drops_unbuildable_pairs() {
        // A pair that can't build a reqwest header would fail every
        // export's request build — dropped at parse, valid pairs kept.
        let h = parse_headers("Good=1,Bad Header=x,BadVal=a\nb,Also-Good=yes", false);
        assert_eq!(
            h,
            vec![
                ("Good".to_string(), "1".to_string()),
                ("Also-Good".to_string(), "yes".to_string())
            ]
        );
    }

    /// `OTEL_EXPORTER_OTLP_HEADERS` values are W3C Baggage-encoded: a
    /// vendor's `Basic%20abc` must reach the wire as `Basic abc`, and a
    /// decoded control byte is still rejected as unbuildable.
    #[test]
    fn parse_headers_percent_decodes_values() {
        let h = parse_headers(
            "Authorization=Basic%20abc,X-List=a%2Cb,Raw=100%,Bad=a%0Ab",
            true,
        );
        assert_eq!(
            h,
            vec![
                ("Authorization".to_string(), "Basic abc".to_string()),
                ("X-List".to_string(), "a,b".to_string()),
                ("Raw".to_string(), "100%".to_string())
            ]
        );
    }

    /// The headers file holds raw values — no percent-decoding there.
    #[test]
    fn header_file_pairs_are_literal() {
        assert_eq!(
            parse_header_pair("Authorization=Basic%20abc"),
            Some(("Authorization".to_string(), "Basic%20abc".to_string()))
        );
    }

    /// Only the standard `OTEL_EXPORTER_OTLP_HEADERS` is percent-decoded;
    /// `KAGEMUSHA_OTLP_HEADERS` keeps its raw values — and wins when both
    /// are set.
    #[test]
    fn only_the_otel_headers_var_is_percent_decoded() {
        let pair = |k: &str, v: &str| vec![(k.to_string(), v.to_string())];
        {
            let env = EnvGuard::new();
            env.set(ENV_OTLP_HEADERS, "x=a%2Fb");
            assert_eq!(Config::load(None).unwrap().otlp_headers, pair("x", "a%2Fb"));
        }
        {
            let env = EnvGuard::new();
            env.set(ENV_OTLP_HEADERS_FALLBACK, "x=a%2Fb");
            assert_eq!(Config::load(None).unwrap().otlp_headers, pair("x", "a/b"));
        }
        let env = EnvGuard::new();
        env.set(ENV_OTLP_HEADERS, "x=a%2Fb");
        env.set(ENV_OTLP_HEADERS_FALLBACK, "y=c%2Fd");
        assert_eq!(Config::load(None).unwrap().otlp_headers, pair("x", "a%2Fb"));
    }

    #[test]
    fn otlp_url_appends_signal() {
        let c = Config {
            otlp_endpoint: Some("https://otlp.example.com/otlp/".into()),
            ..Config::default()
        };
        assert_eq!(
            c.otlp_url("v1/traces").unwrap(),
            "https://otlp.example.com/otlp/v1/traces"
        );
    }

    /// Serializes tests that mutate process env vars. The harness runs tests
    /// on multiple threads, so every env-touching test must hold this lock.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Holds ENV_LOCK and clears every var `apply_env` reads, restoring
    /// the originals on drop — ambient host config (`OTEL_SERVICE_NAME`
    /// and friends) must not leak into assertions, and a failing test
    /// must not leak its own vars into the next one.
    struct EnvGuard {
        saved: Vec<(&'static str, std::ffi::OsString)>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn new() -> Self {
            // A panicking test poisons the lock, but its guard's drop has
            // already restored the environment.
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let saved = KNOWN_ENV_VARS
                .iter()
                .filter_map(|&k| std::env::var_os(k).map(|v| (k, v)))
                .collect();
            for k in KNOWN_ENV_VARS {
                // SAFETY: see `set`.
                unsafe { std::env::remove_var(k) };
            }
            Self { saved, _lock: lock }
        }

        fn set(&self, key: &str, value: &str) {
            // SAFETY: every env write in this crate's tests goes through
            // an EnvGuard holding ENV_LOCK; the other tests touch the
            // environment only through std, which synchronizes with it.
            unsafe { std::env::set_var(key, value) };
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: see `set` — ENV_LOCK is still held here.
            unsafe {
                for k in KNOWN_ENV_VARS {
                    std::env::remove_var(k);
                }
                for (k, v) in &self.saved {
                    std::env::set_var(k, v);
                }
            }
        }
    }

    #[test]
    fn file_then_env_merge() {
        let env = EnvGuard::new();
        let dir = std::env::temp_dir();
        let path = dir.join(format!("kagemusha-cfg-{}.json", std::process::id()));
        std::fs::write(&path, r#"{"hook_port": 9100, "service_name": "from-file"}"#).unwrap();
        env.set(ENV_HOOK_PORT, "9200");
        let c = Config::load(Some(&path)).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(c.hook_port, 9200); // env wins
        assert_eq!(c.service_name, "from-file");
    }

    #[test]
    fn kagemusha_service_name_beats_otel() {
        let env = EnvGuard::new();
        env.set(ENV_SERVICE_NAME_FALLBACK, "otel-svc");
        env.set(ENV_SERVICE_NAME, "kage-svc");
        let c = Config::load(None).unwrap();
        assert_eq!(c.service_name, "kage-svc");
    }

    /// Only known boolean words change `identity_repair`; a typo keeps
    /// the file/default value instead of silently turning repair on.
    #[test]
    fn identity_repair_env_accepts_only_known_words() {
        let env = EnvGuard::new();
        for (value, want) in [("off", false), ("No", false), ("0", false), ("TRUE", true)] {
            env.set(ENV_IDENTITY_REPAIR, value);
            assert_eq!(Config::load(None).unwrap().identity_repair, want, "{value}");
        }
        for typo in ["fasle", "disabled"] {
            env.set(ENV_IDENTITY_REPAIR, typo);
            let mut c = Config {
                identity_repair: false,
                ..Config::default()
            };
            c.apply_env();
            assert!(!c.identity_repair, "{typo} must not enable repair");
        }
    }

    #[test]
    fn clamp_floors_and_caps_budgets() {
        let mut c = Config {
            hook_budget: Duration::ZERO,
            flush_timeout: Duration::ZERO,
            ..Config::default()
        };
        c.clamp();
        assert_eq!(c.hook_budget, Duration::from_secs(1));
        // 0ms floored to 200ms; it stays under the 1s hook budget.
        assert_eq!(c.flush_timeout, Duration::from_millis(200));
    }

    #[test]
    fn clamp_caps_flush_inside_hook_budget() {
        let mut c = Config {
            hook_budget: Duration::from_secs(10),
            flush_timeout: Duration::from_secs(60),
            ..Config::default()
        };
        c.clamp();
        assert_eq!(c.flush_timeout, Duration::from_millis(2500));
    }

    /// Every FileConfig field must reach Config — a field left out of
    /// the merge! table is silently dead config (deny_unknown_fields
    /// would make it an outright typo-trap too).
    #[test]
    fn file_config_every_field_merges() {
        let fc: FileConfig = serde_json::from_str(
            r#"{
                "hook_port": 9101,
                "app_hook_base": "http://app:1",
                "hooks_dir": "/h",
                "otlp_endpoint": "http://o:1",
                "otlp_headers_file": "/f",
                "flush_timeout_ms": 1234,
                "hook_budget_ms": 23456,
                "identity_repair": false,
                "identity_root": "/r",
                "cgroup_root": "/c",
                "service_name": "svc",
                "image_name": "img",
                "meter_interval_ms": 3456,
                "app_ready_url": "http://r:1",
                "shutdown_grace_ms": 4567,
                "app_uid": 1000,
                "app_gid": 1001
            }"#,
        )
        .unwrap();
        let mut c = Config::default();
        c.apply_file(fc);
        assert_eq!(c.hook_port, 9101);
        assert_eq!(c.app_hook_base.as_deref(), Some("http://app:1"));
        assert_eq!(c.hooks_dir, PathBuf::from("/h"));
        assert_eq!(c.otlp_endpoint.as_deref(), Some("http://o:1"));
        assert_eq!(c.otlp_headers_file, Some(PathBuf::from("/f")));
        assert_eq!(c.flush_timeout, Duration::from_millis(1234));
        assert_eq!(c.hook_budget, Duration::from_millis(23456));
        assert!(!c.identity_repair);
        assert_eq!(c.identity_root, PathBuf::from("/r"));
        assert_eq!(c.cgroup_root, PathBuf::from("/c"));
        assert_eq!(c.service_name, "svc");
        assert_eq!(c.image_name.as_deref(), Some("img"));
        assert_eq!(c.meter_interval, Duration::from_millis(3456));
        assert_eq!(c.app_ready_url.as_deref(), Some("http://r:1"));
        assert_eq!(c.shutdown_grace, Duration::from_millis(4567));
        assert_eq!(c.app_uid, Some(1000));
        assert_eq!(c.app_gid, Some(1001));
    }

    #[test]
    fn file_config_rejects_unknown_fields() {
        assert!(serde_json::from_str::<FileConfig>(r#"{"bogus": 1}"#).is_err());
    }

    /// The README's key rule: a `KAGEMUSHA_*` name without the prefix,
    /// lower-cased, is a file key — except `KAGEMUSHA_OTLP_HEADERS`,
    /// which is env-only.
    #[test]
    fn file_config_keys_follow_env_names() {
        for var in KNOWN_ENV_VARS {
            let Some(name) = var.strip_prefix("KAGEMUSHA_") else {
                continue;
            };
            let json = format!(r#"{{"{}": null}}"#, name.to_ascii_lowercase());
            let parsed = serde_json::from_str::<FileConfig>(&json);
            assert_eq!(parsed.is_ok(), *var != ENV_OTLP_HEADERS, "{var}");
        }
    }

    /// An unparsable or overflowing env value must not take down PID 1
    /// or wrap into a privileged uid — it's ignored with a warning.
    #[test]
    fn unparsable_and_overflowing_env_is_ignored() {
        let env = EnvGuard::new();
        env.set(ENV_HOOK_PORT, "notaport");
        env.set(ENV_APP_UID, "4294967296"); // u64 parses, u32 overflows
        env.set(ENV_APP_GID, "-5");
        let c = Config::load(None).unwrap();
        assert_eq!(c.hook_port, 9000); // default kept
        assert_eq!(c.app_uid, None); // must not wrap to root
        assert_eq!(c.app_gid, None);
    }

    /// `apply_env` reads the `ENV_*` constants directly, so nothing in
    /// the type system ties a new constant to KNOWN_ENV_VARS — scan this
    /// file's constant definitions and require the list to match them.
    #[test]
    fn known_env_vars_has_no_duplicates_and_is_complete() {
        let src = include_str!("config.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        let mut consts: Vec<&str> = prod
            .lines()
            .map(|l| l.trim_start().trim_start_matches("pub(crate) "))
            .filter(|l| l.starts_with("const ENV_"))
            .filter_map(|l| l.split('"').nth(1))
            .collect();
        consts.sort_unstable();
        let mut sorted = KNOWN_ENV_VARS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), KNOWN_ENV_VARS.len(), "duplicates in list");
        assert_eq!(sorted, consts, "KNOWN_ENV_VARS vs ENV_* constants");
    }
}
