use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use futures::{stream, Future, Stream};

use never::Never;
use rpc;

pub use memo_core::{DirEntry, GitProvider, Oid};

#[derive(Serialize, Deserialize)]
pub enum RpcRequest {
    BaseEntries { oid: Oid },
    BaseText { oid: Oid, path: PathBuf },
}

#[derive(Serialize, Deserialize)]
pub enum RpcResponse {
    BaseEntries(Vec<DirEntry>),
    BaseText(String),
}

pub struct RemoteGitProvider {
    service: rpc::client::Service<GitProviderService>,
}

pub struct GitProviderService {
    git_provider: Rc<GitProvider>,
}

impl RemoteGitProvider {
    pub fn new(service: rpc::client::Service<GitProviderService>) -> Self {
        Self { service }
    }
}

impl GitProviderService {
    pub fn new(git_provider: Rc<GitProvider>) -> Self {
        Self { git_provider }
    }
}

impl GitProvider for RemoteGitProvider {
    fn base_entries(&self, oid: Oid) -> Box<Stream<Item = DirEntry, Error = io::Error>> {
        Box::new(
            self.service
                .request(RpcRequest::BaseEntries { oid })
                .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("{}", err)))
                .and_then(|response| match response {
                    RpcResponse::BaseEntries(dir_entries) => Ok(stream::iter_ok(dir_entries)),
                    _ => panic!("RemoteGitProvider.base_entries() can't error here"),
                })
                .flatten_stream(),
        )
    }

    fn base_text(&self, oid: Oid, path: &Path) -> Box<Future<Item = String, Error = io::Error>> {
        Box::new(
            self.service
                .request(RpcRequest::BaseText {
                    oid,
                    path: path.to_path_buf(),
                })
                .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("{}", err)))
                .and_then(|response| match response {
                    RpcResponse::BaseText(text) => Ok(text),
                    _ => panic!("RemoteGitProvider.base_text() can't error here",),
                }),
        )
    }
}

impl rpc::server::Service for GitProviderService {
    type State = ();
    type Update = ();
    type Request = RpcRequest;
    type Response = RpcResponse;

    fn init(&mut self, _connection: &rpc::server::Connection) -> () {
        ()
    }

    fn request(
        &mut self,
        request: Self::Request,
        _connection: &rpc::server::Connection,
    ) -> Option<Box<Future<Item = Self::Response, Error = Never>>> {
        match request {
            RpcRequest::BaseEntries { oid } => Some(Box::new(
                self.git_provider
                    .base_entries(oid)
                    .collect()
                    .then(|response| match response {
                        Ok(entries) => Ok(RpcResponse::BaseEntries(entries)),
                        _ => panic!("Can't retrieve an error"),
                    }),
            )),
            RpcRequest::BaseText { oid, path } => {
                Some(Box::new(self.git_provider.base_text(oid, &path).then(
                    |response| match response {
                        Ok(text) => Ok(RpcResponse::BaseText(text)),
                        _ => panic!("Can't retrieve an error"),
                    },
                )))
            }
        }
    }
}
