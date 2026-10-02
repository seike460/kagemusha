//! Hook-server integration tests. Unix-only: fixtures run `sh` scripts
//! and chmod 0755.

#![cfg(unix)]

mod common;

use common::{TestDir, fresh_dir, write_script};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use kagemusha::config::Config;
use kagemusha::ctx::AgentCtx;
use kagemusha::types::HOOK_PATH_PREFIX;
use tokio::net::TcpListener;

/// A stub "application" that records which hooks it got, in order, and
/// answers each with a configured status (and optional delay and body).
struct StubApp {
    addr: SocketAddr,
    received: Arc<Mutex<Vec<String>>>,
}

async fn stub_app(statuses: HashMap<String, u16>, delay: Duration) -> StubApp {
    stub_app_opts(statuses, delay, None, None, Vec::new()).await
}

/// `order_log`: when set, the stub appends `app:<hook>` to the file — a
/// shared ordering channel to compare with hooks.d side effects.
/// `body_override`: respond with this body instead of echoing the request.
/// `extra_headers`: response headers added verbatim (e.g. Location).
async fn stub_app_opts(
    statuses: HashMap<String, u16>,
    delay: Duration,
    order_log: Option<std::path::PathBuf>,
    body_override: Option<bytes::Bytes>,
    extra_headers: Vec<(String, String)>,
) -> StubApp {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let statuses = Arc::new(statuses);
    let order_log = Arc::new(order_log);
    let body_override = Arc::new(body_override);
    let extra_headers = Arc::new(extra_headers);
    let rx = received.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let rx = rx.clone();
            let statuses = statuses.clone();
            let order_log = order_log.clone();
            let body_override = body_override.clone();
            let extra_headers = extra_headers.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| {
                    let rx = rx.clone();
                    let statuses = statuses.clone();
                    let order_log = order_log.clone();
                    let body_override = body_override.clone();
                    let extra_headers = extra_headers.clone();
                    async move {
                        let seg = req
                            .uri()
                            .path()
                            .strip_prefix(HOOK_PATH_PREFIX)
                            .unwrap_or("")
                            .to_string();
                        rx.lock().unwrap().push(seg.clone());
                        if let Some(log) = order_log.as_ref() {
                            use std::io::Write;
                            let mut f = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(log)
                                .unwrap();
                            writeln!(f, "app:{seg}").unwrap();
                        }
                        if delay > Duration::ZERO {
                            tokio::time::sleep(delay).await;
                        }
                        let status = statuses.get(&seg).copied().unwrap_or(200);
                        let body = match body_override.as_ref() {
                            Some(b) => b.clone(),
                            None => req
                                .into_body()
                                .collect()
                                .await
                                .unwrap_or_default()
                                .to_bytes(),
                        };
                        let mut builder = Response::builder().status(status);
                        for (k, v) in extra_headers.iter() {
                            builder = builder.header(k.as_str(), v.as_str());
                        }
                        Ok::<_, std::convert::Infallible>(builder.body(Full::new(body)).unwrap())
                    }
                });
                http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await
                    .ok();
            });
        }
    });
    StubApp { addr, received }
}

/// Response body with no size hint — hyper frames it chunked, so the
/// relay's Content-Length pre-check can't see its size and only the
/// chunk-accumulation cap bounds it.
struct ChunkedBody {
    left: usize,
}

impl hyper::body::Body for ChunkedBody {
    type Data = bytes::Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<bytes::Bytes>, Self::Error>>> {
        let n = self.left.min(16 * 1024);
        self.left -= n;
        std::task::Poll::Ready(
            (n > 0).then(|| Ok(hyper::body::Frame::data(bytes::Bytes::from(vec![b'x'; n])))),
        )
    }
}

/// A stub app answering every hook 200 with a `len`-byte chunked body.
async fn chunked_app(len: usize) -> SocketAddr {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| async move {
                    req.into_body().collect().await.ok();
                    Ok::<_, std::convert::Infallible>(Response::new(ChunkedBody { left: len }))
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

struct Agent {
    addr: SocketAddr,
    ctx: Arc<AgentCtx>,
    client: reqwest::Client,
    /// Default identity root, owned so it's removed with the agent.
    _idroot: Option<TestDir>,
}

async fn agent(mut cfg: Config) -> Agent {
    cfg.hook_port = 0;
    // Tests that don't care about scripts must not run an ambient
    // /etc/kagemusha/hooks.d found on the test host.
    if cfg.hooks_dir == Config::default().hooks_dir {
        cfg.hooks_dir =
            std::env::temp_dir().join(format!("kagemusha-nohooks-{}", std::process::id()));
    }
    // /run's identity repair must never write to the test host's real
    // /run or /etc — redirect it under a per-test temp root.
    let idroot = (cfg.identity_root == Config::default().identity_root).then(|| {
        let dir = fresh_dir("idroot");
        cfg.identity_root = dir.0.clone();
        dir
    });
    // In-process flushes must not read the test host's real cgroup tree.
    if cfg.cgroup_root == Config::default().cgroup_root {
        cfg.cgroup_root =
            std::env::temp_dir().join(format!("kagemusha-nocgroup-{}", std::process::id()));
    }
    let ctx = Arc::new(AgentCtx::new(cfg).unwrap());
    let (addr, _task) = kagemusha::hooks::serve(ctx.clone()).await.unwrap();
    Agent {
        addr,
        ctx,
        client: reqwest::Client::new(),
        _idroot: idroot,
    }
}

impl Agent {
    fn url(&self, hook: &str) -> String {
        format!("http://{}{}{}", self.addr, HOOK_PATH_PREFIX, hook)
    }

    async fn post(&self, hook: &str, body: &str) -> reqwest::Response {
        self.client
            .post(self.url(hook))
            .body(body.to_string())
            .send()
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn suspend_relays_to_app_then_flushes() {
    let app = stub_app(HashMap::new(), Duration::ZERO).await;
    let agent = agent(Config {
        app_hook_base: Some(format!("http://{}", app.addr)),
        ..Config::default()
    })
    .await;

    let resp = agent.post("suspend", "{}").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(app.received.lock().unwrap().as_slice(), ["suspend"]);
    assert_eq!(agent.ctx.telemetry.flush_count(), 1);
}

#[tokio::test]
async fn app_failure_status_is_transparent_but_flush_still_runs() {
    let mut statuses = HashMap::new();
    statuses.insert("suspend".to_string(), 500u16);
    let app = stub_app(statuses, Duration::ZERO).await;
    let agent = agent(Config {
        app_hook_base: Some(format!("http://{}", app.addr)),
        ..Config::default()
    })
    .await;

    let resp = agent.post("suspend", "{}").await;
    assert_eq!(resp.status(), 500);
    assert_eq!(agent.ctx.telemetry.flush_count(), 1);
}

#[tokio::test]
async fn hooks_d_failure_stays_fail_open() {
    let dir = fresh_dir("it");
    let sub = dir.join("suspend");
    std::fs::create_dir_all(&sub).unwrap();
    let marker = dir.join("script-ran");
    write_script(
        &sub,
        "fail",
        &format!("#!/bin/sh\ntouch '{}'\nexit 9", marker.display()),
    );

    let agent = agent(Config {
        hooks_dir: dir.0.clone(),
        ..Config::default()
    })
    .await;

    let resp = agent.post("suspend", "{}").await;
    assert_eq!(resp.status(), 200);
    assert!(marker.exists(), "the failing script never ran");
}

#[tokio::test]
async fn ready_waits_for_spawned_app() {
    let agent = agent(Config::default()).await;
    let resp = agent.post("ready", "{}").await;
    assert_eq!(resp.status(), 503);
    agent
        .ctx
        .app_spawned
        .store(true, std::sync::atomic::Ordering::Release);
    let resp = agent.post("ready", "{}").await;
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn run_forwards_double_wrapped_body_verbatim() {
    let app = stub_app(HashMap::new(), Duration::ZERO).await;
    let agent = agent(Config {
        app_hook_base: Some(format!("http://{}", app.addr)),
        ..Config::default()
    })
    .await;

    let body = r#"{"microvmId":"mvm-1","runHookPayload":"{\"k\":1}"}"#;
    let resp = agent.post("run", body).await;
    assert_eq!(resp.status(), 200);
    // Stub echoes the request body back — proves byte-for-byte forwarding.
    assert_eq!(resp.text().await.unwrap(), body);
}

#[tokio::test]
async fn terminate_marks_terminating() {
    let agent = agent(Config::default()).await;
    let resp = agent.post("terminate", "{}").await;
    assert_eq!(resp.status(), 200);
    assert!(
        agent
            .ctx
            .terminating
            .load(std::sync::atomic::Ordering::Acquire)
    );
}

#[tokio::test]
async fn slow_app_on_build_hook_times_out_as_504() {
    let app = stub_app(HashMap::new(), Duration::from_secs(5)).await;
    let agent = agent(Config {
        app_hook_base: Some(format!("http://{}", app.addr)),
        // Minimum clamped budget: the app delay (5s) exceeds it by design.
        hook_budget: Duration::from_secs(1),
        ..Config::default()
    })
    .await;

    // Build hooks report synthesized failures honestly.
    let resp = agent.post("validate", "{}").await;
    assert_eq!(resp.status(), 504);
}

#[tokio::test]
async fn slow_app_on_runtime_hook_fails_open_as_200() {
    let app = stub_app(HashMap::new(), Duration::from_secs(5)).await;
    let agent = agent(Config {
        app_hook_base: Some(format!("http://{}", app.addr)),
        // Minimum clamped budget: the app delay (5s) exceeds it by design.
        hook_budget: Duration::from_secs(1),
        ..Config::default()
    })
    .await;

    // Runtime hooks never let a synthesized failure block the workload.
    let resp = agent.post("suspend", "{}").await;
    assert_eq!(resp.status(), 200);
    // The flush still ran despite the app timeout.
    assert_eq!(agent.ctx.telemetry.flush_count(), 1);
}

#[tokio::test]
async fn unreachable_app_on_terminate_fails_open_as_200() {
    // Point at a port nothing listens on — the app may legitimately be gone
    // by terminate time, and the hook must still answer 200.
    let agent = agent(Config {
        app_hook_base: Some("http://127.0.0.1:1".to_string()),
        hook_budget: Duration::from_secs(2),
        ..Config::default()
    })
    .await;

    let resp = agent.post("terminate", "{}").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(agent.ctx.telemetry.flush_count(), 1);
}

#[tokio::test]
async fn run_with_garbage_body_fails_open_as_200() {
    let app = stub_app(HashMap::new(), Duration::ZERO).await;
    let agent = agent(Config {
        app_hook_base: Some(format!("http://{}", app.addr)),
        ..Config::default()
    })
    .await;

    let resp = agent.post("run", "this is not json").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(app.received.lock().unwrap().as_slice(), ["run"]);
}

#[tokio::test]
async fn oversized_body_fails_open_as_200() {
    let agent = agent(Config::default()).await;
    let resp = agent.post("run", &"x".repeat(128 * 1024)).await;
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn oversized_body_on_build_hook_is_413() {
    // Build hooks answer honestly (ADR-002/007): an oversized body is a
    // real 413, not a fail-open 200 — only runtime hooks get that.
    let agent = agent(Config::default()).await;
    let resp = agent.post("validate", &"x".repeat(128 * 1024)).await;
    assert_eq!(resp.status(), 413);
}

/// Write a hooks.d script that appends `hooksd:<hook>` to a shared order
/// file, in the subdirectory matching `hook`.
fn order_script(dir: &std::path::Path, hook: &str, order_file: &std::path::Path) {
    let sub = dir.join(hook);
    std::fs::create_dir_all(&sub).unwrap();
    write_script(
        &sub,
        "10-mark",
        &format!(
            "#!/bin/sh\necho 'hooksd:{hook}' >> '{}'\n",
            order_file.display()
        ),
    );
}

#[tokio::test]
async fn suspend_order_is_app_then_hooks_d_then_flush() {
    // Shared ordering file proves the drain order: app relay first, then
    // hooks.d. The flush has no file side effect, but flush_count proves it
    // ran (it sits at the end of the pipeline).
    let dir = fresh_dir("ord");
    let order_file = dir.join("order.log");
    order_script(&dir, "suspend", &order_file);

    let app = stub_app_opts(
        HashMap::new(),
        Duration::ZERO,
        Some(order_file.clone()),
        None,
        Vec::new(),
    )
    .await;
    let agent = agent(Config {
        app_hook_base: Some(format!("http://{}", app.addr)),
        hooks_dir: dir.0.clone(),
        ..Config::default()
    })
    .await;

    let resp = agent.post("suspend", "{}").await;
    assert_eq!(resp.status(), 200);
    let log = std::fs::read_to_string(&order_file).unwrap();
    assert_eq!(log, "app:suspend\nhooksd:suspend\n");
    assert_eq!(agent.ctx.telemetry.flush_count(), 1);
}

#[tokio::test]
async fn resume_order_is_hooks_d_then_app() {
    let dir = fresh_dir("ord2");
    let order_file = dir.join("order.log");
    order_script(&dir, "resume", &order_file);

    let app = stub_app_opts(
        HashMap::new(),
        Duration::ZERO,
        Some(order_file.clone()),
        None,
        Vec::new(),
    )
    .await;
    let agent = agent(Config {
        app_hook_base: Some(format!("http://{}", app.addr)),
        hooks_dir: dir.0.clone(),
        ..Config::default()
    })
    .await;

    let resp = agent.post("resume", "{}").await;
    assert_eq!(resp.status(), 200);
    let log = std::fs::read_to_string(&order_file).unwrap();
    assert_eq!(log, "hooksd:resume\napp:resume\n");
}

#[tokio::test]
async fn run_order_is_hooks_d_then_app() {
    // ADR-005: /run is identity → hooks.d → app relay. The shared order
    // file pins the visible pair (hooks.d before the app).
    let dir = fresh_dir("ord3");
    let order_file = dir.join("order.log");
    order_script(&dir, "run", &order_file);

    let app = stub_app_opts(
        HashMap::new(),
        Duration::ZERO,
        Some(order_file.clone()),
        None,
        Vec::new(),
    )
    .await;
    let agent = agent(Config {
        app_hook_base: Some(format!("http://{}", app.addr)),
        hooks_dir: dir.0.clone(),
        ..Config::default()
    })
    .await;

    let resp = agent.post("run", "{}").await;
    assert_eq!(resp.status(), 200);
    let log = std::fs::read_to_string(&order_file).unwrap();
    assert_eq!(log, "hooksd:run\napp:run\n");
}

#[tokio::test]
async fn app_redirect_is_not_followed() {
    // The trap is a second server that must never be reached. If the agent
    // followed the 307, it would re-POST the hook body cross-origin —
    // the hook body may carry secrets, so following is exfiltration.
    let trap = stub_app(HashMap::new(), Duration::ZERO).await;
    let mut statuses = HashMap::new();
    statuses.insert("run".to_string(), 307u16);
    let app = stub_app_opts(
        statuses,
        Duration::ZERO,
        None,
        None,
        vec![("location".to_string(), format!("http://{}/trap", trap.addr))],
    )
    .await;
    let agent = agent(Config {
        app_hook_base: Some(format!("http://{}", app.addr)),
        ..Config::default()
    })
    .await;

    let resp = agent.post("run", "{}").await;
    assert_eq!(resp.status(), 307);
    // Location is intentionally not forwarded to the platform (ADR-006:
    // only status, body, Content-Type and Content-Encoding pass through).
    assert!(resp.headers().get("location").is_none());
    assert!(trap.received.lock().unwrap().is_empty());
}

#[tokio::test]
async fn app_content_type_passes_through() {
    let app = stub_app_opts(
        HashMap::new(),
        Duration::ZERO,
        None,
        None,
        vec![("content-type".to_string(), "text/plain".to_string())],
    )
    .await;
    let agent = agent(Config {
        app_hook_base: Some(format!("http://{}", app.addr)),
        ..Config::default()
    })
    .await;

    let resp = agent.post("run", "{}").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("content-type").unwrap(), "text/plain");
}

/// A Content-Type that isn't visible ASCII (obs-text is legal on the
/// wire) is relayed byte for byte, not swapped for `application/json`.
#[tokio::test]
async fn app_non_ascii_content_type_passes_through_byte_for_byte() {
    let app = stub_app_opts(
        HashMap::new(),
        Duration::ZERO,
        None,
        None,
        vec![("content-type".to_string(), "text/plain; name=é".to_string())],
    )
    .await;
    let agent = agent(Config {
        app_hook_base: Some(format!("http://{}", app.addr)),
        ..Config::default()
    })
    .await;

    let resp = agent.post("run", "{}").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap().as_bytes(),
        "text/plain; name=é".as_bytes()
    );
}

/// The agent never inflates the untrusted app's bodies: it doesn't ask for
/// gzip, so an app that compresses anyway gets its bytes relayed as-is —
/// with the Content-Encoding that says how to read them.
#[tokio::test]
async fn app_gzip_body_is_relayed_without_decompression() {
    // gzip of `{"ok":true}`
    const GZIPPED: [u8; 31] = [
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff, 0xab, 0x56, 0xca, 0xcf, 0x56,
        0xb2, 0x2a, 0x29, 0x2a, 0x4d, 0xad, 0x05, 0x00, 0x90, 0x5f, 0xd4, 0xa7, 0x0b, 0x00, 0x00,
        0x00,
    ];
    let app = stub_app_opts(
        HashMap::new(),
        Duration::ZERO,
        None,
        Some(bytes::Bytes::from_static(&GZIPPED)),
        vec![("content-encoding".to_string(), "gzip".to_string())],
    )
    .await;
    let agent = agent(Config {
        app_hook_base: Some(format!("http://{}", app.addr)),
        ..Config::default()
    })
    .await;

    let resp = agent.post("validate", "{}").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("content-encoding").unwrap(), "gzip");
    assert_eq!(resp.bytes().await.unwrap(), GZIPPED[..]);
}

/// Codings stack, and each may come on its own field line: every line
/// reaches the platform, in order — not just the first.
#[tokio::test]
async fn every_app_content_encoding_line_passes_through_in_order() {
    let app = stub_app_opts(
        HashMap::new(),
        Duration::ZERO,
        None,
        Some(bytes::Bytes::from_static(b"coded")),
        vec![
            ("content-encoding".to_string(), "gzip".to_string()),
            ("content-encoding".to_string(), "br".to_string()),
        ],
    )
    .await;
    let agent = agent(Config {
        app_hook_base: Some(format!("http://{}", app.addr)),
        ..Config::default()
    })
    .await;

    let resp = agent.post("resume", "{}").await;
    assert_eq!(resp.status(), 200);
    let codings: Vec<_> = resp.headers().get_all("content-encoding").iter().collect();
    assert_eq!(codings, ["gzip", "br"]);
    assert_eq!(resp.bytes().await.unwrap(), "coded");
}

#[tokio::test]
async fn oversized_app_response_fails_open_as_200() {
    // A 100 KiB app body exceeds the 64 KiB relay cap → Transport → fail-open.
    let app = stub_app_opts(
        HashMap::new(),
        Duration::ZERO,
        None,
        Some(bytes::Bytes::from(vec![b'x'; 100 * 1024])),
        Vec::new(),
    )
    .await;
    let agent = agent(Config {
        app_hook_base: Some(format!("http://{}", app.addr)),
        ..Config::default()
    })
    .await;

    let resp = agent.post("suspend", "{}").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(agent.ctx.telemetry.flush_count(), 1);
}

/// Without a Content-Length only the chunk-accumulation cap stops an
/// oversized app body: the runtime hook fails open with the agent's own
/// `{}`, never the app's 100 KiB.
#[tokio::test]
async fn chunked_oversized_app_response_fails_open_as_200() {
    let app = chunked_app(100 * 1024).await;
    let agent = agent(Config {
        app_hook_base: Some(format!("http://{app}")),
        ..Config::default()
    })
    .await;

    let resp = agent.post("suspend", "{}").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap(), "{}");
    assert_eq!(agent.ctx.telemetry.flush_count(), 1);
}

/// Build hooks report the same overflow honestly as 502, while a chunked
/// body under the cap still passes through verbatim.
#[tokio::test]
async fn chunked_oversized_app_response_on_build_hook_is_502() {
    let small = chunked_app(32 * 1024).await;
    let agent_small = agent(Config {
        app_hook_base: Some(format!("http://{small}")),
        ..Config::default()
    })
    .await;
    let resp = agent_small.post("validate", "{}").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().len(), 32 * 1024);

    let big = chunked_app(100 * 1024).await;
    let agent_big = agent(Config {
        app_hook_base: Some(format!("http://{big}")),
        ..Config::default()
    })
    .await;
    assert_eq!(agent_big.post("validate", "{}").await.status(), 502);
}

#[tokio::test]
async fn hooks_d_runs_with_clean_environment() {
    // The agent's env may carry credentials (OTLP headers, AWS vars);
    // scripts must see only PATH and KAGEMUSHA_*.
    let dir = fresh_dir("env");
    let env_file = dir.join("env.txt");
    let sub = dir.join("suspend");
    std::fs::create_dir_all(&sub).unwrap();
    write_script(
        &sub,
        "10-env",
        &format!("#!/bin/sh\nenv > '{}'\n", env_file.display()),
    );

    let agent = agent(Config {
        hooks_dir: dir.0.clone(),
        ..Config::default()
    })
    .await;
    let resp = agent.post("suspend", "{}").await;
    assert_eq!(resp.status(), 200);

    let env = std::fs::read_to_string(&env_file).unwrap();
    assert!(env.contains("KAGEMUSHA_HOOK=suspend"));
    assert!(env.contains("PATH="));
    // Parent-process vars must be gone — HOME certainly exists in our env.
    for line in env.lines() {
        assert!(!line.starts_with("HOME="), "HOME leaked: {line}");
        assert!(!line.starts_with("AWS_"), "AWS var leaked: {line}");
        assert!(!line.starts_with("OTEL_"), "OTEL var leaked: {line}");
    }
}

#[tokio::test]
async fn hooks_d_timeout_kills_the_whole_process_group() {
    let dir = fresh_dir("pg");
    let done_marker = dir.join("grandchild-done");
    let sub = dir.join("suspend");
    std::fs::create_dir_all(&sub).unwrap();
    // Grandchild: touches the marker after 1500ms. Parent: sleeps 30s so
    // the timeout fires ~900ms in (see clamped budgets below). If only the
    // leader is killed, the grandchild still writes the marker at +1500ms —
    // the group kill must prevent that.
    write_script(
        &sub,
        "10-slow",
        &format!(
            "#!/bin/sh\n(sleep 1.5 && touch '{}') &\nsleep 30\n",
            done_marker.display()
        ),
    );

    // Budgets are clamped by AgentCtx::new to ≥1s / ≥200ms, so hooks.d
    // gets ~900ms — the group kill must land before the +1500ms write.
    let agent = agent(Config {
        hooks_dir: dir.0.clone(),
        hook_budget: Duration::from_millis(1100),
        flush_timeout: Duration::from_millis(200),
        ..Config::default()
    })
    .await;

    let resp = agent.post("suspend", "{}").await;
    assert_eq!(resp.status(), 200);
    // Wait past the grandchild's 1500ms write deadline: group-killed → no marker.
    tokio::time::sleep(Duration::from_millis(1800)).await;
    assert!(!done_marker.exists());
}

/// Poll until the script `pid` has exited, reaping it here: this test
/// process is its parent, and a killed script the agent had no budget
/// left to reap stays a zombie — which `kill(pid, 0)` still counts as
/// alive.
async fn script_exited(pid: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let mut status = 0;
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if r == pid
            || (r == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn hooks_d_timeout_kills_the_script() {
    let dir = fresh_dir("kill");
    let pid_file = dir.join("script.pid");
    let sub = dir.join("suspend");
    std::fs::create_dir_all(&sub).unwrap();
    // `exec` keeps the script's pid on the sleep: the pid it publishes
    // is the process the timeout must kill.
    write_script(
        &sub,
        "10-slow",
        &format!(
            "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
            pid_file.display()
        ),
    );

    // Clamped budget 1200ms minus 200ms flush reserve leaves hooks.d
    // ~1000ms — the script must actually be spawned, then killed.
    let agent = agent(Config {
        hooks_dir: dir.0.clone(),
        hook_budget: Duration::from_millis(1200),
        flush_timeout: Duration::from_millis(200),
        ..Config::default()
    })
    .await;

    let started = std::time::Instant::now();
    let resp = agent.post("suspend", "{}").await;
    assert_eq!(resp.status(), 200);
    // The hook gave up near the budget, not after the full 30s sleep.
    assert!(started.elapsed() < Duration::from_secs(10));
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .expect("the script was spawned")
        .trim()
        .parse()
        .unwrap();
    assert!(
        script_exited(pid, Duration::from_secs(2)).await,
        "timed-out script {pid} is still running"
    );
}

#[tokio::test]
async fn unknown_path_and_wrong_method() {
    let agent = agent(Config::default()).await;
    let resp = agent
        .client
        .post(format!("http://{}/nope", agent.addr))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let resp = agent.client.get(agent.url("run")).send().await.unwrap();
    assert_eq!(resp.status(), 405);
}

#[tokio::test]
async fn unread_response_does_not_block_other_requests() {
    // Concurrency smoke test: a client that sends a full request but never
    // reads still gets its response drained into kernel buffers and must
    // not interfere with other connections. (The stall-until-deadline case
    // — where the peer's receive window is genuinely exhausted — is covered
    // by the conn_budget deadline in server.rs, not exercised here.)
    let agent = agent(Config::default()).await;

    let mut stalled = tokio::net::TcpStream::connect(agent.addr).await.unwrap();
    use tokio::io::AsyncWriteExt;
    let path = format!("{HOOK_PATH_PREFIX}suspend");
    stalled
        .write_all(
            format!("POST {path} HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{{}}").as_bytes(),
        )
        .await
        .unwrap();
    // Never read the response.

    // A normal client still gets served while the stalled one lingers.
    let resp = agent.post("suspend", "{}").await;
    assert_eq!(resp.status(), 200);
}

/// Flood protection: once MAX_CONNECTIONS (64) connections hold their
/// slots, the next one is dropped at accept — not parked until the 10 s
/// header timeout — and slots freed by closed peers serve hooks again.
#[tokio::test]
async fn connections_over_capacity_are_dropped_at_accept() {
    use tokio::io::AsyncReadExt;
    let agent = agent(Config::default()).await;

    // Idle peers send no bytes: each holds a slot until the header timeout.
    let mut held = Vec::new();
    for _ in 0..64 {
        held.push(tokio::net::TcpStream::connect(agent.addr).await.unwrap());
    }
    let mut over = tokio::net::TcpStream::connect(agent.addr).await.unwrap();
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(3), over.read(&mut buf))
        .await
        .expect("an over-cap connection must be dropped, not held open");
    assert!(
        matches!(read, Ok(0) | Err(_)),
        "an over-cap connection must get no response: {read:?}"
    );

    drop(held);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match agent.client.post(agent.url("validate")).send().await {
            Ok(resp) => {
                assert_eq!(resp.status(), 200);
                break;
            }
            Err(e) => {
                assert!(Instant::now() < deadline, "slots never freed: {e}");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

/// Slow-drip defence: a body that stalls mid-way is cut at the hook
/// deadline and answered, instead of holding the connection slot open.
#[tokio::test]
async fn slow_drip_body_is_cut_at_the_hook_deadline() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let agent = agent(Config {
        hook_budget: Duration::from_secs(1),
        ..Config::default()
    })
    .await;

    let mut conn = tokio::net::TcpStream::connect(agent.addr).await.unwrap();
    let path = format!("{HOOK_PATH_PREFIX}validate");
    // Promise 100 body bytes, send one, then stall.
    conn.write_all(
        format!("POST {path} HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\n\r\n{{").as_bytes(),
    )
    .await
    .unwrap();
    let t0 = Instant::now();
    let mut resp = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), conn.read_to_end(&mut resp))
        .await
        .expect("a stalled body must be cut at the hook deadline")
        .unwrap();
    // A build hook reports the unreadable body honestly (ADR-007).
    let resp = String::from_utf8_lossy(&resp);
    assert!(resp.starts_with("HTTP/1.1 413"), "got: {resp}");
    assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
}

#[tokio::test]
async fn run_repairs_identity_and_exposes_microvm_id_to_scripts() {
    // /run must rewrite machine-id under the (test-redirected) root and
    // expose microvmId to hooks.d as KAGEMUSHA_MICROVM_ID.
    let dir = fresh_dir("runid");
    let idroot = dir.join("root");
    let sub = dir.join("run");
    std::fs::create_dir_all(&sub).unwrap();
    let env_file = dir.join("env.txt");
    write_script(
        &sub,
        "10-env",
        &format!("#!/bin/sh\nenv > '{}'\n", env_file.display()),
    );

    let agent = agent(Config {
        hooks_dir: dir.0.clone(),
        identity_root: idroot.clone(),
        ..Config::default()
    })
    .await;
    let resp = agent
        .post(
            "run",
            r#"{"microvmId":"mvm-01234567-abcd-ef01-2345-6789abcdef01","runHookPayload":"{}"}"#,
        )
        .await;
    assert_eq!(resp.status(), 200);

    // machine-id derived from microvmId (uuid suffix → 32 hex).
    let mid = std::fs::read_to_string(idroot.join("etc/machine-id")).unwrap();
    assert_eq!(mid, "01234567abcdef0123456789abcdef01\n");
    assert_eq!(
        std::fs::read_to_string(idroot.join("run/machine-id")).unwrap(),
        mid
    );
    // hooks.d saw the identity env var.
    let env = std::fs::read_to_string(&env_file).unwrap();
    assert!(env.contains("KAGEMUSHA_MICROVM_ID=mvm-01234567-abcd-ef01-2345-6789abcdef01"));
    assert!(env.contains("KAGEMUSHA_HOOK=run"));
    // Repair writes under identity_root only — never into the hooks dir
    // (an app-writable hooks.d must not receive root-owned artifacts).
    assert!(!dir.join("etc/machine-id").exists());
}

#[tokio::test]
async fn run_loads_otlp_headers_file() {
    // The headers file is the per-VM credential channel: path in config,
    // contents provisioned at runtime and read inside /run.
    let dir = fresh_dir("otlphdr");
    let file = dir.join("headers.txt");
    std::fs::write(
        &file,
        "# comment\nAuthorization=Basic abc\n\nX-Scope-OrgID = t1\n",
    )
    .unwrap();

    let agent = agent(Config {
        otlp_headers_file: Some(file),
        ..Config::default()
    })
    .await;
    let resp = agent.post("run", "{}").await;
    assert_eq!(resp.status(), 200);

    let headers = agent.ctx.otlp_headers.lock().unwrap().clone();
    assert_eq!(
        headers,
        vec![
            ("Authorization".to_string(), "Basic abc".to_string()),
            ("X-Scope-OrgID".to_string(), "t1".to_string())
        ]
    );
}

#[tokio::test]
async fn duplicate_run_does_not_rewrite_identity() {
    // A forged-second /run must not re-poison machine-id or the stored id
    // (the platform sends /run once; the untrusted app shares this port).
    let dir = fresh_dir("duprun");
    let idroot = dir.join("root");
    let agent = agent(Config {
        identity_root: idroot.clone(),
        ..Config::default()
    })
    .await;

    let body = |uuid: &str| format!(r#"{{"microvmId":"mvm-{uuid}"}}"#);
    assert_eq!(
        agent
            .post("run", &body("01234567-abcd-ef01-2345-6789abcdef01"))
            .await
            .status(),
        200
    );
    assert_eq!(
        agent
            .post("run", &body("ffffffff-ffff-ffff-ffff-ffffffffffff"))
            .await
            .status(),
        200
    );

    // First wins: file and stored id keep the original value.
    let mid = std::fs::read_to_string(idroot.join("etc/machine-id")).unwrap();
    assert_eq!(mid, "01234567abcdef0123456789abcdef01\n");
    assert_eq!(
        agent.ctx.microvm_id.get().unwrap(),
        "mvm-01234567-abcd-ef01-2345-6789abcdef01"
    );
}

#[tokio::test]
async fn duplicate_run_does_not_relay_to_app_twice() {
    // hooks.d/run is strictly one-shot even unclaimed: the first /run
    // POST fires scripts + app relay (fail-open), later forged or
    // resent unclaimed POSTs must not re-deliver bodies to the app's
    // /run handler. A claiming /run still relays — the app must get
    // its real payload — but never re-runs the privileged scripts.
    let dir = fresh_dir("duprel");
    let order_file = dir.join("order.log");
    order_script(&dir, "run", &order_file);

    let app = stub_app(HashMap::new(), Duration::ZERO).await;
    let agent = agent(Config {
        app_hook_base: Some(format!("http://{}", app.addr)),
        hooks_dir: dir.0.clone(),
        ..Config::default()
    })
    .await;
    let run_hits = || {
        app.received
            .lock()
            .unwrap()
            .iter()
            .filter(|h| h.as_str() == "run")
            .count()
    };

    assert_eq!(agent.post("run", "{}").await.status(), 200);
    assert_eq!(agent.post("run", "not json").await.status(), 200);
    assert_eq!(agent.post("run", "{}").await.status(), 200);
    assert_eq!(run_hits(), 1, "unclaimed duplicates must not relay again");

    // The platform's real /run claims and relays (payload delivery),
    // but hooks.d stays strictly once.
    assert_eq!(
        agent
            .post(
                "run",
                r#"{"microvmId":"mvm-01234567-abcd-ef01-2345-6789abcdef01"}"#
            )
            .await
            .status(),
        200
    );
    assert_eq!(run_hits(), 2, "claiming /run still relays to the app");
    let log = std::fs::read_to_string(&order_file).unwrap();
    assert_eq!(
        log.lines().count(),
        1,
        "hooks.d/run must run exactly once: {log}"
    );
}

#[tokio::test]
async fn idless_or_badid_run_does_not_burn_the_real_oneshot() {
    // Regression for the claim gate: a parseable-but-ID-less body (`{}`)
    // and a parsable-but-env-unsafe id must NOT consume the single-shot —
    // the platform's real /run later still repairs identity.
    let dir = fresh_dir("claim");
    let idroot = dir.join("root");
    let agent = agent(Config {
        identity_root: idroot.clone(),
        ..Config::default()
    })
    .await;

    // ID-less body: runs scripts+relay (fail-open) but must not claim.
    assert_eq!(agent.post("run", "{}").await.status(), 200);
    // Env-unsafe id (JSON \u0000 escape): must not claim either.
    assert_eq!(
        agent
            .post("run", "{\"microvmId\":\"bad\\u0000id\"}")
            .await
            .status(),
        200
    );
    // Unparsable body: no id to read, so no claim (ADR-009).
    assert_eq!(agent.post("run", "not json").await.status(), 200);
    assert!(
        !idroot.join("etc/machine-id").exists(),
        "no identity repair before a real /run"
    );

    // The platform's real /run still claims and repairs.
    assert_eq!(
        agent
            .post(
                "run",
                r#"{"microvmId":"mvm-01234567-abcd-ef01-2345-6789abcdef01"}"#
            )
            .await
            .status(),
        200
    );
    let mid = std::fs::read_to_string(idroot.join("etc/machine-id")).unwrap();
    assert_eq!(mid, "01234567abcdef0123456789abcdef01\n");
}

/// `KAGEMUSHA_IDENTITY_REPAIR=false`: the claiming /run still stores
/// microvmId for hooks.d, but writes nothing under the identity root.
#[tokio::test]
async fn run_skips_identity_repair_when_disabled() {
    let dir = fresh_dir("norepair");
    let idroot = dir.join("root");
    let agent = agent(Config {
        identity_root: idroot.clone(),
        identity_repair: false,
        ..Config::default()
    })
    .await;

    let resp = agent
        .post(
            "run",
            r#"{"microvmId":"mvm-01234567-abcd-ef01-2345-6789abcdef01"}"#,
        )
        .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        agent.ctx.microvm_id.get().unwrap(),
        "mvm-01234567-abcd-ef01-2345-6789abcdef01"
    );
    assert!(
        !idroot.exists(),
        "disabled repair must not write machine-id or hostname"
    );
}

#[tokio::test]
async fn run_with_nul_microvm_id_does_not_panic() {
    // NUL in an env value makes Command spawn fail InvalidInput — hooks.d
    // would silently die under fail-open. The id must be rejected from env.
    let dir = fresh_dir("nul");
    let sub = dir.join("run");
    std::fs::create_dir_all(&sub).unwrap();
    let env_file = dir.join("env.txt");
    write_script(
        &sub,
        "10-env",
        &format!("#!/bin/sh\nenv > '{}'\n", env_file.display()),
    );

    let agent = agent(Config {
        hooks_dir: dir.0.clone(),
        ..Config::default()
    })
    .await;
    // NUL in the id: parses fine, fails env_safe_microvm_id → not stored,
    // and the env script still spawns (a NUL env value would fail it).
    let resp = agent.post("run", "{\"microvmId\":\"bad\\u0000id\"}").await;
    assert_eq!(resp.status(), 200);
    assert!(agent.ctx.microvm_id.get().is_none());
    let env = std::fs::read_to_string(&env_file).unwrap();
    assert!(!env.contains("KAGEMUSHA_MICROVM_ID"));
}

/// POST /suspend with `image_name` set and return the env a hooks.d
/// script saw.
async fn suspend_script_env_with_image_name(image_name: &str) -> String {
    let dir = fresh_dir("imgenv");
    let sub = dir.join("suspend");
    std::fs::create_dir_all(&sub).unwrap();
    let env_file = dir.join("env.txt");
    write_script(
        &sub,
        "10-env",
        &format!("#!/bin/sh\nenv > '{}'\n", env_file.display()),
    );

    let agent = agent(Config {
        hooks_dir: dir.0.clone(),
        image_name: Some(image_name.to_string()),
        ..Config::default()
    })
    .await;
    let resp = agent.post("suspend", "{}").await;
    assert_eq!(resp.status(), 200);
    std::fs::read_to_string(&env_file).unwrap()
}

#[tokio::test]
async fn image_name_reaches_script_env() {
    let env = suspend_script_env_with_image_name("my-image").await;
    assert!(
        env.lines().any(|l| l == "KAGEMUSHA_IMAGE_NAME=my-image"),
        "{env}"
    );
}

#[tokio::test]
async fn nul_image_name_never_reaches_script_env() {
    // Same gate class as microvmId: a NUL in a config-sourced image_name
    // would make every Command spawn fail InvalidInput. The env var
    // must be skipped, not carried.
    let env = suspend_script_env_with_image_name("bad\u{0}img").await;
    assert!(!env.contains("KAGEMUSHA_IMAGE_NAME"));
}

/// One request the stub collector received: (arrival time,
/// "path\nbody", `authorization` header).
type OtlpCapture = (SystemTime, String, Option<String>);

/// Stub OTLP/HTTP collector. The arrival time lets tests assert ordering
/// against local file timestamps.
async fn otlp_collector() -> (SocketAddr, Arc<Mutex<Vec<OtlpCapture>>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured: Arc<Mutex<Vec<OtlpCapture>>> = Arc::new(Mutex::new(Vec::new()));
    let c2 = captured.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let c2 = c2.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| {
                    let c2 = c2.clone();
                    async move {
                        let path = req.uri().path().to_string();
                        let authorization = req
                            .headers()
                            .get(hyper::header::AUTHORIZATION)
                            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned());
                        let body = req
                            .into_body()
                            .collect()
                            .await
                            .unwrap_or_default()
                            .to_bytes();
                        c2.lock().unwrap().push((
                            SystemTime::now(),
                            format!("{path}\n{}", String::from_utf8_lossy(&body)),
                            authorization,
                        ));
                        Ok::<_, std::convert::Infallible>(Response::new(Full::new(
                            bytes::Bytes::from("{}"),
                        )))
                    }
                });
                http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await
                    .ok();
            });
        }
    });
    (addr, captured)
}

#[tokio::test]
async fn suspend_exports_usage_metrics_to_otlp() {
    let app = stub_app(HashMap::new(), Duration::ZERO).await;
    let (otlp_addr, captured) = otlp_collector().await;
    let agent = agent(Config {
        app_hook_base: Some(format!("http://{}", app.addr)),
        otlp_endpoint: Some(format!("http://{otlp_addr}")),
        ..Config::default()
    })
    .await;

    // Claim first, so the VM's id and the identity derived from it exist
    // when the flush builds its export.
    let resp = agent
        .post(
            "run",
            r#"{"microvmId":"mvm-01234567-abcd-ef01-2345-6789abcdef01"}"#,
        )
        .await;
    assert_eq!(resp.status(), 200);
    assert!(agent.ctx.microvm_id.get().is_some());

    let resp = agent.post("suspend", "{}").await;
    // flush is synchronous in drain_flow — by the time the 200 comes
    // back the collector already has the POST.
    assert_eq!(resp.status(), 200);
    let got = captured.lock().unwrap();
    assert_eq!(got.len(), 1, "exactly one OTLP export per suspend");
    let mut lines = got[0].1.splitn(2, '\n');
    assert_eq!(lines.next(), Some("/v1/metrics"));
    let v: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    let metrics = v["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
        .as_array()
        .unwrap();
    assert!(
        metrics.iter().any(|m| m["name"] == "kagemusha.uptime"),
        "uptime counter must be exported: {metrics:?}"
    );
    // Facts only, no per-VM identity anywhere (ADR-003): not the id (nor
    // the hostname derived from it), not the machine-id derived from its
    // uuid. Both needles carry letters, so numeric timestamps can't match.
    let export = &got[0].1;
    assert!(!export.contains("mvm-01234567"), "id leaked: {export}");
    assert!(
        !export.contains("01234567abcdef0123456789abcdef01"),
        "machine-id leaked: {export}"
    );
    assert!(!export.contains("microvmId"));
    assert!(!export.contains("microvm_id"));
}

/// The per-VM credential read at /run rides on the export request and
/// wins over the image-baked header, which is sent until then. The image
/// name reaches the collector as `service.namespace`.
#[tokio::test]
async fn otlp_export_carries_headers_file_credential() {
    let dir = fresh_dir("otlpauth");
    let file = dir.join("headers.txt");
    std::fs::write(&file, "Authorization=Bearer per-vm\n").unwrap();
    let (otlp_addr, captured) = otlp_collector().await;
    let agent = agent(Config {
        otlp_endpoint: Some(format!("http://{otlp_addr}")),
        otlp_headers: vec![("Authorization".to_string(), "Bearer baked".to_string())],
        otlp_headers_file: Some(file),
        image_name: Some("my-image".to_string()),
        ..Config::default()
    })
    .await;

    assert_eq!(agent.post("suspend", "{}").await.status(), 200);
    assert_eq!(agent.post("run", "{}").await.status(), 200);
    assert_eq!(agent.post("suspend", "{}").await.status(), 200);

    let got = captured.lock().unwrap();
    let auth: Vec<_> = got.iter().map(|c| c.2.as_deref()).collect();
    assert_eq!(auth, [Some("Bearer baked"), Some("Bearer per-vm")]);
    let body = got[1].1.split_once('\n').unwrap().1;
    let v: serde_json::Value = serde_json::from_str(body).unwrap();
    let attrs = v["resourceMetrics"][0]["resource"]["attributes"]
        .as_array()
        .unwrap();
    assert!(
        attrs
            .iter()
            .any(|a| a["key"] == "service.namespace" && a["value"]["stringValue"] == "my-image"),
        "image name missing from resource attributes: {attrs:?}"
    );
}

/// Ordering guarantee: hooks.d scripts must run to completion BEFORE
/// the synchronous flush exports — a suspend hook writing state must not
/// race the last telemetry sample out the door (ADR-005 ordering).
#[tokio::test]
async fn suspend_runs_scripts_before_flush_export() {
    let dir = fresh_dir("test-ord");
    let script_dir = dir.join("suspend");
    std::fs::create_dir_all(&script_dir).unwrap();
    let marker = dir.join("script-ran");
    // Scripts see only the KAGEMUSHA_* allowlist — the marker goes beside
    // the hook dir (KAGEMUSHA_HOOK_DIR is <dir>/suspend).
    write_script(
        &script_dir,
        "01-marker.sh",
        "#!/bin/sh\nsleep 0.4\ntouch \"$KAGEMUSHA_HOOK_DIR/../script-ran\"\n",
    );
    let app = stub_app(HashMap::new(), Duration::ZERO).await;
    let (otlp_addr, captured) = otlp_collector().await;
    let agent = agent(Config {
        app_hook_base: Some(format!("http://{}", app.addr)),
        otlp_endpoint: Some(format!("http://{otlp_addr}")),
        hooks_dir: dir.0.clone(),
        ..Config::default()
    })
    .await;

    let resp = agent.post("suspend", "{}").await;
    assert_eq!(resp.status(), 200);

    let got = captured.lock().unwrap();
    assert_eq!(got.len(), 1, "flush POST reached the collector");
    let export_at = got[0].0;
    drop(got);

    // Marker written by the script must carry a wall-clock time at or
    // before the export arrived at the collector.
    let meta = std::fs::metadata(&marker).expect("suspend script ran");
    let wrote_at = meta.modified().unwrap();
    assert!(
        wrote_at <= export_at,
        "script marker ({wrote_at:?}) must precede flush export ({export_at:?})"
    );
}

#[tokio::test]
async fn suspend_without_endpoint_still_answers() {
    let app = stub_app(HashMap::new(), Duration::ZERO).await;
    let agent = agent(Config {
        app_hook_base: Some(format!("http://{}", app.addr)),
        ..Config::default()
    })
    .await;
    let resp = agent.post("suspend", "{}").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(agent.ctx.telemetry.flush_count(), 1);
}

#[tokio::test]
async fn transport_failure_on_build_hook_is_502() {
    // Build hooks answer honestly (ADR-002/006): nothing listening on
    // the app endpoint → Transport → 502, never a fail-open 200.
    let agent = agent(Config {
        app_hook_base: Some("http://127.0.0.1:1".to_string()),
        ..Config::default()
    })
    .await;
    let resp = agent.post("validate", "{}").await;
    assert_eq!(resp.status(), 502);
}

#[tokio::test]
async fn ready_probes_app_ready_url() {
    // Apps without a hook endpoint get a GET probe instead of a relay.
    let app = stub_app(HashMap::new(), Duration::ZERO).await;
    let live = agent(Config {
        app_ready_url: Some(format!("http://{}", app.addr)),
        ..Config::default()
    })
    .await;
    assert_eq!(live.post("ready", "{}").await.status(), 200);

    // Nothing listening → 502 (a build hook reports transport failure
    // honestly — never a fail-open or a misleading "not ready").
    let dead = agent(Config {
        app_ready_url: Some("http://127.0.0.1:1".to_string()),
        ..Config::default()
    })
    .await;
    assert_eq!(dead.post("ready", "{}").await.status(), 502);
}

/// The other two probe outcomes: the app's own non-2xx is "not ready"
/// (503), and an app that never answers inside the budget is 504.
#[tokio::test]
async fn ready_probe_reports_not_ready_and_timeout() {
    // The probe GETs "/", which the stub records as the empty segment.
    let statuses = HashMap::from([(String::new(), 500u16)]);
    let failing = stub_app(statuses, Duration::ZERO).await;
    let not_ready = agent(Config {
        app_ready_url: Some(format!("http://{}", failing.addr)),
        ..Config::default()
    })
    .await;
    assert_eq!(not_ready.post("ready", "{}").await.status(), 503);

    let silent = stub_app(HashMap::new(), Duration::from_secs(5)).await;
    let timed_out = agent(Config {
        app_ready_url: Some(format!("http://{}", silent.addr)),
        // Minimum clamped budget: the probe gives up well before the stub answers.
        hook_budget: Duration::from_secs(1),
        ..Config::default()
    })
    .await;
    assert_eq!(timed_out.post("ready", "{}").await.status(), 504);
}

/// With both set, `/ready` relays to the app and never probes
/// `app_ready_url` (ADR-005) — a dead probe URL can't turn it into 502.
#[tokio::test]
async fn ready_relay_wins_over_app_ready_url() {
    let app = stub_app(HashMap::new(), Duration::ZERO).await;
    let agent = agent(Config {
        app_hook_base: Some(format!("http://{}", app.addr)),
        app_ready_url: Some("http://127.0.0.1:1".to_string()),
        ..Config::default()
    })
    .await;
    assert_eq!(agent.post("ready", "{}").await.status(), 200);
    assert_eq!(app.received.lock().unwrap().as_slice(), ["ready"]);
}

/// A FIFO as the headers file must not hang /run: opening a writerless
/// FIFO would block a naive open forever — `O_NONBLOCK` makes the read
/// fail fast and the hook stays fail-open (ADR-012).
#[tokio::test]
async fn fifo_headers_file_fails_open_without_hanging() {
    let dir = fresh_dir("fifo");
    let fifo = dir.join("headers.fifo");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let agent = agent(Config {
        otlp_headers_file: Some(fifo),
        ..Config::default()
    })
    .await;

    let resp = agent.post("run", "{}").await;
    assert_eq!(resp.status(), 200);
    assert!(
        agent.ctx.otlp_headers.lock().unwrap().is_empty(),
        "an unreadable headers file must leave headers empty"
    );
}

/// A headers file that isn't readable at the first /run is retried on
/// each later /run POST until a non-empty set lands (`headers_loaded`).
/// Once loaded it locks: forged /runs must not swap credentials.
#[tokio::test]
async fn headers_file_retries_until_loaded_then_locks() {
    let dir = fresh_dir("hdrretry");
    let file = dir.join("headers.txt");
    let agent = agent(Config {
        otlp_headers_file: Some(file.clone()),
        ..Config::default()
    })
    .await;

    // Missing file: /run answers 200, headers stay empty, no lock yet.
    assert_eq!(agent.post("run", "{}").await.status(), 200);
    assert!(agent.ctx.otlp_headers.lock().unwrap().is_empty());

    // The file appears: the next /run picks it up.
    std::fs::write(&file, "Authorization=Bearer one\n").unwrap();
    assert_eq!(agent.post("run", "{}").await.status(), 200);
    assert_eq!(
        *agent.ctx.otlp_headers.lock().unwrap(),
        vec![("Authorization".to_string(), "Bearer one".to_string())]
    );

    // Loaded → locked: replacing the file must not swap credentials.
    std::fs::write(&file, "Authorization=Bearer two\n").unwrap();
    assert_eq!(agent.post("run", "{}").await.status(), 200);
    assert_eq!(
        *agent.ctx.otlp_headers.lock().unwrap(),
        vec![("Authorization".to_string(), "Bearer one".to_string())]
    );
}

/// The headers file is read before `hooks.d/run` (ADR-005): a file that
/// a `hooks.d/run` script writes misses the platform's `/run` and is
/// picked up only if another `/run` POST arrives.
#[tokio::test]
async fn headers_file_is_read_before_hooks_d_run() {
    let dir = fresh_dir("hdrorder");
    let file = dir.join("headers.txt");
    let sub = dir.join("run");
    std::fs::create_dir_all(&sub).unwrap();
    write_script(
        &sub,
        "10-provision",
        &format!(
            "#!/bin/sh\necho 'Authorization=Bearer late' > '{}'\n",
            file.display()
        ),
    );
    let agent = agent(Config {
        hooks_dir: dir.0.clone(),
        otlp_headers_file: Some(file.clone()),
        ..Config::default()
    })
    .await;

    let run = r#"{"microvmId":"mvm-01234567-abcd-ef01-2345-6789abcdef01"}"#;
    assert_eq!(agent.post("run", run).await.status(), 200);
    assert!(file.exists(), "the hooks.d/run script never ran");
    assert!(
        agent.ctx.otlp_headers.lock().unwrap().is_empty(),
        "the file was read after hooks.d/run on the same /run"
    );

    assert_eq!(agent.post("run", run).await.status(), 200);
    assert_eq!(
        *agent.ctx.otlp_headers.lock().unwrap(),
        vec![("Authorization".to_string(), "Bearer late".to_string())]
    );
}

/// Fail-open end to end: the app is already gone, yet hooks.d must
/// still run and the flush must still fire — the marker file proves
/// the pipeline continued past the transport failure.
#[tokio::test]
async fn runtime_relay_failure_still_runs_hooks_d_and_flush() {
    let dir = fresh_dir("fo");
    let marker = dir.join("script-ran");
    let sub = dir.join("suspend");
    std::fs::create_dir_all(&sub).unwrap();
    write_script(
        &sub,
        "10-mark",
        &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    );
    let agent = agent(Config {
        app_hook_base: Some("http://127.0.0.1:1".to_string()),
        hooks_dir: dir.0.clone(),
        ..Config::default()
    })
    .await;

    let resp = agent.post("suspend", "{}").await;
    assert_eq!(resp.status(), 200);
    assert!(
        marker.exists(),
        "hooks.d must run even when the app is unreachable"
    );
    assert_eq!(agent.ctx.telemetry.flush_count(), 1);
}

/// An oversized body on a DRAIN hook must still flush — and, for
/// /terminate, still signal shutdown. A forged >64 KiB POST must not
/// let the lifecycle answer 200 without the agent doing its job.
#[tokio::test]
async fn oversized_body_on_terminate_still_flushes_and_terminates() {
    let agent = agent(Config::default()).await;
    let resp = agent.post("terminate", &"x".repeat(128 * 1024)).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(agent.ctx.telemetry.flush_count(), 1);
    assert!(
        agent
            .ctx
            .terminating
            .load(std::sync::atomic::Ordering::Acquire),
        "terminate must start shutdown even with an unreadable body"
    );
}

/// A slow body on a DRAIN hook must not starve the flush: at the smallest
/// hook budget, the body read stops short of the flush reserve, so the
/// export reaches the collector before the hook answers — not merely a
/// flush attempt that found no time left to send.
#[tokio::test]
async fn slow_drip_drain_body_still_exports_before_answering() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for hook in ["suspend", "terminate"] {
        let (otlp_addr, captured) = otlp_collector().await;
        let agent = agent(Config {
            otlp_endpoint: Some(format!("http://{otlp_addr}")),
            hook_budget: Duration::from_secs(1),
            flush_timeout: Duration::from_millis(400),
            ..Config::default()
        })
        .await;

        let (mut rd, mut wr) = tokio::net::TcpStream::connect(agent.addr)
            .await
            .unwrap()
            .into_split();
        let path = format!("{HOOK_PATH_PREFIX}{hook}");
        // Promise 100 body bytes, drip a few, then stall. The write half
        // comes back open: a closed one would end the read early instead.
        wr.write_all(
            format!("POST {path} HTTP/1.1\r\nHost: x\r\nContent-Length: 100\r\n\r\n{{").as_bytes(),
        )
        .await
        .unwrap();
        let drip = tokio::spawn(async move {
            for _ in 0..4 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                wr.write_all(b" ").await.unwrap();
            }
            wr
        });
        let t0 = Instant::now();
        let mut head = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut buf = [0u8; 1024];
            while !head.windows(2).any(|w| w == b"\r\n") {
                let n = rd.read(&mut buf).await.unwrap();
                assert!(n > 0, "{hook}: closed without an answer");
                head.extend_from_slice(&buf[..n]);
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{hook}: a stalled body must still be answered"));
        let answered_at = SystemTime::now();
        let head = String::from_utf8_lossy(&head);
        assert!(head.starts_with("HTTP/1.1 200"), "{hook}: got {head}");
        assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());

        let got = captured.lock().unwrap().clone();
        assert_eq!(
            got.len(),
            1,
            "{hook}: the export must arrive before the answer"
        );
        assert!(
            got[0].1.starts_with("/v1/metrics\n"),
            "{hook}: {}",
            got[0].1
        );
        assert!(got[0].0 <= answered_at);
        if hook == "terminate" {
            assert!(
                agent
                    .ctx
                    .terminating
                    .load(std::sync::atomic::Ordering::Acquire),
                "terminate must start shutdown after a stalled body"
            );
        }
        drop(drip.await.unwrap());
    }
}

/// Concurrent /run POSTs fire the pipeline once: while a claiming /run
/// is still inside its identity repair, a forged POST must wait on the
/// pipeline lock rather than fire the pipeline itself. hooks.d/run runs
/// a single time, before any relay, and the claimer still relays its
/// real payload.
#[tokio::test]
async fn concurrent_runs_fire_the_pipeline_once() {
    let dir = fresh_dir("runrace");
    let idroot = dir.join("root");
    let order_file = dir.join("order.log");
    order_script(&dir, "run", &order_file);
    let app = stub_app_opts(
        HashMap::new(),
        Duration::ZERO,
        Some(order_file.clone()),
        None,
        Vec::new(),
    )
    .await;
    let agent = agent(Config {
        app_hook_base: Some(format!("http://{}", app.addr)),
        hooks_dir: dir.0.clone(),
        identity_root: idroot,
        ..Config::default()
    })
    .await;

    // Fire the claiming /run and a flood of forged duplicates together.
    let claiming = agent.post(
        "run",
        r#"{"microvmId":"mvm-01234567-abcd-ef01-2345-6789abcdef01"}"#,
    );
    let forged1 = agent.post("run", "{}");
    let forged2 = agent.post("run", "{}");
    let (r1, r2, r3) = tokio::join!(claiming, forged1, forged2);
    assert_eq!(r1.status(), 200);
    assert_eq!(r2.status(), 200);
    assert_eq!(r3.status(), 200);

    let hits = app
        .received
        .lock()
        .unwrap()
        .iter()
        .filter(|h| h.as_str() == "run")
        .count();
    // The claiming run always relays its real payload — a forged POST
    // that took the lock first may add one earlier relay, but never
    // re-runs scripts and never steals the claim.
    assert!(
        (1..=2).contains(&hits),
        "expected the claimer's relay plus at most one forged: {hits}"
    );
    let log = std::fs::read_to_string(&order_file).unwrap();
    assert_eq!(log, format!("hooksd:run\n{}", "app:run\n".repeat(hits)));
    assert_eq!(
        agent.ctx.microvm_id.get().unwrap(),
        "mvm-01234567-abcd-ef01-2345-6789abcdef01"
    );
}
