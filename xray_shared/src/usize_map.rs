use std::collections::{HashMap, hash_map::{Iter, Keys}};

pub struct UsizeMap<T: Sized> {
    next_id: usize,
    inner: HashMap<usize, T>,
}

impl<T: Sized> UsizeMap<T> {
    pub fn new() -> Self {
        Self {
            next_id: 0,
            inner: HashMap::new(),
        }
    }

    pub fn add(&mut self, data: T) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        self.inner.insert(id, data);

        id
    }

    pub fn get(&self, index: &usize) -> Option<&T> {
        self.inner.get(index)
    }

    pub fn remove(&mut self, index: &usize) -> Option<T> {
        self.inner.remove(index)
    }

    pub fn get_mut(&mut self, index: &usize) -> Option<&mut T> {
        self.inner.get_mut(index)
    }

    pub fn iter(&self) -> Iter<usize, T> {
        self.inner.iter()
    }

    pub fn keys(&self) -> Keys<usize, T> {
        self.inner.keys()
    }
}
