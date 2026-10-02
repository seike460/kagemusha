//! Per-VM identity repair, run once inside `/run`.
//!
//! An image bakes a single `/etc/machine-id` (and usually a generic
//! hostname) into the snapshot, so every MicroVM resumed from it reports
//! the same identity — measured by microvms-agentd
//! (<https://laithalsaadoon.github.io/microvms-agentd/internals/platform/>,
//! 2026-09-23/24) where rewriting machine-id in
//! `/run` was the fix that made it per-VM. We derive the id from the
//! platform's `microvmId` — stable across suspend/resume and joinable
//! with control-plane identity — and fall back to kernel randomness when
//! it carries no 32-hex content. A `/run` without an env-safe
//! `microvmId` doesn't claim the one-shot, so repair waits for one that
//! does (see `hooks::server::run_flow`).
//!
//! Everything here is fail-open: a read-only rootfs, a missing
//! `CAP_SYS_ADMIN` (measured as absent even under
//! `additionalOsCapabilities: ["ALL"]`), or an absent `microvmId` must
//! never wedge the run hook (ADR-002).

use tracing::debug;

use crate::ctx::AgentCtx;
use crate::types::RunHookBody;

/// Where the installed machine-id came from.
#[derive(Debug, PartialEq)]
pub enum MachineIdSource {
    /// Derived from the platform's `microvmId` (preferred).
    MicrovmId,
    /// `/proc/sys/kernel/random/uuid`.
    KernelRandom,
    /// `getrandom(2)` fallback.
    Getrandom,
}

#[derive(Debug)]
pub struct IdentityOutcome {
    /// `None` when no source produced an id or repair was skipped.
    pub machine_id_source: Option<MachineIdSource>,
    /// 32-hex id actually installed, if a write succeeded anywhere.
    pub machine_id: Option<String>,
    /// Hostname actually applied — via `sethostname(2)` or a hostname
    /// file write. `None` when every attempt failed or none was tried.
    pub hostname: Option<String>,
    /// Per-step notes for the caller to log verbatim.
    pub notes: Vec<String>,
}

/// Run every repair step once. Never fails the hook: each write or
/// syscall that errors is noted and skipped.
pub async fn repair(ctx: &AgentCtx, parsed: &RunHookBody) -> IdentityOutcome {
    let mut out = IdentityOutcome {
        machine_id_source: None,
        machine_id: None,
        hostname: None,
        notes: Vec::new(),
    };
    if !ctx.cfg.identity_repair {
        out.notes.push("identity_repair disabled".into());
        return out;
    }

    let root = &ctx.cfg.identity_root;
    if let Some((id, src)) = pick_machine_id(parsed.microvm_id.as_deref()).await {
        out.machine_id_source = Some(src);
        // The id counts as installed once *any* write lands.
        let wrote = write_machine_id(root, &id, &mut out.notes).await;
        if wrote {
            out.machine_id = Some(id);
        }
    } else {
        out.notes.push("no machine-id source available".into());
    }

    if let Some(host) = hostname_from(parsed.microvm_id.as_deref())
        && repair_hostname(root, &host, &mut out.notes).await
    {
        out.hostname = Some(host);
    }
    out
}

/// Pick the machine-id: `microvmId`'s hex content first (its uuid suffix
/// is exactly 32 hex chars), then the kernel uuid, then getrandom.
async fn pick_machine_id(microvm_id: Option<&str>) -> Option<(String, MachineIdSource)> {
    if let Some(id) = microvm_id.and_then(hex32_of) {
        return Some((id, MachineIdSource::MicrovmId));
    }
    if let Ok(u) = tokio::fs::read_to_string("/proc/sys/kernel/random/uuid").await
        && let Some(id) = hex32_of(u.trim())
    {
        return Some((id, MachineIdSource::KernelRandom));
    }
    getrandom_machine_id().map(|id| (id, MachineIdSource::Getrandom))
}

/// Collapse `s` to a 32-hex machine-id: an embedded uuid
/// (`8-4-4-4-12` dashed hex, the measured `mvm-<uuid>` shape) is
/// preferred; otherwise the *last* 32 hex chars of the string win —
/// id-bearing suffixes conventionally sit at the end, and taking the
/// tail keeps prefix hex letters from silently displacing the uuid.
fn hex32_of(s: &str) -> Option<String> {
    if let Some(u) = uuid_hex_in(s) {
        return Some(u);
    }
    let hex: String = s
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    (hex.len() >= 32).then(|| hex[hex.len() - 32..].to_string())
}

/// Find a `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` window and return its
/// 32 hex chars without dashes.
fn uuid_hex_in(s: &str) -> Option<String> {
    let b = s.as_bytes();
    if b.len() < 36 {
        return None;
    }
    'outer: for start in 0..=b.len() - 36 {
        let w = &b[start..start + 36];
        for (i, &c) in w.iter().enumerate() {
            let dash = matches!(i, 8 | 13 | 18 | 23);
            if dash != (c == b'-') {
                continue 'outer;
            }
            if !dash && !c.is_ascii_hexdigit() {
                continue 'outer;
            }
        }
        let hex: String = w
            .iter()
            .filter(|c| c.is_ascii_hexdigit())
            .map(|c| (*c as char).to_ascii_lowercase())
            .collect();
        return Some(hex);
    }
    None
}

/// Shared env-value gate: non-empty printable ASCII within `max` bytes.
fn env_safe(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// `microvmId` becomes `KAGEMUSHA_MICROVM_ID` in hooks.d environments.
/// A NUL/control byte in the value makes every `Command` spawn fail with
/// `InvalidInput` (measured, Rust 1.98.1) — silently dead hooks.d under
/// fail-open — so only printable ASCII within a sane length is accepted.
/// The gate also decides whether `/run` claims the one-shot: an id that
/// fails it is neither stored nor exposed, and repair never runs on it.
pub fn env_safe_microvm_id(id: &str) -> bool {
    env_safe(id, 128)
}

/// Same gate for config-sourced env values (e.g. `KAGEMUSHA_IMAGE_NAME`):
/// a NUL/control byte in a `Command::env` value makes the spawn fail —
/// one bad name would silently kill every hooks.d script. Image names
/// may be longer than ids, so 256.
pub fn env_safe_config_value(s: &str) -> bool {
    env_safe(s, 256)
}

/// Last-ditch entropy when neither microvmId nor /proc delivered.
#[cfg(target_os = "linux")]
fn getrandom_machine_id() -> Option<String> {
    let mut b = [0u8; 16];
    let rc = unsafe { libc::getrandom(b.as_mut_ptr().cast(), b.len(), 0) };
    // A negative ssize_t wraps huge — never equal to b.len().
    (rc as usize == b.len()).then(|| b.iter().map(|x| format!("{x:02x}")).collect())
}

/// BSD/macOS equivalent of the above.
#[cfg(not(target_os = "linux"))]
fn getrandom_machine_id() -> Option<String> {
    let mut b = [0u8; 16];
    let rc = unsafe { libc::getentropy(b.as_mut_ptr().cast(), b.len()) };
    (rc == 0).then(|| b.iter().map(|x| format!("{x:02x}")).collect())
}

/// `/run/machine-id` (what containerised tools bind-mount) and
/// `/etc/machine-id` (the canonical path). Returns true when at least
/// one write landed — partial success still fixes most consumers.
async fn write_machine_id(root: &std::path::Path, id: &str, notes: &mut Vec<String>) -> bool {
    let mut wrote = false;
    for rel in ["run/machine-id", "etc/machine-id"] {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        match write_no_follow(&path, &format!("{id}\n")).await {
            Ok(()) => {
                wrote = true;
                notes.push(format!("wrote {}", path.display()));
            }
            Err(e) => notes.push(format!("{}: {e}", path.display())),
        }
    }
    wrote
}

/// Create or truncate `path` and write `contents` — refusing a symlink at
/// the final component (`O_NOFOLLOW`, fails with ELOOP). The app runs
/// before `/run`; a link it planted must not aim root's write elsewhere.
async fn write_no_follow(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    opts.custom_flags(libc::O_NOFOLLOW);
    let mut file = opts.open(path).await?;
    file.write_all(contents.as_bytes()).await?;
    // tokio completes the write in the background; flush reports its error.
    file.flush().await
}

/// Sanitize `microvmId` into a legal hostname (≤63 chars, ldh-ish).
fn hostname_from(microvm_id: Option<&str>) -> Option<String> {
    let mut h: String = microvm_id?
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    h.truncate(63);
    let h = h.trim_matches('-').to_string();
    (!h.is_empty()).then_some(h)
}

/// Update `/etc/hostname` under `root` and, only when `root` is the real
/// `/`, try `sethostname(2)` — the syscall mutates the *host's* name and
/// must never run against a test root. Returns true when at least one
/// path actually applied the name.
async fn repair_hostname(root: &std::path::Path, host: &str, notes: &mut Vec<String>) -> bool {
    let mut applied = false;
    if root == std::path::Path::new("/") {
        // len arg is size_t on Linux, c_int on BSD/macOS — ≤63 either way.
        // A conversion failure is noted, never unwrapped: panic=abort
        // would take PID 1 down (ADR-012).
        #[cfg_attr(target_os = "linux", allow(clippy::useless_conversion))]
        match host.len().try_into() {
            Ok(len) => {
                let rc = unsafe { libc::sethostname(host.as_ptr().cast(), len) };
                if rc == 0 {
                    applied = true;
                    notes.push(format!("sethostname({host})"));
                } else {
                    // Expected on capability-restricted MicroVMs (CAP_SYS_ADMIN
                    // is not granted even under "ALL" per agentd measurement).
                    notes.push(format!(
                        "sethostname({host}): {}",
                        std::io::Error::last_os_error()
                    ));
                }
            }
            Err(_) => notes.push(format!("sethostname({host}): length out of range")),
        }
    }
    let path = root.join("etc/hostname");
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    match write_no_follow(&path, &format!("{host}\n")).await {
        Ok(()) => {
            applied = true;
            notes.push(format!("wrote {}", path.display()));
        }
        Err(e) => notes.push(format!("{}: {e}", path.display())),
    }
    debug!(host, applied, "hostname repair attempted");
    applied
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn microvm_id_derives_32_hex() {
        let id = hex32_of("mvm-01234567-abcd-ef01-2345-6789abcdef01").unwrap();
        assert_eq!(id, "01234567abcdef0123456789abcdef01");
    }

    #[test]
    fn hex_rejects_short_sources() {
        assert!(hex32_of("mvm-abc").is_none());
        assert!(hex32_of("").is_none());
        assert!(hex32_of("zzzz-no-hex-at-all").is_none());
    }

    #[test]
    fn hex_prefers_uuid_suffix_over_prefix_hex() {
        // A hex-bearing prefix must not displace the uuid — control-plane
        // join-ability depends on taking the uuid itself.
        let id = hex32_of("dead-cafe-01234567-abcd-ef01-2345-6789abcdef01").unwrap();
        assert_eq!(id, "01234567abcdef0123456789abcdef01");
    }

    #[test]
    fn hex_falls_back_to_last_32() {
        let long = "f".repeat(40);
        assert_eq!(hex32_of(&long).unwrap(), "f".repeat(32));
        // uuid chars beyond 32 → tail wins
        let hex = format!("{}{}", "a".repeat(8), "b".repeat(32));
        assert_eq!(hex32_of(&hex).unwrap(), "b".repeat(32));
    }

    #[test]
    fn env_safe_rejects_control_and_huge() {
        assert!(env_safe_microvm_id("mvm-abc-123"));
        assert!(!env_safe_microvm_id("bad\0id"));
        assert!(!env_safe_microvm_id("bad\nid"));
        assert!(!env_safe_microvm_id(""));
        assert!(!env_safe_microvm_id(&"x".repeat(129)));
        assert!(env_safe_microvm_id(&"x".repeat(128)));
    }

    #[test]
    fn hostname_sanitizes_and_truncates() {
        assert_eq!(
            hostname_from(Some("mvm-01234567-ABCD")),
            Some("mvm-01234567-abcd".to_string())
        );
        assert_eq!(
            hostname_from(Some("weird_name/here")),
            Some("weird-name-here".to_string())
        );
        assert!(hostname_from(Some("---")).is_none());
        assert!(hostname_from(None).is_none());
        let h = hostname_from(Some(&"a".repeat(100))).unwrap();
        assert_eq!(h.len(), 63);
    }

    #[tokio::test]
    async fn pick_prefers_microvm_id() {
        let (id, src) = pick_machine_id(Some("mvm-01234567-abcd-ef01-2345-6789abcdef01"))
            .await
            .unwrap();
        assert_eq!(src, MachineIdSource::MicrovmId);
        assert_eq!(id, "01234567abcdef0123456789abcdef01");
    }

    #[tokio::test]
    async fn pick_falls_back_to_randomness() {
        // No microvmId → kernel uuid or getrandom; either way a 32-hex id.
        let (id, src) = pick_machine_id(None).await.unwrap();
        assert!(matches!(
            src,
            MachineIdSource::KernelRandom | MachineIdSource::Getrandom
        ));
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    fn ctx_rooted_at(root: &std::path::Path) -> AgentCtx {
        AgentCtx::new(crate::config::Config {
            identity_root: root.to_path_buf(),
            ..crate::config::Config::default()
        })
        .unwrap()
    }

    fn uuid_run_body() -> RunHookBody {
        RunHookBody::parse(
            br#"{"microvmId":"mvm-01234567-abcd-ef01-2345-6789abcdef01","runHookPayload":null}"#,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn repair_writes_under_test_root() {
        let dir = TempDir::new("id");
        let ctx = ctx_rooted_at(&dir);
        let out = repair(&ctx, &uuid_run_body()).await;
        assert_eq!(out.machine_id_source, Some(MachineIdSource::MicrovmId));
        let etc = std::fs::read_to_string(dir.join("etc/machine-id")).unwrap();
        assert_eq!(etc, "01234567abcdef0123456789abcdef01\n");
        let run = std::fs::read_to_string(dir.join("run/machine-id")).unwrap();
        assert_eq!(etc, run);
        // Test-root runs must not call sethostname, but the file is written.
        assert!(dir.join("etc/hostname").exists());
        assert_eq!(
            out.hostname.as_deref(),
            Some("mvm-01234567-abcd-ef01-2345-6789abcdef01")
        );
    }

    /// A symlink the app planted before `/run` must not redirect root's
    /// write: the linked paths fail (fail-open), the victim keeps its
    /// bytes, and the plain `run/machine-id` still receives the id.
    #[tokio::test]
    async fn repair_does_not_follow_planted_symlinks() {
        let dir = TempDir::new("id-link");
        let victim = dir.join("victim");
        std::fs::write(&victim, "keep\n").unwrap();
        std::fs::create_dir_all(dir.join("etc")).unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("etc/machine-id")).unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("etc/hostname")).unwrap();

        let ctx = ctx_rooted_at(&dir);
        let out = repair(&ctx, &uuid_run_body()).await;
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep\n");
        assert_eq!(
            std::fs::read_to_string(dir.join("run/machine-id")).unwrap(),
            "01234567abcdef0123456789abcdef01\n"
        );
        assert_eq!(
            out.machine_id.as_deref(),
            Some("01234567abcdef0123456789abcdef01")
        );
        assert!(out.hostname.is_none(), "notes: {:?}", out.notes);
    }
}
