# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-09-30

First release.

### Added

- Lifecycle hook relay for `ready`, `validate`, `run`, `resume`, `suspend` and `terminate`. Runtime hooks fail open; build hooks answer honestly (503 while the app is not ready, 502/504 when the relay fails). The app's real answers pass through verbatim: status, body, `Content-Type` and `Content-Encoding` (a compressed body is relayed as is, never decoded).
- Per-VM identity repair on the first `/run` that carries a valid `microvmId` (machine-id, hostname), and per-VM OTLP credentials read from `KAGEMUSHA_OTLP_HEADERS_FILE`.
- `hooks.d` user scripts, run in their own process group with an allowlisted environment.
- Synchronous OTLP/HTTP metrics flush before `/suspend` and `/terminate` answer. Without an OTLP endpoint nothing is sent; a failed or timed-out export is logged as a warning and the hook still answers (fail-open). The flush's time (`KAGEMUSHA_FLUSH_TIMEOUT_MS`) is reserved inside the hook budget: reading the request body, the app relay and `hooks.d` all stop short of it, so a slow or broken body can't leave the export without time. With a body they can't read, `/suspend` and `/terminate` carry on with `{}` and try the app relay, `hooks.d` and the flush in that order, within the remaining budget. Flushes run one at a time, from sampling through export, so overlapping hooks and the final flush send their points in the order they were sampled.
- Process supervision as PID 1: signal forwarding to the app's process group (the agent listens before it spawns the app, so a signal that arrives right after the app starts is forwarded, not lost), central zombie reaping, SIGTERM → grace → SIGKILL on `/terminate`, and the app's exit status as the agent's own.
- cgroup v2 usage facts (CPU, memory, uptime) for cost estimation.
- Configuration through environment variables or a JSON file (`--config`). Only the standard `OTEL_EXPORTER_OTLP_HEADERS` has its values percent-decoded, as the OpenTelemetry spec requires; `KAGEMUSHA_OTLP_HEADERS` and the `KAGEMUSHA_OTLP_HEADERS_FILE` file take values literally.
