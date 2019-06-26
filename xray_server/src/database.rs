use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::cmp::Ordering;

use futures::{Future, Stream};
use memo_core::{self, Operation, OperationEnvelope};
use xray_core::ReplicaId;
use xray_core::work_tree;
use xray_core::notify_cell::{NotifyCell, NotifyCellObserver};
use xray_core::storage::Storage;
use xray_core::ForegroundExecutor;

pub struct Database {
    operations: Rc<RefCell<Vec<Operation>>>,
    trees: RefCell<HashMap<ReplicaId, Rc<RefCell<work_tree::WorkTree>>>>,
}

impl Database {
    pub fn new(path: PathBuf) -> Database {
        let mut path = path.to_path_buf();
        path.push(".xray");
        Database {
            operations: Rc::new(RefCell::new(vec![])),
            trees: RefCell::new(HashMap::new()),
        }
    }

    pub fn add_tree(&self, tree: Rc<RefCell<work_tree::WorkTree>>) {
        let replica_id = tree.borrow().replica_id();
        self.trees.borrow_mut().insert(replica_id, tree);
    }
}

impl Storage for Database {
    fn broadcast(
        &self,
        replica_id: ReplicaId,
        envelopes: Box<Stream<Item = OperationEnvelope, Error = memo_core::Error>>,
    ) {
        let mut operations = self.operations.borrow_mut();
        let new_operations = envelopes
            .wait()
            .map(|envelope| {
                let operation = envelope.unwrap().operation;
                operations.push(operation.clone());
                operation
            })
            .collect::<Vec<Operation>>();

        if new_operations.len() > 0 {
            self.trees.borrow().iter().for_each(|(tree_id, tree)| {
                if &replica_id != tree_id {
                    tree.borrow().apply_ops(new_operations.clone());
                }
            });
        }
    }

    fn fetch(&self) -> Vec<Operation> {
        self.operations.borrow().iter().map(|op| op.clone()).collect::<Vec<Operation>>()
    }
}
