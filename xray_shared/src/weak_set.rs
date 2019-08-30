use std::rc::{Rc, Weak};

pub struct WeakSet<T: Sized>(Vec<Weak<T>>);

impl<T: Sized> WeakSet<T> {
    pub fn new() -> WeakSet<T> {
        WeakSet(Vec::new())
    }

    pub fn insert(&mut self, data: T) -> Rc<T> {
        let data = Rc::new(data);
        self.0.push(Rc::downgrade(&data));
        data
    }

    pub fn find<F>(&mut self, comparison: &mut F) -> Option<Rc<T>>
    where
        F: FnMut(Rc<T>) -> bool,
    {
        let mut found_data = None;
        self.0.retain(|data| {
            if let Some(data) = data.upgrade() {
                if comparison(data.clone()) {
                    found_data = Some(data);
                }
                true
            } else {
                false
            }
        });
        found_data
    }
}
