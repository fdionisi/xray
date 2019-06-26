use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;

use futures::{future, stream, Future};
use memo_core::{self, Change};
pub use memo_core::{Operation, OperationEnvelope};

use buffer::{BufferId, Point, SelectionRanges, SelectionSetId};
use git::{GitProvider, Oid};
use notify_cell::{NotifyCell, NotifyCellObserver};
use storage::Storage;
use ForegroundExecutor;
use ReplicaId;

struct ChangeObserver {
    tree_updates: NotifyCell<()>,
    buffers_updates: RefCell<HashMap<BufferId, Rc<NotifyCell<()>>>>,
}

impl ChangeObserver {
    fn new() -> ChangeObserver {
        ChangeObserver {
            tree_updates: NotifyCell::new(()),
            buffers_updates: RefCell::new(HashMap::new()),
        }
    }

    fn updates(&self, buffer_id: BufferId) -> NotifyCellObserver<()> {
        let mut buffers_updates = self.buffers_updates.borrow_mut();

        let observer = if let Some(observer) = buffers_updates.get(&buffer_id) {
            observer.clone()
        } else {
            Rc::new(NotifyCell::new(()))
        };

        buffers_updates.insert(buffer_id, observer.clone());

        observer.observe()
    }

    fn notify_buffer(&self, buffer_id: BufferId) {
        let buffers_updates = self.buffers_updates.borrow();

        if let Some(buffer_updates) = buffers_updates.get(&buffer_id) {
            buffer_updates.set(())
        }
    }
}

impl memo_core::ChangeObserver for ChangeObserver {
    fn changed(&self, buffer_id: BufferId, _changes: Vec<Change>, _selections: SelectionRanges) {
        self.tree_updates.set(());
        self.notify_buffer(buffer_id);
    }
}

pub struct WorkTree {
    foreground: ForegroundExecutor,
    replica_id: ReplicaId,
    tree: Rc<RefCell<memo_core::WorkTree>>,
    observer: Rc<ChangeObserver>,
    storage: Rc<Storage>,
}

impl WorkTree {
    pub fn new(
        foreground: ForegroundExecutor,
        storage: Rc<Storage>,
        replica_id: ReplicaId,
        base: Option<Oid>,
        git: Rc<GitProvider>,
    ) -> WorkTree {
        let start_ops = storage.fetch();
        let observer = Rc::new(ChangeObserver::new());

        let (tree, ops) =
            memo_core::WorkTree::new(replica_id, base, start_ops, git, Some(observer.clone()))
                .map_err(|err| panic!("err={:?}", err))
                .unwrap();

        storage.broadcast(replica_id, ops);

        WorkTree {
            foreground,
            replica_id,
            tree: Rc::new(RefCell::new(tree)),
            observer,
            storage,
        }
    }

    pub fn replica_id(&self) -> ReplicaId {
        self.replica_id
    }

    pub fn inner(&self) -> Rc<RefCell<memo_core::WorkTree>> {
        self.tree.clone()
    }

    pub fn head(&self) -> Option<Oid> {
        self.tree.borrow().head()
    }

    pub fn buffer_updates(&self, buffer_id: BufferId) -> NotifyCellObserver<()> {
        self.observer.updates(buffer_id)
    }

    pub fn apply_ops(&self, ops: Vec<Operation>) {
        let envelopes_stream = { self.tree.borrow_mut().apply_ops(ops).unwrap() };
        let storage = self.storage.clone();

        self.foreground
            .execute(Box::new(future::ok(
                storage.broadcast(self.replica_id, Box::new(envelopes_stream)),
            )))
            .unwrap()
    }

    pub fn open_text_file(&self, path: PathBuf) -> Box<Future<Item = BufferId, Error = io::Error>> {
        Box::new(
            self.tree
                .borrow()
                .open_text_file(path)
                .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("{}", err))),
        )
    }

    pub fn len(&self, buffer_id: BufferId) -> usize {
        self.tree.borrow().len(buffer_id).unwrap()
    }

    pub fn len_for_row(&self, buffer_id: BufferId, row: u32) -> u32 {
        self.tree.borrow().len_for_row(buffer_id, row).unwrap()
    }

    pub fn longest_row(&self, buffer_id: BufferId) -> u32 {
        self.tree.borrow().longest_row(buffer_id).unwrap()
    }

    pub fn max_point(&self, buffer_id: BufferId) -> Point {
        self.tree.borrow().max_point(buffer_id).unwrap()
    }

    pub fn line(&self, buffer_id: BufferId, row: u32) -> Vec<u16> {
        self.tree.borrow().line(buffer_id, row).unwrap()
    }

    pub fn to_u16_chars(&self, buffer_id: BufferId) -> Vec<u16> {
        self.tree
            .borrow()
            .text(buffer_id)
            .unwrap()
            .collect::<Vec<u16>>()
    }

    pub fn iter_at_point(&self, buffer_id: BufferId, point: Point) -> impl Iterator<Item = u16> {
        self.tree.borrow().iter_at_point(buffer_id, point).unwrap()
    }

    pub fn backward_iter_at_point(
        &self,
        buffer_id: BufferId,
        point: Point,
    ) -> impl Iterator<Item = u16> {
        self.tree
            .borrow()
            .iter_at_point(buffer_id, point)
            .unwrap()
            .rev()
    }

    pub fn edit<I>(&self, buffer_id: BufferId, old_ranges: I, new_text: &str)
    where
        I: IntoIterator<Item = Range<Point>>,
    {
        let envelope = self
            .tree
            .borrow()
            .edit_2d(buffer_id, old_ranges, new_text)
            .unwrap();

        self.broadcast(envelope);
    }

    pub fn add_selection_set<I>(&self, buffer_id: BufferId, ranges: I) -> SelectionSetId
    where
        I: IntoIterator<Item = Range<Point>>,
    {
        let (selection_set_id, envelope) = self
            .tree
            .borrow()
            .add_selection_set(buffer_id, ranges)
            .unwrap();

        self.broadcast(envelope);

        selection_set_id
    }

    pub fn replace_selection_set<I>(
        &self,
        buffer_id: BufferId,
        selection_set_id: SelectionSetId,
        ranges: I,
    ) where
        I: IntoIterator<Item = Range<Point>>,
    {
        let envelope = self
            .tree
            .borrow()
            .replace_selection_set(buffer_id, selection_set_id, ranges)
            .unwrap();

        self.broadcast(envelope);
    }

    pub fn remove_selection_set(&self, buffer_id: BufferId, selection_set_id: SelectionSetId) {
        let envelope = self
            .tree
            .borrow()
            .remove_selection_set(buffer_id, selection_set_id)
            .unwrap();

        self.broadcast(envelope);
    }

    pub fn selection_ranges(&self, buffer_id: BufferId) -> SelectionRanges {
        self.tree.borrow().selection_ranges(buffer_id).unwrap()
    }

    fn broadcast(&self, envelope: OperationEnvelope) {
        self.storage
            .broadcast(self.replica_id, Box::new(stream::iter_ok(vec![envelope])))
    }
}
