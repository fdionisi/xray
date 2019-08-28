use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;

use futures::{stream, Future, Stream};
pub use memo_core::{self, EpochId, Version};

use crate::buffer::{Buffer, BufferId, Point, SelectionRanges, SelectionSetId};
use crate::change_observer::ChangeObserver;
use crate::fs::{FileStatus, FileType};
use crate::git::{GitProvider, Oid};
use crate::network::{NetworkProvider, Operation, OperationEnvelope};
use crate::weak_set::WeakSet;
use crate::{Error, ForegroundExecutor, ReplicaId};

pub struct WorkTree {
    foreground: ForegroundExecutor,
    tree: Rc<RefCell<memo_core::WorkTree>>,
    handle: Rc<BufferHandler>,
    observer: Rc<ChangeObserver>,
    buffers: Rc<RefCell<WeakSet<Buffer>>>,
    pub(crate) network: Rc<dyn NetworkProvider>,
    pub(crate) git: Rc<dyn GitProvider>,
    // history: HashMap<u32, Operation>,
}

pub(crate) struct BufferHandler {
    foreground: ForegroundExecutor,
    tree: Rc<RefCell<memo_core::WorkTree>>,
    network: Rc<dyn NetworkProvider>,
    // history: HashMap<u32, Operation>,
}

pub struct Entry {
    pub file_type: FileType,
    pub depth: usize,
    pub name: String,
    pub path: String,
    pub base_path: Option<String>,
    pub status: FileStatus,
    pub visible: bool,
}

impl WorkTree {
    pub fn new(
        foreground: ForegroundExecutor,
        replica_id: ReplicaId,
        base: Option<Oid>,
        git: Rc<dyn GitProvider>,
        network: Rc<dyn NetworkProvider>,
    ) -> impl Future<Item = Rc<WorkTree>, Error = Error> {
        network.fetch().and_then(move |ops| {
            let observer = Rc::new(ChangeObserver::new());
            let (tree, operations) = memo_core::WorkTree::new(
                replica_id,
                base,
                ops,
                git.clone(),
                Some(observer.clone()),
            )?;

            let tree = Rc::new(RefCell::new(tree));
            let handle = Rc::new(BufferHandler::new(
                foreground.clone(),
                tree.clone(),
                network.clone(),
            ));

            let work_tree = Rc::new(Self {
                foreground: foreground.clone(),
                tree,
                observer,
                buffers: Rc::new(RefCell::new(WeakSet::new())),
                network: network.clone(),
                handle,
                git,
            });

            let work_tree_updater = work_tree.clone();
            let network_updates = network.updates();
            foreground
                .execute(Box::new(network_updates.for_each(move |operation| {
                    work_tree_updater.apply_ops(vec![operation]);

                    Ok(())
                })))
                .unwrap();

            work_tree.broadcast_stream(Box::new(operations.map_err(|err| Error::from(err))));

            Ok(work_tree)
        })
    }

    pub fn new_sync(
        foreground: ForegroundExecutor,
        replica_id: ReplicaId,
        base: Option<Oid>,
        git: Rc<dyn GitProvider>,
        network: Rc<dyn NetworkProvider>,
    ) -> Result<Rc<WorkTree>, Error> {
        WorkTree::new(foreground, replica_id, base, git, network).wait()
    }

    pub fn version(&self) -> Version {
        self.tree.borrow().version()
    }

    pub fn observed(&self, other: Version) -> bool {
        self.tree.borrow().observed(other)
    }

    pub fn head(&self) -> Option<Oid> {
        self.tree.borrow().head()
    }

    pub fn epoch_id(&self) -> EpochId {
        self.tree.borrow().epoch_id()
    }

    pub fn reset(&self, head: Option<Oid>) {
        self.broadcast_stream(Box::new(
            self.tree
                .borrow_mut()
                .reset(head)
                .map_err(|err| Error::from(err)),
        ));
    }

    fn apply_ops(&self, ops: Vec<Operation>) {
        let envelopes_stream = self.tree.borrow_mut().apply_ops(ops).unwrap();
        self.broadcast_stream(Box::new(envelopes_stream.map_err(|err| Error::from(err))));
    }

    pub fn create_file(&self, path: PathBuf, file_type: FileType) -> Result<(), Error> {
        let envelope = self.tree.borrow().create_file(path, file_type)?;

        self.broadcast(vec![envelope]);

        Ok(())
    }

    pub fn rename(&self, old_path: PathBuf, new_path: PathBuf) -> Result<(), Error> {
        let envelope = self.tree.borrow().rename(old_path, new_path)?;

        self.broadcast(vec![envelope]);

        Ok(())
    }

    pub fn remove(&self, path: PathBuf) -> Result<(), Error> {
        let envelope = self.tree.borrow().remove(path)?;

        self.broadcast(vec![envelope]);

        Ok(())
    }

    pub fn exists(&self, path: PathBuf) -> bool {
        self.tree.borrow().exists(path)
    }

    pub fn entries(
        &self,
        descend_into: Option<HashSet<PathBuf>>,
        show_deleted: Option<bool>,
    ) -> Vec<Entry> {
        let mut entries = Vec::new();
        let show_deleted = show_deleted.unwrap_or(false);

        self.tree.borrow().with_cursor(|cursor| loop {
            let entry = cursor.entry().unwrap();
            let mut descend = false;
            if show_deleted || entry.status != FileStatus::Removed {
                let path = cursor.path().unwrap();
                let base_path = cursor.base_path().unwrap();
                entries.push(Entry {
                    file_type: entry.file_type,
                    depth: entry.depth,
                    name: entry.name.to_string_lossy().into_owned(),
                    path: path.to_string_lossy().into_owned(),
                    base_path: base_path.map(|p| p.to_string_lossy().into_owned()),
                    status: entry.status,
                    visible: entry.visible,
                });
                descend = descend_into.as_ref().map_or(true, |d| d.contains(path));
            }

            if !cursor.next(descend) {
                break;
            }
        });

        entries
    }

    pub fn root(&self) -> crate::fs::Entry {
        let root = crate::fs::Entry::dir(PathBuf::from("/"), false, false);
        let mut stack = vec![root.clone()];

        let entries = self.entries(None, Some(false));
        entries.iter().for_each(|entry| {
            stack.truncate(entry.depth);

            match entry.file_type {
                memo_core::FileType::Directory => {
                    let dir =
                        crate::fs::Entry::dir(PathBuf::from(&entry.name), false, !entry.visible);
                    stack.last_mut().unwrap().insert(dir.clone()).unwrap();
                    stack.push(dir);
                }
                memo_core::FileType::Text => {
                    let file =
                        crate::fs::Entry::file(PathBuf::from(&entry.name), false, !entry.visible);
                    stack.last_mut().unwrap().insert(file).unwrap();
                }
            }
        });

        root
    }

    pub fn open_text_file(
        &self,
        path: &PathBuf,
    ) -> Box<dyn Future<Item = Rc<Buffer>, Error = Error>> {
        let handle = self.handle.clone();
        let observer = self.observer.clone();
        let buffers = self.buffers.clone();
        Box::new(
            self.tree
                .borrow()
                .open_text_file(path)
                .map_err(|err| Error::from(err))
                .and_then(move |buffer_id| {
                    let mut buffers = buffers.borrow_mut();
                    let buffer = if let Some(buffer) =
                        buffers.find(&mut |buf: Rc<Buffer>| buf.id() == buffer_id)
                    {
                        buffer.clone()
                    } else {
                        buffers.insert(Buffer::new(buffer_id, handle, observer))
                    };

                    Ok(buffer)
                }),
        )
    }

    pub fn set_active_location(&self, buffer: Option<Rc<Buffer>>) -> Result<(), Error> {
        let envelope = self
            .tree
            .borrow()
            .set_active_location(buffer.map(|buffer| buffer.id()))?;

        self.broadcast(vec![envelope]);

        Ok(())
    }

    pub fn replica_locations(&self) -> HashMap<ReplicaId, PathBuf> {
        self.tree.borrow().replica_locations()
    }

    fn broadcast(&self, envelopes: Vec<OperationEnvelope>) {
        self.foreground
            .execute(Box::new(
                self.network
                    .broadcast(Box::new(stream::iter_ok(envelopes)))
                    .map_err(|_| ()),
            ))
            .unwrap();
    }

    fn broadcast_stream(
        &self,
        envelopes: Box<dyn Stream<Item = OperationEnvelope, Error = Error>>,
    ) {
        self.foreground
            .execute(Box::new(self.network.broadcast(envelopes).map_err(|_| ())))
            .unwrap();
    }
}

impl BufferHandler {
    fn new(
        foreground: ForegroundExecutor,
        tree: Rc<RefCell<memo_core::WorkTree>>,
        network: Rc<dyn NetworkProvider>,
    ) -> Self {
        Self {
            foreground,
            tree,
            network,
        }
    }

    pub fn edit_2d<T: AsRef<str>>(
        &self,
        buffer_id: BufferId,
        old_ranges: Vec<Range<Point>>,
        new_text: T,
    ) -> Result<(), Error> {
        let envelope = self
            .tree
            .borrow()
            .edit_2d(buffer_id, old_ranges, new_text.as_ref())?;

        self.broadcast(vec![envelope]);

        Ok(())
    }

    pub fn edit<T: AsRef<str>>(
        &self,
        buffer_id: BufferId,
        old_ranges: Vec<Range<usize>>,
        new_text: T,
    ) -> Result<(), Error> {
        let envelope = self
            .tree
            .borrow()
            .edit(buffer_id, old_ranges, new_text.as_ref())?;

        self.broadcast(vec![envelope]);

        Ok(())
    }

    pub fn add_selection_set(
        &self,
        buffer_id: BufferId,
        ranges: Vec<Range<Point>>,
    ) -> Result<SelectionSetId, Error> {
        let (set_id, envelope) = self.tree.borrow().add_selection_set(buffer_id, ranges)?;

        self.broadcast(vec![envelope]);

        Ok(set_id)
    }

    pub fn replace_selection_set(
        &self,
        buffer_id: BufferId,
        set_id: SelectionSetId,
        ranges: Vec<Range<Point>>,
    ) -> Result<(), Error> {
        let envelope = self
            .tree
            .borrow()
            .replace_selection_set(buffer_id, set_id, ranges)?;

        self.broadcast(vec![envelope]);

        Ok(())
    }

    pub fn remove_selection_set(
        &self,
        buffer_id: BufferId,
        set_id: SelectionSetId,
    ) -> Result<(), Error> {
        let envelope = self.tree.borrow().remove_selection_set(buffer_id, set_id)?;

        self.broadcast(vec![envelope]);

        Ok(())
    }

    pub fn path(&self, buffer_id: BufferId) -> Option<PathBuf> {
        self.tree.borrow().path(buffer_id)
    }

    pub fn text(&self, buffer_id: BufferId) -> Result<Vec<u16>, Error> {
        Ok(self
            .tree
            .borrow()
            .text(buffer_id)
            .map(|text| text.collect::<Vec<u16>>())?)
    }

    pub fn selection_ranges(&self, buffer_id: BufferId) -> Result<SelectionRanges, Error> {
        Ok(self.tree.borrow().selection_ranges(buffer_id)?)
    }

    pub fn buffer_deferred_ops_len(&self, buffer_id: BufferId) -> Result<usize, Error> {
        Ok(self.tree.borrow().buffer_deferred_ops_len(buffer_id)?)
    }

    pub fn len(&self, buffer_id: BufferId) -> Result<usize, Error> {
        Ok(self.tree.borrow().len(buffer_id)?)
    }

    pub fn len_for_row(&self, buffer_id: BufferId, row: u32) -> Result<u32, Error> {
        Ok(self.tree.borrow().len_for_row(buffer_id, row)?)
    }

    pub fn longest_row(&self, buffer_id: BufferId) -> Result<u32, Error> {
        Ok(self.tree.borrow().longest_row(buffer_id)?)
    }

    pub fn max_point(&self, buffer_id: BufferId) -> Result<Point, Error> {
        Ok(self.tree.borrow().max_point(buffer_id)?)
    }

    pub fn line(&self, buffer_id: BufferId, row: u32) -> Result<Vec<u16>, Error> {
        Ok(self.tree.borrow().line(buffer_id, row)?)
    }

    pub fn iter_at_point(
        &self,
        buffer_id: BufferId,
        point: Point,
    ) -> Result<impl Iterator<Item = u16>, Error> {
        Ok(self.tree.borrow().iter_at_point(buffer_id, point)?)
    }

    pub fn backward_iter_at_point(
        &self,
        buffer_id: BufferId,
        point: Point,
    ) -> Result<impl Iterator<Item = u16>, Error> {
        let iter_at_point = self.tree.borrow().iter_at_point(buffer_id, point)?;
        Ok(iter_at_point.rev())
    }

    fn broadcast(&self, envelopes: Vec<OperationEnvelope>) {
        self.foreground
            .execute(Box::new(
                self.network
                    .broadcast(Box::new(stream::iter_ok(envelopes)))
                    .map_err(|_| ()),
            ))
            .unwrap();
    }
}

#[cfg(test)]
pub mod tests {
    use std::path::PathBuf;
    use std::rc::Rc;

    use futures::Future;
    use tokio_core::reactor;

    use super::*;

    use crate::{
        buffer::Point,
        git::Oid,
        tests::git::{BaseEntry, TestGitProvider},
        tests::network::TestNetworkProvider,
        Error,
    };

    #[test]
    fn basic_api_interaction() -> Result<(), Error> {
        let reactor = reactor::Core::new().unwrap();
        let handle = Rc::new(reactor.handle());

        let git = Rc::new(TestGitProvider::new());

        let oid_0 = git.gen_oid();
        let oid_1 = git.gen_oid();

        git.commit(
            oid_0,
            vec![
                BaseEntry::dir(1, "a"),
                BaseEntry::dir(2, "b"),
                BaseEntry::file(3, "c", "oid0 base text"),
                BaseEntry::dir(3, "d"),
            ],
        );
        git.commit(
            oid_1,
            vec![
                BaseEntry::dir(1, "a"),
                BaseEntry::dir(2, "b"),
                BaseEntry::file(3, "c", "oid1 base text"),
            ],
        );

        let network = Rc::new(TestNetworkProvider::new());

        let tree1 = WorkTree::new_sync(
            handle.clone(),
            uuid::Uuid::from_u128(0),
            Some(oid_0),
            git.clone(),
            network.clone(),
        )?;

        let tree2 = WorkTree::new_sync(
            handle.clone(),
            uuid::Uuid::from_u128(1),
            Some(oid_0),
            git.clone(),
            network.clone(),
        )?;

        assert_eq!(tree1.head(), Some(oid_0));
        assert_eq!(tree2.head(), Some(oid_0));

        tree1.create_file(PathBuf::from("e"), FileType::Text)?;
        tree2.create_file(PathBuf::from("f"), FileType::Text)?;

        // ## Open Text File
        let c_path_buf = PathBuf::from("a/b/c");

        let tree1_buffer_c = tree1.open_text_file(&c_path_buf).wait()?;
        assert_eq!(tree1_buffer_c.path(), Some(c_path_buf.to_path_buf()));
        assert_eq!(tree1_buffer_c.to_string(), String::from("oid0 base text"));
        assert_eq!(tree1_buffer_c.buffer_deferred_ops_len()?, 0);

        let tree2_buffer_c = tree1.open_text_file(&c_path_buf).wait()?;
        assert_eq!(tree2_buffer_c.path(), Some(c_path_buf.to_path_buf()));
        assert_eq!(tree2_buffer_c.to_string(), String::from("oid0 base text"));
        assert_eq!(tree2_buffer_c.buffer_deferred_ops_len()?, 0);

        // ## Edit
        tree1_buffer_c.edit_2d(
            vec![
                Point::new(0, 4)..Point::new(0, 5),
                Point::new(0, 9)..Point::new(0, 10),
            ],
            "-",
        )?;

        assert_eq!(tree1_buffer_c.to_string(), String::from("oid0-base-text"));
        assert_eq!(tree2_buffer_c.to_string(), String::from("oid0-base-text"));

        tree1.create_file(PathBuf::from("x"), FileType::Directory)?;
        tree1.create_file(PathBuf::from("x/y"), FileType::Directory)?;
        tree1.rename(PathBuf::from("x"), PathBuf::from("a/b/x"))?;
        tree1.remove(PathBuf::from("a/b/d"))?;

        // TODO: entries

        assert!(tree1.exists(PathBuf::from("a/b/x")));
        assert!(!tree1.exists(PathBuf::from("a/b/d")));

        tree1.reset(Some(oid_1));
        tree2.reset(Some(oid_1));

        assert_eq!(tree1.head(), Some(oid_1));
        assert_eq!(tree2.head(), Some(oid_1));

        assert_eq!(tree1_buffer_c.to_string(), String::from("oid1 base text"));
        assert_eq!(tree2_buffer_c.to_string(), String::from("oid1 base text"));

        tree1.remove(PathBuf::from("a/b/c")).unwrap();
        assert_eq!(tree1_buffer_c.path(), None);

        tree1.reset(None);
        assert_eq!(tree1.head(), None);

        Ok(())
    }

    #[test]
    fn base_path() -> Result<(), Error> {
        let reactor = reactor::Core::new().unwrap();
        let handle = Rc::new(reactor.handle());

        let git = Rc::new(TestGitProvider::new());

        let oid_0 = git.gen_oid();
        git.commit(
            oid_0,
            vec![
                BaseEntry::dir(1, "a"),
                BaseEntry::dir(2, "b"),
                BaseEntry::file(3, "c", "oid0 base text"),
                BaseEntry::dir(3, "d"),
            ],
        );

        let network = Rc::new(TestNetworkProvider::new());

        let tree = WorkTree::new_sync(handle, uuid::Uuid::from_u128(0), Some(oid_0), git, network)?;

        tree.rename(PathBuf::from("a/b/c"), PathBuf::from("e"))?;
        tree.remove(PathBuf::from("a/b/d"))?;
        tree.create_file(PathBuf::from("f"), FileType::Text)?;
        // TODO: entries

        Ok(())
    }

    #[test]
    fn selections() -> Result<(), Error> {
        let reactor = reactor::Core::new().unwrap();
        let handle = Rc::new(reactor.handle());

        let git = Rc::new(TestGitProvider::new());
        let network = Rc::new(TestNetworkProvider::new());

        let oid_0 = git.gen_oid();

        git.commit(oid_0, vec![BaseEntry::file(1, "a", "abc")]);

        let replica1 = uuid::Uuid::from_u128(0);
        let replica2 = uuid::Uuid::from_u128(1);
        let tree1 = WorkTree::new_sync(
            handle.clone(),
            replica1,
            Some(oid_0),
            git.clone(),
            network.clone(),
        )?;

        let tree2 = WorkTree::new_sync(
            handle.clone(),
            replica2,
            Some(oid_0),
            git.clone(),
            network.clone(),
        )?;

        let buffer1 = tree1.open_text_file(&PathBuf::from("a")).wait()?;
        let buffer2 = tree2.open_text_file(&PathBuf::from("a")).wait()?;

        let buffer1_ranges = vec![Point::zero()..Point::new(0, 1)];
        let set = buffer1.add_selection_set(buffer1_ranges.clone())?;

        let selection_ranges1 = buffer1.selection_ranges()?;
        let local_ranges1 = selection_ranges1.local;
        let remote_ranges1 = selection_ranges1.remote;
        assert_eq!(local_ranges1.get(&set).unwrap(), &buffer1_ranges);
        assert_eq!(remote_ranges1, HashMap::new());

        let selection_ranges2 = buffer2.selection_ranges()?;
        let local_ranges2 = selection_ranges2.local;
        let remote_ranges2 = selection_ranges2.remote;
        assert_eq!(local_ranges2, HashMap::new());
        assert_eq!(remote_ranges2, HashMap::new());

        // assert.equal(selection2Changes.length, 1);
        // assert.deepEqual(last(selection2Changes), buffer2.getSelectionRanges());

        let buffer1_ranges_new = vec![Point::new(0, 2)..Point::new(0, 3)];
        buffer1.replace_selection_set(set, buffer1_ranges_new.clone())?;

        let selection_ranges1 = buffer1.selection_ranges()?;
        let local_ranges1 = selection_ranges1.local;
        let remote_ranges1 = selection_ranges1.remote;
        assert_eq!(local_ranges1.get(&set).unwrap(), &buffer1_ranges_new);
        assert_eq!(remote_ranges1, HashMap::new());

        let selection_ranges2 = buffer2.selection_ranges()?;
        let local_ranges2 = selection_ranges2.local;
        let remote_ranges2 = selection_ranges2.remote;
        assert_eq!(local_ranges2, HashMap::new());
        assert_eq!(remote_ranges2, HashMap::new());

        buffer1.remove_selection_set(set)?;
        let selection_ranges1 = buffer1.selection_ranges()?;
        let local_ranges1 = selection_ranges1.local;
        let remote_ranges1 = selection_ranges1.remote;
        assert_eq!(local_ranges1, HashMap::new());
        assert_eq!(remote_ranges1, HashMap::new());

        let selection_ranges2 = buffer2.selection_ranges()?;
        let local_ranges2 = selection_ranges2.local;
        let remote_ranges2 = selection_ranges2.remote;
        assert_eq!(local_ranges2, HashMap::new());
        assert_eq!(remote_ranges2, HashMap::new());

        // assert.equal(selection2Changes.length, 3);
        // assert.deepEqual(last(selection2Changes), buffer2.getSelectionRanges());

        Ok(())
    }

    // #[test]
    // fn active_location() {
    //     let git = Rc::new(TestGitProvider::new());
    //
    //     let oid = git.gen_oid();
    //     git.commit(oid, vec![
    //         BaseEntry::file(1, "a", "a"),
    //         BaseEntry::file(1, "b", "b"),
    //     ]);
    //
    //     let replica1 = uuid::Uuid::from_u128(0);
    //     let (tree1, init_ops1) = WorkTree::new(
    //         replica1,
    //         Some(oid),
    //         vec![],
    //         git.clone()
    //     ).unwrap();
    //     let mut tree1 = tree1;
    //
    //     let replica2 = uuid::Uuid::from_u128(1);
    //     let (tree2, init_ops2) = WorkTree::new(
    //         replica2,
    //         Some(oid),
    //         collect_ops(init_ops1),
    //         git.clone()
    //     ).unwrap();
    //     let mut tree2 = tree2;
    //     assert_eq!(collect_ops(init_ops2).len(), 0);
    //
    //     let buffer1 = tree1.open_text_file(PathBuf::from("a")).wait().unwrap();
    //     let buffer2 = tree2.open_text_file(PathBuf::from("b")).wait().unwrap();
    //
    //     collect_ops(
    //         Box::new(tree1.apply_ops(vec![tree2.set_active_location(Some(buffer2)).unwrap().operation]).unwrap())
    //     );
    //     collect_ops(
    //         Box::new(tree2.apply_ops(vec![tree1.set_active_location(Some(buffer1)).unwrap().operation]).unwrap())
    //     );
    //
    //     let replica_locations1 = tree1.replica_locations();
    //     assert_eq!(replica_locations1.get(&replica1).unwrap(), &PathBuf::from("a"));
    //     assert_eq!(replica_locations1.get(&replica2).unwrap(), &PathBuf::from("b"));
    //
    //     let replica_locations2 = tree1.replica_locations();
    //     assert_eq!(replica_locations2.get(&replica1).unwrap(), &PathBuf::from("a"));
    //     assert_eq!(replica_locations2.get(&replica2).unwrap(), &PathBuf::from("b"));
    //
    //     collect_ops(
    //         Box::new(tree1.apply_ops(vec![tree2.set_active_location(None).unwrap().operation]).unwrap())
    //     );
    //
    //     let replica_locations1 = tree1.replica_locations();
    //     assert_eq!(replica_locations1.get(&replica1).unwrap(), &PathBuf::from("a"));
    //     assert_eq!(replica_locations1.get(&replica2), None);
    //
    //     let replica_locations2 = tree1.replica_locations();
    //     assert_eq!(replica_locations2.get(&replica1).unwrap(), &PathBuf::from("a"));
    //     assert_eq!(replica_locations2.get(&replica2), None);
    // }

    pub trait TestWorkTree {
        fn new_sync(
            foreground: ForegroundExecutor,
            replica_id: ReplicaId,
            base: Option<Oid>,
            git: Rc<dyn GitProvider>,
            network: Rc<dyn NetworkProvider>,
        ) -> Result<Rc<WorkTree>, Error>;
        fn basic(
            network: Option<Rc<TestNetworkProvider>>,
        ) -> (
            Rc<TestGitProvider>,
            Oid,
            Rc<WorkTree>,
            Rc<TestNetworkProvider>,
        );
    }

    impl TestWorkTree for WorkTree {
        fn new_sync(
            foreground: ForegroundExecutor,
            replica_id: ReplicaId,
            base: Option<Oid>,
            git: Rc<dyn GitProvider>,
            network: Rc<dyn NetworkProvider>,
        ) -> Result<Rc<WorkTree>, Error> {
            WorkTree::new(foreground, replica_id, base, git, network).wait()
        }

        fn basic(
            network: Option<Rc<TestNetworkProvider>>,
        ) -> (
            Rc<TestGitProvider>,
            Oid,
            Rc<WorkTree>,
            Rc<TestNetworkProvider>,
        ) {
            let reactor = reactor::Core::new().unwrap();
            let handle = Rc::new(reactor.handle());

            let network = match network {
                Some(n) => n.clone(),
                _ => Rc::new(TestNetworkProvider::new()),
            };

            let git = Rc::new(TestGitProvider::new());
            let oid = git.gen_oid();

            git.commit(oid, vec![BaseEntry::file(1, "a", "")]);

            let tree = WorkTree::new_sync(
                handle,
                uuid::Uuid::from_u128(0),
                Some(oid),
                git.clone(),
                network.clone(),
            )
            .unwrap();

            (git, oid, tree, network)
        }
    }
}
