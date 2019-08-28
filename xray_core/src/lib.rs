#![feature(unsize, coerce_unsized, test)]

#[cfg(test)]
extern crate test;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate serde_json;
#[cfg(target_arch = "wasm32")]
extern crate wasm_bindgen;

#[cfg(target_arch = "wasm32")]
#[macro_use]
pub mod wasm_logging;

pub mod app;
pub mod buffer;
pub mod fs;
pub mod git;
pub mod network;
pub mod project;
pub mod views;
pub mod window;
pub mod work_tree;
pub mod workspace;

mod change_observer;
mod fuzzy;
mod movement;

pub use xray_shared::*;

#[cfg(test)]
mod stream_ext;

use std::cell::RefCell;
use std::rc::Rc;

use futures::future::{Executor, Future};
pub use memo_core::{Error as MemoError, ReplicaId};
pub use xray_rpc::Error as RpcError;

pub type ForegroundExecutor = Rc<dyn Executor<Box<dyn Future<Item = (), Error = ()> + 'static>>>;
pub type BackgroundExecutor =
    Rc<dyn Executor<Box<dyn Future<Item = (), Error = ()> + Send + 'static>>>;
pub type UserId = usize;

pub(crate) trait IntoShared {
    fn into_shared(self) -> Rc<RefCell<Self>>;
}

impl<T> IntoShared for T {
    fn into_shared(self) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(self))
    }
}

#[derive(Debug)]
pub enum Error {
    Memo(MemoError),
    Rpc(RpcError),
}

impl From<MemoError> for Error {
    fn from(error: MemoError) -> Error {
        Error::Memo(error)
    }
}

impl From<RpcError> for Error {
    fn from(error: RpcError) -> Error {
        Error::Rpc(error)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl PartialEq for Error {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Error::Memo(err_1), Error::Memo(err_2)) => err_1 == err_2,
            (Error::Rpc(err_1), Error::Rpc(err_2)) => err_1 == err_2,
            (Error::Rpc(_), Error::Memo(_)) => false,
            (Error::Memo(_), Error::Rpc(_)) => false,
        }
    }
}

#[cfg(test)]
pub mod tests {
    pub use crate::buffer::tests as buffer;
    pub use crate::git::tests as git;
    pub use crate::network::tests as network;
    pub use crate::work_tree::tests as work_tree;
}
