use crate::connect_policy::LocalTarget;
use rama_core::bytes::Bytes;
use rama_core::error::OpaqueError;
use rama_core::extensions::{Extensions, ExtensionsMut, ExtensionsRef};
use rama_http::body::{Frame, SizeHint, StreamingBody};
use rama_http::{Body, HeaderMap, Version};
use rama_http_backend::client::HttpClientService;
use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const MAX_IDLE: usize = 64;
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, PartialEq, Eq)]
pub(super) struct PoolKey {
    pub origin: String,
    pub local_target: Option<LocalTarget>,
}

#[derive(Clone, Debug)]
pub(super) struct ConnectionAlive(Arc<AtomicBool>);

pub(super) struct LiveStream<S> {
    inner: S,
    alive: ConnectionAlive,
}

impl<S: ExtensionsMut> LiveStream<S> {
    pub fn new(mut inner: S) -> Self {
        let alive = ConnectionAlive(Arc::new(AtomicBool::new(true)));
        inner.extensions_mut().insert(alive.clone());
        Self { inner, alive }
    }
}

impl<S> Drop for LiveStream<S> {
    fn drop(&mut self) {
        self.alive.0.store(false, Ordering::Release);
    }
}

impl<S: ExtensionsRef> ExtensionsRef for LiveStream<S> {
    fn extensions(&self) -> &Extensions {
        self.inner.extensions()
    }
}
impl<S: ExtensionsMut> ExtensionsMut for LiveStream<S> {
    fn extensions_mut(&mut self) -> &mut Extensions {
        self.inner.extensions_mut()
    }
}
impl<S: AsyncRead + Unpin> AsyncRead for LiveStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for LiveStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

pub(super) struct IdleConnection {
    pub key: PoolKey,
    pub connection: HttpClientService<Body>,
    pub version: Version,
    pub idle_since: Instant,
}

impl IdleConnection {
    fn usable(&self) -> bool {
        self.idle_since.elapsed() < IDLE_TIMEOUT
            && self
                .connection
                .extensions()
                .get::<ConnectionAlive>()
                .is_some_and(|alive| alive.0.load(Ordering::Acquire))
    }
}

#[derive(Clone, Default)]
pub(super) struct Pool(Arc<Mutex<VecDeque<IdleConnection>>>);

impl Pool {
    pub fn take(&self, key: &PoolKey) -> Option<IdleConnection> {
        let mut idle = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        idle.retain(IdleConnection::usable);
        let index = idle.iter().rposition(|entry| &entry.key == key)?;
        idle.remove(index)
    }

    pub fn body(&self, body: Body, connection: IdleConnection) -> Body {
        let mut body = PooledBody {
            body,
            connection: Some(connection),
            pool: Arc::downgrade(&self.0),
        };
        if body.body.is_end_stream() {
            body.release();
        }
        Body::new(body)
    }
}

pub(super) fn reusable(version: Version, headers: &HeaderMap) -> bool {
    matches!(version, Version::HTTP_11 | Version::HTTP_2)
        && !headers
            .get_all(rama_http::header::CONNECTION)
            .iter()
            .any(|value| {
                value.to_str().ok().is_some_and(|value| {
                    value
                        .split(',')
                        .any(|token| token.trim().eq_ignore_ascii_case("close"))
                })
            })
}

// A lease is returned only after a complete response. Dropped/error bodies and
// failed requests discard it; never replay a possibly dispatched request.
struct PooledBody {
    body: Body,
    connection: Option<IdleConnection>,
    pool: Weak<Mutex<VecDeque<IdleConnection>>>,
}

impl PooledBody {
    fn release(&mut self) {
        let Some(mut connection) = self.connection.take() else {
            return;
        };
        connection.idle_since = Instant::now();
        if !connection.usable() {
            return;
        }
        if let Some(pool) = self.pool.upgrade() {
            let mut idle = pool
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            idle.retain(IdleConnection::usable);
            if idle.len() == MAX_IDLE {
                idle.pop_front();
            }
            idle.push_back(connection);
        }
    }
}

impl StreamingBody for PooledBody {
    type Data = Bytes;
    type Error = OpaqueError;
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, OpaqueError>>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.body).poll_frame(cx);
        match &result {
            Poll::Ready(Some(Err(_))) => {
                this.connection.take();
            }
            Poll::Ready(None) => this.release(),
            Poll::Ready(Some(Ok(_))) if this.body.is_end_stream() => this.release(),
            _ => {}
        }
        result
    }
    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}
