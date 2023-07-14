//! WebSocket [tower](tower::Service) service.
//!
//! This module contains two structs, [`WebSocketService`] and [`WebSocketLayer`], necessary to
//! implement a WebSocket layer in the [`tower`] stack.

use std::future::Future;
use std::pin::Pin;
use std::task::Poll;

use futures::ready;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{accept_async, WebSocketStream};
pub use tower;
use tower::make::MakeConnection;
use tower::{Layer, Service};
use tracing::info;

use crate::host::HostError;

/// Service handling WebSocket connection.
#[derive(Clone)]
pub struct WebSocketService<S> {
    service: S, // TcpService or TlsService
}

impl<'a, S, C> Service<&'a mut TcpListener> for WebSocketService<S>
where
    S: MakeConnection<&'a mut TcpListener, Connection = C> + Clone + 'static,
    C: AsyncRead + AsyncWrite + Unpin,
    HostError: From<S::Error>,
{
    type Response = WebSocketStream<S::Connection>;
    type Error = HostError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + 'a>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx).map_err(HostError::from)
    }

    fn call(&mut self, req: &'a mut TcpListener) -> Self::Future {
        let mut service = self.service.clone();

        Box::pin(async move {
            let stream = service.make_connection(req).await?;
            let ws_stream = accept_async(stream).await?;

            Ok(ws_stream)
        })
    }
}

/// WebSocket layer to be added in a [tower stack](tower::layer::util::Stack).
pub struct WebSocketLayer;

impl<S> Layer<S> for WebSocketLayer {
    type Service = WebSocketService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        WebSocketService { service: inner }
    }
}

/// Future used to accept a TCP connection.
pub struct TcpAccept<'a> {
    pub(crate) listener: &'a mut TcpListener,
}

impl<'a> Future for TcpAccept<'a> {
    type Output = Result<TcpStream, HostError>;

    /// It polls untill a new connection has been accepted, eventually returning the TCP stream.
    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let res = ready!(self.listener.poll_accept(cx))
            .map(|(stream, addr)| {
                info!("connection from {}", addr);
                stream
            })
            .map_err(HostError::Listen);

        Poll::Ready(res)
    }
}

/// Service responsible for creating a TCP connection.
#[derive(Clone, Copy, Debug)]
pub struct TcpService;

impl<'a> Service<&'a mut TcpListener> for TcpService {
    type Response = TcpStream;
    type Error = HostError;
    type Future = TcpAccept<'a>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: &'a mut TcpListener) -> Self::Future {
        TcpAccept { listener: req }
    }
}
