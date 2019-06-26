use futures::Stream;
use memo_core;
pub use memo_core::{Operation, OperationEnvelope};

use rpc;
use ReplicaId;

pub trait Storage {
    fn broadcast(
        &self,
        replica_id: ReplicaId,
        operations: Box<Stream<Item = OperationEnvelope, Error = memo_core::Error>>,
    );

    fn fetch(&self) -> Vec<Operation>;
}

pub struct RemoteStorage;

impl Storage for RemoteStorage {
    fn broadcast(
        &self,
        replica_id: ReplicaId,
        operations: Box<Stream<Item = OperationEnvelope, Error = memo_core::Error>>,
    ) {
        unimplemented!()
    }

    fn fetch(&self) -> Vec<Operation> {
        unimplemented!()
    }
}

pub enum RpcRequest {
    Broadcast(Vec<OperationEnvelope>),
    Fetch,
}

pub enum RpcResponse {
    Fetch(Vec<Operation>),
}

pub enum RpcUpdate {
    Operation(Operation),
}

pub struct StorageService {}

impl rpc::server::Service for StorageService {
    type State = ();
    type Update = ();
    type Request = ();
    type Response = ();

    fn init(&mut self, connection: &rpc::server::Connection) -> Self::State {
        ()
    }
}
