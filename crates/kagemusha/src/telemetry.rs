//! Telemetry sink — OTLP/HTTP JSON metrics exporter.
//!
//! The flush contract the hook pipeline needs: synchronous, bounded by a
//! deadline, and it *also* takes a fresh usage sample so suspend /
//! terminate export "now", not the last periodic tick. Budget split:
//! sampling gets at most 25% (capped at 3s), the export POST keeps the
//! rest — procfs never starves the network send.
//!
//! Wire format follows the OTLP/HTTP + protobuf-JSON mapping: int64 and
//! `timeUnixNano` are decimal strings, temporality `2` = CUMULATIVE.
//! Everything fails open — a broken collector must never stall a hook.
//!
//! Flushes run one at a time, from sampling through export. `/suspend`,
//! `/terminate` and the supervisor's final flush can overlap; if they
//! interleaved, an older CUMULATIVE point could reach the collector after
//! a newer one and read as a counter reset.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde::Serialize;
use tracing::{debug, warn};

use crate::ctx::AgentCtx;
use crate::meter::UsageSample;

#[derive(Default)]
pub struct Telemetry {
    flushes: AtomicUsize,
    /// Held from sampling through export, so points leave in the order
    /// they were sampled.
    in_flight: tokio::sync::Mutex<()>,
}

impl Telemetry {
    /// Flush pending telemetry: fresh usage sample inside ≤25% of
    /// `budget`, then one bounded OTLP/HTTP POST if an endpoint is
    /// configured. Waits for a flush already running; the wait comes out
    /// of `budget`, and a flush that cannot start within it exports
    /// nothing. Returns how many flushes were attempted. Fail-open:
    /// export errors are logged and never surfaced to the caller.
    pub(crate) async fn flush(&self, ctx: &AgentCtx, budget: Duration) -> usize {
        let t0 = Instant::now();
        // `timeout` polls the lock before its timer, so an uncontended
        // lock is taken even with a zero budget.
        let Ok(_guard) = tokio::time::timeout(budget, self.in_flight.lock()).await else {
            let n = self.flushes.fetch_add(1, Ordering::Relaxed) + 1;
            warn!("flush: another flush ran for the whole budget, export skipped");
            return n;
        };
        let budget = budget.saturating_sub(t0.elapsed());
        let t0 = Instant::now();
        let sample_cap = (budget / 4).min(Duration::from_secs(3));
        // The last stored sample is the fallback when the budget leaves
        // no time to take a fresh one.
        let mut sample = ctx.meter.latest();
        if !sample_cap.is_zero() {
            match tokio::time::timeout(sample_cap, crate::meter::sample_fresh(ctx)).await {
                Ok(s) => {
                    debug!(uptime_ms = s.uptime_ms, "flush: fresh usage sample");
                    // Use the returned sample directly: after a backward
                    // wall-clock step `record` keeps the older one, so
                    // re-reading `ctx.meter` may not give this sample back.
                    sample = Some(s);
                }
                Err(_) => {
                    debug!("flush: usage sampling exceeded budget, using last sample");
                    // A periodic tick may have landed while we waited —
                    // re-read instead of trusting the pre-sample snapshot.
                    sample = ctx.meter.latest();
                }
            }
        }
        // Relaxed suffices for a counter — nothing else is ordered by it.
        let n = self.flushes.fetch_add(1, Ordering::Relaxed) + 1;
        let Some(url) = ctx.cfg.otlp_url("v1/metrics") else {
            debug!("flush: no otlp_endpoint configured, export skipped");
            return n;
        };
        let Some(sample) = sample else {
            debug!("flush: no usage sample yet, export skipped");
            return n;
        };
        let remaining = budget.saturating_sub(t0.elapsed());
        if remaining.is_zero() {
            warn!("flush: no budget left after sampling, export skipped");
            return n;
        }
        match self.export(ctx, &url, &sample, remaining).await {
            Ok(()) => debug!("flush: OTLP metrics exported"),
            Err(e) => warn!(error = %e, "OTLP export failed (fail-open)"),
        }
        n
    }

    /// One bounded POST of the OTLP metrics payload.
    async fn export(
        &self,
        ctx: &AgentCtx,
        url: &str,
        sample: &UsageSample,
        timeout_d: Duration,
    ) -> anyhow::Result<()> {
        let payload = metrics_request(ctx, sample);
        let mut req = ctx.client.post(url).timeout(timeout_d).json(&payload);
        for (k, v) in effective_headers(ctx) {
            req = req.header(k, v);
        }
        // without_url(): reqwest's Display embeds the request URL —
        // never leak a credentialed endpoint into logs.
        let resp = req
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("{}", e.without_url()))?;
        if !resp.status().is_success() {
            anyhow::bail!("OTLP export got HTTP {}", resp.status());
        }
        Ok(())
    }

    /// Test/observability hook: how many flushes ran.
    pub fn flush_count(&self) -> usize {
        self.flushes.load(Ordering::Relaxed)
    }
}

/// Per-VM file headers (loaded at `/run`) win over the image-baked
/// `otlp_headers` — that file is the credential channel.
fn effective_headers(ctx: &AgentCtx) -> Vec<(String, String)> {
    let file_headers = ctx.otlp_headers.lock().unwrap().clone();
    if file_headers.is_empty() {
        ctx.cfg.otlp_headers.clone()
    } else {
        file_headers
    }
}

// ---- OTLP/HTTP JSON payload -------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportMetricsServiceRequest {
    resource_metrics: Vec<ResourceMetrics>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResourceMetrics {
    resource: Resource,
    scope_metrics: Vec<ScopeMetrics>,
}

#[derive(Serialize)]
struct Resource {
    attributes: Vec<Attribute>,
}

#[derive(Serialize)]
struct Attribute {
    key: String,
    value: AnyValue,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AnyValue {
    string_value: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScopeMetrics {
    scope: InstrumentationScope,
    metrics: Vec<Metric>,
}

#[derive(Serialize)]
struct InstrumentationScope {
    name: String,
    version: String,
}

#[derive(Serialize)]
struct Metric {
    name: String,
    unit: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    sum: Option<Sum>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gauge: Option<Gauge>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Sum {
    data_points: Vec<NumberDataPoint>,
    aggregation_temporality: i32,
    is_monotonic: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Gauge {
    data_points: Vec<NumberDataPoint>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NumberDataPoint {
    start_time_unix_nano: String,
    time_unix_nano: String,
    as_int: String,
}

const CUMULATIVE: i32 = 2;

/// Agent start in wall time — anchored once at `AgentCtx::new` so every
/// export emits a bit-identical `startTimeUnixNano` for the series'
/// whole lifespan (cumulative backends treat a moving start as reset).
fn start_nanos(ctx: &AgentCtx) -> u64 {
    let d = ctx
        .start_wall
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

fn counter(name: &str, unit: &'static str, v: u64, t: &str, start: &str) -> Metric {
    Metric {
        name: name.to_string(),
        unit,
        sum: Some(Sum {
            data_points: vec![NumberDataPoint {
                start_time_unix_nano: start.to_string(),
                time_unix_nano: t.to_string(),
                as_int: v.to_string(),
            }],
            aggregation_temporality: CUMULATIVE,
            is_monotonic: true,
        }),
        gauge: None,
    }
}

fn gauge(name: &str, v: u64, t: &str, start: &str) -> Metric {
    Metric {
        name: name.to_string(),
        unit: "By",
        sum: None,
        gauge: Some(Gauge {
            data_points: vec![NumberDataPoint {
                start_time_unix_nano: start.to_string(),
                time_unix_nano: t.to_string(),
                as_int: v.to_string(),
            }],
        }),
    }
}

/// Build the ExportMetricsServiceRequest for one usage sample. Facts
/// only (ADR-004/010): kernel counters and gauges, no rates, no per-VM
/// attributes (ADR-003). `None` fields simply don't become metrics.
fn metrics_request(ctx: &AgentCtx, s: &UsageSample) -> ExportMetricsServiceRequest {
    let t = (u128::from(s.unix_ms) * 1_000_000).to_string();
    let start = start_nanos(ctx).to_string();
    let mut metrics = vec![counter("kagemusha.uptime", "ms", s.uptime_ms, &t, &start)];
    // Counters — cumulative, monotonic.
    let counters = [
        ("kagemusha.cpu.usage_usec", s.cpu_usage_usec, "us"),
        ("kagemusha.cpu.user_usec", s.cpu_user_usec, "us"),
        ("kagemusha.cpu.system_usec", s.cpu_system_usec, "us"),
        ("kagemusha.cpu.throttled_usec", s.cpu_throttled_usec, "us"),
        ("kagemusha.cpu.nr_throttled", s.cpu_nr_throttled, "1"),
    ];
    for (name, val, unit) in counters {
        if let Some(v) = val {
            metrics.push(counter(name, unit, v, &t, &start));
        }
    }
    // Gauges — bytes.
    let gauges = [
        ("kagemusha.memory.current_bytes", s.memory_current_bytes),
        ("kagemusha.memory.peak_bytes", s.memory_peak_bytes),
        ("kagemusha.memory.max_bytes", s.memory_max_bytes),
    ];
    for (name, val) in gauges {
        if let Some(v) = val {
            metrics.push(gauge(name, v, &t, &start));
        }
    }

    let mut attrs = vec![
        Attribute {
            key: "service.name".into(),
            value: AnyValue {
                string_value: ctx.cfg.service_name.clone(),
            },
        },
        Attribute {
            key: "telemetry.sdk.name".into(),
            value: AnyValue {
                string_value: "kagemusha".into(),
            },
        },
        Attribute {
            key: "telemetry.sdk.language".into(),
            value: AnyValue {
                string_value: "rust".into(),
            },
        },
        Attribute {
            key: "telemetry.sdk.version".into(),
            value: AnyValue {
                string_value: env!("CARGO_PKG_VERSION").into(),
            },
        },
    ];
    // Bounded image name is the only identity-ish label we allow — gated
    // the same way as env exposure so a control-byte name can't pollute
    // collector labels either.
    if let Some(img) = ctx
        .cfg
        .image_name
        .as_ref()
        .filter(|n| crate::identity::env_safe_config_value(n))
    {
        attrs.push(Attribute {
            key: "service.namespace".into(),
            value: AnyValue {
                string_value: img.clone(),
            },
        });
    }
    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Resource { attributes: attrs },
            scope_metrics: vec![ScopeMetrics {
                scope: InstrumentationScope {
                    name: "kagemusha".into(),
                    version: env!("CARGO_PKG_VERSION").into(),
                },
                metrics,
            }],
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::test_support::TempDir;

    fn ctx_with(cg: &std::path::Path, endpoint: Option<&str>) -> AgentCtx {
        AgentCtx::new(Config {
            cgroup_root: cg.to_path_buf(),
            otlp_endpoint: endpoint.map(str::to_string),
            ..Config::default()
        })
        .unwrap()
    }

    fn sample() -> UsageSample {
        UsageSample {
            unix_ms: 1_700_000_000_000,
            uptime_ms: 42_000,
            cpu_usage_usec: Some(1_500),
            cpu_user_usec: Some(1_000),
            cpu_system_usec: Some(500),
            cpu_throttled_usec: Some(50),
            cpu_nr_throttled: Some(2),
            memory_current_bytes: Some(1 << 20),
            memory_peak_bytes: Some(2 << 20),
            memory_max_bytes: None,
        }
    }

    #[test]
    fn payload_has_otlp_shape() {
        let dir = std::env::temp_dir();
        let ctx = ctx_with(&dir, None);
        let v = serde_json::to_value(metrics_request(&ctx, &sample())).unwrap();
        let rm = &v["resourceMetrics"][0];
        let attrs = &rm["resource"]["attributes"];
        assert_eq!(attrs[0]["key"], "service.name");
        assert_eq!(attrs[0]["value"]["stringValue"], "kagemusha-app");
        let metrics = rm["scopeMetrics"][0]["metrics"].as_array().unwrap();
        let cpu = metrics
            .iter()
            .find(|m| m["name"] == "kagemusha.cpu.usage_usec")
            .unwrap();
        assert_eq!(cpu["sum"]["aggregationTemporality"], 2);
        assert_eq!(cpu["sum"]["isMonotonic"], true);
        assert_eq!(
            cpu["sum"]["dataPoints"][0]["asInt"], "1500",
            "int64 must serialize as string"
        );
        assert_eq!(
            cpu["sum"]["dataPoints"][0]["timeUnixNano"],
            "1700000000000000000"
        );
        let mem = metrics
            .iter()
            .find(|m| m["name"] == "kagemusha.memory.current_bytes")
            .unwrap();
        assert!(mem["gauge"]["dataPoints"][0]["asInt"].is_string());
        // "max" (unlimited) → no memory.max_bytes metric at all.
        assert!(
            metrics
                .iter()
                .all(|m| m["name"] != "kagemusha.memory.max_bytes")
        );
    }

    /// `image_name` becomes the `service.namespace` resource attribute,
    /// under the same env-safe gate as its script-env exposure.
    #[test]
    fn image_name_becomes_service_namespace() {
        let dir = std::env::temp_dir();
        let namespace = |image_name: Option<&str>| {
            let mut ctx = ctx_with(&dir, None);
            ctx.cfg.image_name = image_name.map(str::to_string);
            let v = serde_json::to_value(metrics_request(&ctx, &sample())).unwrap();
            v["resourceMetrics"][0]["resource"]["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["key"] == "service.namespace")
                .map(|a| a["value"]["stringValue"].clone())
        };
        assert_eq!(namespace(Some("my-image")), Some("my-image".into()));
        assert_eq!(namespace(None), None);
        assert_eq!(namespace(Some("bad\u{0}img")), None);
    }

    #[test]
    fn file_headers_win_over_cfg() {
        let dir = std::env::temp_dir();
        let mut ctx = ctx_with(&dir, None);
        ctx.cfg.otlp_headers = vec![("a".into(), "baked".into())];
        assert_eq!(effective_headers(&ctx)[0].1, "baked");
        *ctx.otlp_headers.lock().unwrap() = vec![("a".into(), "file".into())];
        assert_eq!(effective_headers(&ctx)[0].1, "file");
    }

    #[tokio::test]
    async fn flush_with_zero_budget_still_counts() {
        let dir = TempDir::new("z");
        let ctx = ctx_with(&dir, Some("http://127.0.0.1:1"));
        ctx.meter.record(sample());
        let n = ctx.telemetry.flush(&ctx, Duration::ZERO).await;
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn flush_leaves_fresh_sample_in_meter() {
        let dir = TempDir::new("tel");
        std::fs::write(dir.join("cpu.stat"), "usage_usec 777\n").unwrap();
        std::fs::write(dir.join("memory.current"), "42\n").unwrap();

        let ctx = ctx_with(&dir, None);
        assert!(ctx.meter.latest().is_none());

        ctx.telemetry.flush(&ctx, Duration::from_secs(5)).await;
        let s = ctx.meter.latest().expect("flush must record a sample");
        assert_eq!(s.cpu_usage_usec, Some(777));
        assert_eq!(s.memory_current_bytes, Some(42));
        assert_eq!(ctx.telemetry.flush_count(), 1);
    }

    #[tokio::test]
    async fn flush_posts_metrics_to_collector() {
        use http_body_util::{BodyExt, Full};
        use hyper::body::Incoming;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let svc = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
                let tx = tx.clone();
                async move {
                    // Collect the whole body, however the client split it.
                    let body = req.into_body().collect().await?.to_bytes();
                    let _ = tx.send(body);
                    Ok::<_, hyper::Error>(hyper::Response::new(Full::new(
                        bytes::Bytes::from_static(b"{}"),
                    )))
                }
            });
            hyper::server::conn::http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(sock), svc)
                .await
                .ok();
        });

        let dir = TempDir::new("e");
        std::fs::write(dir.join("cpu.stat"), "usage_usec 9\n").unwrap();
        let ctx = ctx_with(&dir, Some(&format!("http://127.0.0.1:{port}")));
        ctx.telemetry.flush(&ctx, Duration::from_secs(5)).await;
        let body = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("collector got no request")
            .expect("collector task ended without a body");
        let body: serde_json::Value =
            serde_json::from_slice(&body).expect("OTLP body must be JSON");
        let metrics = body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .expect("metrics array");
        assert!(
            metrics
                .iter()
                .any(|m| m["name"] == "kagemusha.cpu.usage_usec")
        );
    }

    /// `/suspend`, `/terminate` and the final flush can overlap; their
    /// exports must not. The collector holds each request for 300 ms, so
    /// two unserialized flushes would both be in it at once.
    #[tokio::test]
    async fn concurrent_flushes_export_one_at_a_time() {
        use std::sync::Arc;

        use http_body_util::{BodyExt, Full};
        use hyper::body::Incoming;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let served = Arc::new(AtomicUsize::new(0));
        let counters = (active.clone(), peak.clone(), served.clone());
        tokio::spawn(async move {
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                let counters = counters.clone();
                tokio::spawn(async move {
                    let svc = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
                        let (active, peak, served) = counters.clone();
                        async move {
                            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                            peak.fetch_max(now, Ordering::SeqCst);
                            req.into_body().collect().await?;
                            tokio::time::sleep(Duration::from_millis(300)).await;
                            active.fetch_sub(1, Ordering::SeqCst);
                            served.fetch_add(1, Ordering::SeqCst);
                            Ok::<_, hyper::Error>(hyper::Response::new(Full::new(
                                bytes::Bytes::from_static(b"{}"),
                            )))
                        }
                    });
                    hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(sock), svc)
                        .await
                        .ok();
                });
            }
        });

        let dir = TempDir::new("par");
        std::fs::write(dir.join("cpu.stat"), "usage_usec 9\n").unwrap();
        let ctx = ctx_with(&dir, Some(&format!("http://127.0.0.1:{port}")));
        let budget = Duration::from_secs(5);
        let (a, b) = tokio::join!(
            ctx.telemetry.flush(&ctx, budget),
            ctx.telemetry.flush(&ctx, budget)
        );
        assert_eq!(a.max(b), 2);
        assert_eq!(served.load(Ordering::SeqCst), 2, "both flushes must export");
        assert_eq!(peak.load(Ordering::SeqCst), 1, "exports must not overlap");
    }

    /// A flush that waits out its whole budget behind another one gives
    /// up without sampling, and still counts as attempted.
    #[tokio::test]
    async fn flush_gives_up_when_another_runs_past_its_budget() {
        let dir = TempDir::new("busy");
        let ctx = ctx_with(&dir, None);
        let _running = ctx.telemetry.in_flight.lock().await;
        let n = ctx.telemetry.flush(&ctx, Duration::from_millis(50)).await;
        assert_eq!(n, 1);
        assert!(ctx.meter.latest().is_none(), "it must not sample");
    }
}
