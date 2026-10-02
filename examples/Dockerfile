# Example MicroVM image build for kagemusha.
#
# Multi-stage: build a static musl binary, then a minimal runtime rootfs
# with your app plus /sbin/kagemusha as the entrypoint. The result is a
# filesystem ready to be snapshotted by the MicroVM image pipeline.
#
# Build from the repository root (the build context):
#   docker build -f examples/Dockerfile .

# ---- build stage -------------------------------------------------------
# Pinned to the repo toolchain (mise.toml) and to the runtime stage's
# Alpine release — a floating `rust:1-*` tag can jump minor versions
# between image builds.
FROM rust:1.98.1-alpine3.24 AS build
# Alpine's rust target is musl; its default crt-static links the binary
# statically — musl's `ldd` answers "Not a valid dynamic program" for it.
# `COPY . .` ships the whole context — the repo-root .dockerignore keeps
# target/, .git/, and backup/editor leftovers out of it.
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p kagemusha

# ---- runtime stage ------------------------------------------------------
FROM alpine:3.24

COPY --from=build /src/target/release/kagemusha /sbin/kagemusha

# Your application — replace with the real thing.
COPY examples/demo-app.sh /opt/demo-app.sh
RUN chmod +x /opt/demo-app.sh

# Optional user scripts, per hook:
#   hooks.d/run/01-my-script
#   hooks.d/suspend/10-checkpoint
# hooks.d must be root-owned and unwritable by the app: scripts run with
# the agent's (root) privileges — a writable dir is a privesc hole.
# COPY --chown=root:root --chmod=0755 hooks.d/ /etc/kagemusha/hooks.d/

# Non-secret config is image-baked env; secrets must NOT be — per-VM
# credentials arrive via KAGEMUSHA_OTLP_HEADERS_FILE, read at /run before
# hooks.d/run, so provision it before the platform's /run (a failed read
# is retried only if another /run POST arrives).
# The agent itself never passes those vars to the app (env is scrubbed
# at spawn), and it drops the app to the unprivileged uid/gid below so
# the root:root 0400 headers file stays unreadable to it.
ENV KAGEMUSHA_HOOK_PORT=9000
# ENV KAGEMUSHA_APP_HOOK_BASE=http://127.0.0.1:8080
# ENV KAGEMUSHA_OTLP_ENDPOINT=http://collector:4318
# ENV KAGEMUSHA_OTLP_HEADERS_FILE=/run/secrets/otlp-headers

# The app is untrusted: run it as an unprivileged user, never as root.
RUN adduser -D -u 10001 app
ENV KAGEMUSHA_APP_UID=10001 KAGEMUSHA_APP_GID=10001

# kagemusha is PID 1 (root, needs sethostname/reap): it supervises the
# app — which runs as the `app` user above — relays hooks, reaps zombies.
ENTRYPOINT ["/sbin/kagemusha", "--", "/opt/demo-app.sh"]
