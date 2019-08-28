use std::cell::RefCell;
use std::rc::Rc;

use futures::{stream, unsync, Async, Future, Stream};
pub use memo_core::{Operation, OperationEnvelope};

use crate::never::Never;
use crate::{Error, ForegroundExecutor};

pub trait NetworkProvider {
    fn broadcast(
        &self,
        envelopes: Box<dyn Stream<Item = OperationEnvelope, Error = Error>>,
    ) -> Box<dyn Future<Item = (), Error = Error>>;
    fn fetch(&self) -> Box<dyn Future<Item = Vec<Operation>, Error = Error>>;
    fn updates(&self) -> Box<dyn Stream<Item = Operation, Error = ()>>;
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

#[derive(Debug, Serialize, Deserialize)]
pub enum RpcResponse {
    Broadcasted,
    Fetch(Vec<Operation>),
}

pub struct NetworkNotify(Vec<unsync::mpsc::UnboundedSender<Operation>>);

pub struct RemoteNetworkProvider {
    service: xray_rpc::client::Service<NetworkProviderService>,
    notify: Rc<RefCell<NetworkNotify>>,
}

pub struct NetworkProviderService {
    network_provider: Rc<dyn NetworkProvider>,
    updates: Box<dyn Stream<Item = Operation, Error = ()>>,
}

impl NetworkNotify {
    pub fn new() -> NetworkNotify {
        NetworkNotify(vec![])
    }

    pub fn broadcast_op(&mut self, operation: Operation) {
        for i in (0..self.0.len()).rev() {
            if self.0[i].unbounded_send(operation.clone()).is_err() {
                self.0.swap_remove(i);
            }
        }
    }

    pub fn updates(&mut self) -> Box<dyn Stream<Item = Operation, Error = ()>> {
        let (tx, rx) = unsync::mpsc::unbounded();
        self.0.push(tx);
        Box::new(rx)
    }
}

impl RemoteNetworkProvider {
    pub fn new(
        foreground: ForegroundExecutor,
        service: xray_rpc::client::Service<NetworkProviderService>,
    ) -> Rc<RemoteNetworkProvider> {
        let service_updates = service.updates().unwrap();

        let notify = Rc::new(RefCell::new(NetworkNotify::new()));
        let network_provider = Rc::new(Self {
            service,
            notify: notify.clone(),
        });

        foreground
            .execute(Box::new(service_updates.for_each(move |operation| {
                match operation {
                    RpcUpdate::Operation(op) => notify.borrow_mut().broadcast_op(op),
                };

                Ok(())
            })))
            .unwrap();

        network_provider
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
                        .and_then(|response| match response {
                            RpcResponse::Broadcasted => Ok(()),
                            RpcResponse::Fetch(_) => unreachable!(),
                        })
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
                    RpcResponse::Broadcasted => unreachable!(),
                }),
        )
    }

    fn updates(&self) -> Box<dyn Stream<Item = Operation, Error = ()>> {
        self.notify.borrow_mut().updates()
    }
}

impl NetworkProviderService {
    pub fn new(network_provider: Rc<dyn NetworkProvider>) -> Self {
        let updates = network_provider.updates();
        Self {
            network_provider,
            updates,
        }
    }

    fn poll_outgoing_op(&mut self) -> Async<Option<RpcUpdate>> {
        self.updates
            .poll()
            .expect("Receiving on a channel cannot produce an error")
            .map(|option| option.map(|update| RpcUpdate::Operation(update)))
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
        self.poll_outgoing_op()
    }

    fn request(
        &mut self,
        request: Self::Request,
        _connection: &xray_rpc::server::Connection,
    ) -> Option<Box<dyn Future<Item = Self::Response, Error = Never>>> {
        match request {
            RpcRequest::Broadcast(envelopes) => match envelopes.len() {
                0 => None,
                _ => Some(Box::new(
                    self.network_provider
                        .broadcast(Box::new(stream::iter_ok(envelopes)))
                        .then(|response| match response {
                            Ok(_) => Ok(RpcResponse::Broadcasted),
                            _ => panic!("Can't fail on broadcast"),
                        }),
                )),
            },
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

    use crate::{
        network::{NetworkNotify, NetworkProvider, Operation, OperationEnvelope},
        Error,
    };

    pub struct TestNetworkProvider {
        inner: RefCell<TestNetworkProviderInner>,
        notify: RefCell<NetworkNotify>,
    }

    struct TestNetworkProviderInner {
        envelopes: Vec<OperationEnvelope>,
    }

    impl TestNetworkProvider {
        pub fn new() -> Self {
            Self {
                inner: RefCell::new(TestNetworkProviderInner::new()),
                notify: RefCell::new(NetworkNotify::new()),
            }
        }

        fn broadcast_one(&self, envelope: OperationEnvelope) {
            let mut inner = self.inner.borrow_mut();

            self.notify
                .borrow_mut()
                .broadcast_op(envelope.operation.clone());

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

        fn updates(&self) -> Box<dyn Stream<Item = Operation, Error = ()>> {
            self.notify.borrow_mut().updates()
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
