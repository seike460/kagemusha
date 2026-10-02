use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::ctx::{AgentCtx, deadline_after, remaining};
use crate::hooksdir;
use crate::types::{HookKind, RunHookBody};

use super::relay::{self, BodyHeaders, RelayOutcome};

/// Largest accepted hook body. The platform's `runHookPayload` is capped at
/// 4 KiB by measurement; 64 KiB leaves ample headroom.
const MAX_HOOK_BODY: usize = 64 * 1024;
/// Cap on the `otlp_headers_file` — a credential file is a few hundred
/// bytes; 64 KiB bounds a runaway or special file's memory cost.
const MAX_OTLP_HEADERS_FILE: usize = 64 * 1024;
/// Probe budget for `app_ready_url` checks inside `/ready`.
const READY_PROBE_BUDGET: Duration = Duration::from_secs(2);
/// Time reserved for the app relay after `hooks.d` on `run`/`resume` — user
/// scripts must not be able to starve the app's own hook out of its budget.
const RELAY_RESERVE: Duration = Duration::from_secs(10);
/// Max concurrent hook connections. The listener is reachable by the
/// untrusted app (see threat note), so flood protection is required — an
/// over-cap connection is dropped, and the platform retries anyway.
const MAX_CONNECTIONS: usize = 64;
/// How long a connection may take to send its request headers.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Independent cap on request-body collection: real hook bodies are ≤4 KiB
/// and arrive promptly, so a slow-drip body must not hold a semaphore slot
/// for anywhere near the whole hook budget.
const BODY_READ_CAP: Duration = Duration::from_secs(15);
/// Allowance for the response-write phase inside the connection deadline.
/// A peer that stops reading can pin a semaphore permit for as long as the
/// write stalls — the write is bounded like every other phase.
const WRITE_ALLOWANCE: Duration = Duration::from_secs(30);
/// `run`'s identity repair must not eat the app relay's time — a wedged
/// filesystem is fail-open, not a hook-sized black hole.
const IDENTITY_REPAIR_CAP: Duration = Duration::from_secs(10);
/// Worst-case connection overhead beyond the hook budget (header read +
/// response write). The supervisor's drain cap adds slack to this so a
/// wedged client can never outlive the drain — the formula lives here.
pub(crate) const CONN_OVERHEAD: Duration = HEADER_READ_TIMEOUT.saturating_add(WRITE_ALLOWANCE);

/// Bind the hook listener and serve connections in the background.
/// Returns the bound address and the accept-loop task.
pub async fn serve(ctx: Arc<AgentCtx>) -> anyhow::Result<(SocketAddr, JoinHandle<()>)> {
    let listener = TcpListener::bind(("0.0.0.0", ctx.cfg.hook_port)).await?;
    let addr = listener.local_addr()?;
    info!(%addr, "hook listener up");

    let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let task = tokio::spawn(async move {
        let mut backoff = Duration::from_millis(100);
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, ?backoff, "accept failed");
                    // Persistent errors (fd exhaustion…) must not spin or
                    // spam logs: exponential backoff capped at 2s.
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(2));
                    continue;
                }
            };
            backoff = Duration::from_millis(100);
            let Ok(permit) = permits.clone().try_acquire_owned() else {
                // debug!, not warn! — a sustained accept flood must not
                // amplify into thousands of log lines per second.
                debug!(%peer, "hook connection dropped: over capacity");
                drop(stream);
                continue;
            };
            debug!(%peer, "hook connection");
            let ctx = ctx.clone();
            // Track BEFORE checking the teardown flag — the reverse order
            // leaves a check-then-track window where a connection lands
            // in-flight after the drain already observed zero. With this
            // order, if we read flag==false the waiter must see count≥1.
            let inflight = ctx.track_hook();
            if ctx.stop_accepting.load(Ordering::SeqCst) {
                debug!(%peer, "shutting down — refusing new connection");
                drop(inflight);
                drop(stream);
                continue;
            }
            tokio::spawn(async move {
                let _permit = permit;
                // In-flight until the connection future resolves — covers
                // dispatch *and* the response write, so the supervisor can
                // drain hooks before exiting on app death.
                let _inflight = inflight;
                // One hard deadline for the whole connection — headers,
                // dispatch, and the response write alike. Without it a
                // client that stops reading could pin the permit forever
                // and starve the platform's hooks.
                let conn_budget = ctx.cfg.hook_budget.saturating_add(CONN_OVERHEAD);
                let io = TokioIo::new(stream);
                let svc = service_fn(move |req| route(ctx.clone(), req));
                let conn = http1::Builder::new()
                    .timer(TokioTimer::new())
                    .header_read_timeout(HEADER_READ_TIMEOUT)
                    // One hook = one request; keep-alive would let an idle
                    // connection hold a semaphore slot indefinitely.
                    .keep_alive(false)
                    .serve_connection(io, svc);
                match tokio::time::timeout(conn_budget, conn).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => debug!(error = %e, "hook connection closed"),
                    Err(_) => debug!("hook connection closed: deadline exceeded"),
                }
            });
        }
    });
    Ok((addr, task))
}

async fn route(
    ctx: Arc<AgentCtx>,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let Some(hook) = HookKind::from_path(req.uri().path()) else {
        return Ok(respond(404, b"not found", None));
    };
    if req.method() != Method::POST {
        return Ok(respond(405, b"method not allowed", None));
    }

    // The deadline covers body collection too — a connection that dribbles
    // bytes cannot hold the hook open past the budget.
    let deadline = deadline_after(ctx.cfg.hook_budget);
    // A drain hook's body read keeps out of the flush reserve, like every
    // later drain stage: a slow body that ate the whole budget would leave
    // the synchronous flush no time to export.
    let drain = matches!(hook, HookKind::Suspend | HookKind::Terminate);
    let body_reserve = if drain {
        ctx.cfg.flush_timeout
    } else {
        Duration::ZERO
    };

    let collected = tokio::time::timeout(
        remaining(deadline)
            .saturating_sub(body_reserve)
            .min(BODY_READ_CAP),
        Limited::new(req.into_body(), MAX_HOOK_BODY).collect(),
    )
    .await;
    let body = match collected {
        Ok(Ok(c)) => c.to_bytes(),
        _ => {
            warn!(
                hook = hook.path_segment(),
                "hook body too large, slow, or unreadable"
            );
            if hook.is_build() {
                return Ok(respond(413, b"payload too large", None));
            }
            // Runtime hooks stay fail-open (ADR-002) — but for the drain
            // hooks the synchronous flush (and /terminate's shutdown flag)
            // is the agent's entire purpose: a forged oversized POST must
            // not let a lifecycle hook answer 200 without flushing or
            // starting shutdown. The pipeline still runs with the empty
            // JSON body the platform would have sent; run/resume skip
            // theirs on junk input (no privileged scripts on garbage).
            if drain {
                let (status, body, app_headers) =
                    drain_flow(&ctx, hook, Bytes::from_static(b"{}"), deadline).await;
                return Ok(respond(status, &body, app_headers.as_ref()));
            }
            return Ok(respond(200, b"{}", None));
        }
    };

    let result = match hook {
        HookKind::Ready => ready_flow(&ctx, deadline).await,
        HookKind::Validate => validate_flow(&ctx, body, deadline).await,
        HookKind::Run => run_flow(&ctx, body, deadline).await,
        HookKind::Resume => resume_flow(&ctx, body, deadline).await,
        HookKind::Suspend | HookKind::Terminate => drain_flow(&ctx, hook, body, deadline).await,
    };
    info!(
        hook = hook.path_segment(),
        status = result.0,
        "hook answered"
    );
    Ok(respond(result.0, &result.1, result.2.as_ref()))
}

/// `(status, body, app_headers)` triple to send back to the platform.
/// `app_headers` carries the app's own body headers on pass-through;
/// `None` (the agent's own answer) yields `application/json`.
type HookAnswer = (u16, Vec<u8>, Option<BodyHeaders>);

/// The canned "agent handled it, nothing to say" answer.
fn ok_empty() -> HookAnswer {
    (200, b"{}".to_vec(), None)
}

fn env_extra(ctx: &AgentCtx) -> Vec<(String, String)> {
    // image_name reaches `Command::env` — a NUL/control byte in a
    // `--config`-sourced value makes every script spawn fail. Same gate
    // class as `env_safe_microvm_id`, with the same warning on rejection.
    let image_name = ctx
        .cfg
        .image_name
        .as_ref()
        .filter(|n| crate::identity::env_safe_config_value(n));
    if ctx.cfg.image_name.is_some() && image_name.is_none() {
        warn!("KAGEMUSHA_IMAGE_NAME not env-safe; withholding it from script env");
    }
    let mut v = Vec::new();
    if let Some(n) = image_name {
        v.push(("KAGEMUSHA_IMAGE_NAME".to_string(), n.clone()));
    }
    // Delivered by /run; scripts on *every* hook may need the id
    // (registration, per-VM files). Never put payload fields here —
    // env is /proc-readable, payloads may carry secrets.
    if let Some(id) = ctx.microvm_id.get() {
        v.push(("KAGEMUSHA_MICROVM_ID".to_string(), id.clone()));
    }
    v
}

/// `otlp_headers_file` is read inside `/run` — its path is image config,
/// but the file's contents are provisioned per-VM (the documented way to
/// deliver credentials that must never be baked into the snapshot). Once
/// populated it replaces the startup header set for telemetry export.
async fn load_otlp_headers_file(ctx: &AgentCtx, deadline: Instant) {
    let Some(path) = ctx.cfg.otlp_headers_file.clone() else {
        return;
    };
    // Bound the read in size *and* time — and run it on the blocking
    // pool under the shared fs-op permit bound. This path is attempted
    // on *every* unclaimed /run, so on a wedged filesystem each forged
    // POST would otherwise park a blocking-pool thread forever (the
    // timeout frees the future, not the thread) until every fs op —
    // machine-id writes, cgroup reads — silently stalls under fail-open.
    let read_budget = remaining(deadline).min(crate::ctx::FS_OP_TIMEOUT);
    let headers =
        match tokio::time::timeout(read_budget, read_headers_file_bounded(path.clone())).await {
            Ok(h) => h,
            Err(_) => {
                warn!(path = %path.display(), "otlp headers file read timed out (fail-open)");
                return;
            }
        };
    if headers.is_empty() {
        return;
    }
    debug!(path = %path.display(), n = headers.len(), "otlp headers file loaded");
    *ctx.otlp_headers.lock().unwrap() = headers;
    ctx.headers_loaded.store(true, Ordering::Release);
}

/// The blocking half of `load_otlp_headers_file`, run under the shared
/// fs-op semaphore — see `ctx::FS_PERMITS` for why the permit is held
/// inside the closure.
async fn read_headers_file_bounded(path: std::path::PathBuf) -> Vec<(String, String)> {
    let permit = match tokio::time::timeout(
        crate::ctx::FS_PERMIT_WAIT,
        crate::ctx::FS_PERMITS.clone().acquire_owned(),
    )
    .await
    {
        Ok(Ok(p)) => p,
        _ => {
            warn!(path = %path.display(), "fs-op slots exhausted; headers read skipped (fail-open)");
            return Vec::new();
        }
    };
    // A JoinError (the task cancelled at runtime shutdown) is fail-open
    // like every other read failure. It is no panic net: release builds
    // are `panic = "abort"`, so `read_headers_file` must never panic.
    tokio::task::spawn_blocking(move || {
        let _permit = permit; // held until the syscalls actually return
        read_headers_file(&path)
    })
    .await
    .unwrap_or_default()
}

/// Read and parse the `k=v` headers file — the synchronous worker run
/// via `spawn_blocking`.
fn read_headers_file(path: &std::path::Path) -> Vec<(String, String)> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    // A FIFO with no writer would block open(2) inside the blocking
    // pool forever. O_NONBLOCK returns immediately; the read then fails
    // fast (EAGAIN/EOF). A no-op on regular files.
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NONBLOCK);
    }
    let file = match opts.open(path) {
        Ok(f) => f,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "otlp headers file unreadable (fail-open)");
            return Vec::new();
        }
    };
    // Byte-bounded read (not metadata.len() + read_to_string): a
    // growing file or fifo must not balloon memory past the cap.
    use std::io::Read;
    let mut buf = Vec::new();
    if file
        .take(MAX_OTLP_HEADERS_FILE as u64 + 1)
        .read_to_end(&mut buf)
        .is_err()
    {
        warn!(path = %path.display(), "otlp headers file unreadable (fail-open)");
        return Vec::new();
    }
    if buf.len() > MAX_OTLP_HEADERS_FILE {
        warn!(path = %path.display(), "otlp headers file >{MAX_OTLP_HEADERS_FILE} bytes; skipped");
        return Vec::new();
    }
    match String::from_utf8(buf) {
        Ok(text) => text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(crate::config::parse_header_pair)
            .collect::<Vec<(String, String)>>(),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "otlp headers file not utf-8 (fail-open)");
            Vec::new()
        }
    }
}

/// `/ready`: during image build the platform retries until 200.
async fn ready_flow(ctx: &AgentCtx, deadline: Instant) -> HookAnswer {
    if let Some(base) = &ctx.cfg.app_hook_base {
        // /ready is a build-time probe — the platform sends no meaningful
        // body, so unlike the runtime hooks we relay an empty one.
        let out = relay::relay(
            &ctx.client,
            base,
            HookKind::Ready,
            Bytes::new(),
            remaining(deadline),
        )
        .await;
        return answer_from_relay(HookKind::Ready, out);
    }
    if let Some(url) = &ctx.cfg.app_ready_url {
        let budget = READY_PROBE_BUDGET.min(remaining(deadline));
        // /ready is a build hook — honest failures (ADR-002/006): the
        // app's own non-2xx is "not ready" (503), but an unreachable or
        // silent endpoint reports 502/504 like any other relay failure.
        return match relay::probe(&ctx.client, url, budget).await {
            relay::ProbeOutcome::Ready => ok_empty(),
            relay::ProbeOutcome::NotReady => (503, b"not ready".to_vec(), None),
            relay::ProbeOutcome::Transport => (502, Vec::new(), None),
            relay::ProbeOutcome::Timeout => (504, Vec::new(), None),
        };
    }
    if ctx.app_spawned.load(Ordering::Acquire) {
        ok_empty()
    } else {
        (503, b"not ready".to_vec(), None)
    }
}

/// `/validate`: relay when the app has hooks, else trivially succeed.
async fn validate_flow(ctx: &AgentCtx, body: Bytes, deadline: Instant) -> HookAnswer {
    match relay_if_configured(ctx, HookKind::Validate, body, deadline).await {
        Some(out) => answer_from_relay(HookKind::Validate, out),
        None => ok_empty(),
    }
}

/// `/run`: identity and credential refresh first, then the app.
///
/// The platform sends `/run` exactly once — the identity side effects
/// (microvm_id store, machine-id repair) are single-shot under
/// `run_seen`. A forged-second POST (the untrusted app shares this port)
/// cannot re-poison the stored id or rewrite machine-id. The one
/// exception is the OTLP header file, retried per `/run` until
/// `headers_loaded` — so a forged POST while the file is still missing
/// or unreadable can trigger a reload. A forged POST landing *before*
/// the platform's real `/run` is an accepted residual — see ADR-009.
async fn run_flow(ctx: &AgentCtx, body: Bytes, deadline: Instant) -> HookAnswer {
    // Claim the one-shot only on a parsable body *carrying a microvmId* —
    // a malformed or ID-less POST (forged or truncated) must not burn the
    // platform's real `/run`, which always carries microvmId per AWS docs.
    // A forged-but-parsable first POST with an id can still win the race:
    // the accepted residual — the same socket can't tell senders apart.
    let parsed = RunHookBody::parse(&body);
    let claims = parsed.as_ref().is_ok_and(|p| {
        p.microvm_id
            .as_deref()
            .is_some_and(crate::identity::env_safe_microvm_id)
    });
    // The whole flow — the claim, identity repair, the fired-flag
    // check, scripts, and relay — must be atomic: a forged POST sneaking
    // in while a claiming /run is still inside repair would otherwise
    // fire the pipeline itself and leave the claimer to relay after it —
    // two /run deliveries to the app (and scripts racing the repair they
    // observe). The flag alone can't express "in progress"; the lock can.
    // Waiters are bounded by their own hook deadline.
    let Ok(_pipeline) =
        tokio::time::timeout(remaining(deadline), ctx.run_pipeline_lock.lock()).await
    else {
        debug!("/run waited out its deadline on the pipeline lock");
        return ok_empty();
    };
    // Claim only under the lock: a claiming /run that waited out its
    // deadline above must leave the one-shot to the next one — else an
    // ID-less POST holding the lock (slow script or relay) would burn it
    // with no repair and no relay.
    let first_run = claims && !ctx.run_seen.swap(true, Ordering::AcqRel);
    match (parsed, first_run) {
        (Ok(parsed), true) => {
            // Payload IDs ride logs/traces only, never metric labels
            // (AGENTS.md rule). Emit the allowlisted keys at debug; the
            // raw payload is never logged — callers may place secrets in it.
            if let Some(serde_json::Value::Object(map)) = &parsed.run_hook_payload {
                for key in crate::types::LOGGABLE_PAYLOAD_KEYS {
                    if let Some(v) = map.get(*key) {
                        debug!(key = *key, value = %v, "run payload id");
                    }
                }
            }
            // `claims` already proved microvmId is present and env-safe.
            if let Some(id) = &parsed.microvm_id {
                let _ = ctx.microvm_id.set(id.clone());
                debug!(microvm_id = %id, "run hook identity received");
            }
            // Bound repair inside the hook budget: a wedged fs must
            // not eat the app relay's time.
            let repair_budget = remaining(deadline).min(IDENTITY_REPAIR_CAP);
            match tokio::time::timeout(repair_budget, crate::identity::repair(ctx, &parsed)).await {
                Ok(outcome) => info!(
                    machine_id_source = ?outcome.machine_id_source,
                    machine_id = ?outcome.machine_id,
                    hostname = ?outcome.hostname,
                    notes = ?outcome.notes,
                    "identity repair"
                ),
                Err(_) => warn!("identity repair exceeded budget (fail-open)"),
            }
        }
        (Ok(_), false) if ctx.run_seen.load(Ordering::Acquire) => {
            debug!("duplicate /run ignored (first wins)")
        }
        (Ok(_), false) => {
            // Parsable but no env-safe microvmId — didn't claim. Warn on
            // the POST that fires the pipeline (once per VM): a platform
            // format change would otherwise leave every VM on the baked
            // machine-id with nothing logged above debug.
            if ctx.run_pipeline_fired.load(Ordering::Acquire) {
                debug!("/run without a usable microvmId; not claimed")
            } else {
                warn!(
                    "/run without a usable microvmId; identity repair waits for one that carries it"
                )
            }
        }
        (Err(e), _) => warn!(error = %e, "unparsable /run body; continuing fail-open"),
    }
    // Headers are config-driven, not body-driven — a broken body must not
    // skip credential provisioning. Retried on every /run until loaded:
    // a read failure on the first /run must not permanently disable OTLP
    // auth, and once loaded the flag also stops forged-second reloads.
    if !ctx.headers_loaded.load(Ordering::Acquire) {
        load_otlp_headers_file(ctx, deadline).await;
    }
    // hooks.d/run is strictly one-shot: the first /run POST drives it,
    // claimed or not — an unparsable body still runs it once so a
    // platform format change can't wedge the lifecycle. Later unclaimed
    // POSTs (forged flood, platform resend of a broken body) are dropped:
    // privileged scripts must not be re-triggerable. A *claiming* /run
    // that arrives after an unclaimed POST already fired skips scripts
    // but still relays — the app must receive its real runHookPayload.
    let pipeline_fired = ctx.run_pipeline_fired.swap(true, Ordering::AcqRel);
    if pipeline_fired && !first_run {
        debug!("duplicate /run: scripts+relay already ran");
        return ok_empty();
    }
    if !pipeline_fired {
        // Reserve enough time for the app relay; scripts must not eat
        // it all. Skipped when no app hook base — no relay will run.
        run_hooks_d(ctx, HookKind::Run, deadline, relay_reserve(ctx)).await;
    }
    match relay_if_configured(ctx, HookKind::Run, body, deadline).await {
        Some(out) => answer_from_relay(HookKind::Run, out),
        None => ok_empty(),
    }
}

/// `/resume`: scripts first, then the app.
async fn resume_flow(ctx: &AgentCtx, body: Bytes, deadline: Instant) -> HookAnswer {
    run_hooks_d(ctx, HookKind::Resume, deadline, relay_reserve(ctx)).await;
    match relay_if_configured(ctx, HookKind::Resume, body, deadline).await {
        Some(out) => answer_from_relay(HookKind::Resume, out),
        None => ok_empty(),
    }
}

/// `/suspend` and `/terminate`: let the app drain, run scripts, flush last.
async fn drain_flow(ctx: &AgentCtx, hook: HookKind, body: Bytes, deadline: Instant) -> HookAnswer {
    // 1. Let the app drain first — but reserve the flush budget up front so
    //    a slow drain can never starve the send-it-all step (the whole
    //    reason this agent exists).
    let app = relay_capped(ctx, hook, body, deadline, ctx.cfg.flush_timeout).await;
    // 2. User scripts — capped so the final flush always has its budget.
    run_hooks_d(ctx, hook, deadline, ctx.cfg.flush_timeout).await;
    // 3. Synchronous telemetry flush gets whatever time is left,
    //    capped by flush_timeout.
    let budget = remaining(deadline).min(ctx.cfg.flush_timeout);
    let n = ctx.telemetry.flush(ctx, budget).await;
    debug!(
        hook = hook.path_segment(),
        flush_count = n,
        "telemetry flushed"
    );
    if hook == HookKind::Terminate {
        ctx.terminating.store(true, Ordering::Release);
        ctx.terminate_notify.notify_waiters();
    }
    match app {
        Some(out) => answer_from_relay(hook, out),
        None => ok_empty(),
    }
}

fn relay_reserve(ctx: &AgentCtx) -> Duration {
    if ctx.cfg.app_hook_base.is_some() {
        RELAY_RESERVE
    } else {
        Duration::ZERO
    }
}

async fn relay_if_configured(
    ctx: &AgentCtx,
    hook: HookKind,
    body: Bytes,
    deadline: Instant,
) -> Option<RelayOutcome> {
    relay_capped(ctx, hook, body, deadline, Duration::ZERO).await
}

/// Relay when `app_hook_base` is configured, shrinking the budget by
/// `reserve` so the caller keeps room for later pipeline stages.
async fn relay_capped(
    ctx: &AgentCtx,
    hook: HookKind,
    body: Bytes,
    deadline: Instant,
    reserve: Duration,
) -> Option<RelayOutcome> {
    let base = ctx.cfg.app_hook_base.as_ref()?;
    let budget = remaining(deadline).saturating_sub(reserve);
    let out = relay::relay(&ctx.client, base, hook, body, budget).await;
    if !out.ok() {
        warn!(
            hook = hook.path_segment(),
            status = out.status(),
            answered = out.answered(),
            detail = %out.describe(),
            "app hook failed"
        );
    }
    Some(out)
}

async fn run_hooks_d(ctx: &AgentCtx, hook: HookKind, deadline: Instant, reserve: Duration) {
    let budget = remaining(deadline).saturating_sub(reserve);
    let outcomes = hooksdir::run_dir(
        &ctx.cfg.hooks_dir,
        hook,
        budget,
        &env_extra(ctx),
        &ctx.reaper,
    )
    .await;
    for o in outcomes {
        if o.ok {
            debug!(hook = hook.path_segment(), script = ?o.path, "hooks.d ok");
        } else {
            warn!(
                hook = hook.path_segment(),
                script = ?o.path,
                detail = %o.detail,
                "hooks.d script failed (fail-open)"
            );
        }
    }
}

/// Translate a relay outcome into the platform-facing answer.
///
/// Real app answers pass through verbatim on every hook. A *synthesized*
/// failure (timeout/transport — e.g. the app is already gone at terminate) is
/// an agent-side concern on runtime hooks, so fail-open applies: 200
/// (ADR-006). Build hooks still report honestly with 502/504.
fn answer_from_relay(hook: HookKind, out: RelayOutcome) -> HookAnswer {
    match out {
        RelayOutcome::Response {
            status,
            body,
            headers,
        } => (status, body, Some(headers)),
        other if hook.is_build() => (other.status(), Vec::new(), None),
        _ => ok_empty(),
    }
}

fn respond(status: u16, body: &[u8], app: Option<&BodyHeaders>) -> Response<Full<Bytes>> {
    let content_type = app
        .and_then(|h| h.content_type.clone())
        .unwrap_or(hyper::header::HeaderValue::from_static("application/json"));
    let mut builder = Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, content_type);
    // The app's body leaves exactly as it came, so every coding the app
    // applied leaves with it — else the platform reads coded bytes as plain.
    for coding in app.map_or(&[][..], |h| h.content_encoding.as_slice()) {
        builder = builder.header(hyper::header::CONTENT_ENCODING, coding.clone());
    }
    builder
        .body(Full::new(Bytes::copy_from_slice(body)))
        .unwrap_or_else(|_| {
            // Unreachable today (statuses are valid HTTP codes and the
            // header values arrive already parsed) — but a bare
            // `Response::new` would fabricate a 200 over an app failure.
            // Pass the real status through; this assignment can't fail.
            let mut r = Response::new(Full::new(Bytes::new()));
            if let Ok(code) = hyper::StatusCode::from_u16(status) {
                *r.status_mut() = code;
            }
            r
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::test_support::TempDir;
    use std::sync::atomic::AtomicUsize;

    const REAL_RUN: &[u8] = br#"{"microvmId":"mvm-01234567-abcd-ef01-2345-6789abcdef01"}"#;

    /// Hook requests seen by `gated_app`. The first one parks until
    /// `release` fires, after signalling `entered`.
    #[derive(Default)]
    struct Gate {
        hits: AtomicUsize,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    /// A stub app whose first hook request is a slow relay the test
    /// controls — it keeps `run_pipeline_lock` held for as long as needed.
    async fn gated_app(gate: Arc<Gate>) -> SocketAddr {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let gate = gate.clone();
                tokio::spawn(async move {
                    let svc = service_fn(move |_req: Request<Incoming>| {
                        let gate = gate.clone();
                        async move {
                            if gate.hits.fetch_add(1, Ordering::SeqCst) == 0 {
                                gate.entered.notify_one();
                                gate.release.notified().await;
                            }
                            let body = Full::new(Bytes::from_static(b"{}"));
                            Ok::<_, std::convert::Infallible>(Response::new(body))
                        }
                    });
                    http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await
                        .ok();
                });
            }
        });
        addr
    }

    /// A claiming `/run` that waits out its deadline on the pipeline lock
    /// must not burn the one-shot: here an ID-less `/run` holds the lock
    /// in a slow app relay. The next claiming `/run` must still repair
    /// identity and relay to the app. Driven through `run_flow` directly —
    /// over HTTP every POST gets the same hook budget, so the waiter can't
    /// be given a shorter deadline than the holder.
    #[tokio::test]
    async fn run_timed_out_on_the_lock_does_not_burn_the_oneshot() {
        let dir = TempDir::new("lockclaim");
        let idroot = dir.join("root");
        let gate = Arc::new(Gate::default());
        let app = gated_app(gate.clone()).await;
        let ctx = Arc::new(
            AgentCtx::new(Config {
                app_hook_base: Some(format!("http://{app}")),
                hooks_dir: dir.join("no-hooks"),
                identity_root: idroot.clone(),
                ..Config::default()
            })
            .unwrap(),
        );
        let long = || deadline_after(Duration::from_secs(30));

        // An ID-less /run fires the pipeline and parks in the app relay.
        let holder = tokio::spawn({
            let ctx = ctx.clone();
            async move { run_flow(&ctx, Bytes::from_static(b"{}"), long()).await }
        });
        gate.entered.notified().await;

        // The platform's /run runs out of time waiting for the lock.
        let short = deadline_after(Duration::from_millis(200));
        let waited = run_flow(&ctx, Bytes::from_static(REAL_RUN), short).await;
        assert_eq!(waited.0, 200);
        assert!(ctx.microvm_id.get().is_none());

        gate.release.notify_one();
        assert_eq!(holder.await.unwrap().0, 200);

        // The next claiming /run still repairs identity and relays.
        let resp = run_flow(&ctx, Bytes::from_static(REAL_RUN), long()).await;
        assert_eq!(resp.0, 200);
        assert_eq!(
            ctx.microvm_id.get().map(String::as_str),
            Some("mvm-01234567-abcd-ef01-2345-6789abcdef01")
        );
        assert_eq!(
            std::fs::read_to_string(idroot.join("etc/machine-id")).unwrap(),
            "01234567abcdef0123456789abcdef01\n"
        );
        assert_eq!(
            gate.hits.load(Ordering::SeqCst),
            2,
            "the claiming /run must relay to the app"
        );
    }
}
