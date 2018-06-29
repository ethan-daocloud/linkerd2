use bytes::Buf;
use futures::{Async, Future, Poll};
use std::io;
use std::sync::Arc;
use std::time::Instant;
use tokio_connect;
use tokio::io::{AsyncRead, AsyncWrite};

use connection::{self, Peek};
use ctx;
use telemetry::event;

/// Wraps a transport with telemetry.
#[derive(Debug)]
pub struct Transport<T> {
    io: T,
    inner: Option<Inner>,
    ctx: Arc<ctx::transport::Ctx>
}

#[derive(Debug)]
struct Inner {
    handle: super::Handle,
    opened_at: Instant,

    rx_bytes: u64,
    tx_bytes: u64,
}

/// Builds client transports with telemetry.
#[derive(Clone, Debug)]
pub struct Connect<C> {
    underlying: C,
    handle: super::Handle,
    ctx: Arc<ctx::transport::Client>,
}

/// Adds telemetry to a pending client transport.
#[derive(Clone, Debug)]
pub struct Connecting<C: tokio_connect::Connect> {
    underlying: C::Future,
    handle: super::Handle,
    ctx: Arc<ctx::transport::Client>,
}

// === impl Transport ===

impl<T: AsyncRead + AsyncWrite> Transport<T> {
    /// Wraps a transport with telemetry and emits a transport open event.
    pub(super) fn open(
        io: T,
        opened_at: Instant,
        handle: &super::Handle,
        ctx: Arc<ctx::transport::Ctx>,
    ) -> Self {
        let mut handle = handle.clone();

        handle.send(|| event::Event::TransportOpen(Arc::clone(&ctx)));

        Transport {
            io,
            ctx,
            inner: Some(Inner {
                handle,
                opened_at,
                rx_bytes: 0,
                tx_bytes: 0,
            }),
        }
    }

    /// Wraps an operation on the underlying transport with error telemetry.
    ///
    /// If the transport operation results in a non-recoverable error, a transport close
    /// event is emitted.
    fn sense_err<F, U>(&mut self, op: F) -> io::Result<U>
    where
        F: FnOnce(&mut T) -> io::Result<U>,
    {
        match op(&mut self.io) {
            Ok(v) => Ok(v),
            Err(e) => {
                if e.kind() != io::ErrorKind::WouldBlock {
                    if let Some(Inner {
                        mut handle,
                        opened_at,
                        rx_bytes,
                        tx_bytes,
                    }) = self.inner.take()
                    {
                        let ctx = self.ctx.clone();
                        handle.send(move || {
                            let duration = opened_at.elapsed();
                            let ev = event::TransportClose {
                                duration,
                                clean: false,
                                rx_bytes,
                                tx_bytes,
                            };
                            event::Event::TransportClose(ctx, ev)
                        });
                    }
                }

                Err(e)
            }
        }
    }
}

impl<T> Drop for Transport<T> {
    fn drop(&mut self) {
        if let Some(Inner {
            mut handle,
            opened_at,
            rx_bytes,
            tx_bytes,
        }) = self.inner.take()
        {
            let ctx = self.ctx.clone();
            handle.send(move || {
                let duration = opened_at.elapsed();
                let ev = event::TransportClose {
                    clean: true,
                    duration,
                    rx_bytes,
                    tx_bytes,
                };
                event::Event::TransportClose(ctx, ev)
            });
        }
    }
}

impl<T: AsyncRead + AsyncWrite> io::Read for Transport<T> {
    fn read(&mut self, mut buf: &mut [u8]) -> io::Result<usize> {
        let bytes = self.sense_err(move |io| io.read(buf))?;

        if let Some(inner) = self.inner.as_mut() {
            inner.rx_bytes += bytes as u64;
        }

        Ok(bytes)
    }
}

impl<T: AsyncRead + AsyncWrite> io::Write for Transport<T> {
    fn flush(&mut self) -> io::Result<()> {
        self.sense_err(|io| io.flush())
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let bytes = self.sense_err(move |io| io.write(buf))?;

        if let Some(inner) = self.inner.as_mut() {
            inner.tx_bytes += bytes as u64;
        }

        Ok(bytes)
    }
}

impl<T: AsyncRead + AsyncWrite> AsyncRead for Transport<T> {
    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [u8]) -> bool {
        self.io.prepare_uninitialized_buffer(buf)
    }
}

impl<T: AsyncRead + AsyncWrite> AsyncWrite for Transport<T> {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        self.sense_err(|io| io.shutdown())
    }

    fn write_buf<B: Buf>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
        let bytes = try_ready!(self.sense_err(|io| io.write_buf(buf)));

        if let Some(inner) = self.inner.as_mut() {
            inner.tx_bytes += bytes as u64;
        }

        Ok(Async::Ready(bytes))
    }
}

impl<T: AsyncRead + AsyncWrite + Peek> Peek for Transport<T> {
    fn poll_peek(&mut self) -> Poll<usize, io::Error> {
        self.sense_err(|io| io.poll_peek())
    }

    fn peeked(&self) -> &[u8] {
        self.io.peeked()
    }
}

impl<T> ctx::transport::MightHaveClientCtx for Transport<T> {
    fn transport_ctx(&self) -> Option<&Arc<ctx::transport::Client>> {
        match self.ctx {
            ctx::transport::Ctx::Client(ref ctx) => Some(ctx),
            _ => None,
        }
    }
}

// === impl Connect ===

impl<C> Connect<C>
where
    C: tokio_connect::Connect<Connected = connection::Connection>,
{
    /// Returns a `Connect` to `addr` and `handle`.
    pub(super) fn new(
        underlying: C,
        handle: &super::Handle,
        ctx: &Arc<ctx::transport::Client>,
    ) -> Self {
        Connect {
            underlying,
            handle: handle.clone(),
            ctx: Arc::clone(ctx),
        }
    }
}

impl<C> tokio_connect::Connect for Connect<C>
where
    C: tokio_connect::Connect<Connected = connection::Connection>,
{
    type Connected = Transport<C::Connected>;
    type Error = C::Error;
    type Future = Connecting<C>;

    fn connect(&self) -> Self::Future {
        Connecting {
            underlying: self.underlying.connect(),
            handle: self.handle.clone(),
            ctx: Arc::clone(&self.ctx),
        }
    }
}

// === impl Connecting ===

impl<C> Future for Connecting<C>
where
    C: tokio_connect::Connect<Connected = connection::Connection>,
{
    type Item = Transport<C::Connected>;
    type Error = C::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let io = try_ready!(self.underlying.poll());
        debug!("client connection open");
        let ctx = ctx::transport::Client::new(
            &self.ctx.proxy,
            &self.ctx.remote,
            self.ctx.metadata.clone(),
            io.tls_status,
        );
        let ctx = Arc::new(ctx.into());
        let trans = Transport::open(io, Instant::now(), &self.handle, ctx);
        Ok(trans.into())
    }
}
