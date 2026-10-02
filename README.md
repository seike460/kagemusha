# kagemusha

kagemusha is a small agent that runs inside AWS Lambda MicroVMs as your container's entrypoint (PID 1). It relays lifecycle hooks, establishes per-VM identity at `/run`, synchronously flushes telemetry before suspend and terminate, and records cgroup v2 usage facts for cost estimation — to any OTLP/HTTP backend that accepts JSON-encoded payloads, such as the OpenTelemetry Collector.

[日本語 README](README.ja.md)

## What it does

- **Lifecycle hook forwarding** — `ready`, `validate`, `run`, `resume`, `suspend`, `terminate` are relayed to your application's hook server, in order, inside the platform deadline. Runtime hooks are fail-open: agent-side failures answer 200 while your app's real responses (including its errors) pass through verbatim — status, body, and the `Content-Type` and `Content-Encoding` that say how to read the body (a compressed body is relayed as is, never decoded). Build hooks (`ready`/`validate`) report honestly — `ready` answers 503 while the app isn't up, and synthesized relay failures answer 502/504.
- **Per-VM identity repair** — on the first `/run` that carries a valid `microvmId`, kagemusha derives the machine-id from `microvmId`, rewrites `/run/machine-id` + `/etc/machine-id` + hostname, and loads per-VM OTLP credentials from a file — repairing the values that were baked into the shared snapshot.
- **`hooks.d` user scripts** — executables under `<hooks_dir>/<hook>/` run in sorted order on `run`/`resume`/`suspend`/`terminate`, in their own process group, with a clean allowlisted environment. Scripts see exactly: `PATH` (inherited), `KAGEMUSHA_HOOK` (the hook name), `KAGEMUSHA_HOOK_DIR` (the per-hook dir, e.g. `<hooks_dir>/run/`), `KAGEMUSHA_IMAGE_NAME` (when env-safe), and `KAGEMUSHA_MICROVM_ID` (once `/run` claimed it). Everything else — including `KAGEMUSHA_HOOKS_DIR` and every other agent config var — is stripped (`env_clear`).
- **Synchronous telemetry flush** — with an OTLP endpoint configured, `/suspend` and `/terminate` try one bounded OTLP/HTTP POST of the latest usage facts *before* answering 200. Without an endpoint nothing is sent. A POST that fails or runs out of time is logged as a warning, and the hook still answers (fail-open).
- **Process supervision** — the app runs in its own process group; TERM/INT/QUIT/HUP/USR1/USR2 are forwarded to the whole group; zombies are reaped centrally (the agent is PID 1); `/terminate` runs SIGTERM → grace → SIGKILL → final flush, and the agent exits with the app's status.
- **Usage facts for cost estimation** — cgroup v2 `cpu.stat` (usage/user/system/throttled), `memory.current`, `memory.peak`, `memory.max`, and uptime. Facts only: rates and billing math belong to the collector/query side.
- **Fail-open agent internals** — a dead collector, a hung script, a broken app hook, or an absent cgroup never stalls the lifecycle answer.

## Quick start

```sh
cargo build --release
# Run as the VM's entrypoint — the app is supervised as a child process:
./target/release/kagemusha -- /path/to/your-app arg1 arg2
```

The hook server listens on `0.0.0.0:9000` and answers the platform's paths under `/aws/lambda-microvms/runtime/v1/{ready,validate,run,resume,suspend,terminate}`.

Outside a MicroVM (e.g. on a development host), set `KAGEMUSHA_IDENTITY_REPAIR=false`, or point `KAGEMUSHA_IDENTITY_ROOT` at a scratch directory such as `$(mktemp -d)`. With the defaults, the first `/run` that carries a valid `microvmId` rewrites the host's `/etc/machine-id`, `/run/machine-id` and `/etc/hostname`, and changes its hostname when the agent runs as root.

## Configuration

Everything is an environment variable. A JSON config file (`kagemusha --config FILE`) can set the same values: each key is the variable name without `KAGEMUSHA_`, lower-cased (e.g. `hook_budget_ms`, `app_hook_base`), and `identity_repair` takes a JSON boolean. `KAGEMUSHA_OTLP_HEADERS`, the `OTEL_*` variables and `RUST_LOG` have no file key. An unknown key is a startup error. Environment variables override file values.

| Variable | Default | Meaning |
|---|---|---|
| `KAGEMUSHA_HOOK_PORT` | `9000` | Hook listener port |
| `KAGEMUSHA_APP_HOOK_BASE` | — | Base URL of your app's hook server, e.g. `http://127.0.0.1:8080`; requests go to `<base>/aws/lambda-microvms/runtime/v1/<hook>` — the app serves the same hook paths |
| `KAGEMUSHA_HOOKS_DIR` | `/etc/kagemusha/hooks.d` | Root of per-hook script dirs (`<dir>/run/`, `<dir>/suspend/`, …) |
| `KAGEMUSHA_OTLP_ENDPOINT` / `OTEL_EXPORTER_OTLP_ENDPOINT` | — | OTLP base; exporter POSTs to `<base>/v1/metrics` (the signal-specific `OTEL_EXPORTER_OTLP_METRICS_*` vars are not read) |
| `KAGEMUSHA_OTLP_HEADERS` / `OTEL_EXPORTER_OTLP_HEADERS` | — | `k=v,k2=v2` static headers for **non-secret** values only (image-baked; credentials belong in `KAGEMUSHA_OTLP_HEADERS_FILE` — see Security). `OTEL_EXPORTER_OTLP_HEADERS` values are percent-decoded as the OpenTelemetry spec requires (`Basic%20abc` → `Basic abc`); `KAGEMUSHA_OTLP_HEADERS` values are taken literally (no percent-decoding) |
| `KAGEMUSHA_OTLP_HEADERS_FILE` | — | `k=v`-line file read at `/run`; **the per-VM credential channel** — wins over the env var. Values are taken literally (no percent-decoding) |
| `KAGEMUSHA_FLUSH_TIMEOUT_MS` | `8000` | Synchronous flush cap (200 ms–120 s; if it would swallow the whole hook budget, it's further capped at `HOOK_BUDGET/4`) |
| `KAGEMUSHA_HOOK_BUDGET_MS` | `55000` | Total per-hook processing cap (1 s–300 s) |
| `KAGEMUSHA_METER_INTERVAL_MS` | `15000` | Periodic cgroup sample interval (1 s–3600 s) |
| `KAGEMUSHA_SHUTDOWN_GRACE_MS` | `10000` | SIGTERM→SIGKILL grace on terminate (100 ms–30 s) |
| `KAGEMUSHA_IDENTITY_REPAIR` | `true` | machine-id/hostname repair at `/run`: `true`/`false`, `1`/`0`, `yes`/`no` or `on`/`off`; any other value is ignored with a warning |
| `KAGEMUSHA_IDENTITY_ROOT` | `/` | Root under which identity files are written (tests/dev) |
| `KAGEMUSHA_CGROUP_ROOT` | `/sys/fs/cgroup` | cgroup v2 mount root |
| `KAGEMUSHA_SERVICE_NAME` / `OTEL_SERVICE_NAME` | `kagemusha-app` | `service.name` resource attribute |
| `KAGEMUSHA_IMAGE_NAME` | — | Bounded image label → `service.namespace` |
| `KAGEMUSHA_APP_READY_URL` | — | Optional readiness probe inside `/ready`, used only when `KAGEMUSHA_APP_HOOK_BASE` is unset (otherwise `/ready` is relayed to the app) |
| `KAGEMUSHA_APP_UID` / `KAGEMUSHA_APP_GID` | — | Drop the app to this uid/gid (keeps root-owned files out of the app's reach) |
| `RUST_LOG` | `info` | tracing filter, e.g. `debug` or `kagemusha=trace` |

Set `KAGEMUSHA_HOOK_BUDGET_MS` below the hook timeout you configure on the platform — runtime hooks allow 1–60 s (default 55 s fits), image hooks up to 3600 s — so the agent always answers inside the platform's window.

The app doesn't inherit `KAGEMUSHA_APP_HOOK_BASE`, `KAGEMUSHA_APP_READY_URL`, or the OTLP endpoint, headers and header-file variables — including the standard `OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_EXPORTER_OTLP_HEADERS` (see Security notes). If the app uses an OpenTelemetry SDK, configure its exporter with the signal-specific variables (e.g. `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`), which pass through, or with the SDK's own configuration.

## Telemetry

One OTLP/HTTP JSON `ExportMetricsServiceRequest` per flush. Flushes run one at a time, from sampling through export, so points reach the collector in the order they were sampled; a flush that waits out its whole budget behind another one sends nothing. Counters (`sum`, CUMULATIVE, monotonic): `kagemusha.uptime` (ms), `kagemusha.cpu.{usage,user,system,throttled}_usec`, `kagemusha.cpu.nr_throttled`. Gauges: `kagemusha.memory.{current,peak,max}_bytes`. Resource attributes: `service.name`, `telemetry.sdk.*`, and `image_name` as `service.namespace`. No VM/tenant/session/request IDs in labels — identity lives in logs, never in metric cardinality (see `docs/decisions.md`).

**Limitation:** metrics can't tell apart VMs started from the same image. They carry no per-VM attribute, and `startTimeUnixNano` is when the agent started during the image build, so the snapshot gives every VM the same value. VMs from one image that run at the same time therefore write their CUMULATIVE counters into one series, and a backend sees their values interleaved, as false counter resets and jumps. Rates or sums over that series are not correct usage for those VMs.

## Security notes

To report a vulnerability, see [SECURITY.md](SECURITY.md). Please don't open a public issue.

- The app inside the MicroVM is untrusted: hook bodies are capped (64 KiB), and so are app *response* bodies (64 KiB) — an oversized response counts as a relay failure, reported honestly as 502/504 on build hooks and fail-open 200 on runtime hooks, hooks.d scripts get a clean allowlisted environment, request payload fields other than `microvmId` are never exported to script env (they'd be visible via `/proc/*/environ`), and the credential-bearing agent env vars (OTLP headers, header-file path, OTLP endpoints, app hook/ready URLs — URLs can embed userinfo credentials) are scrubbed from both the app's inherited environment and the agent's own `/proc/1/environ`. The denylist can't catch secrets an operator adds by hand (`AWS_*`, `DATABASE_URL`, …) — set `KAGEMUSHA_APP_UID`/`GID` to run the app as another user so root-owned files stay out of its reach. Setting only one of the pair leaves the other ID at 0 and is warned at startup. The drop clears supplementary groups too and fails the spawn closed if the agent lacks `CAP_SETGID`/`CAP_SETUID` (e.g. non-root), rather than running the app with leftover privilege.
- The hook port takes unauthenticated lifecycle commands: `/terminate` stops the app, and `/suspend` runs `hooks.d` with the agent's privileges. Keep the port off every external path. Scope the `allowedPorts` of MicroVM auth tokens to your app's ports — a token that allows all ports reaches the hook port through the MicroVM endpoint too (see [Networking](https://docs.aws.amazon.com/lambda/latest/dg/microvms-networking.html)). Outside a MicroVM (e.g. local development), the listener on `0.0.0.0` is reachable from the network, so firewall the port.
- hooks.d scripts must be idempotent: `resume`/`suspend`/`terminate` scripts re-run on every POST to the shared hook port. `hooks.d/run` fires exactly once — on the first `/run` POST whose body is read, claimed or not and even if the body isn't valid JSON — so a forged-first POST can trigger it before the platform's real `/run` (accepted residual: the socket can't tell senders apart), but never twice. A hook body that is over 64 KiB, too slow, or unreadable changes this: `/run` and `/resume` answer 200 at once and run neither scripts nor the app relay (no privileged scripts on junk input), while `/suspend` and `/terminate` carry on with an empty `{}` body. Within the remaining hook budget they try the app relay, then the `hooks.d` scripts, then the telemetry flush, in that order; a stage whose budget is used up doesn't run. The flush always keeps its own time (`KAGEMUSHA_FLUSH_TIMEOUT_MS`): reading the body, the relay and the scripts all stop short of it, so a slow body can't take it. `/terminate` still starts the shutdown.
- The agent's own HTTP traffic (app relay, readiness probe, OTLP export) ignores `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`. Those variables stay available to your app, but hook bodies and OTLP headers never detour through a proxy, so the OTLP endpoint must be reachable directly.
- Per-VM credentials go through `KAGEMUSHA_OTLP_HEADERS_FILE` read at `/run` — not the image env and not `runHookPayload`. Put the file in place before the platform's `/run` arrives: the agent reads it before `hooks.d/run`, so a `hooks.d/run` script can't provide it. A missing, unreadable, or header-less file is retried only when another `/run` POST arrives (the platform normally sends one), until it yields a non-empty header set; until then, exports carry only the static env headers.
- `/run` identity is single-shot: the first `/run` whose body carries a valid `microvmId` (1–128 bytes of printable ASCII) claims `microvmId` and machine-id repair. A `/run` without a usable id claims nothing, so a later `/run` that carries one still repairs. Once claimed, any later `/run` — forged or platform resent — cannot re-poison them. The identity files are written without following a symlink at the last path component, so a link planted before `/run` can't redirect the write; such a path is skipped and noted (fail-open).

## Example image

See [`examples/Dockerfile`](examples/Dockerfile) — multi-stage build producing a static musl binary plus your app, with `/sbin/kagemusha` as the entrypoint. The app runs as an unprivileged `app` user (uid/gid 10001) through `KAGEMUSHA_APP_UID`/`GID`. Build it from the repository root, which is the build context:

```sh
docker build -f examples/Dockerfile .
```

**Platform support:** the production target is Linux (MicroVM guest, PID 1). The crate also builds and runs on macOS for development and tests. Hook relay, signal forwarding, process-group kills, and `sethostname` (as root) take the same code path there, but macOS has no cgroup v2, so only `kagemusha.uptime` is exported, and the agent isn't PID 1, so it doesn't reap orphaned processes. Don't run untrusted workloads on non-Linux builds. Non-unix targets such as Windows are not supported, and the build stops with a compile error.

## Design

Architecture decisions live in [`docs/decisions.md`](docs/decisions.md) (ADR-001 onward, in Japanese). Agent-facing rules are in [AGENTS.md](AGENTS.md) (in Japanese).

## License

[Apache License 2.0](LICENSE)
