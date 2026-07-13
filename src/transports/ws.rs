//! WebSocket Transport

use self::compat::{TcpStream, TlsStream};
use crate::{
    api::SubscriptionId,
    error::{self, TransportError},
    helpers, rpc, BatchTransport, DuplexTransport, Error, RequestId, Transport,
};
use futures::{
    channel::{mpsc, oneshot},
    task::{Context, Poll},
    AsyncRead, AsyncWrite, Future, FutureExt, Stream, StreamExt,
};
use soketto::{
    connection,
    handshake::{Client, ServerResponse},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    marker::Unpin,
    pin::Pin,
    sync::{atomic, Arc},
};
use url::Url;

const DEFAULT_MAX_WS_RESPONSE_SIZE: usize = 16 * 1024 * 1024;
const DEFAULT_WS_CHANNEL_CAPACITY: usize = 256;

impl From<soketto::handshake::Error> for Error {
    fn from(err: soketto::handshake::Error) -> Self {
        Error::Transport(TransportError::Message(format!("Handshake Error: {:?}", err)))
    }
}

impl From<connection::Error> for Error {
    fn from(err: connection::Error) -> Self {
        Error::Transport(TransportError::Message(format!("Connection Error: {:?}", err)))
    }
}

type SingleResult = error::Result<rpc::Value>;
type BatchResult = error::Result<Vec<SingleResult>>;
struct Pending {
    ids: Vec<RequestId>,
    sender: oneshot::Sender<BatchResult>,
}
type Subscription = mpsc::Sender<rpc::Value>;

/// Stream, either plain TCP or TLS.
enum MaybeTlsStream<P, T> {
    /// Unencrypted socket stream.
    Plain(P),
    /// Encrypted socket stream.
    #[allow(dead_code, reason = "non-TLS feature builds retain the type-unifying TLS variant")]
    Tls(T),
}

impl<P, T> AsyncRead for MaybeTlsStream<P, T>
where
    P: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context, buf: &mut [u8]) -> Poll<Result<usize, std::io::Error>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            MaybeTlsStream::Tls(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl<P, T> AsyncWrite for MaybeTlsStream<P, T>
where
    P: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<Result<usize, std::io::Error>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            MaybeTlsStream::Tls(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(s) => Pin::new(s).poll_flush(cx),
            MaybeTlsStream::Tls(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), std::io::Error>> {
        match self.get_mut() {
            MaybeTlsStream::Plain(s) => Pin::new(s).poll_close(cx),
            MaybeTlsStream::Tls(s) => Pin::new(s).poll_close(cx),
        }
    }
}

struct WsServerTask {
    pending: BTreeMap<RequestId, Pending>,
    subscriptions: BTreeMap<SubscriptionId, Subscription>,
    sender: connection::Sender<MaybeTlsStream<TcpStream, TlsStream>>,
    receiver: connection::Receiver<MaybeTlsStream<TcpStream, TlsStream>>,
}

impl WsServerTask {
    /// Create new WebSocket transport.
    pub async fn new(url: &str, max_response_size: usize) -> error::Result<Self> {
        let url = Url::parse(url)?;

        let scheme = match url.scheme() {
            s if s == "ws" || s == "wss" => s,
            s => {
                return Err(error::Error::Transport(TransportError::Message(format!(
                    "Wrong scheme: {}",
                    s
                ))))
            }
        };

        let host = match url.host_str() {
            Some(s) => s,
            None => {
                return Err(error::Error::Transport(TransportError::Message(
                    "Wrong host name".to_string(),
                )))
            }
        };

        let port = url.port().unwrap_or(if scheme == "ws" { 80 } else { 443 });

        #[cfg(not(any(feature = "ws-tls-tokio", feature = "ws-tls-async-io", feature = "ws-rustls-tokio")))]
        if scheme == "wss" {
            return Err(Error::Transport(TransportError::Message(
                "WSS requires ws-tls-tokio, ws-rustls-tokio, or ws-tls-async-io".into(),
            )));
        }
        #[cfg(all(
            feature = "ws-tokio",
            feature = "ws-tls-async-io",
            not(feature = "ws-tls-tokio"),
            not(feature = "ws-rustls-tokio")
        ))]
        if scheme == "wss" {
            return Err(Error::Transport(TransportError::Message(
                "ws-tls-async-io cannot provide TLS when ws-tokio selects the Tokio runtime".into(),
            )));
        }

        let addrs = format!("{}:{}", host, port);

        log::trace!("Connecting TcpStream with address: {}", addrs);
        let stream = compat::raw_tcp_stream(addrs).await?;
        stream.set_nodelay(true)?;
        let socket = if scheme == "wss" {
            #[cfg(feature = "ws-tls-tokio")]
            {
                let connector = native_tls::TlsConnector::new().map_err(|error| {
                    Error::Transport(TransportError::Message(format!("TLS connector error: {error}")))
                })?;
                let stream = tokio_native_tls::TlsConnector::from(connector)
                    .connect(host, stream)
                    .await
                    .map_err(|error| {
                        Error::Transport(TransportError::Message(format!("TLS connection error: {error}")))
                    })?;
                MaybeTlsStream::Tls(compat::compat(stream))
            }
            #[cfg(all(
                feature = "ws-rustls-tokio",
                not(feature = "ws-tls-tokio")
            ))]
            {
                let stream = tokio_rustls_connect(host, stream).await?;
                MaybeTlsStream::Tls(compat::compat(stream))
            }
            #[cfg(all(
                feature = "ws-tls-async-io",
                not(feature = "ws-tokio"),
                not(feature = "ws-tls-tokio"),
                not(feature = "ws-rustls-tokio")
            ))]
            {
                let stream = async_native_tls::connect(host, stream).await.map_err(|error| {
                    Error::Transport(TransportError::Message(format!("TLS connection error: {error}")))
                })?;
                MaybeTlsStream::Tls(compat::compat(stream))
            }
            #[cfg(all(
                feature = "ws-tokio",
                feature = "ws-tls-async-io",
                not(feature = "ws-tls-tokio"),
                not(feature = "ws-rustls-tokio")
            ))]
            return Err(Error::Transport(TransportError::Message(
                "ws-tls-async-io cannot provide TLS when ws-tokio selects the Tokio runtime".into(),
            )));
            #[cfg(not(any(feature = "ws-tls-tokio", feature = "ws-tls-async-io", feature = "ws-rustls-tokio")))]
            return Err(Error::Transport(TransportError::Message(
                "WSS requires ws-tls-tokio, ws-rustls-tokio, or ws-tls-async-io".into(),
            )));
        } else {
            let stream = compat::compat(stream);
            MaybeTlsStream::Plain(stream)
        };

        let resource = match url.query() {
            Some(q) => format!("{}?{}", url.path(), q),
            None => url.path().to_owned(),
        };

        log::trace!(
            "Connecting websocket client with host: {} and resource: {}",
            host,
            resource
        );
        let mut client = Client::new(socket, host, &resource);
        let maybe_encoded = url.password().map(|password| {
            use headers::authorization::{Authorization, Credentials};
            Authorization::basic(url.username(), password)
                .0
                .encode()
                .as_bytes()
                .to_vec()
        });

        let headers = maybe_encoded.as_ref().map(|head| {
            [soketto::handshake::client::Header {
                name: "Authorization",
                value: head,
            }]
        });

        if let Some(ref head) = headers {
            client.set_headers(head);
        }
        let handshake = client.handshake();
        let (sender, receiver) = match handshake.await? {
            ServerResponse::Accepted { .. } => {
                let mut builder = client.into_builder();
                builder.set_max_message_size(max_response_size);
                builder.set_max_frame_size(max_response_size);
                builder.finish()
            }
            ServerResponse::Redirect { status_code, .. } => {
                return Err(error::Error::Transport(TransportError::Code(status_code)))
            }
            ServerResponse::Rejected { status_code } => {
                return Err(error::Error::Transport(TransportError::Code(status_code)))
            }
        };

        Ok(Self {
            pending: Default::default(),
            subscriptions: Default::default(),
            sender,
            receiver,
        })
    }

    async fn into_task(self, requests: mpsc::Receiver<TransportMessage>) {
        let Self {
            receiver,
            mut sender,
            mut pending,
            mut subscriptions,
        } = self;

        let receiver = as_data_stream(receiver).fuse();
        let requests = requests.fuse();
        pin_mut!(receiver);
        pin_mut!(requests);
        loop {
            select! {
                msg = requests.next() => match msg {
                    Some(TransportMessage::Request { ids, request, sender: tx }) => {
                        let Some(id) = ids.first().copied() else {
                            if tx.send(Err(Error::InvalidResponse("empty WebSocket batch request".into()))).is_err() {
                                log::trace!("WebSocket receiver dropped for empty batch request");
                            }
                            continue;
                        };
                        if ids.iter().copied().collect::<BTreeSet<_>>().len() != ids.len() {
                            let error = Error::InvalidResponse(format!(
                                "duplicate request id in WebSocket batch beginning with {id}"
                            ));
                            if tx.send(Err(error)).is_err() {
                                log::trace!("Request receiver was dropped after duplicate batch id: {:?}", id);
                            }
                            continue;
                        }
                        let collision = pending
                            .values()
                            .any(|request| request.ids.iter().any(|pending_id| ids.contains(pending_id)));
                        if collision {
                            let error = Error::Transport(TransportError::Message(format!(
                                "request id collision in batch beginning with {id}"
                            )));
                            if tx.send(Err(error)).is_err() {
                                log::trace!("Request receiver was dropped after id collision: {:?}", id);
                            }
                            continue;
                        }
                        pending.insert(id, Pending { ids, sender: tx });
                        let res = sender.send_text(request).await;
                        let res2 = sender.flush().await;
                        if let Err(e) = res.and(res2) {
                            // TODO [ToDr] Re-connect.
                            log::error!("WS connection error: {:?}", e);
                            let error = Error::from(e);
                            fail_pending(&mut pending, &error);
                            break;
                        }
                    }
                    Some(TransportMessage::Subscribe { id, sink }) => {
                        if subscriptions.insert(id.clone(), sink).is_some() {
                            log::warn!("Replacing already-registered subscription with id {:?}", id);
                        }
                    }
                    Some(TransportMessage::Unsubscribe { id }) if subscriptions.remove(&id).is_none() => {
                        log::warn!("Unsubscribing from non-existent subscription with id {:?}", id);
                    }
                    Some(TransportMessage::Unsubscribe { .. }) => {}
                    None => {}
                },
                res = receiver.next() => match res {
                    Some(Ok(data)) => {
                        if let Err(error) = handle_message(&data, &mut subscriptions, &mut pending) {
                            log::error!("Invalid WS response: {}", error);
                            fail_pending(&mut pending, &error);
                            break;
                        }
                    },
                    Some(Err(e)) => {
                        log::error!("WS connection error: {:?}", e);
                        let error = Error::from(e);
                        fail_pending(&mut pending, &error);
                        break;
                    },
                    None => {
                        let error = Error::Unreachable;
                        fail_pending(&mut pending, &error);
                        break;
                    },
                },
                complete => break,
            }
        }
    }
}

#[cfg(all(feature = "ws-rustls-tokio", not(feature = "ws-tls-tokio")))]
async fn tokio_rustls_connect(
    host: &str,
    stream: tokio::net::TcpStream,
) -> error::Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>> {
    use rustls_pki_types::ServerName;
    use std::convert::TryFrom;
    use tokio_rustls::rustls::{ClientConfig, RootCertStore};

    let client_conf = ClientConfig::builder()
        .with_root_certificates({
            let mut root_cert_store = RootCertStore::empty();
            root_cert_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            root_cert_store
        })
        .with_no_client_auth();

    let dnsname = ServerName::try_from(host)
        .map_err(|err| error::Error::Transport(TransportError::Message(format!("Invalid host: {err}"))))?
        .to_owned();

    Ok(tokio_rustls::TlsConnector::from(Arc::new(client_conf))
        .connect(dnsname, stream)
        .await?)
}

fn as_data_stream<T: Unpin + futures::AsyncRead + futures::AsyncWrite>(
    receiver: soketto::connection::Receiver<T>,
) -> impl Stream<Item = Result<Vec<u8>, soketto::connection::Error>> {
    futures::stream::unfold(receiver, |mut receiver| async move {
        let mut data = Vec::new();
        Some(match receiver.receive_data(&mut data).await {
            Ok(_) => (Ok(data), receiver),
            Err(e) => (Err(e), receiver),
        })
    })
}

fn handle_message(
    data: &[u8],
    subscriptions: &mut BTreeMap<SubscriptionId, Subscription>,
    pending: &mut BTreeMap<RequestId, Pending>,
) -> error::Result<()> {
    log::trace!("Message received: {:?}", data);
    if let Ok(notification) = helpers::to_notification_from_slice(data) {
        if notification.method != "eth_subscription" {
            log::warn!("Ignoring unsupported WS notification method: {}", notification.method);
            return Ok(());
        }
        if let rpc::Params::Map(params) = notification.params {
            let id = params.get("subscription");
            let result = params.get("result");

            if let (Some(rpc::Value::String(id)), Some(result)) = (id, result) {
                let id: SubscriptionId = id.clone().into();
                let remove_subscription = if let Some(stream) = subscriptions.get_mut(&id) {
                    if let Err(error) = stream.try_send(result.clone()) {
                        log::error!("Closing backpressured subscription {:?}: {:?}", id, error);
                        true
                    } else {
                        false
                    }
                } else {
                    log::warn!("Got notification for unknown subscription (id: {:?})", id);
                    false
                };
                if remove_subscription {
                    subscriptions.remove(&id);
                }
            } else {
                return Err(Error::InvalidResponse(format!("unsupported notification id: {id:?}")));
            }
        } else {
            return Err(Error::InvalidResponse("notification parameters are not an object".into()));
        }
    } else {
        let outputs = match helpers::to_response_from_slice(data)? {
            rpc::Response::Single(output) => vec![output],
            rpc::Response::Batch(outputs) if !outputs.is_empty() => outputs,
            rpc::Response::Batch(_) => return Err(Error::InvalidResponse("empty batch response".into())),
        };

        let mut outputs_by_id = BTreeMap::new();
        for output in outputs {
            let id = match output.id() {
                rpc::Id::Num(num) => RequestId::try_from(*num)
                    .map_err(|_| Error::InvalidResponse(format!("response id {num} does not fit RequestId")))?,
                id => return Err(Error::InvalidResponse(format!("unsupported response id: {id:?}"))),
            };
            if outputs_by_id.insert(id, output).is_some() {
                return Err(Error::InvalidResponse(format!("duplicate response id: {id}")));
            }
        }

        let first_id = outputs_by_id
            .keys()
            .next()
            .copied()
            .ok_or_else(|| Error::InvalidResponse("response contained no outputs".into()))?;
        let pending_key = pending
            .iter()
            .find_map(|(key, request)| request.ids.contains(&first_id).then_some(*key))
            .ok_or_else(|| Error::InvalidResponse(format!("response for unknown request id: {first_id}")))?;
        let expected_ids = &pending.get(&pending_key).ok_or(Error::Internal)?.ids;
        if expected_ids.len() != outputs_by_id.len() || expected_ids.iter().any(|id| !outputs_by_id.contains_key(id)) {
            return Err(Error::InvalidResponse(format!(
                "batch response IDs do not match request IDs: expected {expected_ids:?}"
            )));
        }
        let request = pending.remove(&pending_key).ok_or(Error::Internal)?;
        let ordered_outputs = request
            .ids
            .iter()
            .map(|id| outputs_by_id.remove(id).ok_or(Error::Internal))
            .collect::<error::Result<Vec<_>>>()?;
        log::trace!("Responding to ids {:?} with {:?}", request.ids, ordered_outputs);
        if let Err(err) = request.sender.send(helpers::to_results_from_outputs(ordered_outputs)) {
            log::warn!("Sending a response to deallocated channel: {:?}", err);
        }
    }

    Ok(())
}

fn fail_pending(pending: &mut BTreeMap<RequestId, Pending>, error: &Error) {
    for (id, request) in std::mem::take(pending) {
        if request.sender.send(Err(error.clone())).is_err() {
            log::trace!("Request receiver was dropped while failing WS request: {:?}", id);
        }
    }
}

enum TransportMessage {
    Request {
        ids: Vec<RequestId>,
        request: String,
        sender: oneshot::Sender<BatchResult>,
    },
    Subscribe {
        id: SubscriptionId,
        sink: mpsc::Sender<rpc::Value>,
    },
    Unsubscribe {
        id: SubscriptionId,
    },
}

/// WebSocket transport
#[derive(Clone)]
pub struct WebSocket {
    id: Arc<atomic::AtomicUsize>,
    requests: Arc<parking_lot::Mutex<mpsc::Sender<TransportMessage>>>,
    channel_capacity: usize,
}

impl fmt::Debug for WebSocket {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("WebSocket").field("id", &self.id).finish()
    }
}

impl WebSocket {
    /// Create a WebSocket transport with a 16 MiB incoming frame and message limit.
    pub async fn new(url: &str) -> error::Result<Self> {
        Self::new_with_limits(url, DEFAULT_MAX_WS_RESPONSE_SIZE, DEFAULT_WS_CHANNEL_CAPACITY).await
    }

    /// Create a WebSocket transport with a maximum incoming frame and message size.
    pub async fn new_with_max_response_size(url: &str, max_response_size: usize) -> error::Result<Self> {
        Self::new_with_limits(url, max_response_size, DEFAULT_WS_CHANNEL_CAPACITY).await
    }

    /// Create a WebSocket transport with response-size and internal-channel bounds.
    pub async fn new_with_limits(
        url: &str,
        max_response_size: usize,
        channel_capacity: usize,
    ) -> error::Result<Self> {
        if channel_capacity == 0 {
            return Err(Error::Transport(TransportError::Message(
                "WebSocket channel capacity must be greater than zero".into(),
            )));
        }
        let id = Arc::new(atomic::AtomicUsize::new(1));
        let task = WsServerTask::new(url, max_response_size).await?;
        let (sink, stream) = mpsc::channel(channel_capacity);
        // Spawn background task for the transport.
        #[cfg(feature = "ws-tokio")]
        tokio::spawn(task.into_task(stream));
        #[cfg(all(feature = "ws-async-io", not(feature = "ws-tokio")))]
        async_global_executor::spawn(task.into_task(stream)).detach();

        Ok(Self {
            id,
            requests: Arc::new(parking_lot::Mutex::new(sink)),
            channel_capacity,
        })
    }

    fn send(&self, msg: TransportMessage) -> error::Result {
        self.requests.lock().try_send(msg).map_err(|error| {
            let message = if error.is_full() {
                "WebSocket request queue is full"
            } else {
                "Cannot send request because the WebSocket task finished"
            };
            Error::Transport(TransportError::Message(message.into()))
        })
    }

    fn send_request(&self, ids: Vec<RequestId>, request: rpc::Request) -> error::Result<oneshot::Receiver<BatchResult>> {
        let request = helpers::to_string(&request)?;
        log::debug!("[{:?}] Calling: {}", ids, request);
        let (sender, receiver) = oneshot::channel();
        self.send(TransportMessage::Request { ids, request, sender })?;
        Ok(receiver)
    }
}

fn dropped_err<T>(_: T) -> error::Error {
    Error::Transport(TransportError::Message(
        "Cannot send request. Internal task finished.".into(),
    ))
}

fn batch_to_single(response: BatchResult) -> SingleResult {
    match response?.into_iter().next() {
        Some(res) => res,
        None => Err(Error::InvalidResponse("Expected single, got batch.".into())),
    }
}

fn batch_to_batch(res: BatchResult) -> BatchResult {
    res
}

enum ResponseState {
    Receiver(Option<error::Result<oneshot::Receiver<BatchResult>>>),
    Waiting(oneshot::Receiver<BatchResult>),
}

/// A WS response wrapper.
pub struct Response<R, T> {
    extract: T,
    state: ResponseState,
    _data: std::marker::PhantomData<R>,
}

impl<R, T> Response<R, T> {
    fn new(response: error::Result<oneshot::Receiver<BatchResult>>, extract: T) -> Self {
        Self {
            extract,
            state: ResponseState::Receiver(Some(response)),
            _data: Default::default(),
        }
    }
}

impl<R, T> Future for Response<R, T>
where
    R: Unpin + 'static,
    T: Fn(BatchResult) -> error::Result<R> + Unpin + 'static,
{
    type Output = error::Result<R>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        loop {
            match self.state {
                ResponseState::Receiver(ref mut res) => {
                    let Some(response) = res.take() else {
                        return Poll::Ready(Err(Error::Internal));
                    };
                    let receiver = response?;
                    self.state = ResponseState::Waiting(receiver)
                }
                ResponseState::Waiting(ref mut future) => {
                    let response = ready!(future.poll_unpin(cx)).map_err(dropped_err)?;
                    return Poll::Ready((self.extract)(response));
                }
            }
        }
    }
}

impl Transport for WebSocket {
    type Out = Response<rpc::Value, fn(BatchResult) -> SingleResult>;

    fn prepare(&self, method: &str, params: Vec<rpc::Value>) -> (RequestId, rpc::Call) {
        let id = helpers::next_request_id(&self.id);
        let request = helpers::build_request(id, method, params);

        (id, request)
    }

    fn send(&self, id: RequestId, request: rpc::Call) -> Self::Out {
        let response = self.send_request(vec![id], rpc::Request::Single(request));
        Response::new(response, batch_to_single)
    }
}

impl BatchTransport for WebSocket {
    type Batch = Response<Vec<SingleResult>, fn(BatchResult) -> BatchResult>;

    fn send_batch<T>(&self, requests: T) -> Self::Batch
    where
        T: IntoIterator<Item = (RequestId, rpc::Call)>,
    {
        let requests = requests.into_iter().collect::<Vec<_>>();
        if requests.is_empty() {
            return Response::new(Err(Error::InvalidResponse("empty WebSocket batch request".into())), batch_to_batch);
        }
        let ids = requests.iter().map(|(id, _)| *id).collect();
        let calls = requests.into_iter().map(|(_, call)| call).collect();
        let response = self.send_request(ids, rpc::Request::Batch(calls));
        Response::new(response, batch_to_batch)
    }
}

impl DuplexTransport for WebSocket {
    type NotificationStream = mpsc::Receiver<rpc::Value>;

    fn subscribe(&self, id: SubscriptionId) -> error::Result<Self::NotificationStream> {
        let (sink, stream) = mpsc::channel(self.channel_capacity);
        self.send(TransportMessage::Subscribe { id, sink })?;
        Ok(stream)
    }

    fn unsubscribe(&self, id: SubscriptionId) -> error::Result {
        self.send(TransportMessage::Unsubscribe { id })
    }
}

/// Compatibility layer between async-std and tokio
#[cfg(all(feature = "ws-async-io", not(feature = "ws-tokio")))]
#[doc(hidden)]
pub mod compat {
    pub use async_net::{TcpListener, TcpStream};
    /// TLS stream type for the async-io runtime.
    #[cfg(feature = "ws-tls-async-io")]
    pub type TlsStream = async_native_tls::TlsStream<TcpStream>;
    /// Dummy TLS stream type.
    #[cfg(not(feature = "ws-tls-async-io"))]
    pub type TlsStream = TcpStream;

    /// Create new TcpStream object.
    pub async fn raw_tcp_stream(addrs: String) -> std::io::Result<TcpStream> {
        TcpStream::connect(addrs).await
    }

    /// Wrap given argument into compatibility layer.
    #[inline(always)]
    pub fn compat<T>(t: T) -> T {
        t
    }
}

/// Compatibility layer between async-std and tokio
#[cfg(feature = "ws-tokio")]
pub mod compat {
    use std::io;
    use tokio::io::AsyncRead;
    use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};

    /// async-std compatible TcpStream.
    pub type TcpStream = Compat<tokio::net::TcpStream>;
    /// async-std compatible TcpListener.
    pub type TcpListener = tokio::net::TcpListener;
    /// TLS stream type for tokio runtime.
    #[cfg(feature = "ws-tls-tokio")]
    pub type TlsStream = Compat<tokio_native_tls::TlsStream<tokio::net::TcpStream>>;
    /// Rustls TLS stream type for tokio runtime.
    #[cfg(all(feature = "ws-rustls-tokio", not(feature = "ws-tls-tokio")))]
    pub type TlsStream = Compat<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>;
    /// Dummy TLS stream type.
    #[cfg(all(not(feature = "ws-tls-tokio"), not(feature = "ws-rustls-tokio")))]
    pub type TlsStream = TcpStream;

    /// Create new TcpStream object.
    pub async fn raw_tcp_stream(addrs: String) -> io::Result<tokio::net::TcpStream> {
        tokio::net::TcpStream::connect(addrs).await
    }

    /// Wrap given argument into compatibility layer.
    pub fn compat<T: AsyncRead>(t: T) -> Compat<T> {
        TokioAsyncReadCompatExt::compat(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{rpc, Transport};
    use futures::io::{BufReader, BufWriter};
    use soketto::handshake;

    #[test]
    fn bounds_matching() {
        fn async_rw<T: AsyncRead + AsyncWrite>() {}

        async_rw::<TcpStream>();
        async_rw::<MaybeTlsStream<TcpStream, TlsStream>>();
    }

    #[test]
    fn reorders_batch_responses_by_request_id() {
        let (sender, receiver) = oneshot::channel();
        let mut subscriptions = BTreeMap::new();
        let mut pending = BTreeMap::from([(
            1,
            Pending {
                ids: vec![1, 2],
                sender,
            },
        )]);

        handle_message(
            br#"[{"jsonrpc":"2.0","id":2,"result":"second"},{"jsonrpc":"2.0","id":1,"result":"first"}]"#,
            &mut subscriptions,
            &mut pending,
        )
        .unwrap();

        let response = futures::executor::block_on(receiver).unwrap().unwrap();
        assert_eq!(
            response,
            vec![
                Ok(rpc::Value::String("first".into())),
                Ok(rpc::Value::String("second".into()))
            ]
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn rejects_duplicate_batch_response_ids() {
        let (sender, _receiver) = oneshot::channel();
        let mut subscriptions = BTreeMap::new();
        let mut pending = BTreeMap::from([(
            1,
            Pending {
                ids: vec![1, 2],
                sender,
            },
        )]);
        let result = handle_message(
            br#"[{"jsonrpc":"2.0","id":1,"result":1},{"jsonrpc":"2.0","id":1,"result":2}]"#,
            &mut subscriptions,
            &mut pending,
        );

        assert!(matches!(result, Err(Error::InvalidResponse(message)) if message.contains("duplicate")));
    }

    #[test]
    fn ignores_unrelated_notifications_and_rejects_malformed_subscriptions() {
        let mut subscriptions = BTreeMap::new();
        let mut pending = BTreeMap::new();
        assert!(handle_message(
            br#"{"jsonrpc":"2.0","method":"other_event","params":{"subscription":"0x1","result":1}}"#,
            &mut subscriptions,
            &mut pending,
        )
        .is_ok());

        let result = handle_message(
            br#"{"jsonrpc":"2.0","method":"eth_subscription","params":[]}"#,
            &mut subscriptions,
            &mut pending,
        );
        assert!(matches!(result, Err(Error::InvalidResponse(message)) if message.contains("not an object")));
    }

    #[test]
    fn closes_backpressured_subscription() {
        let id: SubscriptionId = "0x1".to_owned().into();
        let (mut sink, _stream) = mpsc::channel(1);
        while sink.try_send(rpc::Value::Null).is_ok() {}
        let mut subscriptions = BTreeMap::from([(id.clone(), sink)]);
        let mut pending = BTreeMap::new();

        handle_message(
            br#"{"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":"0x1","result":1}}"#,
            &mut subscriptions,
            &mut pending,
        )
        .unwrap();

        assert!(!subscriptions.contains_key(&id));
    }

    #[test]
    fn reports_full_request_queue() {
        let (mut requests, _task_requests) = mpsc::channel(1);
        while requests
            .try_send(TransportMessage::Unsubscribe {
                id: "fill".to_owned().into(),
            })
            .is_ok()
        {}
        let websocket = WebSocket {
            id: Arc::new(atomic::AtomicUsize::new(1)),
            requests: Arc::new(parking_lot::Mutex::new(requests)),
            channel_capacity: 1,
        };

        let result = websocket.unsubscribe("overflow".to_owned().into());

        assert!(matches!(
            result,
            Err(Error::Transport(TransportError::Message(message))) if message.contains("queue is full")
        ));
    }

    #[tokio::test]
    async fn rejects_zero_channel_capacity_before_connecting() {
        let result = WebSocket::new_with_limits(
            "ws://invalid.invalid",
            DEFAULT_MAX_WS_RESPONSE_SIZE,
            0,
        )
        .await;

        assert!(matches!(
            result,
            Err(Error::Transport(TransportError::Message(message))) if message.contains("greater than zero")
        ));
    }

    #[cfg(not(any(feature = "ws-tls-tokio", feature = "ws-tls-async-io", feature = "ws-rustls-tokio")))]
    #[tokio::test]
    async fn rejects_wss_before_attempting_a_connection_without_tls() {
        let result = WsServerTask::new("wss://invalid.invalid", DEFAULT_MAX_WS_RESPONSE_SIZE).await;
        assert!(matches!(
            result,
            Err(Error::Transport(TransportError::Message(message))) if message.contains("requires")
        ));
    }

    #[tokio::test]
    async fn should_send_a_request() {
        let _ = env_logger::try_init();
        // given
        let listener = futures::executor::block_on(compat::TcpListener::bind("127.0.0.1:0"))
            .expect("Failed to bind");
        let addr = listener.local_addr().expect("Failed to read bound address").to_string();
        println!("Starting the server.");
        tokio::spawn(server(listener, addr.clone()));

        let ws = WebSocket::new(&format!("ws://{addr}")).await.unwrap();

        // when
        let res = ws.execute("eth_accounts", vec![rpc::Value::String("1".into())]);

        // then
        assert_eq!(res.await, Ok(rpc::Value::String("x".into())));
    }

    #[tokio::test]
    async fn rejects_duplicate_outgoing_batch_ids() {
        let listener = futures::executor::block_on(compat::TcpListener::bind("127.0.0.1:0"))
            .expect("Failed to bind");
        let addr = listener.local_addr().expect("Failed to read bound address").to_string();
        tokio::spawn(server(listener, addr.clone()));
        let ws = WebSocket::new(&format!("ws://{addr}")).await.unwrap();
        let calls = vec![
            (7, helpers::build_request(7, "first", Vec::new())),
            (7, helpers::build_request(7, "second", Vec::new())),
        ];

        assert!(matches!(
            ws.send_batch(calls).await,
            Err(Error::InvalidResponse(message)) if message.contains("duplicate request id")
        ));
    }

    async fn server(listener: compat::TcpListener, addr: String) {
        println!("Listening on: {}", addr);
        let (socket, _) = listener.accept().await.unwrap();
        let socket = compat::compat(socket);
        let mut server = handshake::Server::new(BufReader::new(BufWriter::new(socket)));
        let key = {
            let req = server.receive_request().await.unwrap();
            req.key()
        };
        let accept = handshake::server::Response::Accept { key, protocol: None };
        server.send_response(&accept).await.unwrap();
        let (mut sender, mut receiver) = server.into_builder().finish();
        loop {
            let mut data = Vec::new();
            match receiver.receive_data(&mut data).await {
                Ok(data_type) if data_type.is_text() => {
                    assert_eq!(
                        std::str::from_utf8(&data),
                        Ok(r#"{"jsonrpc":"2.0","method":"eth_accounts","params":["1"],"id":1}"#)
                    );
                    sender
                        .send_text(r#"{"jsonrpc":"2.0","id":1,"result":"x"}"#)
                        .await
                        .unwrap();
                    sender.flush().await.unwrap();
                }
                Err(soketto::connection::Error::Closed) => break,
                e => panic!("Unexpected data: {:?}", e),
            }
        }
    }
}
