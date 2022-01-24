//! Re-exports of the tokio runtime for use with arti.
//!
//! This crate helps define a slim API around our async runtime so that we
//! can easily swap it out.

/// Types used for networking (tokio implementation)
pub(crate) mod net {
    use crate::traits;
    use async_trait::async_trait;

    pub(crate) use tokio_crate::net::{
        TcpListener as TokioTcpListener, TcpStream as TokioTcpStream,
    };

    use futures::io::{AsyncRead, AsyncWrite};
    use tokio_util::compat::{Compat, TokioAsyncReadCompatExt as _};

    use std::io::Result as IoResult;
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// Wrapper for Tokio's TcpStream that implements the standard
    /// AsyncRead and AsyncWrite.
    pub struct TcpStream {
        /// Underlying tokio_util::compat::Compat wrapper.
        s: Compat<TokioTcpStream>,
    }
    impl TcpStream {
        /// Get a reference to the underlying tokio `TcpStream`.
        pub fn get_ref(&self) -> &TokioTcpStream {
            self.s.get_ref()
        }

        /// Get a mutable reference to the underlying tokio `TcpStream`.
        pub fn get_mut(&mut self) -> &mut TokioTcpStream {
            self.s.get_mut()
        }

        /// Convert this type into its underlying tokio `TcpStream`.
        pub fn into_inner(self) -> TokioTcpStream {
            self.s.into_inner()
        }
    }
    impl From<TokioTcpStream> for TcpStream {
        fn from(s: TokioTcpStream) -> TcpStream {
            let s = s.compat();
            TcpStream { s }
        }
    }
    impl AsyncRead for TcpStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<IoResult<usize>> {
            Pin::new(&mut self.s).poll_read(cx, buf)
        }
    }
    impl AsyncWrite for TcpStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<IoResult<usize>> {
            Pin::new(&mut self.s).poll_write(cx, buf)
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> {
            Pin::new(&mut self.s).poll_flush(cx)
        }
        fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> {
            Pin::new(&mut self.s).poll_close(cx)
        }
    }

    /// Wrap a Tokio TcpListener to behave as a futures::io::TcpListener.
    pub struct TcpListener {
        /// The underlying listener.
        pub(super) lis: TokioTcpListener,
    }

    /// Asynchronous stream that yields incoming connections from a
    /// TcpListener.
    ///
    /// This is analogous to async_std::net::Incoming.
    pub struct IncomingTcpStreams {
        /// Reference to the underlying listener.
        pub(super) lis: TokioTcpListener,
    }

    impl futures::stream::Stream for IncomingTcpStreams {
        type Item = IoResult<(TcpStream, SocketAddr)>;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            match self.lis.poll_accept(cx) {
                Poll::Ready(Ok((s, a))) => Poll::Ready(Some(Ok((s.into(), a)))),
                Poll::Ready(Err(e)) => Poll::Ready(Some(Err(e))),
                Poll::Pending => Poll::Pending,
            }
        }
    }
    #[async_trait]
    impl traits::TcpListener for TcpListener {
        type TcpStream = TcpStream;
        type Incoming = IncomingTcpStreams;
        async fn accept(&self) -> IoResult<(Self::TcpStream, SocketAddr)> {
            let (stream, addr) = self.lis.accept().await?;
            Ok((stream.into(), addr))
        }
        fn incoming(self) -> Self::Incoming {
            IncomingTcpStreams { lis: self.lis }
        }
        fn local_addr(&self) -> IoResult<SocketAddr> {
            self.lis.local_addr()
        }
    }
}

// ==============================

use crate::traits::*;
use async_trait::async_trait;
use futures::Future;
use std::io::Result as IoResult;
use std::time::Duration;

/// Helper: Declare that a given tokio runtime object implements the
/// prerequisites for Runtime.
// TODO: Maybe we can do this more simply with a simpler trait?
macro_rules! implement_traits_for {
    ($runtime:ty) => {
        impl SleepProvider for $runtime {
            type SleepFuture = tokio_crate::time::Sleep;
            fn sleep(&self, duration: Duration) -> Self::SleepFuture {
                tokio_crate::time::sleep(duration)
            }
        }

        #[async_trait]
        impl crate::traits::TcpProvider for $runtime {
            type TcpStream = net::TcpStream;
            type TcpListener = net::TcpListener;

            async fn connect(&self, addr: &std::net::SocketAddr) -> IoResult<Self::TcpStream> {
                let s = net::TokioTcpStream::connect(addr).await?;
                Ok(s.into())
            }
            async fn listen(&self, addr: &std::net::SocketAddr) -> IoResult<Self::TcpListener> {
                let lis = net::TokioTcpListener::bind(*addr).await?;
                Ok(net::TcpListener { lis })
            }
        }
    };
}

/// Create and return a new Tokio multithreaded runtime.
pub(crate) fn create_runtime() -> IoResult<async_executors::TokioTp> {
    let mut builder = async_executors::TokioTpBuilder::new();
    builder.tokio_builder().enable_all();
    builder.build()
}

/// Wrapper around a Handle to a tokio runtime.
///
/// Ideally, this type would go away, and we would just use
/// `tokio::runtime::Handle` directly.  Unfortunately, we can't implement
/// `futures::Spawn` on it ourselves because of Rust's orphan rules, so we need
/// to define a new type here.
///
/// # Limitations
///
/// Note that Arti requires that the runtime should have working implementations
/// for Tokio's time, net, and io facilities, but we have no good way to check
/// that when creating this object.
#[derive(Clone, Debug)]
pub struct TokioRuntimeHandle {
    /// The underlying Handle.
    handle: tokio_crate::runtime::Handle,
}

impl TokioRuntimeHandle {
    /// Wrap a tokio runtime handle into a format that Arti can use.
    ///
    /// # Limitations
    ///
    /// Note that Arti requires that the runtime should have working
    /// implementations for Tokio's time, net, and io facilities, but we have
    /// no good way to check that when creating this object.
    pub(crate) fn new(handle: tokio_crate::runtime::Handle) -> Self {
        handle.into()
    }
}

impl From<tokio_crate::runtime::Handle> for TokioRuntimeHandle {
    fn from(handle: tokio_crate::runtime::Handle) -> Self {
        Self { handle }
    }
}

impl SpawnBlocking for async_executors::TokioTp {
    fn block_on<F: Future>(&self, f: F) -> F::Output {
        async_executors::TokioTp::block_on(self, f)
    }
}

impl SpawnBlocking for TokioRuntimeHandle {
    fn block_on<F: Future>(&self, f: F) -> F::Output {
        self.handle.block_on(f)
    }
}

impl futures::task::Spawn for TokioRuntimeHandle {
    fn spawn_obj(
        &self,
        future: futures::task::FutureObj<'static, ()>,
    ) -> Result<(), futures::task::SpawnError> {
        let join_handle = self.handle.spawn(future);
        drop(join_handle); // this makes the task detached.
        Ok(())
    }
}

implement_traits_for! {async_executors::TokioTp}
implement_traits_for! {TokioRuntimeHandle}
