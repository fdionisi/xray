use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use futures::{stream, Future, Stream};
pub use memo_core::{GitProvider, Oid};

use crate::fs::DirEntry;
use crate::never::Never;

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
    service: xray_rpc::client::Service<GitProviderService>,
}

pub struct GitProviderService {
    git_provider: Rc<dyn GitProvider>,
}

impl RemoteGitProvider {
    pub fn new(service: xray_rpc::client::Service<GitProviderService>) -> Self {
        Self { service }
    }
}

impl GitProviderService {
    pub fn new(git_provider: Rc<dyn GitProvider>) -> Self {
        Self { git_provider }
    }
}

impl GitProvider for RemoteGitProvider {
    fn base_entries(&self, oid: Oid) -> Box<dyn Stream<Item = DirEntry, Error = io::Error>> {
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

    fn base_text(
        &self,
        oid: Oid,
        path: &Path,
    ) -> Box<dyn Future<Item = String, Error = io::Error>> {
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

impl xray_rpc::server::Service for GitProviderService {
    type State = ();
    type Update = ();
    type Request = RpcRequest;
    type Response = RpcResponse;

    fn init(&mut self, _connection: &xray_rpc::server::Connection) -> () {
        ()
    }

    fn request(
        &mut self,
        request: Self::Request,
        _connection: &xray_rpc::server::Connection,
    ) -> Option<Box<dyn Future<Item = Self::Response, Error = Never>>> {
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

#[cfg(test)]
pub mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::str::FromStr;

    use futures::{future, stream, Future, Stream};

    use super::*;
    use crate::fs::FileType;

    #[derive(Clone)]
    pub struct BaseEntry(DirEntry, Option<String>);

    pub struct TestGitProvider {
        inner: RefCell<TestGitProviderInner>,
        next_oid: RefCell<u64>,
    }

    struct TestGitProviderInner {
        entries: HashMap<Oid, Vec<BaseEntry>>,
        text: HashMap<Oid, HashMap<PathBuf, Option<String>>>,
    }

    impl BaseEntry {
        pub fn file(depth: usize, name: &'static str, text: &'static str) -> Self {
            BaseEntry(
                DirEntry {
                    depth,
                    name: OsString::from(name),
                    file_type: FileType::Text,
                },
                Some(String::from_str(text).unwrap()),
            )
        }

        pub fn dir(depth: usize, name: &'static str) -> Self {
            BaseEntry(
                DirEntry {
                    depth,
                    name: OsString::from(name),
                    file_type: FileType::Directory,
                },
                None,
            )
        }
    }

    impl TestGitProvider {
        pub fn new() -> Self {
            TestGitProvider {
                inner: RefCell::new(TestGitProviderInner::new()),
                next_oid: RefCell::new(0),
            }
        }

        pub fn commit(&self, oid: Oid, entries: Vec<BaseEntry>) {
            let mut inner = self.inner.borrow_mut();
            inner.entries.insert(oid, entries.clone());

            let mut text_by_path: HashMap<PathBuf, Option<String>> = HashMap::default();

            let mut path: Vec<String> = vec![];
            for entry in entries.iter() {
                path.truncate(entry.0.depth - 1);
                path.push(entry.0.name.to_string_lossy().to_owned().to_string());
                let BaseEntry(dir_entry, string) = entry;
                let string = string.clone();
                if dir_entry.file_type == FileType::Text {
                    text_by_path.insert(PathBuf::from(path.join("/")), string);
                }
            }

            inner.text.insert(oid, text_by_path);
        }

        pub fn gen_oid(&self) -> Oid {
            let mut next_oid = self.next_oid.borrow_mut();
            let mut oid = [0; 20];
            oid[0] = (*next_oid >> 0) as u8;
            oid[1] = (*next_oid >> 8) as u8;
            oid[2] = (*next_oid >> 16) as u8;
            oid[3] = (*next_oid >> 24) as u8;
            oid[4] = (*next_oid >> 32) as u8;
            oid[5] = (*next_oid >> 40) as u8;
            oid[6] = (*next_oid >> 48) as u8;
            oid[7] = (*next_oid >> 56) as u8;
            *next_oid += 1;
            oid
        }
    }

    impl GitProvider for TestGitProvider {
        fn base_entries(&self, oid: Oid) -> Box<dyn Stream<Item = DirEntry, Error = io::Error>> {
            let inner = self.inner.borrow();
            let entries = inner.entries.get(&oid);

            match entries {
                Some(e) => Box::new(stream::iter_ok(
                    e.iter()
                        .map(|entry| entry.0.clone())
                        .collect::<Vec<DirEntry>>(),
                )),
                _ => panic!("yy"),
            }
        }

        fn base_text(
            &self,
            oid: Oid,
            path: &Path,
        ) -> Box<dyn Future<Item = String, Error = io::Error>> {
            let inner = self.inner.borrow();
            let text_by_path = inner.text.get(&oid);

            if let Some(tbp) = text_by_path {
                let text = tbp.get(path);
                if let Some(Some(t)) = text {
                    return Box::new(future::ok(t.clone()));
                } else {
                    panic!("No text found at path {:?}", path);
                }
            } else {
                panic!("No commit found with oid {:?}", oid);
            }
        }
    }

    impl TestGitProviderInner {
        fn new() -> Self {
            Self {
                entries: HashMap::default(),
                text: HashMap::default(),
            }
        }
    }
}
