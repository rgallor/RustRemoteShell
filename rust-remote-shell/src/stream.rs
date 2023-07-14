//! Deadlock-free Stream wrapping a [`WebSocketStream`].
//!
//! This module provides defines a struct implementing the [`Stream`] trait,
//! containing a [`WebSocketStream`].
//! This is contained in an Arc/Mutex structure to avoid deadlock conditions that may
//! occur while trying to read and write at the same time from the stream/sink.
//! To this extent, the `poll_next` method checks the following conditions:
//! * if the `lock` method is called, it checks that the WebSocket stream has been locked,
//! eventually waiting for the lock to be freed
//! * if the Mutex is not locked, lock it and try to poll the next [`Message`]. If no messages
//! are available, free the lock and await, otherwise map the Message stream into
//! a [`DeviceMsg`] stream.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::Poll,
};

use futures::{ready, stream::FusedStream, FutureExt, Sink, Stream, StreamExt};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    pin,
    sync::{Mutex, MutexGuard},
};
use tokio_tungstenite::{tungstenite::Message, WebSocketStream};
use tracing::{info, instrument};

use crate::{host::HostError, protocol::DeviceMsg};

#[derive(Debug)]
pub(crate) struct MessageHandler<S> {
    inner: Arc<SharedMessageHandler<S>>,
}

impl<S> MessageHandler<S> {
    pub(crate) fn new(ws_stream: WebSocketStream<S>) -> Self {
        Self {
            inner: Arc::new(SharedMessageHandler::new(ws_stream)),
        }
    }
}

impl<S> Clone for MessageHandler<S> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[derive(Debug)]
struct SharedMessageHandler<S> {
    ws_stream: Mutex<WebSocketStream<S>>,
    ended: AtomicBool, // store
}

impl<S> SharedMessageHandler<S> {
    fn new(ws_stream: WebSocketStream<S>) -> Self {
        Self {
            ws_stream: Mutex::new(ws_stream),
            ended: AtomicBool::new(false),
        }
    }

    fn poll_lock(
        &self,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<MutexGuard<'_, WebSocketStream<S>>> {
        let fut = self.ws_stream.lock();
        pin!(fut);
        fut.poll_unpin(cx)
    }
}

impl<S> MessageHandler<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub(crate) async fn close(&self) -> Result<(), HostError> {
        self.inner.ws_stream.lock().await.close(None).await?;
        Ok(())
    }

    pub(crate) async fn sink(
        &self,
    ) -> MutexGuard<'_, impl Sink<Message, Error = <WebSocketStream<S> as Sink<Message>>::Error>>
    {
        self.inner.ws_stream.lock().await
    }

    /// Consume the elements of the stream.
    #[instrument(skip_all)]
    pub(crate) async fn empty_stream(&mut self) -> Result<(), HostError> {
        while let Some(res) = self.next().await {
            // propagate the error
            res?;
        }

        Ok(())
    }
}

impl<S> Stream for MessageHandler<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    type Item = Result<DeviceMsg, HostError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let (res, ended) = {
            let mut lock = ready!(self.inner.poll_lock(cx));
            let res = ready!(lock.poll_next_unpin(cx));
            (res, lock.is_terminated())
        };

        self.inner.ended.store(ended, Ordering::Relaxed);

        let res = res.map(|res| {
            res.map_err(HostError::from).and_then(|msg| match msg {
                Message::Close(_) => {
                    info!("Closing WebSocket connection.");
                    Ok(DeviceMsg::Eof)
                }
                Message::Binary(v) => {
                    let msg: DeviceMsg = bson::from_slice(&v)?;
                    Ok(msg)
                }
                msg => Err(HostError::WrongWsMessage(msg)),
            })
        });

        Poll::Ready(res)
    }
}

impl<S> FusedStream for MessageHandler<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn is_terminated(&self) -> bool {
        self.inner.ended.load(Ordering::Relaxed)
    }
}
