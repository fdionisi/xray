use std::io;
use std::iter::Iterator;
use std::path::PathBuf;
use std::sync::Arc;

use futures::Future;
pub use memo_core::{DirEntry, FileStatus, FileType};
use parking_lot::RwLock;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub type EntryId = usize;

pub trait File {
    fn write_utf16(&self, snapshot: Vec<u16>) -> Box<dyn Future<Item = (), Error = io::Error>>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Entry {
    #[serde(serialize_with = "serialize_dir", deserialize_with = "deserialize_dir")]
    Dir(Arc<DirInner>),
    #[serde(
        serialize_with = "serialize_file",
        deserialize_with = "deserialize_file"
    )]
    File(Arc<FileInner>),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DirInner {
    name: PathBuf,
    #[serde(skip_serializing, skip_deserializing)]
    name_chars: Vec<char>,
    #[serde(serialize_with = "serialize_dir_children")]
    #[serde(deserialize_with = "deserialize_dir_children")]
    children: RwLock<Arc<Vec<Entry>>>,
    symlink: bool,
    ignored: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileInner {
    name: PathBuf,
    #[serde(skip_serializing, skip_deserializing)]
    name_chars: Vec<char>,
    symlink: bool,
    ignored: bool,
}

impl Entry {
    pub fn file(name: PathBuf, symlink: bool, ignored: bool) -> Self {
        Entry::File(Arc::new(FileInner {
            name_chars: name.to_string_lossy().chars().collect(),
            name,
            symlink,
            ignored,
        }))
    }

    pub fn dir(name: PathBuf, symlink: bool, ignored: bool) -> Self {
        let mut name_chars: Vec<char> = name.to_string_lossy().chars().collect();
        name_chars.push('/');
        Entry::Dir(Arc::new(DirInner {
            name_chars,
            name,
            children: RwLock::new(Arc::new(Vec::new())),
            symlink,
            ignored,
        }))
    }

    pub fn is_dir(&self) -> bool {
        match self {
            &Entry::Dir(_) => true,
            &Entry::File(_) => false,
        }
    }

    pub fn id(&self) -> EntryId {
        match self {
            &Entry::Dir(ref inner) => inner.as_ref() as *const DirInner as EntryId,
            &Entry::File(ref inner) => inner.as_ref() as *const FileInner as EntryId,
        }
    }

    pub fn name(&self) -> &PathBuf {
        match self {
            &Entry::Dir(ref inner) => &inner.name,
            &Entry::File(ref inner) => &inner.name,
        }
    }

    pub fn name_chars(&self) -> &[char] {
        match self {
            &Entry::Dir(ref inner) => &inner.name_chars,
            &Entry::File(ref inner) => &inner.name_chars,
        }
    }

    pub fn is_symlink(&self) -> bool {
        match self {
            &Entry::Dir(ref inner) => inner.symlink,
            &Entry::File(ref inner) => inner.symlink,
        }
    }

    pub fn is_ignored(&self) -> bool {
        match self {
            &Entry::Dir(ref inner) => inner.ignored,
            &Entry::File(ref inner) => inner.ignored,
        }
    }

    pub fn children(&self) -> Option<Arc<Vec<Entry>>> {
        match self {
            &Entry::Dir(ref inner) => Some(inner.children.read().clone()),
            &Entry::File(..) => None,
        }
    }

    pub fn insert(&self, new_entry: Entry) -> Result<(), ()> {
        match self {
            &Entry::Dir(ref inner) => {
                let mut children = inner.children.write();
                let children = Arc::make_mut(&mut children);
                if children
                    .last()
                    .map(|child| child.name() < new_entry.name())
                    .unwrap_or(true)
                {
                    children.push(new_entry);
                    Ok(())
                } else {
                    let index = {
                        let new_name = new_entry.name();
                        match children.binary_search_by(|child| child.name().cmp(new_name)) {
                            Ok(_) => return Err(()), // An entry already exists with this name
                            Err(index) => index,
                        }
                    };
                    children.insert(index, new_entry);
                    Ok(())
                }
            }
            &Entry::File(_) => Err(()),
        }
    }
}

fn serialize_dir<S: Serializer>(dir: &Arc<DirInner>, serializer: S) -> Result<S::Ok, S::Error> {
    dir.serialize(serializer)
}

fn deserialize_dir<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Arc<DirInner>, D::Error> {
    let mut inner = DirInner::deserialize(deserializer)?;

    let mut name_chars: Vec<char> = inner.name.to_string_lossy().chars().collect();
    name_chars.push('/');
    inner.name_chars = name_chars;

    Ok(Arc::new(inner))
}

fn serialize_file<S: Serializer>(file: &Arc<FileInner>, serializer: S) -> Result<S::Ok, S::Error> {
    file.serialize(serializer)
}

fn deserialize_file<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Arc<FileInner>, D::Error> {
    let mut inner = FileInner::deserialize(deserializer)?;
    inner.name_chars = inner.name.to_string_lossy().chars().collect();
    Ok(Arc::new(inner))
}

fn serialize_dir_children<S: Serializer>(
    children: &RwLock<Arc<Vec<Entry>>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    children.read().serialize(serializer)
}

fn deserialize_dir_children<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<RwLock<Arc<Vec<Entry>>>, D::Error> {
    Ok(RwLock::new(Arc::new(Vec::deserialize(deserializer)?)))
}

#[cfg(test)]
pub(crate) mod tests {
    // use super::*;
    // use bincode::{deserialize, serialize};
    // use cross_platform::PathComponent;
    // use futures::{future, task, Async, IntoFuture, Poll};
    // use never::Never;
    // use notify_cell::NotifyCell;
    // use rpc;
    // use std::collections::HashMap;
    // use std::ffi::OsString;
    // use std::path::PathBuf;
    // use stream_ext::StreamExt;
    // use tokio_core::reactor;
    //
    // #[test]
    // fn test_insert() {
    //     let root = Entry::dir(PathComponent::from("root"), false, false);
    //     assert_eq!(
    //         root.insert(Entry::file(PathComponent::from("a"), false, false)),
    //         Ok(())
    //     );
    //     assert_eq!(
    //         root.insert(Entry::file(PathComponent::from("c"), false, false)),
    //         Ok(())
    //     );
    //     assert_eq!(
    //         root.insert(Entry::file(PathComponent::from("b"), false, false)),
    //         Ok(())
    //     );
    //     assert_eq!(
    //         root.insert(Entry::file(PathComponent::from("a"), false, false)),
    //         Err(())
    //     );
    //     assert_eq!(root.child_names(), vec!["a", "b", "c"]);
    // }
    //
    // #[test]
    // fn test_serialize_deserialize() {
    //     let root = Entry::from_json(
    //         "root",
    //         &json!({
    //             "child-1": {
    //                 "subchild-1-1": null
    //             },
    //             "child-2": null,
    //             "child-3": {
    //                 "subchild-3-1": {
    //                     "subchild-3-1-1": null,
    //                     "subchild-3-1-2": null,
    //                 }
    //             }
    //         }),
    //     );
    //     assert_eq!(
    //         deserialize::<Entry>(&serialize(&root).unwrap()).unwrap(),
    //         root
    //     );
    // }
    //
    // #[test]
    // fn test_tree_replication() {
    //     let mut reactor = reactor::Core::new().unwrap();
    //     let handle = Rc::new(reactor.handle());
    //
    //     let local_tree = Rc::new(TestTree::new(
    //         "/foo/bar",
    //         Entry::from_json(
    //             "root",
    //             &json!({
    //                 "child-1": {
    //                     "subchild": null
    //                 },
    //                 "child-2": null,
    //             }),
    //         ),
    //     ));
    //     let remote_tree = RemoteTree::new(
    //         handle,
    //         rpc::tests::connect(&mut reactor, TreeService::new(local_tree.clone())),
    //     );
    //     assert_eq!(remote_tree.root().name(), local_tree.root().name());
    //     assert_eq!(remote_tree.root().children().unwrap().len(), 0);
    //
    //     let mut remote_tree_updates = remote_tree.updates();
    //     local_tree.populated.set(true);
    //     remote_tree_updates.wait_next(&mut reactor);
    //     assert_eq!(remote_tree.root(), local_tree.root());
    // }
    //
    // pub struct TestTree {
    //     path: cross_platform::Path,
    //     root: Entry,
    //     pub populated: NotifyCell<bool>,
    // }
    //
    // pub struct TestFileProvider(Rc<RefCell<TestFileProviderState>>);
    //
    // struct TestFileProviderState {
    //     next_file_id: FileId,
    //     files: HashMap<PathBuf, TestFile>,
    // }
    //
    // #[derive(Clone)]
    // struct TestFile(Rc<RefCell<TestFileState>>);
    //
    // struct TestFileState {
    //     id: FileId,
    //     content: String,
    // }
    //
    // struct NextTick(bool);
    //
    // impl TestTree {
    //     pub fn new<T: Into<OsString>>(path: T, root: Entry) -> Self {
    //         Self {
    //             path: cross_platform::Path::from(path.into()),
    //             root,
    //             populated: NotifyCell::new(false),
    //         }
    //     }
    //
    //     pub fn from_json<T: Into<PathBuf>>(path: T, json: serde_json::Value) -> Self {
    //         let path = path.into();
    //         let root = Entry::from_json(PathComponent::from(path.file_name().unwrap()), &json);
    //         Self::new(path, root)
    //     }
    // }
    //
    // impl Tree for TestTree {
    //     fn root(&self) -> Entry {
    //         self.root.clone()
    //     }
    //
    //     fn updates(&self) -> Box<Stream<Item = (), Error = ()>> {
    //         unimplemented!()
    //     }
    // }
    //
    // impl LocalTree for TestTree {
    //     fn path(&self) -> &cross_platform::Path {
    //         &self.path
    //     }
    //
    //     fn populated(&self) -> Box<Future<Item = (), Error = ()>> {
    //         if self.populated.get() {
    //             Box::new(future::ok(()))
    //         } else {
    //             Box::new(
    //                 self.populated
    //                     .observe()
    //                     .skip_while(|p| Ok(!p))
    //                     .into_future()
    //                     .then(|_| Ok(())),
    //             )
    //         }
    //     }
    //
    //     fn as_tree(&self) -> &Tree {
    //         self
    //     }
    // }
    //
    // impl Entry {
    //     fn from_json<T: Into<PathComponent>>(name: T, json: &serde_json::Value) -> Self {
    //         if json.is_object() {
    //             let object = json.as_object().unwrap();
    //             let dir = Entry::dir(name.into(), false, false);
    //             for (key, value) in object {
    //                 let child_entry = Self::from_json(key.as_str(), value);
    //                 assert_eq!(dir.insert(child_entry), Ok(()));
    //             }
    //             dir
    //         } else {
    //             Entry::file(name.into(), false, false)
    //         }
    //     }
    //
    //     fn child_names(&self) -> Vec<String> {
    //         match self {
    //             &Entry::Dir(ref inner) => inner
    //                 .children
    //                 .read()
    //                 .iter()
    //                 .map(|ref entry| entry.name().to_string_lossy().into_owned())
    //                 .collect(),
    //             _ => panic!(),
    //         }
    //     }
    // }
    //
    // impl PartialEq for Entry {
    //     fn eq(&self, other: &Self) -> bool {
    //         self.name() == other.name() && self.name_chars() == other.name_chars()
    //             && self.is_dir() == other.is_dir()
    //             && self.is_ignored() == other.is_ignored()
    //             && self.children() == other.children()
    //     }
    // }
    //
    // impl TestFileProvider {
    //     pub fn new() -> Self {
    //         TestFileProvider(Rc::new(RefCell::new(TestFileProviderState {
    //             next_file_id: 0,
    //             files: HashMap::new(),
    //         })))
    //     }
    //
    //     pub fn write_sync<S: Into<String>>(&self, path: cross_platform::Path, content: S) {
    //         let mut state = self.0.borrow_mut();
    //
    //         let file_id = state.next_file_id;
    //         state.next_file_id += 1;
    //
    //         state.files.insert(
    //             path.to_path_buf(),
    //             TestFile(Rc::new(RefCell::new(TestFileState {
    //                 id: file_id,
    //                 content: content.into(),
    //             }))),
    //         );
    //     }
    // }
    //
    // impl FileProvider for TestFileProvider {
    //     fn open(
    //         &self,
    //         path: &cross_platform::Path,
    //     ) -> Box<Future<Item = Box<File>, Error = io::Error>> {
    //         let path = path.to_path_buf();
    //         let state = self.0.clone();
    //         Box::new(NextTick::new().then(move |_| {
    //             let state = state.borrow();
    //             state
    //                 .files
    //                 .get(&path)
    //                 .map(|file| Box::new(file.clone()) as Box<File>)
    //                 .ok_or(io::Error::new(io::ErrorKind::NotFound, "Path not found"))
    //                 .into_future()
    //         }))
    //     }
    // }
    //
    // impl File for TestFile {
    //     fn id(&self) -> FileId {
    //         self.0.borrow().id
    //     }
    //
    //     fn read(&self) -> Box<Future<Item = String, Error = io::Error>> {
    //         let file = self.0.clone();
    //         Box::new(NextTick::new().then(move |_| {
    //             let file = file.borrow();
    //             future::ok(file.content.clone())
    //         }))
    //     }
    //
    //     fn write_snapshot(
    //         &self,
    //         snapshot: BufferSnapshot,
    //     ) -> Box<Future<Item = (), Error = io::Error>> {
    //         let file = self.0.clone();
    //         Box::new(NextTick::new().then(move |_| {
    //             let mut file = file.borrow_mut();
    //             file.content = snapshot.to_string();
    //             future::ok(())
    //         }))
    //     }
    // }
    //
    // impl NextTick {
    //     fn new() -> Self {
    //         NextTick(false)
    //     }
    // }
    //
    // impl Future for NextTick {
    //     type Item = ();
    //     type Error = Never;
    //
    //     fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
    //         if self.0 {
    //             Ok(Async::Ready(()))
    //         } else {
    //             self.0 = true;
    //             task::current().notify();
    //             Ok(Async::NotReady)
    //         }
    //     }
    // }
}
