use std::error::Error as StdError;
use std::future::{self, Future};
use std::io;

use http_body::Body as HttpBody;
use hyper::Request;
use hyper::body::Incoming;
use hyper::server::conn::http1::Builder as Http1Builder;
use hyper::service::Service;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio::time::{Instant as TokioInstant, sleep_until};

use crate::header_deadline::{HeaderDeadline, HeaderDeadlineExceeded, HeaderIo, HeaderService};
use crate::ingress_config::AxumIngressConfig;
use crate::response::AxumBodyError;
use crate::response::EgressConnection;
use crate::service::AxumIngressAbort;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConnectionExit {
    AdmissionAborted,
    Completed,
    DeadlineExceeded,
}

/// Owns HTTP/1 parsing, idle/header expiry and the existing response deadline race.
#[expect(
    clippy::integer_division_remainder_used,
    reason = "tokio::select! expands to internal randomized branch selection arithmetic"
)]
pub(crate) fn serve_http1<ServiceType, ResponseBody>(
    stream: TcpStream,
    service: ServiceType,
    responses: EgressConnection,
    limits: AxumIngressConfig,
) -> impl Future<Output = Result<ConnectionExit, hyper::Error>>
where
    ServiceType: Service<Request<Incoming>, Response = hyper::Response<ResponseBody>>,
    ServiceType::Error: Into<Box<dyn StdError + Send + Sync>>,
    ServiceType::Future: 'static,
    ResponseBody: HttpBody + 'static,
    ResponseBody::Error: Into<Box<dyn StdError + Send + Sync>>,
{
    // Stamp before scheduling the connection future, not on its first poll.
    let head = HeaderDeadline::new(limits.head_read_timeout());
    async move {
        let connection_future = Http1Builder::new()
            .keep_alive(true)
            .max_headers(limits.max_header_count())
            .max_header_size(limits.max_raw_head_bytes())
            .max_buf_size(limits.parser_buffer_bytes())
            .header_read_timeout(limits.head_read_timeout())
            .timer(head.clone())
            .serve_connection(
                TokioIo::new(HeaderIo::new(stream, head.clone())),
                HeaderService::new(service, head.clone(), limits, responses.clone()),
            );
        let mut connection = Box::pin(connection_future);

        loop {
            if head
                .deadline()
                .is_some_and(|deadline| TokioInstant::now() >= deadline)
            {
                responses.signal_elapsed_deadline(TokioInstant::now());
                responses.signal_transport_error();
                drop(connection);
                return Ok(ConnectionExit::DeadlineExceeded);
            }
            let next_deadline = head
                .deadline()
                .into_iter()
                .chain(responses.next_deadline())
                .min();
            let wait = async move {
                if let Some(deadline) = next_deadline {
                    sleep_until(deadline).await;
                } else {
                    future::pending::<()>().await;
                }
            };
            tokio::select! {
                biased;
                () = wait => {
                    if responses.signal_elapsed_deadline(TokioInstant::now()) {
                        drop(connection);
                        return Ok(ConnectionExit::DeadlineExceeded);
                    }
                }
                result = &mut connection => {
                    responses.signal_transport_error();
                    drop(connection);
                    return classify_connection_result(result);
                }
                () = head.changed() => {}
                () = responses.changed() => {}
            }
        }
    }
}

fn classify_connection_result(
    result: Result<(), hyper::Error>,
) -> Result<ConnectionExit, hyper::Error> {
    match result {
        Ok(()) => Ok(ConnectionExit::Completed),
        Err(error) if error_chain_has_admission_abort(&error) => {
            Ok(ConnectionExit::AdmissionAborted)
        }
        Err(error) if error_chain_has_deadline(&error) => Ok(ConnectionExit::DeadlineExceeded),
        Err(error) => Err(error),
    }
}

fn error_chain_has_admission_abort(error: &hyper::Error) -> bool {
    let mut source = error.source();
    while let Some(current) = source {
        if current.downcast_ref::<AxumIngressAbort>().is_some() {
            return true;
        }
        source = current.source();
    }
    false
}

fn error_chain_has_deadline(error: &hyper::Error) -> bool {
    if error.is_timeout() {
        return true;
    }
    let mut source = error.source();
    while let Some(current) = source {
        if current
            .downcast_ref::<AxumBodyError>()
            .is_some_and(|body_error| body_error.is_deadline())
        {
            return true;
        }
        if current.downcast_ref::<HeaderDeadlineExceeded>().is_some() {
            return true;
        }
        if current
            .downcast_ref::<io::Error>()
            .and_then(io::Error::get_ref)
            .is_some_and(<dyn StdError + Send + Sync>::is::<HeaderDeadlineExceeded>)
        {
            return true;
        }
        source = current.source();
    }
    false
}

#[cfg(test)]
mod bounded_tests {
    use std::convert::Infallible;
    use std::time::Duration;

    use axum::body::Body;
    use futures::future::join;
    use hyper::Response;
    use hyper::service::service_fn;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::timeout;

    use super::serve_http1;
    use crate::ingress_config::AxumIngressConfig;
    use crate::response::EgressConnection;

    async fn raw_exchange(request: &[u8]) -> Vec<u8> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = async move {
            let (socket, _) = listener.accept().await.unwrap();
            let service = service_fn(|_request| async {
                Ok::<_, Infallible>(Response::new(Body::from("ok")))
            });
            let _result = serve_http1(
                socket,
                service,
                EgressConnection::default(),
                AxumIngressConfig::default(),
            )
            .await;
        };
        let client = async {
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream.write_all(request).await.unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            response
        };
        let ((), response) = timeout(Duration::from_secs(2), join(server, client))
            .await
            .unwrap();
        response
    }

    #[tokio::test]
    async fn accepted_deadline_precedes_first_poll_but_not_admitted_handler_work() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::time::sleep;

        for delayed_poll in [true, false] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&calls);
            let server = async move {
                let (socket, _) = listener.accept().await.unwrap();
                let service = service_fn(move |_request| {
                    let counted = Arc::clone(&observed);
                    async move {
                        counted.fetch_add(1, Ordering::SeqCst);
                        sleep(Duration::from_millis(200)).await;
                        Ok::<_, Infallible>(Response::new(Body::from("ok")))
                    }
                });
                let limits =
                    AxumIngressConfig::new(1, 8192, 100, Duration::from_millis(100)).unwrap();
                let connection = serve_http1(socket, service, EgressConnection::default(), limits);
                if delayed_poll {
                    sleep(Duration::from_millis(150)).await;
                }
                let _result = connection.await;
            };
            let client = async {
                let mut stream = TcpStream::connect(address).await.unwrap();
                stream
                    .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
                    .await
                    .unwrap();
                let mut response = Vec::new();
                let _read = stream.read_to_end(&mut response).await;
                response
            };
            let ((), response) = timeout(Duration::from_secs(2), join(server, client))
                .await
                .unwrap();
            if delayed_poll {
                assert!(response.is_empty());
                assert_eq!(calls.load(Ordering::SeqCst), 0);
            } else {
                assert!(response.starts_with(b"HTTP/1.1 200"));
                assert_eq!(calls.load(Ordering::SeqCst), 1);
            }
        }
    }

    #[tokio::test]
    async fn default_raw_head_limit_rejects_first_byte_over_before_service() {
        let prefix = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nX-Pad: ";
        let mut request = prefix.to_vec();
        request.extend(vec![b'a'; 0x0001_0000 - prefix.len() - 4]);
        request.extend_from_slice(b"\r\n\r\n");
        let response = raw_exchange(&request).await;
        assert!(response.starts_with(b"HTTP/1.1 200"));
        request.insert(prefix.len(), b'a');
        let rejected = raw_exchange(&request).await;
        assert!(rejected.starts_with(b"HTTP/1.1 431"), "unexpected status");
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::Poll;
    use std::time::Duration;

    use bytes::Bytes;
    use edgezero_core::app::App;
    use edgezero_core::body::Body;
    use edgezero_core::error::EdgeError;
    use edgezero_core::http::{HeaderMap, Method, StatusCode, Version, response_builder};
    use edgezero_core::ingress::{AdmissionDecision, IngressGrant};
    use edgezero_core::response_egress::{
        ResponseEgressAttempt, ResponseEgressCompletion, ResponseEgressHead,
        ResponseEgressObserver, ResponseEgressObserverHandle, ResponseEgressOutcome,
        ResponseEgressPolicy, ResponseEgressReport,
    };
    use edgezero_core::router::{RouteMetadata, RouterService};
    use edgezero_core::time::{Deadline, MonotonicClock, MonotonicInstant};
    use futures_util::stream::{pending, poll_fn, repeat_with};
    use hyper::Request;
    use hyper::Response;
    use hyper::body::Incoming;
    use hyper::service::service_fn;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::task::{LocalSet, spawn_local};
    use tokio::time::timeout;

    use super::{ConnectionExit, serve_http1};
    use crate::ingress_config::AxumIngressConfig;
    use crate::response::{AxumEgressBody, EgressConnection};
    use crate::service::AxumServiceState;

    #[derive(Clone, Copy)]
    enum DeclaredTail {
        Clean,
        Empty,
        EmptyForever,
        Error,
        Excess,
        Pending,
    }

    #[derive(Clone, Default)]
    struct RecordingObserver(Arc<Mutex<Vec<ResponseEgressReport>>>);

    struct SourceDropProbe(Arc<AtomicUsize>);

    impl ResponseEgressObserver for RecordingObserver {
        fn complete(&self, report: &ResponseEgressReport) {
            self.0.lock().expect("reports lock").push(report.clone());
        }
    }

    impl Drop for SourceDropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn declared_stream_app(
        length: &'static str,
        tail: DeclaredTail,
        observer: &RecordingObserver,
        polls: &Arc<AtomicUsize>,
        drops: &Arc<AtomicUsize>,
    ) -> App {
        let source_polls = Arc::clone(polls);
        let source_drops = Arc::clone(drops);
        let router = RouterService::builder()
            .get("/", move |_ctx| {
                let observed_polls = Arc::clone(&source_polls);
                let drop_probe = SourceDropProbe(Arc::clone(&source_drops));
                async move {
                    let stream = poll_fn(move |_context| {
                        let _keep_source_alive = &drop_probe;
                        let index = observed_polls.fetch_add(1, Ordering::SeqCst);
                        if index == 0 && length != "0" {
                            return Poll::Ready(Some(Ok(Bytes::from_static(b"abc"))));
                        }
                        match tail {
                            DeclaredTail::Empty if index <= 1 => {
                                Poll::Ready(Some(Ok(Bytes::new())))
                            }
                            DeclaredTail::Clean | DeclaredTail::Empty => Poll::Ready(None),
                            DeclaredTail::EmptyForever => Poll::Ready(Some(Ok(Bytes::new()))),
                            DeclaredTail::Error => Poll::Ready(Some(Err(EdgeError::internal(
                                anyhow::anyhow!("late source error"),
                            )))),
                            DeclaredTail::Excess => {
                                Poll::Ready(Some(Ok(Bytes::from_static(b"extra"))))
                            }
                            DeclaredTail::Pending => Poll::Pending,
                        }
                    });
                    let response = response_builder()
                        .header("content-length", length)
                        .body(Body::from_stream(stream))
                        .expect("declared response");
                    Ok::<_, EdgeError>(response)
                }
            })
            .build();
        let mut app = App::new(router);
        app.set_response_egress_observer(observer.clone());
        let budget = if matches!(tail, DeclaredTail::Pending | DeclaredTail::EmptyForever) {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(5)
        };
        app.set_response_egress_policy(move |_, started_at| ResponseEgressPolicy {
            write_deadline: Deadline::at_instant(started_at.checked_add(budget).expect("deadline")),
        });
        app
    }

    async fn exchange_with_app(app: App) -> (String, Result<ConnectionExit, hyper::Error>) {
        LocalSet::new()
            .run_until(async {
                let listener = TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind listener");
                let address = listener.local_addr().expect("listener address");
                let state = AxumServiceState::from_app(app);
                let server = spawn_local(async move {
                    let (socket, peer) = listener.accept().await.expect("accept client");
                    let responses = EgressConnection::default();
                    let service = state.for_connection(peer, responses.clone());
                    serve_http1(socket, service, responses, AxumIngressConfig::default()).await
                });
                let mut client = TcpStream::connect(address).await.expect("connect client");
                client
                    .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                    .await
                    .expect("write request");
                let mut wire = Vec::new();
                timeout(Duration::from_secs(2), client.read_to_end(&mut wire))
                    .await
                    .expect("bounded response lifetime")
                    .expect("read response");
                let exit = server.await.expect("server task");
                (String::from_utf8(wire).expect("ASCII response"), exit)
            })
            .await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn oversized_response_head_closes_and_completes_once_without_polling_body() {
        let observer = RecordingObserver::default();
        let polls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let resource_drops = Arc::new(AtomicUsize::new(0));
        let observed_polls = Arc::clone(&polls);
        let observed_drops = Arc::clone(&drops);
        let router = RouterService::builder()
            .get("/", move |_ctx| {
                let source_polls = Arc::clone(&observed_polls);
                let probe = SourceDropProbe(Arc::clone(&observed_drops));
                async move {
                    let source = poll_fn(move |_cx| {
                        let _keep_alive = &probe;
                        source_polls.fetch_add(1, Ordering::SeqCst);
                        Poll::<Option<Result<Bytes, EdgeError>>>::Pending
                    });
                    Ok::<_, EdgeError>(
                        response_builder()
                            .header(
                                "x-oversized",
                                "a".repeat(AxumIngressConfig::default().max_raw_head_bytes()),
                            )
                            .body(Body::from_stream(source))
                            .unwrap(),
                    )
                }
            })
            .build();
        let mut app = App::new(router);
        app.set_response_egress_observer(observer.clone());
        let observed_resources = Arc::clone(&resource_drops);
        app.set_ingress_admission_policy(move |head| AdmissionDecision::Admit {
            completion: ResponseEgressCompletion::new({
                let resource = SourceDropProbe(Arc::clone(&observed_resources));
                move |_report| drop(resource)
            }),
            grant: IngressGrant::empty(),
            read_deadline: head.read_deadline_after(Duration::from_secs(1)),
        });
        let (wire, exit) = exchange_with_app(app).await;
        assert!(wire.is_empty());
        let _error = exit.expect_err("oversized response head must close the connection");
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(resource_drops.load(Ordering::SeqCst), 1);
        let reports = observer.0.lock().unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::TransportError);
        assert_eq!(reports[0].bytes_written, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hyper_declared_stream_checks_clean_eof_including_zero() {
        for (length, tail, expected_body, expected_polls) in [
            ("3", DeclaredTail::Clean, "abc", 2),
            ("0", DeclaredTail::Clean, "", 1),
            ("3", DeclaredTail::Empty, "abc", 3),
            ("0", DeclaredTail::Empty, "", 3),
        ] {
            let observer = RecordingObserver::default();
            let polls = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let app = declared_stream_app(length, tail, &observer, &polls, &drops);

            let (wire, exit) = exchange_with_app(app).await;
            assert_eq!(exit.expect("clean connection"), ConnectionExit::Completed);
            let (head, body) = wire.split_once("\r\n\r\n").expect("response head");
            assert!(head.starts_with("HTTP/1.1 200"));
            assert_eq!(body, expected_body);
            assert_eq!(polls.load(Ordering::SeqCst), expected_polls);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            let reports = observer.0.lock().expect("reports lock");
            assert_eq!(reports.len(), 1);
            assert_eq!(reports[0].outcome, ResponseEgressOutcome::HostHandoff);
            assert_eq!(
                reports[0].bytes_written,
                u64::try_from(expected_body.len()).expect("body length")
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hyper_declared_stream_rejects_excess_and_error_tails() {
        for tail in [DeclaredTail::Excess, DeclaredTail::Error] {
            let observer = RecordingObserver::default();
            let polls = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let app = declared_stream_app("3", tail, &observer, &polls, &drops);

            let (wire, exit) = exchange_with_app(app).await;
            assert!(exit.is_err(), "bad tail must close the connection");
            assert!(!wire.ends_with("abc"), "unvalidated final frame was sent");
            assert_eq!(polls.load(Ordering::SeqCst), 2);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            let reports = observer.0.lock().expect("reports lock");
            assert_eq!(reports.len(), 1);
            assert_eq!(reports[0].outcome, ResponseEgressOutcome::SourceError);
            assert_eq!(reports[0].bytes_written, 0);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hyper_declared_stream_pending_eof_is_deadline_bounded() {
        let observer = RecordingObserver::default();
        let polls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let app = declared_stream_app("3", DeclaredTail::Pending, &observer, &polls, &drops);

        let (wire, exit) = exchange_with_app(app).await;
        assert_eq!(
            exit.expect("deadline closes connection"),
            ConnectionExit::DeadlineExceeded
        );
        assert!(!wire.ends_with("abc"));
        assert!(polls.load(Ordering::SeqCst) >= 2);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let reports = observer.0.lock().expect("reports lock");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::DeadlineExceeded);
        assert_eq!(reports[0].bytes_written, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hyper_zero_declared_stream_validation_is_bounded_before_handoff() {
        for tail in [
            DeclaredTail::Excess,
            DeclaredTail::Error,
            DeclaredTail::Pending,
        ] {
            let observer = RecordingObserver::default();
            let polls = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let app = declared_stream_app("0", tail, &observer, &polls, &drops);

            let (wire, exit) = exchange_with_app(app).await;
            assert_eq!(exit.expect("bounded fallback"), ConnectionExit::Completed);
            let expected = if matches!(tail, DeclaredTail::Pending) {
                ("HTTP/1.1 504", ResponseEgressOutcome::DeadlineExceeded)
            } else {
                ("HTTP/1.1 500", ResponseEgressOutcome::ConversionError)
            };
            assert!(wire.starts_with(expected.0));
            assert!(polls.load(Ordering::SeqCst) > 0);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            let reports = observer.0.lock().expect("reports lock");
            assert_eq!(reports.len(), 1);
            assert_eq!(reports[0].outcome, expected.1);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hyper_eof_lookahead_yields_to_host_deadlines_with_a_frozen_clock() {
        for length in ["3", "0"] {
            let observer = RecordingObserver::default();
            let polls = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let mut app = declared_stream_app(
                length,
                DeclaredTail::EmptyForever,
                &observer,
                &polls,
                &drops,
            );
            let frozen_at = MonotonicInstant::now();
            app.set_monotonic_clock(MonotonicClock::new(move || frozen_at));

            let (wire, exit) = exchange_with_app(app).await;
            if length == "3" {
                assert_eq!(
                    exit.expect("host deadline closes connection"),
                    ConnectionExit::DeadlineExceeded
                );
                assert!(!wire.ends_with("abc"));
            } else {
                assert_eq!(exit.expect("bounded fallback"), ConnectionExit::Completed);
                assert!(wire.starts_with("HTTP/1.1 504"));
            }
            assert!(polls.load(Ordering::SeqCst) > 1);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            let reports = observer.0.lock().expect("reports lock");
            assert_eq!(reports.len(), 1);
            assert_eq!(reports[0].outcome, ResponseEgressOutcome::DeadlineExceeded);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hyper_zero_length_registration_expiry_uses_timeout_fallback() {
        let observer = RecordingObserver::default();
        let polls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let mut app = declared_stream_app("0", DeclaredTail::Clean, &observer, &polls, &drops);
        let started_at = MonotonicInstant::now();
        let deadline = started_at
            .checked_add(Duration::from_secs(5))
            .expect("deadline");
        let observed_polls = Arc::clone(&polls);
        let after_eof_samples = AtomicUsize::new(0);
        app.set_monotonic_clock(MonotonicClock::new(move || {
            if observed_polls.load(Ordering::SeqCst) == 0
                || after_eof_samples.fetch_add(1, Ordering::SeqCst) == 0
            {
                started_at
            } else {
                deadline
            }
        }));

        let (wire, exit) = exchange_with_app(app).await;
        assert_eq!(exit.expect("bounded fallback"), ConnectionExit::Completed);
        assert!(
            wire.starts_with("HTTP/1.1 504"),
            "expiry at registration must remain a timeout"
        );
        assert_eq!(polls.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let reports = observer.0.lock().expect("reports lock");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::DeadlineExceeded);
    }

    fn attempt(
        observer: &RecordingObserver,
        started_at: MonotonicInstant,
    ) -> ResponseEgressAttempt {
        let headers = HeaderMap::new();
        let route = RouteMetadata::new(Method::GET, "/pending");
        let head = ResponseEgressHead::new(
            StatusCode::OK,
            Version::HTTP_11,
            &headers,
            None,
            started_at,
            Some(&route),
        );
        ResponseEgressAttempt::new(
            &head,
            started_at,
            ResponseEgressCompletion::empty(),
            ResponseEgressObserverHandle::new(observer.clone()),
            MonotonicClock::default(),
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn independent_deadline_closes_connection_with_pending_non_send_body() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let listener = TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind listener");
                let address = listener.local_addr().expect("listener address");
                let observer = RecordingObserver::default();
                let server_observer = observer.clone();

                let server = spawn_local(async move {
                    let (stream, _) = listener.accept().await.expect("accept client");
                    let responses = EgressConnection::default();
                    let service_responses = responses.clone();
                    let service = service_fn(move |_request: Request<Incoming>| {
                        let started_at = MonotonicInstant::now();
                        let deadline = Deadline::at_instant(
                            started_at
                                .checked_add(Duration::from_millis(25))
                                .expect("deadline"),
                        );
                        let body_result = AxumEgressBody::application(
                            Body::from_stream(pending()),
                            deadline,
                            MonotonicClock::default(),
                            attempt(&server_observer, started_at),
                            &service_responses,
                        );
                        async move {
                            let Ok(registered_body) = body_result else {
                                panic!("register body");
                            };
                            Ok::<_, Infallible>(Response::new(registered_body))
                        }
                    });

                    serve_http1(stream, service, responses, AxumIngressConfig::default()).await
                });

                let mut client = TcpStream::connect(address).await.expect("connect client");
                client
                    .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
                    .await
                    .expect("write request");

                let exit = server
                    .await
                    .expect("server task")
                    .expect("serve connection");
                assert_eq!(exit, ConnectionExit::DeadlineExceeded);

                let mut response = Vec::new();
                timeout(
                    Duration::from_millis(100),
                    client.read_to_end(&mut response),
                )
                .await
                .expect("connection closes promptly")
                .expect("read response");
                let reports = observer.0.lock().expect("reports lock");
                assert_eq!(reports.len(), 1);
                assert_eq!(reports[0].outcome, ResponseEgressOutcome::DeadlineExceeded);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn zero_read_client_is_closed_when_socket_backpressure_reaches_deadline() {
        let local = LocalSet::new();
        local
            .run_until(async {
                let listener = TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind listener");
                let address = listener.local_addr().expect("listener address");
                let observer = RecordingObserver::default();
                let server_observer = observer.clone();

                let server = spawn_local(async move {
                    let (stream, _) = listener.accept().await.expect("accept client");
                    let responses = EgressConnection::default();
                    let service_responses = responses.clone();
                    let service = service_fn(move |_request: Request<Incoming>| {
                        let started_at = MonotonicInstant::now();
                        let deadline = Deadline::at_instant(
                            started_at
                                .checked_add(Duration::from_millis(50))
                                .expect("deadline"),
                        );
                        let source =
                            repeat_with(|| Ok(bytes::Bytes::from(vec![0_u8; 256 * 1_024])));
                        let body_result = AxumEgressBody::application(
                            Body::from_stream(source),
                            deadline,
                            MonotonicClock::default(),
                            attempt(&server_observer, started_at),
                            &service_responses,
                        );
                        async move {
                            let Ok(registered_body) = body_result else {
                                panic!("register body");
                            };
                            Ok::<_, Infallible>(Response::new(registered_body))
                        }
                    });
                    serve_http1(stream, service, responses, AxumIngressConfig::default()).await
                });

                let mut client = TcpStream::connect(address).await.expect("connect client");
                client
                    .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
                    .await
                    .expect("write request");

                let exit = timeout(Duration::from_secs(2), server)
                    .await
                    .expect("deadline closes blocked connection")
                    .expect("server task")
                    .expect("serve connection");
                assert_eq!(exit, ConnectionExit::DeadlineExceeded);
                let reports = observer.0.lock().expect("reports lock");
                assert_eq!(reports.len(), 1);
                assert_eq!(reports[0].outcome, ResponseEgressOutcome::DeadlineExceeded);
                assert!(reports[0].bytes_written > 0);
            })
            .await;
    }

    #[test]
    fn deadline_attributes_winner_and_ordered_pipeline_collateral() {
        let observer = RecordingObserver::default();
        let responses = EgressConnection::default();
        let started_at = MonotonicInstant::now();
        let later = Deadline::at_instant(
            started_at
                .checked_add(Duration::from_secs(2))
                .expect("later deadline"),
        );
        let earlier = Deadline::at_instant(
            started_at
                .checked_add(Duration::from_secs(1))
                .expect("earlier deadline"),
        );
        let first_result = AxumEgressBody::application(
            Body::from_stream(pending()),
            later,
            MonotonicClock::default(),
            attempt(&observer, started_at),
            &responses,
        );
        let second_result = AxumEgressBody::application(
            Body::from_stream(pending()),
            earlier,
            MonotonicClock::default(),
            attempt(&observer, started_at),
            &responses,
        );
        let (Ok(first_body), Ok(second_body)) = (first_result, second_result) else {
            panic!("register pipelined bodies");
        };

        let elapsed = responses.next_deadline().expect("next deadline");
        assert!(responses.signal_elapsed_deadline(elapsed));
        drop(first_body);
        drop(second_body);

        let reports = observer.0.lock().expect("reports lock");
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::TransportError);
        assert_eq!(reports[1].outcome, ResponseEgressOutcome::DeadlineExceeded);
    }

    #[test]
    fn host_timer_uses_deadline_as_the_minimum_terminal_timestamp() {
        let observer = RecordingObserver::default();
        let responses = EgressConnection::default();
        let started_at = MonotonicInstant::now();
        let deadline_at = started_at
            .checked_add(Duration::from_secs(1))
            .expect("deadline");
        let frozen_clock = MonotonicClock::new(move || started_at);
        let headers = HeaderMap::new();
        let head = ResponseEgressHead::new(
            StatusCode::OK,
            Version::HTTP_11,
            &headers,
            None,
            started_at,
            None,
        );
        let attempt = ResponseEgressAttempt::new(
            &head,
            started_at,
            ResponseEgressCompletion::empty(),
            ResponseEgressObserverHandle::new(observer.clone()),
            frozen_clock.clone(),
        );
        let body_result = AxumEgressBody::application(
            Body::from_stream(pending()),
            Deadline::at_instant(deadline_at),
            frozen_clock,
            attempt,
            &responses,
        );
        let Ok(body) = body_result else {
            panic!("register body");
        };

        let elapsed = responses.next_deadline().expect("host deadline");
        assert!(responses.signal_elapsed_deadline(elapsed));
        drop(body);

        let reports = observer.0.lock().expect("reports lock");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].outcome, ResponseEgressOutcome::DeadlineExceeded);
        assert_eq!(reports[0].elapsed, Duration::from_secs(1));
    }
}
