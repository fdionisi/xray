use memo_core::{Operation, OperationEnvelope};
use xray_core::notify_cell::{NotifyCell, NotifyCellObserver};

pub struct Database {
    operations: Vec<Operation>,
    observer: NotifyCell<()>,
}

impl Database {
    pub fn new() -> Database {
        Database {
            operations: vec![],
            observer: NotifyCell::new(()),
        }
    }

    pub fn updates(&self) -> NotifyCellObserver<()> {
        self.observer.observe()
    }

    pub fn add(&mut self, envelope: OperationEnvelope) {
        self.operations.push(envelope.operation);
        self.observer.set(());
    }

    pub fn operations(&self) -> Vec<Operation> {
        self.operations.clone()
    }
}
