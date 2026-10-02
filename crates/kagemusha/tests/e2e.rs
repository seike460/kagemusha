//! Full-lifecycle E2E against the real `kagemusha` binary: a demo app
//! (`sh -c 'sleep 60'`), a hook simulator (the "application's" hook
//! endpoint), and a mock OTLP collector — all wired over real HTTP.
//!
//! Drives `ready → run → suspend → resume → terminate` and asserts the
//! contract end to end: relay order, identity repair under a redirected
//! root, hooks.d env exposure, synchronous OTLP export on suspend AND
//! terminate AND the post-death final flush, plus a clean shutdown with
//! the app's exit code. Unix-only: the suite leans on sh/sleep/signals.

#![cfg(unix)]

mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{
    AGENT, AgentProc, fresh_dir, spawn_until_port, status_code, strip_agent_env, write_script,
};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use kagemusha::types::HOOK_PATH_PREFIX;
use tokio::net::TcpListener;

/// Generic HTTP recorder: stores "path\nbody" per request, answers
/// `status` + "{}", forever.
struct Recorder {
    addr: SocketAddr,
    captured: Arc<Mutex<Vec<String>>>,
}

async fn recorder(status: u16) -> Recorder {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
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
                        let body = req
                            .into_body()
                            .collect()
                            .await
                            .unwrap_or_default()
                            .to_bytes();
                        c2.lock()
                            .unwrap()
                            .push(format!("{path}\n{}", String::from_utf8_lossy(&body)));
                        Ok::<_, std::convert::Infallible>(
                            Response::builder()
                                .status(status)
                                .header("content-type", "application/json")
                                .body(Full::new(bytes::Bytes::from("{}")))
                                .unwrap(),
                        )
                    }
                });
                http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await
                    .ok();
            });
        }
    });
    Recorder { addr, captured }
}

fn paths(r: &Recorder) -> Vec<String> {
    r.captured
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.lines().next().unwrap_or("").to_string())
        .collect()
}

struct E2eSpec {
    app_hook_base: String,
    otlp_endpoint: String,
    flush_timeout_ms: u64,
}

/// Spawn the agent and wait for its hook port (retries on port TOCTOU).
fn spawn_e2e(dir: &std::path::Path, spec: &E2eSpec, script: &str) -> (AgentProc, u16) {
    spawn_e2e_env(dir, spec, script, &[])
}

/// `spawn_e2e` with extra environment for the agent process.
fn spawn_e2e_env(
    dir: &std::path::Path,
    spec: &E2eSpec,
    script: &str,
    extra_env: &[(&str, &str)],
) -> (AgentProc, u16) {
    let hooks_dir = dir.join("hooks.d");
    let idroot = dir.join("idroot");
    let cgroup = dir.join("cgroup");
    spawn_until_port(|port| {
        let mut cmd = std::process::Command::new(AGENT);
        strip_agent_env(&mut cmd);
        cmd.env("KAGEMUSHA_HOOK_PORT", port.to_string())
            .env("KAGEMUSHA_APP_HOOK_BASE", &spec.app_hook_base)
            .env("KAGEMUSHA_OTLP_ENDPOINT", &spec.otlp_endpoint)
            .env("KAGEMUSHA_HOOKS_DIR", &hooks_dir)
            .env("KAGEMUSHA_IDENTITY_ROOT", &idroot)
            .env("KAGEMUSHA_CGROUP_ROOT", &cgroup)
            .env("KAGEMUSHA_SHUTDOWN_GRACE_MS", "1000")
            .env(
                "KAGEMUSHA_FLUSH_TIMEOUT_MS",
                spec.flush_timeout_ms.to_string(),
            )
            .envs(extra_env.iter().copied())
            .args(["--", "sh", "-c", script]);
        AgentProc::spawn(&mut cmd, dir.join("agent.log"))
    })
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

/// One full lifecycle through the real binary. The demo app is a plain
/// `sleep 60` — the agent is everything around it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_lifecycle_e2e() {
    let app_hooks = recorder(200).await;
    let otlp = recorder(200).await;
    let dir = fresh_dir("life");
    let hooks_dir = dir.join("hooks.d");
    let idroot = dir.join("idroot");
    let cgroup = dir.join("cgroup");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    std::fs::create_dir_all(&cgroup).unwrap();

    // hooks.d script (per-hook subdir layout: <hooks_dir>/<hook>/): proves
    // the per-VM id reaches scripts inside the real binary at /run.
    let run_hooks = hooks_dir.join("run");
    std::fs::create_dir_all(&run_hooks).unwrap();
    let marker = dir.join("hook-marker");
    write_script(
        &run_hooks,
        "01-mark",
        &format!(
            "#!/bin/sh\necho \"id=$KAGEMUSHA_MICROVM_ID hook=$KAGEMUSHA_HOOK\" > {}\n",
            marker.display()
        ),
    );

    // Fake cgroup v2 tree so the meter has real counters to export.
    std::fs::write(
        cgroup.join("cpu.stat"),
        "usage_usec 9001\nuser_usec 7000\nsystem_usec 2001\nnr_throttled 3\nthrottled_usec 88\n",
    )
    .unwrap();
    std::fs::write(cgroup.join("memory.current"), "10485760\n").unwrap();
    std::fs::write(cgroup.join("memory.peak"), "20971520\n").unwrap();
    std::fs::write(cgroup.join("memory.max"), "536870912\n").unwrap();

    let spec = E2eSpec {
        app_hook_base: format!("http://{}", app_hooks.addr),
        otlp_endpoint: format!("http://{}", otlp.addr),
        flush_timeout_ms: 8000,
    };
    let (mut agent, hook_port) = spawn_e2e(&dir, &spec, "sleep 60");

    let client = client();
    let url = |seg: &str| format!("http://127.0.0.1:{hook_port}{HOOK_PATH_PREFIX}{seg}");

    // The platform's real sequence.
    let r = client.post(url("ready")).body("{}").send().await.unwrap();
    assert_eq!(r.status(), 200);
    let r = client
        .post(url("run"))
        .body(r#"{"microvmId":"mvm-01234567-abcd-ef01-2345-6789abcdef01","runHookPayload":"{\"tenant\":\"t1\"}"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let r = client.post(url("suspend")).body("{}").send().await.unwrap();
    assert_eq!(r.status(), 200);
    let r = client.post(url("resume")).body("{}").send().await.unwrap();
    assert_eq!(r.status(), 200);
    let r = client
        .post(url("terminate"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // The app hook simulator saw every lifecycle hook in order.
    assert_eq!(
        paths(&app_hooks),
        vec![
            format!("{HOOK_PATH_PREFIX}ready"),
            format!("{HOOK_PATH_PREFIX}run"),
            format!("{HOOK_PATH_PREFIX}suspend"),
            format!("{HOOK_PATH_PREFIX}resume"),
            format!("{HOOK_PATH_PREFIX}terminate"),
        ]
    );

    // Identity repair ran once under the redirected root, and the
    // machine-id is derived FROM the microvmId's embedded uuid — the
    // MicrovmId derivation path is pinned, not just kernel entropy.
    let machine_id = std::fs::read_to_string(idroot.join("run/machine-id")).unwrap();
    assert_eq!(machine_id.trim(), "01234567abcdef0123456789abcdef01");
    assert!(idroot.join("etc/machine-id").exists());
    assert!(idroot.join("etc/hostname").exists());

    // hooks.d ran inside the real binary and saw the repaired identity.
    let mark = std::fs::read_to_string(&marker).expect("hooks.d marker written");
    assert_eq!(
        mark,
        "id=mvm-01234567-abcd-ef01-2345-6789abcdef01 hook=run\n"
    );

    // The agent exits with the app's SIGTERM status (128+15). final_flush
    // is awaited before exit and its POST resolves only once the recorder
    // has answered — on the multi-thread runtime all three exports
    // (suspend drain + terminate drain + post-death final) have landed
    // by the time the child is gone.
    let deadline = Instant::now() + Duration::from_secs(15);
    let code = loop {
        if let Some(st) = agent.child.try_wait().unwrap() {
            break status_code(st);
        }
        assert!(Instant::now() < deadline, "agent did not exit in time");
        tokio::time::sleep(Duration::from_millis(30)).await;
    };
    assert_eq!(code, 143, "agent must exit with app's SIGTERM status");

    let got = otlp.captured.lock().unwrap();
    assert_eq!(
        got.len(),
        3,
        "suspend + terminate drain + final flush exports: {got:?}"
    );
    for entry in got.iter() {
        let mut lines = entry.splitn(2, '\n');
        assert_eq!(lines.next(), Some("/v1/metrics"));
        let v: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        let metrics = v["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .unwrap();
        assert!(
            metrics.iter().any(|m| m["name"] == "kagemusha.uptime"),
            "uptime always exported: {entry}"
        );
        assert!(
            metrics
                .iter()
                .any(|m| m["name"] == "kagemusha.cpu.usage_usec"
                    && m["sum"]["dataPoints"][0]["asInt"] == "9001"),
            "fake cgroup counter must flow through: {entry}"
        );
        assert!(
            metrics
                .iter()
                .any(|m| m["name"] == "kagemusha.memory.max_bytes"
                    && m["gauge"]["dataPoints"][0]["asInt"] == "536870912"),
            "memory.max gauge: {entry}"
        );
        // No per-VM identity leaks into telemetry (ADR-003). The
        // "mvm-" prefix can't collide with the numeric timestamps.
        assert!(!entry.contains("mvm-01234567"));
    }
}

/// A forged second `/run` must not re-poison identity: hooks.d keeps
/// seeing the FIRST microvmId (single-shot, ADR-009).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_run_cannot_reforge_identity() {
    let app_hooks = recorder(200).await;
    let dir = fresh_dir("forge");
    let run_hooks = dir.join("hooks.d/run");
    std::fs::create_dir_all(&run_hooks).unwrap();
    let marker = dir.join("runs.log");
    write_script(
        &run_hooks,
        "01-append",
        &format!(
            "#!/bin/sh\necho \"id=$KAGEMUSHA_MICROVM_ID\" >> {}\n",
            marker.display()
        ),
    );

    let spec = E2eSpec {
        app_hook_base: format!("http://{}", app_hooks.addr),
        // Privileged port 1 accepts no listeners — guaranteed unreachable.
        otlp_endpoint: "http://127.0.0.1:1".to_string(),
        flush_timeout_ms: 800,
    };
    let (_agent, hook_port) = spawn_e2e(&dir, &spec, "sleep 60");
    let client = client();
    let url = |seg: &str| format!("http://127.0.0.1:{hook_port}{HOOK_PATH_PREFIX}{seg}");

    let r = client
        .post(url("run"))
        .body(r#"{"microvmId":"mvm-real-1","runHookPayload":null}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // Forged duplicate — an app racing the platform sends a different id.
    let r = client
        .post(url("run"))
        .body(r#"{"microvmId":"attacker-id","runHookPayload":null}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let log = std::fs::read_to_string(&marker).unwrap();
    assert_eq!(
        log, "id=mvm-real-1\n",
        "second /run must not change identity NOR re-run scripts: {log}"
    );
}

/// A `/run` without a usable microvmId skips identity repair. The binary
/// must say so at the default log level — a platform format change would
/// otherwise leave every VM on the baked machine-id unnoticed — once,
/// not once per forged POST.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idless_run_warns_once_at_default_level() {
    let app_hooks = recorder(200).await;
    let dir = fresh_dir("idless");
    std::fs::create_dir_all(dir.join("hooks.d")).unwrap();

    let spec = E2eSpec {
        app_hook_base: format!("http://{}", app_hooks.addr),
        // Port 1 accepts no listeners → connection refused, no bind race.
        otlp_endpoint: "http://127.0.0.1:1".to_string(),
        flush_timeout_ms: 800,
    };
    // The default filter, pinned against an ambient RUST_LOG.
    let (agent, hook_port) = spawn_e2e_env(&dir, &spec, "sleep 60", &[("RUST_LOG", "info")]);
    let client = client();
    let url = format!("http://127.0.0.1:{hook_port}{HOOK_PATH_PREFIX}run");
    for _ in 0..3 {
        let r = client.post(&url).body("{}").send().await.unwrap();
        assert_eq!(r.status(), 200);
    }

    let log = std::fs::read(&agent.log).unwrap();
    let warns = String::from_utf8_lossy(&log)
        .lines()
        .filter(|l| l.contains(r#""level":"WARN""#) && l.contains("without a usable microvmId"))
        .count();
    assert_eq!(warns, 1, "expected exactly one warning in the agent log");
}

/// Collector down = fail-open: hooks still answer fast and the agent
/// stays up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_survives_dead_collector() {
    let app_hooks = recorder(200).await;
    let dir = fresh_dir("dead");
    std::fs::create_dir_all(dir.join("hooks.d")).unwrap();

    let spec = E2eSpec {
        app_hook_base: format!("http://{}", app_hooks.addr),
        // Port 1 accepts no listeners → connection refused, no bind race.
        otlp_endpoint: "http://127.0.0.1:1".to_string(),
        flush_timeout_ms: 800,
    };
    let (mut agent, hook_port) = spawn_e2e(&dir, &spec, "sleep 60");

    let client = client();
    let url = |seg: &str| format!("http://127.0.0.1:{hook_port}{HOOK_PATH_PREFIX}{seg}");
    let t0 = Instant::now();
    let r = client.post(url("suspend")).body("{}").send().await.unwrap();
    assert_eq!(r.status(), 200);
    // Connection refused fails in ms — the hook must not block on export.
    assert!(
        t0.elapsed() < Duration::from_secs(3),
        "dead collector must not stall the hook"
    );
    let r = client
        .post(url("terminate"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let deadline = Instant::now() + Duration::from_secs(15);
    let code = loop {
        if let Some(st) = agent.child.try_wait().unwrap() {
            break status_code(st);
        }
        assert!(Instant::now() < deadline, "agent did not exit in time");
        tokio::time::sleep(Duration::from_millis(30)).await;
    };
    assert_eq!(code, 143);
    assert_eq!(
        paths(&app_hooks),
        vec![
            format!("{HOOK_PATH_PREFIX}suspend"),
            format!("{HOOK_PATH_PREFIX}terminate"),
        ]
    );
}

/// Proxy variables in the image are for the app. The agent's own traffic
/// — the loopback relay and the OTLP export — must go direct, or hook
/// bodies and OTLP headers would detour through the proxy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_traffic_ignores_proxy_env() {
    let app_hooks = recorder(200).await;
    let otlp = recorder(200).await;
    let proxy = recorder(200).await;
    let dir = fresh_dir("proxy");
    std::fs::create_dir_all(dir.join("hooks.d")).unwrap();

    let spec = E2eSpec {
        app_hook_base: format!("http://{}", app_hooks.addr),
        otlp_endpoint: format!("http://{}", otlp.addr),
        flush_timeout_ms: 2000,
    };
    let proxy_url = format!("http://{}", proxy.addr);
    let (_agent, hook_port) = spawn_e2e_env(
        &dir,
        &spec,
        "sleep 60",
        &[
            ("HTTP_PROXY", &proxy_url),
            ("http_proxy", &proxy_url),
            ("ALL_PROXY", &proxy_url),
            ("NO_PROXY", ""),
            ("no_proxy", ""),
        ],
    );

    let url = format!("http://127.0.0.1:{hook_port}{HOOK_PATH_PREFIX}suspend");
    let r = client().post(url).body("{}").send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(paths(&proxy).is_empty(), "proxied: {:?}", paths(&proxy));
    assert_eq!(
        paths(&app_hooks),
        vec![format!("{HOOK_PATH_PREFIX}suspend")]
    );
    assert_eq!(paths(&otlp), vec!["/v1/metrics".to_string()]);
}

/// The app dies *while* `/terminate` is mid-flight (hooks.d script still
/// running). The supervisor must drain the hook — response, script,
/// flush — before `process::exit` can cut it. Regression for the
/// app-death-during-drain race.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn app_death_mid_terminate_still_completes_hook() {
    let app_hooks = recorder(200).await;
    let otlp = recorder(200).await;
    let dir = fresh_dir("middrain");
    let hooks_dir = dir.join("hooks.d");
    let cgroup = dir.join("cgroup");
    std::fs::create_dir_all(&cgroup).unwrap();
    std::fs::write(cgroup.join("memory.current"), "1048576\n").unwrap();

    // Terminate script outlives the app: it marks its start, then runs
    // 3s more before writing its completion marker.
    let term_hooks = hooks_dir.join("terminate");
    std::fs::create_dir_all(&term_hooks).unwrap();
    let started = dir.join("term-script-started");
    let marker = dir.join("term-script-ran");
    write_script(
        &term_hooks,
        "01-slow",
        &format!(
            "#!/bin/sh\ntouch '{}'\nsleep 3\ntouch '{}'\n",
            started.display(),
            marker.display()
        ),
    );

    let spec = E2eSpec {
        app_hook_base: format!("http://{}", app_hooks.addr),
        otlp_endpoint: format!("http://{}", otlp.addr),
        flush_timeout_ms: 8000,
    };
    // App exits 0 once the terminate script has started — inside the
    // terminate drain however long the agent took to come up. No fixed
    // sleep: a slow start can't let the app die before /terminate lands.
    let app = format!(
        "while [ ! -e '{}' ]; do sleep 0.05; done",
        started.display()
    );
    let (mut agent, hook_port) = spawn_e2e(&dir, &spec, &app);

    let client = client();
    let url = |seg: &str| format!("http://127.0.0.1:{hook_port}{HOOK_PATH_PREFIX}{seg}");
    // Without the drain-wait, the agent exits when the app dies and this
    // request fails (connection reset) instead of answering 200.
    let r = client
        .post(url("terminate"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(marker.exists(), "terminate hooks.d script must finish");

    let deadline = Instant::now() + Duration::from_secs(15);
    let code = loop {
        if let Some(st) = agent.child.try_wait().unwrap() {
            break status_code(st);
        }
        assert!(Instant::now() < deadline, "agent did not exit in time");
        tokio::time::sleep(Duration::from_millis(30)).await;
    };
    assert_eq!(code, 0, "agent must exit with the app's exit code");
    // Terminate's synchronous flush reached the collector before exit.
    assert!(
        otlp.captured
            .lock()
            .unwrap()
            .iter()
            .any(|s| s.contains("/v1/metrics")),
        "terminate flush must land before exit"
    );
}
