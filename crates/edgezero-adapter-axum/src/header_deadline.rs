use std::error::Error as StdError;
use std::future::Future;
use std::io::{self, IoSlice};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use edgezero_core::http::{HeaderMap, HeaderName, HeaderValue};
use hyper::Request;
use hyper::body::Incoming;
use hyper::ext::ReasonPhrase;
use hyper::rt::{Sleep, Timer};
use hyper::service::Service;
use hyper_util::rt::TokioTimer;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::time::Instant as TokioInstant;

use crate::ingress_config::AxumIngressConfig;
use crate::response::EgressConnection;
/// Shares Hyper's head-read phase with the connection's independent timer race.
/// A successful service call disarms it; parser-error writes retain the deadline.
#[derive(Clone)]
pub(crate) struct HeaderDeadline {
    changed: Arc<Notify>,
    deadline: Arc<Mutex<Option<TokioInstant>>>,
}

#[derive(Debug, thiserror::Error)]
#[error("HTTP/1 request head deadline exceeded")]
pub(crate) struct HeaderDeadlineExceeded;

pub(crate) struct HeaderIo {
    deadline: HeaderDeadline,
    stream: TcpStream,
}

pub(crate) struct HeaderService<S> {
    deadline: HeaderDeadline,
    limits: AxumIngressConfig,
    responses: EgressConnection,
    service: S,
}

#[derive(Debug, thiserror::Error)]
#[error("HTTP/1 response head exceeds adapter parser storage limits")]
struct ResponseHeadExceeded;

impl HeaderDeadline {
    pub(crate) async fn changed(&self) {
        self.changed.notified().await;
    }

    fn check_io(&self) -> io::Result<()> {
        if self
            .deadline()
            .is_some_and(|limit| TokioInstant::now() >= limit)
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                HeaderDeadlineExceeded,
            ));
        }
        Ok(())
    }

    pub(crate) fn deadline(&self) -> Option<TokioInstant> {
        *self.deadline.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn finish_head(&self, now: TokioInstant) -> Result<(), HeaderDeadlineExceeded> {
        let mut deadline = self.deadline.lock().unwrap_or_else(PoisonError::into_inner);
        if deadline.is_some_and(|limit| now >= limit) {
            return Err(HeaderDeadlineExceeded);
        }
        *deadline = None;
        self.changed.notify_one();
        Ok(())
    }

    pub(crate) fn new(timeout: Duration) -> Self {
        let now = TokioInstant::now();
        Self {
            changed: Arc::new(Notify::new()),
            deadline: Arc::new(Mutex::new(Some(now.checked_add(timeout).unwrap_or(now)))),
        }
    }
}

impl Timer for HeaderDeadline {
    fn now(&self) -> Instant {
        TokioInstant::now().into_std()
    }

    fn reset(&self, sleep: &mut Pin<Box<dyn Sleep>>, new_deadline: Instant) {
        *sleep = self.sleep_until(new_deadline);
    }

    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Sleep>> {
        let now = self.now();
        self.sleep_until(now.checked_add(duration).unwrap_or(now))
    }

    fn sleep_until(&self, deadline: Instant) -> Pin<Box<dyn Sleep>> {
        let requested = TokioInstant::from_std(deadline);
        let mut state = self.deadline.lock().unwrap_or_else(PoisonError::into_inner);
        let effective = state.map_or(requested, |existing| existing.min(requested));
        *state = Some(effective);
        self.changed.notify_one();
        TokioTimer::new().sleep_until(effective.into_std())
    }
}

impl HeaderIo {
    pub(crate) fn new(stream: TcpStream, deadline: HeaderDeadline) -> Self {
        Self { deadline, stream }
    }
}

impl AsyncRead for HeaderIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.deadline.check_io()?;
        Pin::new(&mut this.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for HeaderIo {
    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        this.deadline.check_io()?;
        Pin::new(&mut this.stream).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }

    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.deadline.check_io()?;
        Pin::new(&mut this.stream).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.deadline.check_io()?;
        Pin::new(&mut this.stream).poll_write_vectored(cx, bufs)
    }
}

impl<S> HeaderService<S> {
    pub(crate) fn new(
        service: S,
        deadline: HeaderDeadline,
        limits: AxumIngressConfig,
        responses: EgressConnection,
    ) -> Self {
        Self {
            deadline,
            limits,
            responses,
            service,
        }
    }
}

impl<S, B> Service<Request<Incoming>> for HeaderService<S>
where
    S: Service<Request<Incoming>, Response = hyper::Response<B>>,
    S::Error: Into<Box<dyn StdError + Send + Sync>>,
    S::Future: 'static,
    B: 'static,
{
    type Error = Box<dyn StdError + Send + Sync>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;
    type Response = hyper::Response<B>;

    fn call(&self, req: Request<Incoming>) -> Self::Future {
        if let Err(error) = self.deadline.finish_head(TokioInstant::now()) {
            let boxed_error: Self::Error = Box::new(error);
            return Box::pin(async move { Err(boxed_error) });
        }
        let future = self.service.call(req);
        let limits = self.limits;
        let responses = self.responses.clone();
        Box::pin(async move {
            let mut response = future.await.map_err(Into::into)?;
            if let Err(error) = bound_response_head(&mut response, limits) {
                responses.signal_transport_error();
                let boxed_error: Self::Error = Box::new(error);
                return Err(boxed_error);
            }
            Ok(response)
        })
    }
}

fn bound_response_head<B>(
    response: &mut hyper::Response<B>,
    limits: AxumIngressConfig,
) -> Result<(), ResponseHeadExceeded> {
    let count = response.headers().len();
    let bytes = response
        .headers()
        .iter()
        .try_fold(0_usize, |charged, (name, value)| {
            charged
                .checked_add(name.as_str().len())?
                .checked_add(value.as_bytes().len())?
                .checked_add(4)
        });
    if count > limits.max_response_header_count()
        || bytes.is_none_or(|total| total > limits.max_response_header_bytes())
    {
        return Err(ResponseHeadExceeded);
    }
    // Discard application-supplied spare capacity before Hyper caches this map.
    let mut compact = HeaderMap::with_capacity(count);
    for (name, value) in response.headers() {
        let copied_name = HeaderName::from_bytes(name.as_str().as_bytes())
            .map_err(|_invalid_name| ResponseHeadExceeded)?;
        let mut copied_value = HeaderValue::from_bytes(value.as_bytes())
            .map_err(|_invalid_value| ResponseHeadExceeded)?;
        copied_value.set_sensitive(value.is_sensitive());
        compact.append(copied_name, copied_value);
    }
    *response.headers_mut() = compact;
    // Only canonical status phrases participate in the bounded encoded head.
    response.extensions_mut().remove::<ReasonPhrase>();
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use edgezero_core::http::{HeaderMap, HeaderValue};
    use hyper::ext::ReasonPhrase;
    use hyper::rt::Timer as _;
    use tokio::time::{Instant, advance};

    use super::{HeaderDeadline, bound_response_head};
    use crate::ingress_config::AxumIngressConfig;

    #[test]
    fn response_head_compacts_capacity_preserves_duplicates_and_rejects_overflow() {
        let limits = AxumIngressConfig::default();
        let mut response = hyper::Response::new(());
        *response.headers_mut() = HeaderMap::with_capacity(0x4000);
        response
            .headers_mut()
            .append("set-cookie", HeaderValue::from_static("a=1"));
        let mut sensitive = HeaderValue::from_static("b=2");
        sensitive.set_sensitive(true);
        response.headers_mut().append("set-cookie", sensitive);
        response
            .extensions_mut()
            .insert(ReasonPhrase::from_static(b"custom"));
        bound_response_head(&mut response, limits).unwrap();
        assert_eq!(response.headers().get_all("set-cookie").iter().count(), 2);
        assert!(response.headers().capacity() < 100);
        assert!(
            response
                .headers()
                .get_all("set-cookie")
                .iter()
                .nth(1)
                .unwrap()
                .is_sensitive()
        );
        assert!(response.extensions().get::<ReasonPhrase>().is_none());
        let oversized = HeaderValue::from_bytes(&vec![b'a'; limits.max_raw_head_bytes()]).unwrap();
        response.headers_mut().insert("x-large", oversized);
        bound_response_head(&mut response, limits).unwrap_err();
        response.headers_mut().clear();
        for _ in 0..=limits.max_response_header_count() {
            response
                .headers_mut()
                .append("x-dup", HeaderValue::from_static("a"));
        }
        bound_response_head(&mut response, limits).unwrap_err();
    }

    #[tokio::test(start_paused = true)]
    async fn rearmed_header_timer_remains_in_tokio_clock_domain() {
        let deadline = HeaderDeadline::new(Duration::from_secs(10));
        deadline.finish_head(Instant::now()).unwrap();
        advance(Duration::from_secs(20)).await;
        drop(deadline.sleep(Duration::from_secs(10)));
        assert_eq!(
            deadline
                .deadline()
                .unwrap()
                .checked_duration_since(Instant::now()),
            Some(Duration::from_secs(10))
        );
    }

    #[tokio::test]
    async fn first_arm_cannot_extend_accept_deadline_and_next_head_gets_new_budget() {
        let timer = HeaderDeadline::new(Duration::from_millis(10));
        let accepted_deadline = timer.deadline().unwrap();
        let _sleep = timer.sleep_until((Instant::now() + Duration::from_secs(1)).into_std());
        assert_eq!(timer.deadline(), Some(accepted_deadline));
        timer
            .finish_head(accepted_deadline - Duration::from_millis(1))
            .unwrap();
        assert_eq!(timer.deadline(), None);
        let next = Instant::now() + Duration::from_secs(1);
        let _next_sleep = timer.sleep_until(next.into_std());
        assert_eq!(timer.deadline(), Some(next));
        assert!(timer.finish_head(next).is_err());
    }
}
