//! Batching Transport

use crate::{
    error::{self, Error, TransportError},
    rpc, BatchTransport, RequestId, Transport,
};
use futures::{
    channel::oneshot,
    task::{Context, Poll},
    Future, FutureExt,
};
use parking_lot::Mutex;
use std::{collections::BTreeMap, pin::Pin, sync::Arc};

type Pending = oneshot::Sender<error::Result<rpc::Value>>;
type PendingRequests = Arc<Mutex<BTreeMap<RequestId, Pending>>>;

struct PendingBatchGuard {
    ids: Vec<RequestId>,
    pending: PendingRequests,
    armed: bool,
}

impl PendingBatchGuard {
    fn take_senders(&mut self) -> Vec<(usize, RequestId, Pending)> {
        let senders = {
            let mut pending = self.pending.lock();
            self.ids
                .iter()
                .copied()
                .enumerate()
                .filter_map(|(idx, id)| pending.remove(&id).map(|sender| (idx, id, sender)))
                .collect()
        };
        self.armed = false;
        senders
    }
}

impl Drop for PendingBatchGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        let error = Error::Transport(TransportError::Message("batch submission was cancelled".into()));
        for (_, id, sender) in self.take_senders() {
            if sender.send(Err(error.clone())).is_err() {
                log::trace!("Cancelled batch receiver was dropped: {:?}", id);
            }
        }
    }
}

/// Transport allowing to batch queries together.
///
/// Note: cloned instances of [Batch] share the queues of pending and unsent requests.
/// If you want to avoid it, use [Batch::new] repeatedly instead.
#[derive(Debug, Clone)]
pub struct Batch<T> {
    transport: T,
    pending: PendingRequests,
    batch: Arc<Mutex<Vec<(RequestId, rpc::Call)>>>,
}

impl<T> Batch<T>
where
    T: BatchTransport,
{
    /// Creates new Batch transport given existing transport supporting batch requests.
    pub fn new(transport: T) -> Self {
        Batch {
            transport,
            pending: Default::default(),
            batch: Default::default(),
        }
    }

    /// Sends all requests as a batch.
    pub fn submit_batch(&self) -> impl Future<Output = error::Result<Vec<error::Result<rpc::Value>>>> {
        let batch = std::mem::take(&mut *self.batch.lock());
        let ids = batch.iter().map(|&(id, _)| id).collect::<Vec<_>>();

        let batch = self.transport.send_batch(batch);
        let mut guard = PendingBatchGuard {
            ids,
            pending: self.pending.clone(),
            armed: true,
        };

        async move {
            let res = match batch.await {
                Ok(results) if results.len() != guard.ids.len() => Err(Error::InvalidResponse(format!(
                    "batch returned {} responses for {} requests",
                    results.len(),
                    guard.ids.len()
                ))),
                res => res,
            };
            let senders = guard.take_senders();
            for (idx, request_id, sender) in senders {
                let send_result = match res {
                    Ok(ref results) => sender.send(results.get(idx).cloned().unwrap_or(Err(Error::Internal))),
                    Err(ref err) => sender.send(Err(err.clone())),
                };
                if send_result.is_err() {
                    log::trace!("Batched request receiver was dropped: {:?}", request_id);
                }
            }
            res
        }
    }
}

impl<T> Transport for Batch<T>
where
    T: BatchTransport,
{
    type Out = SingleResult;

    fn prepare(&self, method: &str, params: Vec<rpc::Value>) -> (RequestId, rpc::Call) {
        self.transport.prepare(method, params)
    }

    fn send(&self, id: RequestId, request: rpc::Call) -> Self::Out {
        let (tx, rx) = oneshot::channel();
        let rejected = {
            let mut pending = self.pending.lock();
            match pending.entry(id) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(tx);
                    None
                }
                std::collections::btree_map::Entry::Occupied(_) => Some(tx),
            }
        };
        if let Some(tx) = rejected {
            let error = Error::Transport(TransportError::Message(format!("batch request id collision: {id}")));
            if tx.send(Err(error)).is_err() {
                log::trace!("Colliding batch request receiver was dropped: {:?}", id);
            }
            return SingleResult(rx);
        }
        self.batch.lock().push((id, request));

        SingleResult(rx)
    }
}

/// Result of calling a single method that will be part of the batch.
/// Converts `oneshot::Receiver` error into `Error::Internal`
pub struct SingleResult(oneshot::Receiver<error::Result<rpc::Value>>);

impl Future for SingleResult {
    type Output = error::Result<rpc::Value>;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context) -> Poll<Self::Output> {
        Poll::Ready(ready!(self.0.poll_unpin(ctx)).map_err(|_| Error::Internal)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::{self, BoxFuture, FutureExt};

    #[derive(Clone, Debug)]
    struct MockBatchTransport {
        response: Option<error::Result<Vec<error::Result<rpc::Value>>>>,
    }

    impl Transport for MockBatchTransport {
        type Out = future::Ready<error::Result<rpc::Value>>;

        fn prepare(&self, method: &str, params: Vec<rpc::Value>) -> (RequestId, rpc::Call) {
            (1, crate::helpers::build_request(1, method, params))
        }

        fn send(&self, _: RequestId, _: rpc::Call) -> Self::Out {
            future::ready(Err(Error::Internal))
        }
    }

    impl BatchTransport for MockBatchTransport {
        type Batch = BoxFuture<'static, error::Result<Vec<error::Result<rpc::Value>>>>;

        fn send_batch<T>(&self, _: T) -> Self::Batch
        where
            T: IntoIterator<Item = (RequestId, rpc::Call)>,
        {
            match self.response.clone() {
                Some(response) => future::ready(response).boxed(),
                None => future::pending().boxed(),
            }
        }
    }

    #[test]
    fn rejects_batch_response_cardinality_mismatch() {
        for results in [Vec::new(), vec![Ok(rpc::Value::Null), Ok(rpc::Value::Null)]] {
            let transport = Batch::new(MockBatchTransport {
                response: Some(Ok(results)),
            });
            let (id, request) = transport.prepare("test", Vec::new());
            let single = transport.send(id, request);

            assert!(matches!(
                futures::executor::block_on(transport.submit_batch()),
                Err(Error::InvalidResponse(_))
            ));
            assert!(matches!(futures::executor::block_on(single), Err(Error::InvalidResponse(_))));
        }
    }

    #[test]
    fn cancelling_submission_fails_captured_requests() {
        let transport = Batch::new(MockBatchTransport { response: None });
        let (id, request) = transport.prepare("test", Vec::new());
        let single = transport.send(id, request);

        drop(transport.submit_batch());

        assert!(matches!(
            futures::executor::block_on(single),
            Err(Error::Transport(TransportError::Message(message))) if message.contains("cancelled")
        ));
    }

    #[test]
    fn duplicate_request_id_is_rejected_without_replacing_the_first() {
        let transport = Batch::new(MockBatchTransport { response: None });
        let (_, request) = transport.prepare("test", Vec::new());
        let first = transport.send(7, request.clone());
        let duplicate = transport.send(7, request);

        assert!(matches!(
            futures::executor::block_on(duplicate),
            Err(Error::Transport(TransportError::Message(message))) if message.contains("collision")
        ));
        drop(transport.submit_batch());
        assert!(matches!(futures::executor::block_on(first), Err(Error::Transport(_))));
    }
}
