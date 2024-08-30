//! Example of creating a dynamic proxy service. Important thing to note here - the service does
//! not use any generated code, as it is fully generic over the actual grpc calls being made under
//! the hood
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::response::Response;
use bytes::{Buf, BufMut, Bytes};
use http::{Request, Uri};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

use tonic::body::{boxed, BoxBody};
use tonic::client::GrpcService;
use tonic::codec::{BufferSettings, Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::codegen::Service;
use tonic::service::Routes;
use tonic::transport::Server;
use tonic::{Status, Streaming};

#[derive(Clone)]
struct ProxyService {
    target_uri: Uri,
    proxy_client: Client<HttpConnector, BoxBody>,
}

impl ProxyService {
    fn new(target_uri: Uri) -> Self {
        let proxy_client = Client::builder(TokioExecutor::new())
            .http2_only(true)
            .build_http();

        Self {
            proxy_client,
            target_uri,
        }
    }

    async fn proxy_request(
        &mut self,
        req: Request<BoxBody>,
    ) -> Result<http::Response<BoxBody>, Status> {
        println!("Proxy serving request to {}", &req.uri().path());

        let (mut parts, body) = req.into_parts();

        let mut uri_parts = parts.uri.into_parts();
        uri_parts.authority = self.target_uri.authority().map(|a| a.clone());
        parts.uri = Uri::from_parts(uri_parts).unwrap();

        let req = Request::from_parts(parts, body);

        let delegated_server_call = GrpcService::call(&mut self.proxy_client, req);

        let resp = delegated_server_call
            .await
            .map_err(|e| Status::from_error(e.into()));

        match resp {
            Ok(resp) => {
                let status = resp.status();

                let (p, body) = resp.into_parts();
                let stream = Streaming::new_response(NoopDecoder, body, status, None, None);

                let mapped_res = tonic::codec::encode_server(
                    NoopEncoder,
                    stream,
                    None,
                    Default::default(),
                    None,
                );

                Ok(Response::from_parts(p, boxed(mapped_res)))
            }
            Err(status) => Ok(status.into_http()),
        }
    }
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

impl Service<Request<Body>> for ProxyService {
    type Response = http::Response<BoxBody>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let mut this = self.clone();
        Box::pin(async move {
            Ok(this
                .proxy_request(req.map(|b| boxed(b)))
                .await
                .unwrap_or_else(|status| status.into_http()))
        })
    }
}

/// This codec does nothing but pass the bytes through
pub struct NoopCodec;

impl Codec for NoopCodec {
    type Encode = Bytes;
    type Decode = Bytes;

    type Encoder = NoopEncoder;
    type Decoder = NoopDecoder;

    fn encoder(&mut self) -> Self::Encoder {
        NoopEncoder
    }

    fn decoder(&mut self) -> Self::Decoder {
        NoopDecoder
    }
}

pub struct NoopEncoder;

impl Encoder for NoopEncoder {
    type Item = Bytes;
    type Error = Status;

    fn encode(&mut self, item: Self::Item, buf: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        buf.put(&*item);

        Ok(())
    }

    fn buffer_settings(&self) -> BufferSettings {
        Default::default()
    }
}

pub struct NoopDecoder;

impl Decoder for NoopDecoder {
    type Item = Bytes;
    type Error = Status;

    fn decode(&mut self, buf: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
        Ok(Some(buf.copy_to_bytes(buf.remaining())))
    }

    fn buffer_settings(&self) -> BufferSettings {
        Default::default()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let router = axum::Router::new().route_service(
        "/*catchall",
        ProxyService::new("http://[::1]:50051".parse().unwrap()),
    );

    Server::builder()
        .add_routes(Routes::from(router))
        .serve("[::1]:50050".parse().unwrap())
        .await?;

    Ok(())
}
