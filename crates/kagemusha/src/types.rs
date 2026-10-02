use serde::Deserialize;

/// URL prefix the platform uses for all lifecycle hook POSTs.
pub const HOOK_PATH_PREFIX: &str = "/aws/lambda-microvms/runtime/v1/";

/// The six lifecycle hooks.
///
/// `Ready` and `Validate` run during image build, before token delivery.
/// `Run`, `Resume`, `Suspend`, `Terminate` run at runtime with a platform
/// timeout of at most 60 s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    Ready,
    Validate,
    Run,
    Resume,
    Suspend,
    Terminate,
}

impl HookKind {
    pub const ALL: [HookKind; 6] = [
        HookKind::Ready,
        HookKind::Validate,
        HookKind::Run,
        HookKind::Resume,
        HookKind::Suspend,
        HookKind::Terminate,
    ];

    pub fn path_segment(self) -> &'static str {
        match self {
            HookKind::Ready => "ready",
            HookKind::Validate => "validate",
            HookKind::Run => "run",
            HookKind::Resume => "resume",
            HookKind::Suspend => "suspend",
            HookKind::Terminate => "terminate",
        }
    }

    /// Parse a request path like `/aws/lambda-microvms/runtime/v1/<hook>`.
    pub fn from_path(path: &str) -> Option<Self> {
        let seg = path.strip_prefix(HOOK_PATH_PREFIX)?;
        let seg = seg.trim_matches('/');
        Self::ALL.iter().copied().find(|h| h.path_segment() == seg)
    }

    /// Build-time hooks run inside the image build and may legitimately fail
    /// the build when the app is broken. All others are runtime hooks, which
    /// must never wedge the workload (fail-open rule).
    pub fn is_build(self) -> bool {
        matches!(self, HookKind::Ready | HookKind::Validate)
    }
}

/// Body of the `/run` hook POST.
///
/// Measured behaviour (2026-08-05): the platform wraps the caller-supplied
/// payload — the body is `{"microvmId": "...", "runHookPayload": "<string>"}`
/// where `runHookPayload` is itself a *string* containing the JSON that was
/// passed to `RunMicrovm`. Both layers must be decoded.
#[derive(Debug)]
pub struct RunHookBody {
    pub microvm_id: Option<String>,
    /// Inner payload parsed as JSON, when it is JSON. May legitimately be any
    /// shape (object, string, number) so we keep a `serde_json::Value`.
    pub run_hook_payload: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunHookOuter {
    microvm_id: Option<String>,
    run_hook_payload: Option<serde_json::Value>,
}

impl RunHookBody {
    pub fn parse(body: &[u8]) -> anyhow::Result<Self> {
        let outer: RunHookOuter = serde_json::from_slice(body)?;
        let parsed = match outer.run_hook_payload {
            // Inner layer: the string itself may hold JSON.
            Some(serde_json::Value::String(s)) => {
                serde_json::from_str::<serde_json::Value>(&s).ok()
            }
            // Tolerate a non-string payload in case the platform changes.
            Some(other) => Some(other),
            None => None,
        };
        Ok(Self {
            microvm_id: outer.microvm_id,
            run_hook_payload: parsed,
        })
    }
}

/// Payload keys safe to emit in debug logs (bounded, non-secret by
/// contract). Anything else is forwarded to the app untouched; the agent
/// never logs the raw payload verbatim because callers may place secrets
/// in it.
pub const LOGGABLE_PAYLOAD_KEYS: &[&str] = &["image_name", "tenant", "session", "traceparent"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_paths_round_trip() {
        for h in HookKind::ALL {
            let p = format!("{}{}", HOOK_PATH_PREFIX, h.path_segment());
            assert_eq!(HookKind::from_path(&p), Some(h));
        }
        assert_eq!(HookKind::from_path("/healthz"), None);
        assert_eq!(
            HookKind::from_path("/aws/lambda-microvms/runtime/v1/nope"),
            None
        );
    }

    #[test]
    fn build_vs_runtime() {
        assert!(HookKind::Ready.is_build());
        assert!(HookKind::Validate.is_build());
        assert!(!HookKind::Suspend.is_build());
        assert!(!HookKind::Run.is_build());
    }

    #[test]
    fn run_body_unwraps_double_layer() {
        // Measured wire format: outer object, inner payload is a *string*.
        let body =
            br#"{"microvmId":"mvm-abc","runHookPayload":"{\"tenant\":\"t1\",\"token\":\"s\"}"}"#;
        let b = RunHookBody::parse(body).unwrap();
        assert_eq!(b.microvm_id.as_deref(), Some("mvm-abc"));
        let inner = b.run_hook_payload.unwrap();
        assert_eq!(inner["tenant"], "t1");
        assert_eq!(inner["token"], "s");
    }

    #[test]
    fn run_body_tolerates_plain_string_payload() {
        let body = br#"{"microvmId":"mvm-1","runHookPayload":"not-json"}"#;
        let b = RunHookBody::parse(body).unwrap();
        assert!(b.run_hook_payload.is_none());
    }

    #[test]
    fn run_body_tolerates_missing_fields() {
        let b = RunHookBody::parse(br#"{}"#).unwrap();
        assert!(b.microvm_id.is_none());
        assert!(b.run_hook_payload.is_none());
    }

    #[test]
    fn run_body_errors_on_broken_outer_json() {
        assert!(RunHookBody::parse(b"not json").is_err());
        assert!(RunHookBody::parse(br#"{"microvmId":"#).is_err());
    }

    #[test]
    fn run_body_tolerates_null_payload() {
        let b = RunHookBody::parse(br#"{"microvmId":"m","runHookPayload":null}"#).unwrap();
        assert_eq!(b.microvm_id.as_deref(), Some("m"));
        assert!(b.run_hook_payload.is_none());
    }

    #[test]
    fn run_body_tolerates_object_payload() {
        // Platform-side change tolerance: payload as a real object, not a string.
        let b = RunHookBody::parse(br#"{"runHookPayload":{"tenant":"t9"}}"#).unwrap();
        assert_eq!(b.run_hook_payload.unwrap()["tenant"], "t9");
    }

    #[test]
    fn from_path_trims_trailing_slash() {
        assert_eq!(
            HookKind::from_path("/aws/lambda-microvms/runtime/v1/suspend/"),
            Some(HookKind::Suspend)
        );
    }
}
