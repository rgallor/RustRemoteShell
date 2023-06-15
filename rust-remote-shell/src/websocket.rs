use std::future::Future;
use std::pin::Pin;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{accept_async, WebSocketStream};
pub use tower;
use tower::{Layer, Service};

use crate::host::HostError;

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

pub struct WebSocketLayer;

impl<S> Layer<S> for WebSocketLayer {
    type Service = WebSocketService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        WebSocketService { service: inner }
    }
}
