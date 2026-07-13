//! HTTP Transport

use crate::{
    error::{Error, Result, TransportError},
    helpers, BatchTransport, RequestId, Transport,
};
#[cfg(not(target_arch = "wasm32"))]
use futures::future::BoxFuture;
#[cfg(target_arch = "wasm32")]
use futures::future::LocalBoxFuture as BoxFuture;
use futures::StreamExt;
use jsonrpc_core::types::{Call, Output, Request, Value};
use reqwest::{Client, Url};
use serde::de::DeserializeOwned;
use std::{
    collections::HashMap,
    sync::{
        atomic::AtomicUsize,
        Arc,
    },
};

const DEFAULT_MAX_HTTP_RESPONSE_SIZE: usize = 16 * 1024 * 1024;

/// HTTP Transport
#[derive(Clone, Debug)]
pub struct Http {
    // Client is already an Arc so doesn't need to be part of inner.
    client: Client,
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    url: Url,
    id: AtomicUsize,
    max_response_size: usize,
}

impl Http {
    /// Create new HTTP transport connecting to given URL.
    ///
    /// Note that the http [Client] automatically enables some features like setting the basic auth
    /// header or enabling a proxy from the environment. You can customize it with
    /// [Http::with_client]. Responses are limited to 16 MiB by default; use
    /// [Http::new_with_max_response_size] to choose another bound.
    pub fn new(url: &str) -> Result<Self> {
        Self::new_with_max_response_size(url, DEFAULT_MAX_HTTP_RESPONSE_SIZE)
    }

    /// Create a new HTTP transport with a configurable maximum response size in bytes.
    pub fn new_with_max_response_size(url: &str, max_response_size: usize) -> Result<Self> {
        #[allow(unused_mut, reason = "the wasm configuration omits the native user-agent mutation")]
        let mut builder = Client::builder();
        #[cfg(not(target_arch = "wasm32"))]
        {
            builder = builder.user_agent(reqwest::header::HeaderValue::from_static("web3.rs"));
        }
        let client = builder
            .build()
            .map_err(|err| Error::Transport(TransportError::Message(format!("failed to build client: {}", err))))?;
        Ok(Self::with_client_and_max_response_size(
            client,
            url.parse()?,
            max_response_size,
        ))
    }

    /// Like `new` but with a user provided client instance.
    pub fn with_client(client: Client, url: Url) -> Self {
        Self::with_client_and_max_response_size(client, url, DEFAULT_MAX_HTTP_RESPONSE_SIZE)
    }

    /// Like [`Http::with_client`], with a configurable maximum response size in bytes.
    pub fn with_client_and_max_response_size(client: Client, url: Url, max_response_size: usize) -> Self {
        Self {
            client,
            inner: Arc::new(Inner {
                url,
                id: AtomicUsize::new(0),
                max_response_size,
            }),
        }
    }

    fn next_id(&self) -> RequestId {
        helpers::next_request_id(&self.inner.id)
    }

    fn new_request(&self) -> (Client, Url, usize) {
        (
            self.client.clone(),
            self.inner.url.clone(),
            self.inner.max_response_size,
        )
    }
}

// The id is used for logging and response correlation.
async fn execute_rpc<T: DeserializeOwned>(
    client: &Client,
    url: Url,
    request: &Request,
    id: RequestId,
    max_response_size: usize,
) -> Result<T> {
    log::debug!("[id:{}] sending request: {:?}", id, serde_json::to_string(&request)?);
    let response = client
        .post(url)
        .json(request)
        .send()
        .await
        .map_err(|err| Error::Transport(TransportError::Message(format!("failed to send request: {}", err))))?;
    let status = response.status();
    if response
        .content_length()
        // Native supported targets have pointer widths no greater than 64 bits.
        .is_some_and(|length| length > max_response_size as u64)
    {
        return Err(Error::Transport(TransportError::Message(format!(
            "HTTP response exceeded {max_response_size} bytes"
        ))));
    }
    let mut response_stream = response.bytes_stream();
    let mut response = Vec::new();
    while let Some(chunk) = response_stream.next().await {
        let chunk = chunk.map_err(|err| {
            Error::Transport(TransportError::Message(format!("failed to read response bytes: {err}")))
        })?;
        // Allocated buffers and slices are each bounded by `isize::MAX`; their lengths sum to
        // at most `usize::MAX - 1`, so this addition cannot overflow a supported native target.
        #[allow(
            clippy::arithmetic_side_effects,
            reason = "two allocated object lengths sum to less than usize::MAX"
        )]
        let response_len = response.len() + chunk.len();
        if response_len > max_response_size {
            return Err(Error::Transport(TransportError::Message(format!(
                "HTTP response exceeded {max_response_size} bytes"
            ))));
        }
        response.extend_from_slice(&chunk);
    }
    log::debug!(
        "[id:{}] received response: {:?}",
        id,
        String::from_utf8_lossy(&response)
    );
    if !status.is_success() {
        return Err(Error::Transport(TransportError::Code(status.as_u16())));
    }
    helpers::arbitrary_precision_deserialize_workaround(&response).map_err(|err| {
        Error::Transport(TransportError::Message(format!(
            "failed to deserialize response: {}: {}",
            err,
            String::from_utf8_lossy(&response)
        )))
    })
}

type RpcResult = Result<Value>;

impl Transport for Http {
    type Out = BoxFuture<'static, Result<Value>>;

    fn prepare(&self, method: &str, params: Vec<Value>) -> (RequestId, Call) {
        let id = self.next_id();
        let request = helpers::build_request(id, method, params);
        (id, request)
    }

    fn send(&self, id: RequestId, call: Call) -> Self::Out {
        let (client, url, max_response_size) = self.new_request();
        Box::pin(async move {
            let output: Output = execute_rpc(&client, url, &Request::Single(call), id, max_response_size).await?;
            handle_single_response(id, output)
        })
    }
}

fn handle_single_response(expected_id: RequestId, output: Output) -> Result<Value> {
    let response_id = id_of_output(&output)?;
    if response_id != expected_id {
        return Err(Error::InvalidResponse(format!(
            "response id {response_id} does not match request id {expected_id}"
        )));
    }
    helpers::to_result_from_output(output)
}

impl BatchTransport for Http {
    type Batch = BoxFuture<'static, Result<Vec<RpcResult>>>;

    fn send_batch<T>(&self, requests: T) -> Self::Batch
    where
        T: IntoIterator<Item = (RequestId, Call)>,
    {
        // Batch calls don't need an id but it helps associate the response log with the request log.
        let id = self.next_id();
        let (client, url, max_response_size) = self.new_request();
        let (ids, calls): (Vec<_>, Vec<_>) = requests.into_iter().unzip();
        Box::pin(async move {
            let value = execute_rpc(&client, url, &Request::Batch(calls), id, max_response_size).await?;
            let outputs = handle_possible_error_object_for_batched_request(value)?;
            handle_batch_response(&ids, outputs)
        })
    }
}

fn handle_possible_error_object_for_batched_request(value: Value) -> Result<Vec<Output>> {
    if value.is_object() {
        let output: Output = serde_json::from_value(value)?;
        return Err(match output {
            Output::Failure(failure) => Error::Rpc(failure.error),
            Output::Success(success) => {
                // totally unlikely - we got json success object for batched request
                Error::InvalidResponse(format!("Invalid response for batched request: {:?}", success))
            }
        });
    }
    let outputs = serde_json::from_value(value)?;
    Ok(outputs)
}

// According to the jsonrpc specification batch responses can be returned in any order so we need to
// restore the intended order.
fn handle_batch_response(ids: &[RequestId], outputs: Vec<Output>) -> Result<Vec<RpcResult>> {
    if ids.len() != outputs.len() {
        return Err(Error::InvalidResponse("unexpected number of responses".to_string()));
    }
    let mut outputs = outputs
        .into_iter()
        .map(|output| Ok((id_of_output(&output)?, helpers::to_result_from_output(output))))
        .collect::<Result<HashMap<_, _>>>()?;
    ids.iter()
        .map(|id| {
            outputs
                .remove(id)
                .ok_or_else(|| Error::InvalidResponse(format!("batch response is missing id {}", id)))
        })
        .collect()
}

fn id_of_output(output: &Output) -> Result<RequestId> {
    let id = match output {
        Output::Success(success) => &success.id,
        Output::Failure(failure) => &failure.id,
    };
    match id {
        jsonrpc_core::Id::Num(num) => RequestId::try_from(*num)
            .map_err(|_| Error::InvalidResponse(format!("response id {num} does not fit RequestId"))),
        _ => Err(Error::InvalidResponse("response id is not u64".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error::Rpc;
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use jsonrpc_core::ErrorCode;
    use std::net::TcpListener;

    fn get_available_port() -> Option<u16> {
        Some(TcpListener::bind(("127.0.0.1", 0)).ok()?.local_addr().ok()?.port())
    }

    async fn server(req: hyper::Request<hyper::body::Incoming>) -> hyper::Result<hyper::Response<Full<Bytes>>> {
        let expected = r#"{"jsonrpc":"2.0","method":"eth_getAccounts","params":[],"id":0}"#;
        let response = r#"{"jsonrpc":"2.0","id":0,"result":"x"}"#;

        assert_eq!(req.method(), &hyper::Method::POST);
        assert_eq!(req.uri().path(), "/");
        let mut content: Vec<u8> = vec![];
        let body = req.into_body();
        let body = body.collect().await?.to_bytes().to_vec();
        content.extend(body);

        assert_eq!(std::str::from_utf8(&content), Ok(expected));

        Ok(hyper::Response::new(Full::new(Bytes::from(response))))
    }

    #[tokio::test]
    async fn should_make_a_request() {
        use hyper::service::service_fn;
        use hyper_util::{
            rt::{TokioExecutor, TokioIo},
            server::conn::auto,
        };
        use tokio::net::TcpListener;
        // given
        let addr = format!("127.0.0.1:{}", get_available_port().unwrap());
        let addr_clone = addr.clone();
        // start server
        let listener = TcpListener::bind(addr.clone()).await.unwrap();
        tokio::spawn(async move {
            println!("Listening on http://{}", addr_clone);
            let (stream, _) = listener.accept().await.unwrap();
            let io = TokioIo::new(stream);
            auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, service_fn(server))
                .await
                .unwrap();
        });

        // when
        let client = Http::new(&format!("http://{}", &addr)).unwrap();
        println!("Sending request");
        let response = client.execute("eth_getAccounts", vec![]).await;
        println!("Got response");

        // then
        assert_eq!(response, Ok(Value::String("x".into())));
    }

    #[tokio::test]
    async fn catch_generic_json_error_for_batched_request() {
        use http_body_util::Full;
        use hyper::service::service_fn;
        use hyper_util::{
            rt::{TokioExecutor, TokioIo},
            server::conn::auto,
        };
        use tokio::net::TcpListener;

        async fn handler(_req: hyper::Request<hyper::body::Incoming>) -> hyper::Result<hyper::Response<Full<Bytes>>> {
            let response = r#"{
                "jsonrpc":"2.0",
                "error":{
                    "code":0,
                    "message":"we can't execute this request"
                },
                "id":null
            }"#;
            Ok(hyper::Response::new(Full::new(Bytes::from(response))))
        }

        // Given
        let addr = format!("127.0.0.1:{}", get_available_port().unwrap());
        let addr_clone = addr.clone();
        // start server
        let listener = TcpListener::bind(addr.clone()).await.unwrap();
        tokio::spawn(async move {
            println!("Listening on http://{}", addr_clone);
            let (stream, _) = listener.accept().await.unwrap();
            let io = TokioIo::new(stream);
            auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, service_fn(handler))
                .await
                .unwrap();
        });

        // when
        let client = Http::new(&format!("http://{}", &addr)).unwrap();
        println!("Sending request");
        let response = client
            .send_batch(vec![client.prepare("some_method", vec![])].into_iter())
            .await;
        println!("Got response");

        // then
        assert_eq!(
            response,
            Err(Rpc(crate::rpc::error::Error {
                code: ErrorCode::ServerError(0),
                message: "we can't execute this request".to_string(),
                data: None,
            }))
        );
    }

    #[tokio::test]
    async fn rejects_oversized_advertised_response() {
        use hyper::service::service_fn;
        use hyper_util::{
            rt::{TokioExecutor, TokioIo},
            server::conn::auto,
        };
        use tokio::net::TcpListener;

        async fn handler(_req: hyper::Request<hyper::body::Incoming>) -> hyper::Result<hyper::Response<Full<Bytes>>> {
            Ok(hyper::Response::new(Full::new(Bytes::from(vec![b' '; 33]))))
        }

        let addr = format!("127.0.0.1:{}", get_available_port().unwrap());
        let listener = TcpListener::bind(addr.clone()).await.unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            auto::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(stream), service_fn(handler))
                .await
                .unwrap();
        });

        let client = Http::with_client_and_max_response_size(
            Client::new(),
            format!("http://{addr}").parse().unwrap(),
            32,
        );
        let result = client.execute("eth_getAccounts", vec![]).await;
        assert!(matches!(
            result,
            Err(Error::Transport(TransportError::Message(ref message))) if message.contains("exceeded")
        ), "unexpected result: {result:?}");
    }

    #[tokio::test]
    async fn rejects_oversized_chunked_response() {
        use http_body_util::StreamBody;
        use hyper::{body::Frame, service::service_fn};
        use hyper_util::{
            rt::{TokioExecutor, TokioIo},
            server::conn::auto,
        };
        use std::convert::Infallible;
        use tokio::net::TcpListener;

        async fn handler(
            _req: hyper::Request<hyper::body::Incoming>,
        ) -> hyper::Result<
            hyper::Response<StreamBody<impl futures::Stream<Item = std::result::Result<Frame<Bytes>, Infallible>>>>,
        > {
            let chunks = [Bytes::from(vec![b' '; 20]), Bytes::from(vec![b' '; 20])];
            let body = StreamBody::new(futures::stream::iter(chunks.map(|chunk| Ok(Frame::data(chunk)))));
            Ok(hyper::Response::new(body))
        }

        let addr = format!("127.0.0.1:{}", get_available_port().unwrap());
        let listener = TcpListener::bind(addr.clone()).await.unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            auto::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(stream), service_fn(handler))
                .await
                .unwrap();
        });

        let client = Http::with_client_and_max_response_size(
            Client::new(),
            format!("http://{addr}").parse().unwrap(),
            32,
        );
        let result = client.execute("eth_getAccounts", vec![]).await;
        assert!(matches!(
            result,
            Err(Error::Transport(TransportError::Message(ref message))) if message.contains("exceeded 32 bytes")
        ));
    }

    #[tokio::test]
    async fn accepts_response_at_exact_size_limit() {
        use hyper::service::service_fn;
        use hyper_util::{
            rt::{TokioExecutor, TokioIo},
            server::conn::auto,
        };
        use tokio::net::TcpListener;

        const RESPONSE: &str = r#"{"jsonrpc":"2.0","id":0,"result":"x"}"#;
        async fn handler(_req: hyper::Request<hyper::body::Incoming>) -> hyper::Result<hyper::Response<Full<Bytes>>> {
            Ok(hyper::Response::new(Full::new(Bytes::from_static(RESPONSE.as_bytes()))))
        }

        let addr = format!("127.0.0.1:{}", get_available_port().unwrap());
        let listener = TcpListener::bind(addr.clone()).await.unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            auto::Builder::new(TokioExecutor::new())
                .serve_connection(TokioIo::new(stream), service_fn(handler))
                .await
                .unwrap();
        });

        let client = Http::with_client_and_max_response_size(
            Client::new(),
            format!("http://{addr}").parse().unwrap(),
            RESPONSE.len(),
        );
        assert_eq!(client.execute("eth_getAccounts", vec![]).await, Ok(Value::String("x".into())));
    }

    #[test]
    fn handles_batch_response_being_in_different_order_than_input() {
        let ids = vec![0, 1, 2];
        // This order is different from the ids.
        let outputs = [1u64, 0, 2]
            .iter()
            .map(|&id| {
                Output::Success(jsonrpc_core::Success {
                    jsonrpc: None,
                    result: id.into(),
                    id: jsonrpc_core::Id::Num(id),
                })
            })
            .collect();
        let results = handle_batch_response(&ids, outputs)
            .unwrap()
            .into_iter()
            .map(|result| result.unwrap().as_u64().unwrap() as usize)
            .collect::<Vec<_>>();
        // The order of the ids should have been restored.
        assert_eq!(ids, results);
    }

    #[test]
    fn rejects_single_response_with_wrong_id() {
        let output = Output::Success(jsonrpc_core::Success {
            jsonrpc: Some(jsonrpc_core::Version::V2),
            result: Value::String("wrong request".into()),
            id: jsonrpc_core::Id::Num(2),
        });

        assert!(matches!(
            handle_single_response(1, output),
            Err(Error::InvalidResponse(message)) if message.contains("does not match")
        ));
    }

    #[test]
    fn request_ids_are_unique_across_threads() {
        let transport = Http::new("http://127.0.0.1").unwrap();
        let threads = (0..4)
            .map(|_| {
                let transport = transport.clone();
                std::thread::spawn(move || {
                    (0..256)
                        .map(|_| transport.prepare("test", Vec::new()).0)
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        let ids = threads
            .into_iter()
            .flat_map(|thread| thread.join().unwrap())
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(ids.len(), 1_024);
    }
}
