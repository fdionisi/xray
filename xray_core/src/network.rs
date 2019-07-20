use std::rc::Rc;

use futures::{stream, Async, Future, Stream};
pub use memo_core::{Operation, OperationEnvelope};

use crate::never::Never;
use crate::notify_cell::NotifyCell;
use crate::{Error, ForegroundExecutor};

pub trait NetworkProvider {
    fn broadcast(
        &self,
        envelopes: Box<dyn Stream<Item = OperationEnvelope, Error = Error>>,
    ) -> Box<dyn Future<Item = (), Error = Error>>;
    fn fetch(&self) -> Box<dyn Future<Item = Vec<Operation>, Error = Error>>;
    fn updates(&self) -> Box<dyn Stream<Item = Option<Operation>, Error = ()>>;
}

#[derive(Serialize, Deserialize)]
pub enum RpcUpdate {
    Operation(Operation),
}

#[derive(Serialize, Deserialize)]
pub enum RpcRequest {
    Broadcast(Vec<OperationEnvelope>),
    Fetch,
}

#[derive(Serialize, Deserialize)]
pub enum RpcResponse {
    Fetch(Vec<Operation>),
}

pub struct RemoteNetworkProvider {
    service: xray_rpc::client::Service<NetworkProviderService>,
    updates: NotifyCell<Option<Operation>>,
}

pub struct NetworkProviderService {
    network_provider: Rc<NetworkProvider>,
    updates: Box<dyn Stream<Item = Option<Operation>, Error = ()>>,
}

impl RemoteNetworkProvider {
    pub fn new(
        foreground: ForegroundExecutor,
        service: xray_rpc::client::Service<NetworkProviderService>,
    ) -> Self {
        let service_updates = service.updates().unwrap();
        let updates = NotifyCell::new(None);

        let receive_updates = updates.clone();
        foreground
            .execute(Box::new(service_updates.for_each(move |operation| {
                match operation {
                    RpcUpdate::Operation(op) => receive_updates.set(Some(op)),
                }
                Ok(())
            })))
            .unwrap();

        Self { service, updates }
    }
}

impl NetworkProvider for RemoteNetworkProvider {
    fn broadcast(
        &self,
        envelopes: Box<dyn Stream<Item = OperationEnvelope, Error = Error>>,
    ) -> Box<dyn Future<Item = (), Error = Error>> {
        let service = self.service.clone();
        Box::new(
            envelopes
                .collect()
                .map_err(|err| Error::from(err))
                .and_then(move |envelopes| {
                    service
                        .request(RpcRequest::Broadcast(envelopes))
                        .map_err(|err| Error::from(err))
                        .and_then(|_| Ok(()))
                }),
        )
    }

    fn fetch(&self) -> Box<dyn Future<Item = Vec<Operation>, Error = Error>> {
        Box::new(
            self.service
                .request(RpcRequest::Fetch)
                .map_err(|err| Error::from(err))
                .and_then(|response| match response {
                    RpcResponse::Fetch(ops) => Ok(ops),
                }),
        )
    }

    fn updates(&self) -> Box<dyn Stream<Item = Option<Operation>, Error = ()>> {
        Box::new(self.updates.observe())
    }
}

impl NetworkProviderService {
    pub fn new(network_provider: Rc<NetworkProvider>) -> Self {
        let updates = network_provider.updates();
        Self {
            network_provider,
            updates,
        }
    }
}

impl xray_rpc::server::Service for NetworkProviderService {
    type State = ();
    type Update = RpcUpdate;
    type Request = RpcRequest;
    type Response = RpcResponse;

    fn init(&mut self, _connection: &xray_rpc::server::Connection) -> () {
        ()
    }

    fn poll_update(&mut self, _: &xray_rpc::server::Connection) -> Async<Option<Self::Update>> {
        match self.updates.poll() {
            Ok(Async::Ready(Some(op))) => Async::Ready(op.map(|o| RpcUpdate::Operation(o))),
            Ok(Async::Ready(None)) | Err(_) => Async::Ready(None),
            Ok(Async::NotReady) => Async::NotReady,
        }
    }

    fn request(
        &mut self,
        request: Self::Request,
        _connection: &xray_rpc::server::Connection,
    ) -> Option<Box<Future<Item = Self::Response, Error = Never>>> {
        match request {
            RpcRequest::Broadcast(envelope) => {
                self.network_provider
                    .broadcast(Box::new(stream::iter_ok(envelope)));
                None
            }
            RpcRequest::Fetch => {
                Some(Box::new(self.network_provider.fetch().then(
                    |response| match response {
                        Ok(ops) => Ok(RpcResponse::Fetch(ops)),
                        _ => panic!("Can't fail on fetch"),
                    },
                )))
            }
        }
    }
}

#[cfg(test)]
pub mod tests {
    use std::cell::RefCell;

    use futures::{future, Future, Stream};
    use xray_shared::notify_cell::NotifyCell;

    use crate::{
        network::{NetworkProvider, Operation, OperationEnvelope},
        Error,
    };

    pub struct TestNetworkProvider {
        inner: RefCell<TestNetworkProviderInner>,
        updates: NotifyCell<Option<Operation>>,
    }

    struct TestNetworkProviderInner {
        envelopes: Vec<OperationEnvelope>,
    }

    impl TestNetworkProvider {
        pub fn new() -> Self {
            Self {
                inner: RefCell::new(TestNetworkProviderInner::new()),
                updates: NotifyCell::new(None),
            }
        }

        fn broadcast_one(&self, envelope: OperationEnvelope) {
            let mut inner = self.inner.borrow_mut();

            self.updates.set(Some(envelope.operation.clone()));

            inner.insert(envelope);
        }
    }

    impl NetworkProvider for TestNetworkProvider {
        fn broadcast(
            &self,
            envelopes: Box<dyn Stream<Item = OperationEnvelope, Error = Error>>,
        ) -> Box<dyn Future<Item = (), Error = Error>> {
            envelopes.wait().for_each(|envelope| {
                self.broadcast_one(envelope.unwrap());
            });

            Box::new(future::ok(()))
        }

        fn fetch(&self) -> Box<dyn Future<Item = Vec<Operation>, Error = Error>> {
            let inner = self.inner.borrow();

            Box::new(future::ok(
                inner
                    .envelopes
                    .iter()
                    .map(|envelope| envelope.operation.clone())
                    .collect::<Vec<Operation>>(),
            ))
        }

        fn updates(&self) -> Box<dyn Stream<Item = Option<Operation>, Error = ()>> {
            Box::new(self.updates.observe())
        }
    }

    impl TestNetworkProviderInner {
        fn new() -> Self {
            TestNetworkProviderInner { envelopes: vec![] }
        }

        fn insert(&mut self, envelope: OperationEnvelope) {
            self.envelopes.push(envelope)
        }
    }

    pub fn collect_ops(
        ops: Box<dyn Stream<Item = OperationEnvelope, Error = Error>>,
    ) -> Vec<Operation> {
        ops.wait()
            .map(|op| op.unwrap().operation)
            .collect::<Vec<Operation>>()
    }
}
