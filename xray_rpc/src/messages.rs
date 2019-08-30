use std::collections::{HashMap, HashSet};

use bytes::Bytes;

use super::Error;

pub type RequestId = usize;
pub type ServiceId = usize;

#[derive(Serialize, Deserialize)]
pub enum MessageToClient {
    Update {
        insertions: HashMap<ServiceId, Bytes>,
        updates: HashMap<ServiceId, Vec<Bytes>>,
        removals: HashSet<ServiceId>,
        responses: HashMap<ServiceId, Vec<(RequestId, Response)>>,
    },
}

pub type Response = Result<Bytes, Error>;

#[derive(Debug, Serialize, Deserialize)]
pub enum MessageToServer {
    Request {
        service_id: ServiceId,
        request_id: RequestId,
        payload: Bytes,
    },
    DroppedService(ServiceId),
}
