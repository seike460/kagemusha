//! End-to-end supervisor tests against the real `kagemusha` binary:
//! signal forwarding, exit-code propagation, the /terminate shutdown
//! sequence, and the hooks.d path inside the real agent.

#![cfg(unix)]

mod common;

use std::process::Stdio;
use std::time::Duration;

use common::{
    AGENT, AgentProc, TestDir, fresh_dir, spawn_until_port, strip_agent_env, wait_exit, wait_file,
    wait_file_contains, wait_gone, wait_reaped, write_script,
};
use kagemusha::types::HOOK_PATH_PREFIX;

/// Build the agent Command for one spawn attempt: `kagemusha -- sh -c
/// <script>` with the shared env matrix. Every env var the agent's
/// config reads (`config::KNOWN_ENV_VARS`) that is not set here is
/// stripped so host configuration cannot leak in.
fn spawn_cmd(
    script: &str,
    hook_port: u16,
    hooks_dir: &std::path::Path,
    dir: &std::path::Path,
    extra_env: &[(&str, &str)],
) -> AgentProc {
    let mut command = std::process::Command::new(AGENT);
    strip_agent_env(&mut command);
    command
        .env("KAGEMUSHA_HOOK_PORT", hook_port.to_string())
        .env("KAGEMUSHA_SHUTDOWN_GRACE_MS", "1500")
        .env("KAGEMUSHA_HOOKS_DIR", hooks_dir)
        .env("KAGEMUSHA_IDENTITY_ROOT", dir.join("idroot"));
    for (k, v) in extra_env {
        command.env(k, v);
    }
    command.args(["--", "sh", "-c", script]);
    AgentProc::spawn(&mut command, dir.join("agent.log"))
}

/// Spawn once without waiting for the port (exit-code tests).
/// `hooks_dir` of `None` creates an empty dir inside the fixture dir.
/// Returns (dir, agent): callers bind in that order so drop order is
/// agent-then-dir — the child is killed before its fixtures are removed.
fn spawn_agent(
    script: &str,
    hook_port: u16,
    tag: &str,
    hooks_dir: Option<&std::path::Path>,
) -> (TestDir, AgentProc) {
    let dir = fresh_dir(&format!("sup-{tag}"));
    let hooks = match hooks_dir {
        Some(d) => d.to_path_buf(),
        None => {
            let d = dir.join("hooks.d");
            std::fs::create_dir_all(&d).unwrap();
            d
        }
    };
    let agent = spawn_cmd(script, hook_port, &hooks, &dir, &[]);
    (dir, agent)
}

/// Spawn an agent and wait for its hook port to answer; retries with a
/// fresh port when the child exits during startup (port TOCTOU).
/// Returns (dir, agent, port): agent is dropped before dir on unwind.
fn spawn_until_up(script: &str, tag: &str) -> (TestDir, AgentProc, u16) {
    spawn_until_up_with(script, tag, None)
}

fn spawn_until_up_with(
    script: &str,
    tag: &str,
    hooks_dir: Option<&std::path::Path>,
) -> (TestDir, AgentProc, u16) {
    let dir = fresh_dir(&format!("sup-{tag}"));
    let hooks = match hooks_dir {
        Some(d) => d.to_path_buf(),
        None => {
            let d = dir.join("hooks.d");
            std::fs::create_dir_all(&d).unwrap();
            d
        }
    };
    let (agent, port) = spawn_until_port(|port| spawn_cmd(script, port, &hooks, &dir, &[]));
    (dir, agent, port)
}

fn post_hook(port: u16, hook: &str) -> u16 {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}{HOOK_PATH_PREFIX}{hook}"))
            .body("{}")
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    })
}

#[test]
fn supervisor_spawns_the_app_command() {
    // The agent must actually exec the supervised command — a marker
    // the app itself writes proves the spawn path end to end (not just
    // "agent process is up").
    let dir = fresh_dir("spawnproof");
    let marker = dir.join("app-ran");
    let (_agent_dir, _agent) = spawn_agent(
        &format!("touch '{}'; sleep 60", marker.display()),
        0,
        "spawnproof",
        None,
    );
    wait_file(&marker, Duration::from_secs(5));
}

#[test]
fn app_exit_code_propagates() {
    // Port 0 binds an ephemeral port — no port TOCTOU to retry around.
    let (_dir, mut agent) = spawn_agent("exit 42", 0, "exit", None);
    assert_eq!(
        wait_exit(&mut agent.child, Duration::from_secs(10)),
        Some(42)
    );
}

#[test]
fn app_signal_exit_maps_to_128_plus_signal() {
    // kill -9 on itself → agent should exit 128+9 = 137.
    let (_dir, mut agent) = spawn_agent("kill -9 $$", 0, "sigexit", None);
    let code = wait_exit(&mut agent.child, Duration::from_secs(10));
    assert_eq!(code, Some(137));
}

#[test]
fn forwarded_signal_reaches_app_process_group() {
    let dir = fresh_dir("sig");
    let marker = dir.join("got-usr1");
    let ready = dir.join("trap-ready");
    // The app touches `ready` only after its trap is installed, and the
    // agent registers its signal streams before it spawns the app — so
    // `ready` proves both handlers are in place. No fixed sleep: a loaded
    // CI host can't race us into a default-disposition SIGUSR1 kill.
    let (_agent_dir, mut agent, _port) = spawn_until_up(
        &format!(
            "trap 'touch {}; exit 7' USR1; touch {}; sleep 60 & wait",
            marker.display(),
            ready.display()
        ),
        "fwd",
    );
    wait_file(&ready, Duration::from_secs(5));
    unsafe {
        libc::kill(agent.child.id() as i32, libc::SIGUSR1);
    }
    assert_eq!(
        wait_exit(&mut agent.child, Duration::from_secs(10)),
        Some(7)
    );
    assert!(marker.exists());
}

#[test]
fn terminate_hook_runs_graceful_shutdown() {
    let dir = fresh_dir("term");
    let marker = dir.join("got-term");
    let ready = dir.join("trap-ready");
    let (_agent_dir, mut agent, port) = spawn_until_up(
        &format!(
            "trap 'touch {}; exit 0' TERM; touch {}; sleep 60",
            marker.display(),
            ready.display()
        ),
        "term",
    );
    wait_file(&ready, Duration::from_secs(5));

    assert_eq!(post_hook(port, "terminate"), 200);
    // App trapped TERM and exited 0 → agent exits 0 after final flush.
    assert_eq!(
        wait_exit(&mut agent.child, Duration::from_secs(15)),
        Some(0)
    );
    assert!(marker.exists());
}

#[test]
fn terminate_sigkills_after_grace() {
    let dir = fresh_dir("kill");
    let ready = dir.join("trap-ready");
    let (_agent_dir, mut agent, port) = spawn_until_up(
        &format!("trap '' TERM; touch {}; sleep 60", ready.display()),
        "kill",
    );
    wait_file(&ready, Duration::from_secs(5));
    assert_eq!(post_hook(port, "terminate"), 200);
    // App ignores TERM: grace (1500ms env) expires → SIGKILL → 137.
    assert_eq!(
        wait_exit(&mut agent.child, Duration::from_secs(15)),
        Some(137)
    );
}

/// Regression for `AgentProc::drop`: a test that fails before its
/// /terminate drops the agent with the app still up. An app that ignores
/// the forwarded SIGTERM keeps the agent alive past the drop's wait, and
/// killing only the agent then orphaned the app's whole group.
#[test]
fn dropping_agent_kills_app_group_that_ignores_sigterm() {
    let dir = fresh_dir("dropkill");
    let pid_file = dir.join("app.pids");
    let (_agent_dir, agent, _port) = spawn_until_up(
        &format!(
            "trap '' TERM; sleep 60 & printf '%s %s\\n' $$ $! > '{}'; wait",
            pid_file.display()
        ),
        "dropkill",
    );
    // The app leader and its `sleep 60` — both ignore TERM. The app wrote
    // them, so it is up — and the agent registers its SIGTERM handler
    // before spawning the app: drop's SIGTERM reaches that handler.
    let pids: Vec<i32> = wait_file_contains(&pid_file, "\n", Duration::from_secs(5))
        .split_whitespace()
        .map(|p| p.parse().unwrap())
        .collect();

    drop(agent);
    for pid in pids {
        assert!(
            wait_gone(pid, Duration::from_secs(5)),
            "app-group member {pid} outlived AgentProc::drop"
        );
    }
}

/// Regression for the macOS `post_hook` ECONNRESET flake: a live port
/// probe must make the agent's bind fail. There `free_port`'s probe gets
/// `FD_CLOEXEC` only after socket(2) returns, so a child another test
/// thread spawns in that window inherits the probe and keeps it
/// listening after `free_port` drops it. A bind that succeeded beside it
/// (the old loopback probe vs the agent's wildcard bind — BSD
/// `SO_REUSEADDR` semantics) let the kernel route the test's 127.0.0.1
/// POST to the never-accepting probe, reset when its holder exited. A
/// failed bind is the TOCTOU `spawn_until_port` already retries. Holding
/// the probe here stands in for the leaked copy: same listening socket.
#[test]
fn live_port_probe_makes_the_agent_bind_fail() {
    let probe = common::port_probe();
    let port = probe.local_addr().unwrap().port();
    let (dir, mut agent) = spawn_agent("sleep 60", port, "probe", None);
    assert_eq!(
        wait_exit(&mut agent.child, Duration::from_secs(10)),
        Some(1),
        "the agent must not share a port with a live probe"
    );
    let log = std::fs::read_to_string(dir.join("agent.log")).unwrap();
    assert!(log.contains("Address already in use"), "{log}");
    drop(probe);
}

/// Regression for the post-drain `terminating` re-check: the app leader
/// exits while a detached group member lives on; a /terminate accepted
/// during the drain must still SIGKILL the whole app group — otherwise
/// the orphaned member outlives the VM's shutdown contract.
#[test]
fn terminate_during_drain_kills_orphaned_app_group() {
    let dir = fresh_dir("orphkill");
    let pid_file = dir.join("app.pids");
    let started = dir.join("suspend-started");
    let go = dir.join("leader-go");
    let hooks = dir.join("hooks.d");
    let sub = hooks.join("suspend");
    std::fs::create_dir_all(&sub).unwrap();
    // A slow suspend script keeps the hook in flight while the
    // supervisor drains — the /terminate lands inside that drain. It
    // marks its start, so the test knows the hook is in flight.
    write_script(
        &sub,
        "01-slow",
        &format!("#!/bin/sh\ntouch '{}'\nsleep 3\n", started.display()),
    );
    // App: fork a detached `sleep 30` into its own group, publish the
    // leader's pid and its, then the leader exits 0 once the test says go.
    let (_agent_dir, mut agent, port) = spawn_until_up_with(
        &format!(
            "sleep 30 & printf '%s %s\\n' $$ $! > '{}'; while [ ! -e '{}' ]; do sleep 0.05; done",
            pid_file.display(),
            go.display()
        ),
        "orphkill",
        Some(&hooks),
    );
    let pids: Vec<i32> = wait_file_contains(&pid_file, "\n", Duration::from_secs(5))
        .split_whitespace()
        .map(|p| p.parse().unwrap())
        .collect();
    let (leader_pid, orphan_pid) = (pids[0], pids[1]);

    // No fixed sleeps: each step waits for the state the next one needs.
    // /suspend runs its 3s script on a worker thread — in flight once
    // the script has started.
    let suspend = std::thread::spawn(move || post_hook(port, "suspend"));
    wait_file(&started, Duration::from_secs(5));
    // The leader exits; once the agent has reaped it, the supervisor is
    // past the app's death and draining the in-flight /suspend.
    std::fs::write(&go, "").unwrap();
    assert!(
        wait_reaped(leader_pid, Duration::from_secs(5)),
        "the agent did not reap the app leader {leader_pid}"
    );
    assert_eq!(post_hook(port, "terminate"), 200);
    assert_eq!(suspend.join().unwrap(), 200);

    // Leader exited 0 → agent exits 0 after the drain + recheck kill.
    assert_eq!(
        wait_exit(&mut agent.child, Duration::from_secs(15)),
        Some(0)
    );
    // The detached group member must have been SIGKILLed by the
    // post-drain recheck.
    assert!(
        wait_gone(orphan_pid, Duration::from_secs(5)),
        "orphaned app-group member survived /terminate"
    );
}

#[test]
fn hooks_d_script_runs_inside_real_agent() {
    // A script in <hooks_dir>/suspend/ is executed on the platform's
    // /suspend hook — the whole pipeline through the real binary.
    let dir = fresh_dir("hd");
    let sub = dir.join("suspend");
    std::fs::create_dir_all(&sub).unwrap();
    let marker = dir.join("script-ran");
    write_script(
        &sub,
        "10-mark",
        &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    );

    let (_fixture, mut agent, port) = spawn_until_up_with("sleep 60", "hd", Some(dir.as_path()));

    assert_eq!(post_hook(port, "suspend"), 200);
    assert!(marker.exists());
    assert!(agent.child.try_wait().unwrap().is_none());
}

#[test]
fn app_never_sees_agent_credentials() {
    // The app is untrusted — OTLP credentials in the agent's env must
    // not be inherited by the supervised child.
    let dir = fresh_dir("envscrub");
    let env_file = dir.join("appenv.txt");
    let script = format!("env > '{}'; sleep 60", env_file.display());

    let (_fixture, _agent) = {
        let fixture = fresh_dir("sup-scrub");
        let hooks = fixture.join("hooks.d");
        std::fs::create_dir_all(&hooks).unwrap();
        let mut cmd = std::process::Command::new(AGENT);
        strip_agent_env(&mut cmd);
        cmd.env("KAGEMUSHA_HOOK_PORT", "0") // no hooks are POSTed — ephemeral is enough
            .env("KAGEMUSHA_HOOKS_DIR", &hooks)
            .env("KAGEMUSHA_IDENTITY_ROOT", fixture.join("idroot"))
            .env("KAGEMUSHA_OTLP_HEADERS", "Authorization=Bearer topsecret")
            .env("KAGEMUSHA_OTLP_HEADERS_FILE", "/run/secrets/otlp")
            .env("KAGEMUSHA_OTLP_ENDPOINT", "http://user:pw@collector:4318")
            .env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://user:pw@otel:4318")
            .env("OTEL_EXPORTER_OTLP_HEADERS", "x-key=abc")
            .env("KAGEMUSHA_APP_HOOK_BASE", "http://user:pw@127.0.0.1:1")
            .env(
                "KAGEMUSHA_APP_READY_URL",
                "http://user:pw@127.0.0.1:1/ready",
            )
            .args(["--", "sh", "-c", &script]);
        let agent = AgentProc::spawn(&mut cmd, fixture.join("agent.log"));
        (fixture, agent)
    };

    let env = wait_file_contains(&env_file, "PATH=", Duration::from_secs(10));
    assert!(!env.contains("topsecret"), "app env leaked: {env}");
    assert!(!env.contains("KAGEMUSHA_OTLP_HEADERS"));
    assert!(!env.contains("KAGEMUSHA_OTLP_HEADERS_FILE"));
    assert!(!env.contains("OTEL_EXPORTER_OTLP_HEADERS"));
    assert!(
        !env.contains("OTLP_ENDPOINT"),
        "endpoint env leaked to app: {env}"
    );
    // App-facing URLs can carry userinfo credentials too — same denylist.
    assert!(!env.contains("KAGEMUSHA_APP_HOOK_BASE"));
    assert!(!env.contains("KAGEMUSHA_APP_READY_URL"));
    assert!(
        !env.contains("user:pw"),
        "endpoint userinfo leaked to app: {env}"
    );
    assert!(env.contains("PATH"), "app env should keep PATH: {env}");

    // remove_var alone is not enough on Linux — /proc/<pid>/environ shows
    // the initial stack's env region. The agent must have overwritten the
    // values in place at startup (they read back as empty "KEY=" strings).
    #[cfg(target_os = "linux")]
    {
        let proc_env = std::fs::read(format!("/proc/{}/environ", _agent.child.id())).unwrap();
        let proc_env = String::from_utf8_lossy(&proc_env);
        assert!(!proc_env.contains("topsecret"), "/proc environ leaked");
        assert!(!proc_env.contains("user:pw"), "/proc environ leaked");
    }
    // AgentProc's Drop kills+reaps the agent — no manual cleanup needed.
}

/// With `KAGEMUSHA_APP_UID`/`GID` the app runs as that user with no
/// supplementary groups. Without `CAP_SETGID`/`CAP_SETUID` (non-root) the
/// spawn fails closed: the agent exits 1 and the app never runs.
#[test]
fn app_uid_gid_drop_applies_or_fails_closed() {
    use std::os::unix::fs::PermissionsExt;
    let dir = fresh_dir("sup-uiddrop");
    // World-writable, so an app dropped to another user can write its ids.
    std::fs::set_permissions(&*dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let hooks = dir.join("hooks.d");
    std::fs::create_dir_all(&hooks).unwrap();
    let ids = dir.join("app-ids");
    let script = format!("{{ id -u; id -g; id -G; }} > '{}'", ids.display());
    let mut agent = spawn_cmd(
        &script,
        0,
        &hooks,
        &dir,
        &[
            ("KAGEMUSHA_APP_UID", "65534"),
            ("KAGEMUSHA_APP_GID", "65534"),
        ],
    );
    let code = wait_exit(&mut agent.child, Duration::from_secs(10));
    if unsafe { libc::geteuid() } == 0 {
        assert_eq!(code, Some(0));
        assert_eq!(
            std::fs::read_to_string(&ids).unwrap(),
            "65534\n65534\n65534\n"
        );
    } else {
        assert_eq!(code, Some(1), "the spawn must fail closed");
        assert!(!ids.exists(), "the app ran although the drop failed");
    }
}

/// `--config FILE` is read: a hook port set only in the file is the one
/// the agent listens on.
#[test]
fn config_file_sets_hook_port() {
    let dir = fresh_dir("sup-cfgfile");
    let hooks = dir.join("hooks.d");
    std::fs::create_dir_all(&hooks).unwrap();
    let cfg = dir.join("kagemusha.json");
    let (_agent, port) = spawn_until_port(|port| {
        let file = serde_json::json!({
            "hook_port": port,
            "hooks_dir": hooks,
            "identity_root": dir.join("idroot"),
        });
        std::fs::write(&cfg, file.to_string()).unwrap();
        let mut command = std::process::Command::new(AGENT);
        strip_agent_env(&mut command);
        command
            .arg("--config")
            .arg(&cfg)
            .args(["--", "sh", "-c", "sleep 60"]);
        AgentProc::spawn(&mut command, dir.join("agent.log"))
    });
    assert_eq!(post_hook(port, "suspend"), 200);
}

/// Argument errors exit 2 with the usage text before anything starts.
#[test]
fn bad_arguments_exit_2_with_usage() {
    let cases: &[(&[&str], &str)] = &[
        (
            &["--config", "--x", "--", "true"],
            "--config requires a path, got flag --x",
        ),
        (&["--config"], "--config requires a path"),
        (&["--bogus", "--", "true"], "unknown flag: --bogus"),
        (&[], "USAGE:"),
        (&["--"], "USAGE:"),
    ];
    for (args, message) in cases {
        let mut command = std::process::Command::new(AGENT);
        strip_agent_env(&mut command);
        let out = command
            .env("KAGEMUSHA_HOOK_PORT", "0")
            .args(*args)
            .stdin(Stdio::null())
            .output()
            .expect("run kagemusha");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {stderr}");
        assert!(stderr.contains(message), "{args:?}: {stderr}");
        assert!(stderr.contains("USAGE:"), "{args:?}: {stderr}");
    }
}

#[test]
fn version_and_help_flags_exit_0() {
    for flag in ["-V", "--version"] {
        let out = std::process::Command::new(AGENT)
            .arg(flag)
            .output()
            .expect("run kagemusha");
        assert_eq!(out.status.code(), Some(0));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("kagemusha {}\n", env!("CARGO_PKG_VERSION"))
        );
    }
    for flag in ["-h", "--help"] {
        let out = std::process::Command::new(AGENT)
            .arg(flag)
            .output()
            .expect("run kagemusha");
        assert_eq!(out.status.code(), Some(0));
        assert!(String::from_utf8_lossy(&out.stdout).contains("USAGE:"));
    }
}
