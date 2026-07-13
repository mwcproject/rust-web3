//! IPC transport

use crate::{
    api::SubscriptionId, error::TransportError, helpers, BatchTransport, DuplexTransport, Error, RequestId, Result,
    Transport,
};
use futures::{
    future::{join_all, JoinAll},
    stream::StreamExt,
};
use jsonrpc_core as rpc;
use std::{
    collections::BTreeMap,
    path::Path,
    pin::Pin,
    sync::{atomic::AtomicUsize, Arc},
    task::{Context, Poll},
};
use tokio::{
    io::AsyncWriteExt,
    sync::{mpsc, oneshot},
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::io::ReaderStream;

const DEFAULT_MAX_IPC_RESPONSE_SIZE: usize = 16 * 1024 * 1024;
const DEFAULT_IPC_CHANNEL_CAPACITY: usize = 256;

#[cfg(unix)]
use tokio::net::UnixStream;

/// Unix Domain Sockets (IPC) transport.
#[derive(Debug, Clone)]
pub struct Ipc {
    id: Arc<AtomicUsize>,
    messages_tx: mpsc::Sender<TransportMessage>,
    channel_capacity: usize,
}

#[cfg(unix)]
impl Ipc {
    /// Creates a new IPC transport from a given path.
    ///
    /// IPC is only available on Unix. Buffered responses are limited to 16 MiB by default;
    /// use [`Ipc::new_with_max_response_size`] to choose another bound.
    pub async fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::new_with_limits(path, DEFAULT_MAX_IPC_RESPONSE_SIZE, DEFAULT_IPC_CHANNEL_CAPACITY).await
    }

    /// Creates an IPC transport with a configurable maximum buffered response size.
    pub async fn new_with_max_response_size<P: AsRef<Path>>(path: P, max_response_size: usize) -> Result<Self> {
        Self::new_with_limits(path, max_response_size, DEFAULT_IPC_CHANNEL_CAPACITY).await
    }

    /// Creates an IPC transport with response-size and internal-channel bounds.
    pub async fn new_with_limits<P: AsRef<Path>>(
        path: P,
        max_response_size: usize,
        channel_capacity: usize,
    ) -> Result<Self> {
        if channel_capacity == 0 {
            return Err(Error::Transport(TransportError::Message(
                "IPC channel capacity must be greater than zero".into(),
            )));
        }
        let stream = UnixStream::connect(path).await?;

        Ok(Self::with_stream_and_limits(stream, max_response_size, channel_capacity))
    }

    #[cfg(test)]
    fn with_stream(stream: UnixStream) -> Self {
        Self::with_stream_and_limits(stream, DEFAULT_MAX_IPC_RESPONSE_SIZE, DEFAULT_IPC_CHANNEL_CAPACITY)
    }

    #[cfg(test)]
    fn with_stream_and_max_response_size(stream: UnixStream, max_response_size: usize) -> Self {
        Self::with_stream_and_limits(stream, max_response_size, DEFAULT_IPC_CHANNEL_CAPACITY)
    }

    fn with_stream_and_limits(stream: UnixStream, max_response_size: usize, channel_capacity: usize) -> Self {
        let id = Arc::new(AtomicUsize::new(1));
        let (messages_tx, messages_rx) = mpsc::channel(channel_capacity);

        tokio::spawn(async move {
            if let Err(error) = run_server(stream, ReceiverStream::new(messages_rx), max_response_size).await {
                log::error!("IPC task terminated: {}", error);
            }
        });

        Ipc {
            id,
            messages_tx,
            channel_capacity,
        }
    }
}

impl Transport for Ipc {
    type Out = SingleResponse;

    fn prepare(&self, method: &str, params: Vec<rpc::Value>) -> (crate::RequestId, rpc::Call) {
        let id = helpers::next_request_id(&self.id);
        let request = helpers::build_request(id, method, params);
        (id, request)
    }

    fn send(&self, id: RequestId, call: rpc::Call) -> Self::Out {
        let (response_tx, response_rx) = oneshot::channel();
        let message = TransportMessage::Single((id, call, response_tx));

        let queued = self.messages_tx.try_send(message).map_err(|error| {
            let message = match error {
                mpsc::error::TrySendError::Full(_) => "IPC request queue is full",
                mpsc::error::TrySendError::Closed(_) => "Cannot send request because the IPC task finished",
            };
            Error::Transport(TransportError::Message(message.into()))
        });
        SingleResponse(queued.map(|()| response_rx))
    }
}

impl BatchTransport for Ipc {
    type Batch = BatchResponse;

    fn send_batch<T: IntoIterator<Item = (RequestId, rpc::Call)>>(&self, requests: T) -> Self::Batch {
        let mut response_rxs = vec![];

        let message = TransportMessage::Batch(
            requests
                .into_iter()
                .map(|(id, call)| {
                    let (response_tx, response_rx) = oneshot::channel();
                    response_rxs.push(response_rx);

                    (id, call, response_tx)
                })
                .collect(),
        );

        BatchResponse(
            self.messages_tx
                .try_send(message)
                .map(|()| join_all(response_rxs))
                .map_err(|error| {
                    let message = match error {
                        mpsc::error::TrySendError::Full(_) => "IPC request queue is full",
                        mpsc::error::TrySendError::Closed(_) => {
                            "Cannot send batch because the IPC task finished"
                        }
                    };
                    Error::Transport(TransportError::Message(message.into()))
                }),
        )
    }
}

impl DuplexTransport for Ipc {
    type NotificationStream = ReceiverStream<rpc::Value>;

    fn subscribe(&self, id: SubscriptionId) -> Result<Self::NotificationStream> {
        let (tx, rx) = mpsc::channel(self.channel_capacity);
        self.messages_tx
            .try_send(TransportMessage::Subscribe(id, tx))
            .map_err(|error| {
                let message = match error {
                    mpsc::error::TrySendError::Full(_) => "IPC request queue is full",
                    mpsc::error::TrySendError::Closed(_) => {
                        "Cannot subscribe because the IPC task finished"
                    }
                };
                Error::Transport(TransportError::Message(message.into()))
            })?;
        Ok(ReceiverStream::new(rx))
    }

    fn unsubscribe(&self, id: SubscriptionId) -> Result<()> {
        self.messages_tx
            .try_send(TransportMessage::Unsubscribe(id))
            .map_err(|error| {
                let message = match error {
                    mpsc::error::TrySendError::Full(_) => "IPC request queue is full",
                    mpsc::error::TrySendError::Closed(_) => {
                        "Cannot unsubscribe because the IPC task finished"
                    }
                };
                Error::Transport(TransportError::Message(message.into()))
            })
    }
}

/// A future representing a pending RPC request. Resolves to a JSON RPC output.
pub struct SingleResponse(Result<oneshot::Receiver<Result<rpc::Output>>>);

impl futures::Future for SingleResponse {
    type Output = Result<rpc::Value>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match &mut self.0 {
            Err(err) => Poll::Ready(Err(err.clone())),
            Ok(rx) => {
                let output = ready!(futures::Future::poll(Pin::new(rx), cx))??;
                Poll::Ready(helpers::to_result_from_output(output))
            }
        }
    }
}

/// A future representing a pending batch RPC request. Resolves to a vector of JSON RPC value.
pub struct BatchResponse(Result<JoinAll<oneshot::Receiver<Result<rpc::Output>>>>);

impl futures::Future for BatchResponse {
    type Output = Result<Vec<Result<rpc::Value>>>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match &mut self.0 {
            Err(err) => Poll::Ready(Err(err.clone())),
            Ok(rxs) => {
                let poll = futures::Future::poll(Pin::new(rxs), cx);
                let values = ready!(poll)
                    .into_iter()
                    .map(|r| r.map_err(Into::into))
                    .map(|r| r.and_then(|output| output))
                    .map(|r| r.and_then(helpers::to_result_from_output))
                    .collect();

                Poll::Ready(Ok(values))
            }
        }
    }
}

type TransportRequest = (RequestId, rpc::Call, oneshot::Sender<Result<rpc::Output>>);

#[derive(Debug)]
enum TransportMessage {
    Single(TransportRequest),
    Batch(Vec<TransportRequest>),
    Subscribe(SubscriptionId, mpsc::Sender<rpc::Value>),
    Unsubscribe(SubscriptionId),
}

#[cfg(unix)]
async fn run_server(
    unix_stream: UnixStream,
    messages_rx: ReceiverStream<TransportMessage>,
    max_response_size: usize,
) -> Result<()> {
    let (socket_reader, mut socket_writer) = unix_stream.into_split();
    let mut pending_response_txs = BTreeMap::default();
    let mut pending_batches = BTreeMap::default();
    let mut subscription_txs = BTreeMap::default();

    let mut socket_reader = ReaderStream::new(socket_reader);
    let mut messages_rx = messages_rx.fuse();
    let mut read_buffer = vec![];
    let mut closed = false;

    while !closed || !pending_response_txs.is_empty() {
        tokio::select! {
            message = messages_rx.next(), if !closed => match message {
                None => closed = true,
                Some(TransportMessage::Subscribe(id, tx)) => {
                    if subscription_txs.insert(id.clone(), tx).is_some() {
                        log::warn!("Replacing a subscription with id {:?}", id);
                    }
                },
                Some(TransportMessage::Unsubscribe(id)) => {
                    if subscription_txs.remove(&id).is_none() {
                        log::warn!("Unsubscribing not subscribed id {:?}", id);
                    }
                },
                Some(TransportMessage::Single((request_id, rpc_call, response_tx))) => {
                    if pending_response_txs.contains_key(&request_id) {
                        let error = Error::Transport(TransportError::Message(format!(
                            "request id collision: {request_id}"
                        )));
                        if response_tx.send(Err(error)).is_err() {
                            log::trace!("IPC receiver dropped after id collision: {:?}", request_id);
                        }
                        continue;
                    }
                    pending_response_txs.insert(request_id, response_tx);

                    let bytes = match helpers::to_string(&rpc::Request::Single(rpc_call)) {
                        Ok(request) => request.into_bytes(),
                        Err(error) => {
                            if let Some(response_tx) = pending_response_txs.remove(&request_id)
                                && response_tx.send(Err(error)).is_err()
                            {
                                log::trace!("IPC receiver dropped after serialization failure: {:?}", request_id);
                            }
                            continue;
                        }
                    };
                    if let Err(err) = socket_writer.write_all(&bytes).await {
                        log::error!("IPC write error: {:?}", err);
                        let error = Error::from(err);
                        fail_all_pending(&mut pending_response_txs, &error);
                        return Err(error);
                    }
                }
                Some(TransportMessage::Batch(requests)) => {
                    let mut request_ids = vec![];
                    let mut rpc_calls = vec![];

                    for (request_id, rpc_call, response_tx) in requests {
                        if let std::collections::btree_map::Entry::Vacant(entry) =
                            pending_response_txs.entry(request_id)
                        {
                            request_ids.push(request_id);
                            rpc_calls.push(rpc_call);
                            entry.insert(response_tx);
                        } else {
                            let error = Error::Transport(TransportError::Message(format!(
                                "request id collision: {request_id}"
                            )));
                            if response_tx.send(Err(error)).is_err() {
                                log::trace!("IPC receiver dropped after id collision: {:?}", request_id);
                            }
                        }
                    }

                    if rpc_calls.is_empty() {
                        continue;
                    }

                    let bytes = match helpers::to_string(&rpc::Request::Batch(rpc_calls)) {
                        Ok(request) => request.into_bytes(),
                        Err(error) => {
                            fail_pending_ids(&mut pending_response_txs, &request_ids, &error);
                            continue;
                        }
                    };

                    if let Err(err) = socket_writer.write_all(&bytes).await {
                        log::error!("IPC write error: {:?}", err);
                        let error = Error::from(err);
                        fail_all_pending(&mut pending_response_txs, &error);
                        return Err(error);
                    }
                    if let Some(first_id) = request_ids.first().copied() {
                        pending_batches.insert(first_id, request_ids);
                    }
                }
            },
            bytes = socket_reader.next() => match bytes {
                Some(Ok(bytes)) => {
                    // Allocated buffers and slices are each bounded by `isize::MAX`; their lengths sum to
                    // at most `usize::MAX - 1`, so this addition cannot overflow a supported native target.
                    #[allow(
                        clippy::arithmetic_side_effects,
                        reason = "two allocated object lengths sum to less than usize::MAX"
                    )]
                    let buffered_len = read_buffer.len() + bytes.len();
                    if buffered_len > max_response_size {
                        let error = Error::InvalidResponse(format!(
                            "IPC response exceeded {max_response_size} bytes"
                        ));
                        fail_all_pending(&mut pending_response_txs, &error);
                        return Err(error);
                    }
                    read_buffer.extend_from_slice(&bytes);

                    let read_len = {
                        let mut de: serde_json::StreamDeserializer<_, serde_json::Value> =
                            serde_json::Deserializer::from_slice(&read_buffer).into_iter();

                        loop {
                            match de.next() {
                                Some(Ok(value)) => {
                                    if let Ok(notification) = serde_json::from_value::<rpc::Notification>(value.clone()) {
                                        notify(&mut subscription_txs, notification);
                                    } else if let Ok(response) = serde_json::from_value::<rpc::Response>(value) {
                                        if let Err(error) = respond(
                                            &mut pending_response_txs,
                                            &mut pending_batches,
                                            response,
                                        ) {
                                            fail_all_pending(&mut pending_response_txs, &error);
                                            return Err(error);
                                        }
                                    } else {
                                        let error = Error::InvalidResponse(
                                            "IPC JSON is neither a response nor a notification".into()
                                        );
                                        fail_all_pending(&mut pending_response_txs, &error);
                                        return Err(error);
                                    }
                                }
                                Some(Err(error)) if error.is_eof() => break,
                                Some(Err(error)) => {
                                    let error = Error::InvalidResponse(format!("invalid IPC JSON: {error}"));
                                    fail_all_pending(&mut pending_response_txs, &error);
                                    return Err(error);
                                }
                                None => break,
                            }
                        }

                        de.byte_offset()
                    };

                    read_buffer.drain(..read_len);
                },
                Some(Err(err)) => {
                    log::error!("IPC read error: {:?}", err);
                    let error = Error::from(err);
                    fail_all_pending(&mut pending_response_txs, &error);
                    return Err(error);
                },
                None if !read_buffer.is_empty() => {
                    let error = Error::InvalidResponse("IPC stream ended with incomplete JSON".into());
                    fail_all_pending(&mut pending_response_txs, &error);
                    return Err(error);
                },
                None if pending_response_txs.is_empty() => break,
                None => {
                    let error = Error::Transport(TransportError::Message(
                        "IPC stream ended with pending requests".into()
                    ));
                    fail_all_pending(&mut pending_response_txs, &error);
                    return Err(error);
                },
            }
        };
    }

    Ok(())
}

fn notify(
    subscription_txs: &mut BTreeMap<SubscriptionId, mpsc::Sender<rpc::Value>>,
    notification: rpc::Notification,
) {
    if notification.method != "eth_subscription" {
        log::warn!("Ignoring unsupported IPC notification method: {}", notification.method);
        return;
    }
    if let rpc::Params::Map(params) = notification.params {
        let id = params.get("subscription");
        let result = params.get("result");

        if let (Some(rpc::Value::String(id)), Some(result)) = (id, result) {
            let id: SubscriptionId = id.clone().into();
            let remove_subscription = if let Some(tx) = subscription_txs.get(&id) {
                if let Err(error) = tx.try_send(result.clone()) {
                    let reason = match error {
                        mpsc::error::TrySendError::Full(_) => "queue is full",
                        mpsc::error::TrySendError::Closed(_) => "receiver was dropped",
                    };
                    log::error!("Closing IPC subscription {:?}: {}", id, reason);
                    true
                } else {
                    false
                }
            } else {
                log::warn!("Got notification for unknown subscription (id: {:?})", id);
                false
            };
            if remove_subscription {
                subscription_txs.remove(&id);
            }
        } else {
            log::error!("Got unsupported notification (id: {:?})", id);
        }
    } else {
        log::error!("IPC eth_subscription notification parameters are not an object");
    }
}

fn respond(
    pending_response_txs: &mut BTreeMap<RequestId, oneshot::Sender<Result<rpc::Output>>>,
    pending_batches: &mut BTreeMap<RequestId, Vec<RequestId>>,
    response: rpc::Response,
) -> Result<()> {
    match response {
        rpc::Response::Single(output) => {
            let id = output_request_id(&output)?;
            if pending_batches.values().any(|ids| ids.contains(&id)) {
                return Err(Error::InvalidResponse(format!(
                    "received a single response for batched request id {id}"
                )));
            }
            respond_output(pending_response_txs, output)
        }
        rpc::Response::Batch(outputs) => {
            if outputs.is_empty() {
                return Err(Error::InvalidResponse("empty IPC batch response".into()));
            }
            let mut outputs_by_id = BTreeMap::new();
            let mut response_error = None;
            for output in outputs {
                match output_request_id(&output) {
                    Ok(id) => {
                        if outputs_by_id.insert(id, output).is_some() {
                            return Err(Error::InvalidResponse(format!("duplicate IPC batch response id {id}")));
                        }
                    }
                    Err(error) if response_error.is_none() => response_error = Some(error),
                    Err(error) => log::warn!("Additional invalid IPC batch output: {error}"),
                }
            }
            let matching_batches = pending_batches
                .iter()
                .filter_map(|(key, ids)| ids.iter().any(|id| outputs_by_id.contains_key(id)).then_some(*key))
                .collect::<Vec<_>>();
            if matching_batches.len() > 1 {
                return Err(Error::InvalidResponse(
                    "IPC response combined outputs from multiple pending batches".into(),
                ));
            }
            if let Some(batch_key) = matching_batches.first().copied() {
                if let Some(error) = response_error {
                    return Err(error);
                }
                let expected_ids = pending_batches.get(&batch_key).ok_or(Error::Internal)?;
                if expected_ids.len() != outputs_by_id.len()
                    || expected_ids.iter().any(|id| !outputs_by_id.contains_key(id))
                {
                    return Err(Error::InvalidResponse(format!(
                        "IPC batch response IDs do not match request IDs: expected {expected_ids:?}"
                    )));
                }
                let expected_ids = pending_batches.remove(&batch_key).ok_or(Error::Internal)?;
                for id in expected_ids {
                    let output = outputs_by_id.remove(&id).ok_or(Error::Internal)?;
                    respond_output(pending_response_txs, output)?;
                }
                return Ok(());
            }

            for output in outputs_by_id.into_values() {
                if let Err(error) = respond_output(pending_response_txs, output)
                    && response_error.is_none()
                {
                    response_error = Some(error);
                }
            }
            match response_error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }
}

fn output_request_id(output: &rpc::Output) -> Result<RequestId> {
    match output.id() {
        rpc::Id::Num(num) => RequestId::try_from(*num)
            .map_err(|_| Error::InvalidResponse(format!("IPC response id {num} does not fit RequestId"))),
        id => Err(Error::InvalidResponse(format!("unsupported IPC response id: {id:?}"))),
    }
}

fn respond_output(
    pending_response_txs: &mut BTreeMap<RequestId, oneshot::Sender<Result<rpc::Output>>>,
    output: rpc::Output,
) -> Result<()> {
    let id = output_request_id(&output)?;

    let response_tx = pending_response_txs
        .remove(&id)
        .ok_or_else(|| Error::InvalidResponse(format!("IPC response for unknown request id {id}")))?;

    if let Err(err) = response_tx.send(Ok(output)) {
        log::warn!("Sending a response to deallocated channel: {:?}", err);
    }
    Ok(())
}

fn fail_pending_ids(
    pending: &mut BTreeMap<RequestId, oneshot::Sender<Result<rpc::Output>>>,
    ids: &[RequestId],
    error: &Error,
) {
    for id in ids {
        if let Some(response_tx) = pending.remove(id)
            && response_tx.send(Err(error.clone())).is_err()
        {
            log::trace!("IPC receiver dropped while failing request: {:?}", id);
        }
    }
}

fn fail_all_pending(
    pending: &mut BTreeMap<RequestId, oneshot::Sender<Result<rpc::Output>>>,
    error: &Error,
) {
    for (id, response_tx) in std::mem::take(pending) {
        if response_tx.send(Err(error.clone())).is_err() {
            log::trace!("IPC receiver dropped while failing request: {:?}", id);
        }
    }
}

impl From<oneshot::error::RecvError> for Error {
    fn from(err: oneshot::error::RecvError) -> Self {
        Error::Transport(TransportError::Message(format!("Recv Error: {:?}", err)))
    }
}

#[cfg(all(test, unix))]
mod test {
    use super::*;
    use serde_json::json;
    use tokio::{io::AsyncWriteExt, net::UnixStream};

    #[tokio::test]
    async fn rejects_zero_channel_capacity_before_connecting() {
        let result = Ipc::new_with_limits(
            "/path/that/does/not/exist",
            DEFAULT_MAX_IPC_RESPONSE_SIZE,
            0,
        )
        .await;

        assert!(matches!(
            result,
            Err(Error::Transport(TransportError::Message(message))) if message.contains("greater than zero")
        ));
    }

    #[test]
    fn reports_full_request_queue() {
        let (messages_tx, _messages_rx) = mpsc::channel(1);
        let ipc = Ipc {
            id: Arc::new(AtomicUsize::new(1)),
            messages_tx,
            channel_capacity: 1,
        };
        assert!(ipc.unsubscribe("fill".to_owned().into()).is_ok());

        let result = ipc.unsubscribe("overflow".to_owned().into());

        assert!(matches!(
            result,
            Err(Error::Transport(TransportError::Message(message))) if message.contains("queue is full")
        ));
    }

    #[test]
    fn closes_backpressured_subscription() {
        let id: SubscriptionId = "0x1".to_owned().into();
        let (tx, _rx) = mpsc::channel(1);
        tx.try_send(rpc::Value::Null).unwrap();
        let mut subscriptions = BTreeMap::from([(id.clone(), tx)]);
        let notification = serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "method": "eth_subscription",
            "params": {"subscription": "0x1", "result": 1}
        }))
        .unwrap();

        notify(&mut subscriptions, notification);

        assert!(!subscriptions.contains_key(&id));
    }

    #[tokio::test]
    async fn works_for_single_requests() {
        let (stream1, stream2) = UnixStream::pair().unwrap();
        let ipc = Ipc::with_stream(stream1);

        tokio::spawn(eth_node_single(stream2));

        let (req_id, request) = ipc.prepare(
            "eth_test",
            vec![json!({
                "test": -1,
            })],
        );
        let response = ipc.send(req_id, request).await;
        let expected_response_json: serde_json::Value = json!({
            "test": 1,
        });
        assert_eq!(response, Ok(expected_response_json));

        let (req_id, request) = ipc.prepare(
            "eth_test",
            vec![json!({
                "test": 3,
            })],
        );
        let response = ipc.send(req_id, request).await;
        let expected_response_json: serde_json::Value = json!({
            "test": "string1",
        });
        assert_eq!(response, Ok(expected_response_json));
    }

    #[tokio::test]
    async fn malformed_json_fails_pending_request() {
        let (stream1, stream2) = UnixStream::pair().unwrap();
        let ipc = Ipc::with_stream(stream1);
        tokio::spawn(async move {
            let (reader, mut writer) = stream2.into_split();
            let mut reader = ReaderStream::new(reader);
            assert!(reader.next().await.is_some());
            writer.write_all(b"}").await.unwrap();
            writer.flush().await.unwrap();
        });

        let result = ipc.execute("eth_test", vec![]).await;
        assert!(matches!(result, Err(Error::InvalidResponse(message)) if message.contains("invalid IPC JSON")));
    }

    #[tokio::test]
    async fn incomplete_json_at_eof_fails_pending_request() {
        let (stream1, stream2) = UnixStream::pair().unwrap();
        let ipc = Ipc::with_stream(stream1);
        tokio::spawn(async move {
            let (reader, mut writer) = stream2.into_split();
            let mut reader = ReaderStream::new(reader);
            assert!(reader.next().await.is_some());
            writer.write_all(b"{").await.unwrap();
            writer.flush().await.unwrap();
        });

        let result = ipc.execute("eth_test", vec![]).await;
        assert!(matches!(result, Err(Error::InvalidResponse(message)) if message.contains("incomplete JSON")));
    }

    #[tokio::test]
    async fn invalid_response_id_fails_pending_request() {
        let (stream1, stream2) = UnixStream::pair().unwrap();
        let ipc = Ipc::with_stream(stream1);
        tokio::spawn(async move {
            let (reader, mut writer) = stream2.into_split();
            let mut reader = ReaderStream::new(reader);
            assert!(reader.next().await.is_some());
            writer
                .write_all(br#"{"jsonrpc":"2.0","id":"wrong","result":1}"#)
                .await
                .unwrap();
        });

        let result = ipc.execute("eth_test", vec![]).await;
        assert!(matches!(result, Err(Error::InvalidResponse(message)) if message.contains("unsupported IPC response id")));
    }

    #[tokio::test]
    async fn incomplete_batch_response_fails_every_request() {
        let (stream1, stream2) = UnixStream::pair().unwrap();
        let ipc = Ipc::with_stream(stream1);
        tokio::spawn(async move {
            let (reader, mut writer) = stream2.into_split();
            let mut reader = ReaderStream::new(reader);
            assert!(reader.next().await.is_some());
            writer
                .write_all(br#"[{"jsonrpc":"2.0","id":1,"result":1}]"#)
                .await
                .unwrap();
        });

        let requests = [ipc.prepare("first", vec![]), ipc.prepare("second", vec![])];
        let result = ipc.send_batch(requests).await.unwrap();
        assert_eq!(result.len(), 2);
        assert!(result.into_iter().all(|item| matches!(item, Err(Error::InvalidResponse(_)))));
    }

    #[tokio::test]
    async fn oversized_response_fails_pending_request() {
        let (stream1, stream2) = UnixStream::pair().unwrap();
        let ipc = Ipc::with_stream_and_max_response_size(stream1, 32);
        tokio::spawn(async move {
            let (reader, mut writer) = stream2.into_split();
            let mut reader = ReaderStream::new(reader);
            assert!(reader.next().await.is_some());
            writer.write_all(&[b' '; 33]).await.unwrap();
        });

        let result = ipc.execute("eth_test", vec![]).await;
        assert!(matches!(result, Err(Error::InvalidResponse(message)) if message.contains("exceeded 32 bytes")));
    }

    async fn eth_node_single(stream: UnixStream) {
        let (rx, mut tx) = stream.into_split();

        let mut rx = ReaderStream::new(rx);
        if let Some(Ok(bytes)) = rx.next().await {
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

            assert_eq!(
                v,
                json!({
                    "jsonrpc": "2.0",
                    "method": "eth_test",
                    "id": 1,
                    "params": [{
                        "test": -1
                    }]
                })
            );

            tx.write_all(r#"{"jsonrpc": "2.0", "id": 1, "result": {"test": 1}}"#.as_ref())
                .await
                .unwrap();
            tx.flush().await.unwrap();
        }

        if let Some(Ok(bytes)) = rx.next().await {
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

            assert_eq!(
                v,
                json!({
                    "jsonrpc": "2.0",
                    "method": "eth_test",
                    "id": 2,
                    "params": [{
                        "test": 3
                    }]
                })
            );

            let response_bytes = r#"{"jsonrpc": "2.0", "id": 2, "result": {"test": "string1"}}"#;
            for chunk in response_bytes.as_bytes().chunks(3) {
                tx.write_all(chunk).await.unwrap();
                tx.flush().await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn works_for_batch_request() {
        let (stream1, stream2) = UnixStream::pair().unwrap();
        let ipc = Ipc::with_stream(stream1);

        tokio::spawn(eth_node_batch(stream2));

        let requests = vec![json!({"test": -1,}), json!({"test": 3,})];
        let requests = requests.into_iter().map(|v| ipc.prepare("eth_test", vec![v]));

        let response = ipc.send_batch(requests).await;
        let expected_response_json = vec![Ok(json!({"test": 1})), Ok(json!({"test": "string1"}))];

        assert_eq!(response, Ok(expected_response_json));
    }

    async fn eth_node_batch(stream: UnixStream) {
        let (rx, mut tx) = stream.into_split();

        let mut rx = ReaderStream::new(rx);
        if let Some(Ok(bytes)) = rx.next().await {
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

            assert_eq!(
                v,
                json!([{
                    "jsonrpc": "2.0",
                    "method": "eth_test",
                    "id": 1,
                    "params": [{
                        "test": -1
                    }]
                }, {
                    "jsonrpc": "2.0",
                    "method": "eth_test",
                    "id": 2,
                    "params": [{
                        "test": 3
                    }]
                }])
            );

            let response = json!([
                {"jsonrpc": "2.0", "id": 1, "result": {"test": 1}},
                {"jsonrpc": "2.0", "id": 2, "result": {"test": "string1"}},
            ]);

            tx.write_all(serde_json::to_string(&response).unwrap().as_ref())
                .await
                .unwrap();

            tx.flush().await.unwrap();
        }
    }

    #[tokio::test]
    async fn works_for_partial_batches() {
        let (stream1, stream2) = UnixStream::pair().unwrap();
        let ipc = Ipc::with_stream(stream1);

        tokio::spawn(eth_node_partial_batches(stream2));

        let requests = vec![json!({"test": 0}), json!({"test": 1}), json!({"test": 2})];
        let requests = requests.into_iter().map(|v| ipc.execute("eth_test", vec![v]));
        let responses = join_all(requests).await;

        assert_eq!(responses[0], Ok(json!({"test": 0})));
        assert_eq!(responses[2], Ok(json!({"test": 2})));
        assert!(responses[1].is_err());
    }

    async fn eth_node_partial_batches(stream: UnixStream) {
        let (rx, mut tx) = stream.into_split();
        let mut buf = vec![];
        let mut rx = ReaderStream::new(rx);
        while let Some(Ok(bytes)) = rx.next().await {
            buf.extend(bytes);

            let requests: std::result::Result<Vec<serde_json::Value>, serde_json::Error> =
                serde_json::Deserializer::from_slice(&buf).into_iter().collect();

            if let Ok(requests) = requests
                && requests.len() == 3
            {
                break;
            }
        }

        let response = json!([
            {"jsonrpc": "2.0", "id": 1, "result": {"test": 0}},
            {"jsonrpc": "2.0", "id": "2", "result": {"test": 2}},
            {"jsonrpc": "2.0", "id": 3, "result": {"test": 2}},
        ]);

        tx.write_all(serde_json::to_string(&response).unwrap().as_ref())
            .await
            .unwrap();

        tx.flush().await.unwrap();
    }
}
