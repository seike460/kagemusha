use std::ffi::OsString;
use std::path::PathBuf;
use std::process::exit;

use kagemusha::config::Config;

const USAGE: &str = "\
kagemusha — in-VM agent for AWS Lambda MicroVMs

USAGE:
    kagemusha [OPTIONS] -- <command> [args...]

OPTIONS:
    --config <PATH>   JSON config file (env vars override file values)
    -h, --help        Print help
    -V, --version     Print version

The agent takes the container entrypoint's place: it spawns <command>,
serves the platform lifecycle hooks, relays them to the application,
repairs per-VM identity at /run, and flushes telemetry synchronously
before suspend and terminate.
";

struct Args {
    config: Option<PathBuf>,
    command: Vec<OsString>,
}

fn parse_args() -> Result<Args, String> {
    let mut config = None;
    let mut command = Vec::new();
    // args_os, not args: a non-UTF-8 argv element (legal on unix) would
    // panic the iterator — and panic=abort takes PID 1 down at startup.
    let mut it = std::env::args_os().skip(1);
    while let Some(a) = it.next() {
        match a.to_str() {
            Some("--config") => {
                let p = it.next().ok_or("--config requires a path")?;
                // A flag-looking value is a missing path, not a path.
                // Non-UTF-8 values stay valid paths (PathBuf is OsStr).
                if p.to_str().is_some_and(|s| s.starts_with('-')) {
                    return Err(format!(
                        "--config requires a path, got flag {}",
                        p.to_string_lossy()
                    ));
                }
                config = Some(PathBuf::from(p));
            }
            Some("-h") | Some("--help") => {
                print!("{USAGE}");
                exit(0);
            }
            Some("-V") | Some("--version") => {
                println!("kagemusha {}", env!("CARGO_PKG_VERSION"));
                exit(0);
            }
            Some("--") => {
                command.extend(it);
                break;
            }
            Some(other) if other.starts_with('-') => {
                return Err(format!("unknown flag: {other}"));
            }
            _ => {
                command.push(a);
                command.extend(it);
                break;
            }
        }
    }
    Ok(Args { config, command })
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            exit(2);
        }
    };
    if args.command.is_empty() {
        eprintln!("{USAGE}");
        exit(2);
    }

    let cfg = Config::load(args.config.as_deref())?;
    // Config has copied what it needs — remove the credential-bearing
    // vars from OUR environ now, while still single-threaded. The
    // untrusted app runs as the same uid unless uid/gid are dropped, and
    // could otherwise read them back from /proc/1/environ.
    // SAFETY: no other thread exists yet; the runtime is built below.
    unsafe { kagemusha::supervisor::scrub_secret_env() };

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(args, cfg))
}

async fn run(args: Args, cfg: Config) -> anyhow::Result<()> {
    tracing::info!(
        hook_port = cfg.hook_port,
        otlp = cfg.otlp_endpoint.is_some(),
        "kagemusha starting"
    );

    let ctx = std::sync::Arc::new(kagemusha::ctx::AgentCtx::new(cfg)?);
    // Accept loop runs detached on the runtime; the address is logged
    // inside `serve` — nothing here needs the returned handle.
    kagemusha::hooks::serve(ctx.clone()).await?;
    // Usage meter: periodic cgroup v2 + uptime sampling; the flush path
    // reads `ctx.meter` (and takes a fresh sample at suspend/terminate).
    tokio::spawn(kagemusha::meter::run(ctx.clone()));

    // Supervise the app: forward signals, reap zombies centrally, run the
    // /terminate shutdown sequence, exit with the app's status.
    let code = kagemusha::supervisor::run(ctx, &args.command).await?;
    exit(code);
}
