use std::time::Duration;

use bytes::Bytes;
use reqwest::header::{CONTENT_ENCODING, CONTENT_TYPE, HeaderValue};

use crate::types::{HOOK_PATH_PREFIX, HookKind};

/// Largest app hook response body we buffer for pass-through.
const MAX_RELAY_BODY: usize = 64 * 1024;

/// The app's headers that say how to read its response body. The body is
/// relayed exactly as received — this client never decodes it — so these
/// go with it, byte for byte: a coded body without its Content-Encoding
/// would reach the platform as unreadable bytes of the declared type.
#[derive(Debug, Default)]
pub struct BodyHeaders {
    pub content_type: Option<HeaderValue>,
    /// Every Content-Encoding field line, in order (codings stack).
    pub content_encoding: Vec<HeaderValue>,
}

/// Result of forwarding one hook POST to the application's hook endpoint.
#[derive(Debug)]
pub enum RelayOutcome {
    /// App answered; status, body, and the body's headers pass through
    /// verbatim.
    Response {
        status: u16,
        body: Vec<u8>,
        headers: BodyHeaders,
    },
    /// App did not answer (or finish its body) within the given budget.
    Timeout,
    /// Connection-level failure (refused, DNS, reset, oversized body…).
    Transport(String),
}

impl RelayOutcome {
    /// Status code the platform should see when the app itself failed.
    pub fn status(&self) -> u16 {
        match self {
            RelayOutcome::Response { status, .. } => *status,
            RelayOutcome::Timeout => 504,
            RelayOutcome::Transport(_) => 502,
        }
    }

    pub fn ok(&self) -> bool {
        matches!(self, RelayOutcome::Response { status, .. } if (200..300).contains(status))
    }

    /// Human-readable failure detail for logs (transport error text or status).
    pub fn describe(&self) -> String {
        match self {
            RelayOutcome::Response { status, .. } => format!("http {status}"),
            RelayOutcome::Timeout => "timeout".to_string(),
            RelayOutcome::Transport(e) => format!("transport: {e}"),
        }
    }

    /// Whether the app actually produced an answer (vs. a synthesized
    /// timeout/transport failure). Only real answers pass through verbatim.
    pub fn answered(&self) -> bool {
        matches!(self, RelayOutcome::Response { .. })
    }
}

/// POST the hook body to `{app_hook_base}/aws/lambda-microvms/runtime/v1/<hook>`.
///
/// `budget` covers the whole round trip including reading the response body:
/// an app that dribbles its body cannot hold the hook open past the deadline.
/// The body is streamed through `Limited` so a dishonest or missing
/// Content-Length cannot allocate unbounded agent memory.
pub async fn relay(
    client: &reqwest::Client,
    app_hook_base: &str,
    hook: HookKind,
    body: Bytes,
    budget: Duration,
) -> RelayOutcome {
    if budget.is_zero() {
        return RelayOutcome::Timeout;
    }
    let deadline = crate::ctx::deadline_after(budget);
    let url = format!(
        "{}{}{}",
        app_hook_base.trim_end_matches('/'),
        HOOK_PATH_PREFIX,
        hook.path_segment()
    );
    // Hook bodies are JSON by the platform API contract; we declare it
    // unconditionally rather than forwarding an arbitrary client header.
    let fut = client
        .post(&url)
        .header(CONTENT_TYPE, "application/json")
        .body(body)
        .send();
    let resp = match tokio::time::timeout(budget, fut).await {
        Err(_) => return RelayOutcome::Timeout,
        Ok(Err(e)) => return RelayOutcome::Transport(e.without_url().to_string()),
        Ok(Ok(resp)) => resp,
    };
    let status = resp.status().as_u16();
    let headers = BodyHeaders {
        content_type: resp.headers().get(CONTENT_TYPE).cloned(),
        content_encoding: resp
            .headers()
            .get_all(CONTENT_ENCODING)
            .iter()
            .cloned()
            .collect(),
    };
    if resp
        .content_length()
        .is_some_and(|n| n as usize > MAX_RELAY_BODY)
    {
        return RelayOutcome::Transport(format!("body exceeds {MAX_RELAY_BODY} bytes"));
    }
    let left = crate::ctx::remaining(deadline);
    // Accumulate chunks with a hard cap — `resp.bytes()` would buffer the
    // whole body first, so a missing or dishonest Content-Length could OOM
    // the agent before any post-check ran.
    let collect = async {
        let mut resp = resp;
        let mut buf = Vec::new();
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    if buf.len() + chunk.len() > MAX_RELAY_BODY {
                        return Err(format!("body exceeds {MAX_RELAY_BODY} bytes"));
                    }
                    buf.extend_from_slice(&chunk);
                }
                Ok(None) => return Ok(buf),
                Err(e) => return Err(e.without_url().to_string()),
            }
        }
    };
    let body = match tokio::time::timeout(left, collect).await {
        Err(_) => return RelayOutcome::Timeout,
        Ok(Err(e)) => return RelayOutcome::Transport(e),
        Ok(Ok(b)) => b,
    };
    RelayOutcome::Response {
        status,
        body,
        headers,
    }
}

/// Outcome of one readiness GET — the build-hook contract needs to
/// tell "app answered non-2xx" apart from "couldn't reach it" and
/// "never answered" (503 vs 502 vs 504), so a bare bool won't do.
#[derive(Debug)]
pub enum ProbeOutcome {
    /// 2xx — the app says it is ready.
    Ready,
    /// Non-2xx answer — the app is up but not ready.
    NotReady,
    /// Connection-level failure (refused, DNS, reset).
    Transport,
    /// No answer inside the budget.
    Timeout,
}

/// GET a readiness URL once (for apps without a `/ready` hook endpoint).
pub async fn probe(client: &reqwest::Client, url: &str, budget: Duration) -> ProbeOutcome {
    match tokio::time::timeout(budget, client.get(url).send()).await {
        Ok(Ok(resp)) if resp.status().is_success() => ProbeOutcome::Ready,
        Ok(Ok(_)) => ProbeOutcome::NotReady,
        Ok(Err(_)) => ProbeOutcome::Transport,
        Err(_) => ProbeOutcome::Timeout,
    }
}
