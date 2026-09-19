use std::error::Error as StdError;

use http_body::Body as HttpBody;
use hyper::Request;
use hyper::body::Incoming;
use hyper::server::conn::http1::Builder as Http1Builder;
use hyper::service::Service;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio::time::{Instant as TokioInstant, sleep_until};

use crate::response::AxumBodyError;
use crate::response::EgressConnection;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConnectionExit {
    Completed,
    DeadlineExceeded,
}

/// Owns one HTTP/1 connection until Hyper completes or an active response deadline closes it.
#[expect(
    clippy::integer_division_remainder_used,
    reason = "tokio::select! expands to internal randomized branch selection arithmetic"
)]
pub(crate) async fn serve_http1<ServiceType, ResponseBody>(
    stream: TcpStream,
    service: ServiceType,
    responses: EgressConnection,
) -> Result<ConnectionExit, hyper::Error>
where
    ServiceType: Service<Request<Incoming>, Response = hyper::Response<ResponseBody>>,
    ServiceType::Error: Into<Box<dyn StdError + Send + Sync>>,
    ResponseBody: HttpBody + 'static,
    ResponseBody::Error: Into<Box<dyn StdError + Send + Sync>>,
{
    let connection_future = Http1Builder::new()
        .keep_alive(true)
        .serve_connection(TokioIo::new(stream), service);
    let mut connection = Box::pin(connection_future);

    loop {
        let changed = responses.changed();
        tokio::pin!(changed);
        if let Some(deadline) = responses.next_deadline() {
            tokio::select! {
                result = &mut connection => {
                    responses.signal_transport_error();
                    drop(connection);
                    return classify_connection_result(result);
                }
                () = sleep_until(deadline) => {
                    if responses.signal_elapsed_deadline(TokioInstant::now()) {
                        drop(connection);
                        return Ok(ConnectionExit::DeadlineExceeded);
                    }
                }
                () = &mut changed => {}
            }
        } else {
            tokio::select! {
                result = &mut connection => {
                    responses.signal_transport_error();
                    drop(connection);
                    return classify_connection_result(result);
                }
                () = &mut changed => {}
            }
        }
    }
}

fn classify_connection_result(
    result: Result<(), hyper::Error>,
) -> Result<ConnectionExit, hyper::Error> {
    match result {
        Ok(()) => Ok(ConnectionExit::Completed),
        Err(error) if error_chain_has_deadline(&error) => Ok(ConnectionExit::DeadlineExceeded),
        Err(error) => Err(error),
    }
}

fn error_chain_has_deadline(error: &hyper::Error) -> bool {
    let mut source = error.source();
    while let Some(current) = source {
        if current
            .downcast_ref::<AxumBodyError>()
            .is_some_and(|body_error| body_error.is_deadline())
        {
            return true;
        }
        source = current.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use edgezero_core::body::Body;
    use edgezero_core::http::{HeaderMap, Method, StatusCode, Version};
    use edgezero_core::response_egress::{
        ResponseEgressAttempt, ResponseEgressCompletion, ResponseEgressHead,
        ResponseEgressObserver, ResponseEgressObserverHandle, ResponseEgressOutcome,
        ResponseEgressReport,
    };
    use edgezero_core::router::RouteMetadata;
    use edgezero_core::time::{Deadline, MonotonicClock, MonotonicInstant};
    use futures_util::stream::{pending, repeat_with};
    use hyper::Request;
    use hyper::Response;
    use hyper::body::Incoming;
    use hyper::service::service_fn;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::task::{LocalSet, spawn_local};
    use tokio::time::timeout;

    use super::{ConnectionExit, serve_http1};
    use crate::response::{AxumEgressBody, EgressConnection};

    #[derive(Clone, Default)]
    struct RecordingObserver(Arc<Mutex<Vec<ResponseEgressReport>>>);

    impl ResponseEgressObserver for RecordingObserver {
        fn complete(&self, report: &ResponseEgressReport) {
            self.0.lock().expect("reports lock").push(report.clone());
        }
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

                    serve_http1(stream, service, responses).await
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
                    serve_http1(stream, service, responses).await
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
