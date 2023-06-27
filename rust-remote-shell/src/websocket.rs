//! WebSocket [tower](tower::Service) service.
//!
//! This module contains two structs, [`WebSocketService`] and [`WebSocketLayer`], necessary to
//! implement a WebSocket layer in the [`tower`] stack.

use std::future::Future;
use std::pin::Pin;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{accept_async, WebSocketStream};
pub use tower;
use tower::{Layer, Service};

use crate::host::HostError;

/// Service handling WebSocket connection.
///
/// This service is responsible for accepting a WebSocket connection from a device.
#[derive(Clone)]
pub struct WebSocketService<S> {
    service: S,
}

impl<S, Request, E> Service<Request> for WebSocketService<S>
where
    S: Service<WebSocketStream<Request>, Error = E> + Clone + 'static,
    Request: AsyncWrite + AsyncRead + Unpin + 'static,
    HostError: From<E>,
{
    type Response = S::Response;
    type Error = HostError;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, HostError>>>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx).map_err(HostError::from) // ready when the wrapping service is ready
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let clone = self.service.clone();
        // take the service that was ready
        let mut inner = std::mem::replace(&mut self.service, clone);

        Box::pin(async move {
            let ws_stream = accept_async(req)
                .await
                .map_err(HostError::WebSocketConnect)?;
            inner.call(ws_stream).await.map_err(HostError::from)
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
