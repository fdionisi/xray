use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

pub use memo_core::Change;

use crate::buffer::{BufferId, Point, SelectionRanges};
use crate::notify_cell::{NotifyCell, NotifyCellObserver};

#[derive(Clone)]
pub enum TreeChange {
    Text { range: Range<Point>, text: Vec<u16> },
    Selections(SelectionRanges),
}

pub struct ChangeObserver {
    tree: NotifyCell<Vec<TreeChange>>,
    buffers_changes: Rc<RefCell<ObserverMap<Vec<TreeChange>>>>,
    buffers_updates: Rc<RefCell<ObserverMap<()>>>,
}

pub struct ObserverMap<T: Clone>(HashMap<BufferId, Rc<NotifyCell<T>>>);

impl From<&Change> for TreeChange {
    fn from(change: &Change) -> TreeChange {
        TreeChange::Text {
            range: change.range.clone(),
            text: change.code_units.clone(),
        }
    }
}

impl ChangeObserver {
    pub fn new() -> Self {
        ChangeObserver {
            tree: NotifyCell::new(vec![]),
            buffers_changes: Rc::new(RefCell::new(ObserverMap::new())),
            buffers_updates: Rc::new(RefCell::new(ObserverMap::new())),
        }
    }

    pub fn updates_changes(&self, buffer_id: BufferId) -> NotifyCellObserver<Vec<TreeChange>> {
        let mut observers = self.buffers_changes.borrow_mut();
        let observer = observers.get_or_insert(buffer_id, vec![]);

        observer.observe()
    }

    pub fn updates(&self, buffer_id: BufferId) -> NotifyCellObserver<()> {
        let mut observers = self.buffers_updates.borrow_mut();
        let observer = observers.get_or_insert(buffer_id, ());

        observer.observe()
    }
}

impl memo_core::ChangeObserver for ChangeObserver {
    fn changed(&self, buffer_id: BufferId, changes: Vec<Change>, selections: SelectionRanges) {
        let change_observers = self.buffers_changes.borrow();
        let change_observer = change_observers.get(buffer_id);

        let updates_observers = self.buffers_updates.borrow();
        let updates_observer = updates_observers.get(buffer_id);

        let mut tree_changes = changes
            .iter()
            .map(|change| TreeChange::from(change))
            .collect::<Vec<TreeChange>>();

        tree_changes.push(TreeChange::Selections(selections));

        self.tree.set(tree_changes.clone());

        if let Some(o) = change_observer {
            o.set(tree_changes);
        }

        if let Some(o) = updates_observer {
            o.set(());
        }
    }
}

impl<T: Clone> ObserverMap<T> {
    fn new() -> Self {
        ObserverMap(HashMap::new())
    }

    fn get(&self, buffer_id: BufferId) -> Option<Rc<NotifyCell<T>>> {
        self.0.get(&buffer_id).map(|notify| notify.clone())
    }

    fn get_or_insert(&mut self, buffer_id: BufferId, def: T) -> Rc<NotifyCell<T>> {
        let notify = if let Some(notify) = self.0.get(&buffer_id) {
            notify.clone()
        } else {
            let notify = Rc::new(NotifyCell::new(def));
            self.0.insert(buffer_id, notify.clone());
            notify
        };

        notify
    }
}
