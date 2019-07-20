use std::cell::RefCell;
use std::path::PathBuf;

use futures::{future, Future, Stream};
use xray_core::network::{self, Operation, OperationEnvelope};
use xray_core::notify_cell::NotifyCell;
use xray_core::Error;

pub struct NetworkProvider {
    pub path: PathBuf,
    inner: RefCell<NetworkProviderInner>,
    updates: NotifyCell<Option<Operation>>,
}

struct NetworkProviderInner {
    envelopes: Vec<OperationEnvelope>,
}

impl NetworkProvider {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            inner: RefCell::new(NetworkProviderInner::new()),
            updates: NotifyCell::new(None),
        }
    }

    fn broadcast_one(&self, envelope: OperationEnvelope) {
        let mut inner = self.inner.borrow_mut();

        self.updates.set(Some(envelope.operation.clone()));

        inner.insert(envelope);
    }
}

impl network::NetworkProvider for NetworkProvider {
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

impl NetworkProviderInner {
    fn new() -> Self {
        NetworkProviderInner { envelopes: vec![] }
    }

    fn insert(&mut self, envelope: OperationEnvelope) {
        self.envelopes.push(envelope)
    }
}
