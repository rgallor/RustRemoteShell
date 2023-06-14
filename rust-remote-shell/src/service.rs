use std::future::Future;

use std::pin::Pin;

pub use tower;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{accept_async, WebSocketStream};
use tower::{Layer, Service};

#[derive(Clone)]
pub struct WebSocketService<S> {
    service: S,
}

impl<S, Request> Service<Request> for WebSocketService<S>
where
    S: Service<WebSocketStream<Request>> + Clone + 'static,
    Request: AsyncWrite + AsyncRead + Unpin + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, S::Error>>>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx) // ready when the wrapping service is ready
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let clone = self.service.clone();
        // take the service that was ready
        let mut inner = std::mem::replace(&mut self.service, clone);

        Box::pin(async move {
            let ws_stream = accept_async(req).await.expect("err");
            inner.call(ws_stream).await
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
