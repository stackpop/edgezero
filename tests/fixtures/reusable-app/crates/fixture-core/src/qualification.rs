//! Opt-in, synthetic hosted probes through standard application/adaptor lifecycles.

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use edgezero_core::app::{App, Hooks};
use edgezero_core::context::RequestContext;
use edgezero_core::http::{Method, Response, StatusCode, response_builder};
use edgezero_core::ingress::{AdmissionDecision, IngressGrant};
use edgezero_core::response_egress::{
    ResponseEgressCompletion, ResponseEgressDeadline, ResponseEgressResource,
};
use edgezero_core::router::RouterService;
use edgezero_core::{Body, EdgeError, action};
use futures::Stream;
use serde_json::{Value, json};

const CASES: &[&str] = &[
    "normal",
    "head",
    "204",
    "205",
    "304",
    "slow",
    "nonread",
    "disconnect",
    "cancel",
    "pending",
    "expired",
    "source-error",
    "conversion-error",
];

pub struct QualificationApp;

#[derive(Default)]
struct Counters {
    polls: u64,
    polls_after_terminal: u64,
    resource_drops: u64,
    sequence: u32,
    source_drops: u64,
    terminals: u64,
}

struct ProbeTrace {
    artifact: String,
    case: String,
    counters: Mutex<Counters>,
    probe: String,
    revision: String,
    run: String,
}

struct ResourceDropProbe(Arc<ProbeTrace>);

struct ProbeGrant {
    resource: ResponseEgressResource<ResourceDropProbe>,
    trace: Arc<ProbeTrace>,
}

struct ProbeSource {
    chunk: Bytes,
    remaining: u32,
    trace: Arc<ProbeTrace>,
}

impl Hooks for QualificationApp {
    fn configure(app: &mut App) -> Result<(), EdgeError> {
        let revision = option_env!("EDGEZERO_QUALIFICATION_REVISION").unwrap_or("");
        let artifact = option_env!("EDGEZERO_QUALIFICATION_ARTIFACT").unwrap_or("");
        if !hex_identity(revision, 40) || !hex_identity(artifact, 64) {
            return Err(EdgeError::internal(std::io::Error::other(
                "qualification build identity is absent",
            )));
        }
        app.set_name("lifecycle-qualification");
        app.set_ingress_admission_policy(move |head| {
            let token = |name| head.headers().get(name).and_then(|value| value.to_str().ok())
                .filter(|value| valid_token(value)).map(str::to_owned);
            let case = head.target().path().strip_prefix("/qualification/")
                .filter(|name| CASES.contains(name));
            let (Some(run), Some(probe), Some(case)) = (token("x-qualification-run"), token("x-qualification-probe"), case) else {
                let mut response = Response::new(Body::from("invalid qualification probe\n"));
                *response.status_mut() = StatusCode::BAD_REQUEST;
                return AdmissionDecision::Refuse { completion: ResponseEgressCompletion::empty(),
                    response };
            };
            let trace = Arc::new(ProbeTrace { artifact: artifact.to_owned(), case: case.to_owned(),
                counters: Mutex::new(Counters::default()), probe, revision: revision.to_owned(), run });
            let (resource, ownership) = ResponseEgressCompletion::late_bound();
            let terminal_trace = Arc::clone(&trace);
            let terminal = ResponseEgressCompletion::new(move |report| {
                terminal_trace.record("terminal", |state| state.terminals += 1, json!({
                    "outcome": format!("{:?}", report.outcome), "body_kind": format!("{:?}", report.body_kind),
                    "bytes_written": report.bytes_written,
                    "fallback": report.fallback_disposition.map(|value| format!("{value:?}")),
                }));
            });
            AdmissionDecision::Admit {
                completion: terminal.join(ownership), grant: IngressGrant::new(ProbeGrant { resource, trace }),
                read_deadline: head.read_deadline_after(Duration::from_secs(1)),
            }
        });
        Ok(())
    }

    fn routes() -> RouterService {
        RouterService::builder()
            .get("/qualification/{case}", response_probe)
            .route("/qualification/{case}", Method::HEAD, response_probe)
            .build()
    }
}

impl ProbeTrace {
    fn event(&self, kind: &str, sequence: u32, fields: Value) -> Value {
        let mut event = json!({ "schema": 1, "run": self.run, "probe": self.probe, "case": self.case,
            "revision": self.revision, "artifact": self.artifact, "instance": "unobserved",
            "event": kind, "sequence": sequence });
        if let (Some(object), Some(fields)) = (event.as_object_mut(), fields.as_object()) {
            object.extend(
                fields
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone())),
            );
        }
        event
    }

    fn record(&self, kind: &str, update: impl FnOnce(&mut Counters), fields: Value) {
        let mut state = self
            .counters
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        update(&mut state);
        let event = self.event(kind, state.sequence, fields);
        state.sequence += 1;
        log::info!(target: "edgezero_qualification", "{event}");
    }
}

impl Drop for ProbeTrace {
    fn drop(&mut self) {
        let state = self
            .counters
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let event = self.event(
            "trace-closed",
            state.sequence,
            json!({ "terminals": state.terminals,
            "source_drops": state.source_drops, "resource_drops": state.resource_drops,
            "polls": state.polls, "polls_after_terminal": state.polls_after_terminal }),
        );
        log::info!(target: "edgezero_qualification", "{event}");
    }
}

impl Drop for ResourceDropProbe {
    fn drop(&mut self) {
        self.0.record(
            "resource-drop",
            |state| state.resource_drops += 1,
            json!({}),
        );
    }
}

impl ProbeSource {
    fn new(trace: Arc<ProbeTrace>) -> Self {
        let large = matches!(
            trace.case.as_str(),
            "slow" | "nonread" | "disconnect" | "cancel"
        );
        Self {
            chunk: Bytes::from(vec![b'x'; if large { 16 * 1024 } else { 1024 }]),
            remaining: if large { 512 } else { 3 },
            trace,
        }
    }
}

impl Stream for ProbeSource {
    type Item = Result<Bytes, EdgeError>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        {
            let mut state = self
                .trace
                .counters
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.polls += 1;
            if state.terminals != 0 {
                state.polls_after_terminal += 1;
            }
        }
        if self.trace.case == "pending" {
            return Poll::Pending;
        }
        if self.remaining == 0 {
            return Poll::Ready(None);
        }
        if self.trace.case == "source-error" && self.remaining == 2 {
            self.remaining = 0;
            return Poll::Ready(Some(Err(EdgeError::bad_gateway(
                "synthetic qualification source error",
            ))));
        }
        self.remaining -= 1;
        Poll::Ready(Some(Ok(self.chunk.clone())))
    }
}

impl Drop for ProbeSource {
    fn drop(&mut self) {
        self.trace
            .record("source-drop", |state| state.source_drops += 1, json!({}));
    }
}

fn hex_identity(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

#[action]
async fn response_probe(ctx: RequestContext) -> Result<Response, EdgeError> {
    let grant = ctx
        .take_ingress_grant()
        .and_then(|grant| grant.downcast::<ProbeGrant>().ok())
        .ok_or_else(|| {
            EdgeError::internal(std::io::Error::other("qualification grant is absent"))
        })?;
    grant
        .resource
        .install(ResourceDropProbe(Arc::clone(&grant.trace)))
        .map_err(|_error| {
            EdgeError::internal(std::io::Error::other("qualification resource is closed"))
        })?;
    let case = grant.trace.case.clone();
    let status = match case.as_str() {
        "204" => StatusCode::NO_CONTENT,
        "205" => StatusCode::RESET_CONTENT,
        "304" => StatusCode::NOT_MODIFIED,
        _ => StatusCode::OK,
    };
    let mut response = response_builder()
        .status(status)
        .header("content-type", "application/octet-stream")
        .body(Body::from_stream(ProbeSource::new(grant.trace)))
        .map_err(EdgeError::internal)?;
    if case == "conversion-error" {
        response.headers_mut().insert(
            "content-length",
            "not-a-length".parse().map_err(EdgeError::internal)?,
        );
    }
    response
        .extensions_mut()
        .insert(ResponseEgressDeadline::after(if case == "expired" {
            Duration::ZERO
        } else {
            Duration::from_millis(500)
        }));
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;

    #[test]
    fn finite_source_releases_once_and_never_polls_after_terminal() {
        let trace = test_trace("normal");
        let mut source = ProbeSource::new(Arc::clone(&trace));
        let mut bytes = 0;
        futures::executor::block_on(async {
            while let Some(chunk) = source.next().await {
                bytes += chunk.expect("source chunk").len();
            }
        });
        assert_eq!(bytes, 3072);
        drop(source);
        let counters = trace.counters.lock().expect("counters");
        assert_eq!(counters.source_drops, 1);
        assert_eq!(counters.polls, 4);
        assert_eq!(counters.polls_after_terminal, 0);
    }

    #[test]
    fn pending_source_is_pending_and_source_failure_is_real() {
        let trace = test_trace("pending");
        let mut source = Box::pin(ProbeSource::new(trace));
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        assert!(source.as_mut().poll_next(&mut cx).is_pending());
        let trace = test_trace("source-error");
        let source = ProbeSource::new(trace);
        let chunks = futures::executor::block_on(source.collect::<Vec<_>>());
        assert_eq!(chunks.len(), 2);
        assert!(chunks.last().expect("source error").is_err());
    }

    fn test_trace(case: &str) -> Arc<ProbeTrace> {
        Arc::new(ProbeTrace {
            artifact: "b".repeat(64),
            case: case.to_owned(),
            counters: Mutex::new(Counters::default()),
            probe: "test-1".to_owned(),
            revision: "a".repeat(40),
            run: "test-run".to_owned(),
        })
    }
}
