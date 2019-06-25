use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io;
use std::mem;
use std::ops::Range;
use std::rc::Rc;

use futures::{future, Async, Future, Stream};
use memo_core;

use fs;
use never::Never;
use notify_cell::NotifyCell;
use rpc;
use ForegroundExecutor;

pub use memo_core::{BufferId, LocalSelectionSetId as SelectionSetId, Point, ReplicaId};

#[derive(Serialize, Deserialize)]
pub struct RpcState {
    id: BufferId,
    text: Vec<u16>,
    local_selections: HashMap<SelectionSetId, Vec<Range<Point>>>,
    remote_selections: HashMap<ReplicaId, Vec<Vec<Range<Point>>>>,
}

#[derive(Serialize, Deserialize)]
pub enum RpcRequest {
    Edit {
        ranges: Vec<Range<Point>>,
        text: String,
    },
    AddSelectionSet {
        ranges: Vec<Range<Point>>,
    },
    ReplaceSelectionSet {
        selection_set_id: SelectionSetId,
        ranges: Vec<Range<Point>>,
    },
    RemoveSelectionSet(SelectionSetId),
    Save,
}

#[derive(Serialize, Deserialize)]
pub enum RpcResponse {
    Edited(String),
    UpdatedSelectionSet(SelectionSetId, Vec<Range<Point>>),
    RemovedSelectionSet(SelectionSetId),
    Saved,
}

#[derive(Serialize, Deserialize)]
pub enum RpcUpdate {
    Text(String),
    Selections {
        updated: HashMap<SelectionSetId, Vec<Range<Point>>>,
        removed: HashSet<SelectionSetId>,
    },
}

pub struct BufferService {
    buffer: Rc<RefCell<LocalBuffer>>,
}

impl BufferService {
    pub fn new(buffer: Rc<RefCell<LocalBuffer>>) -> Self {
        Self { buffer }
    }
}

impl rpc::server::Service for BufferService {
    type State = RpcState;
    type Update = RpcUpdate;
    type Request = RpcRequest;
    type Response = RpcResponse;

    fn init(&mut self, _: &rpc::server::Connection) -> Self::State {
        let buffer = self.buffer.borrow_mut();
        RpcState {
            id: buffer.id,
            text: buffer.text().wait().unwrap(),
            local_selections: buffer.local_selections().unwrap(),
            remote_selections: buffer.remote_selections().unwrap(),
        }
    }

    fn poll_update(&mut self, _: &rpc::server::Connection) -> Async<Option<Self::Update>> {
        unimplemented!()
    }

    fn request(
        &mut self,
        request: Self::Request,
        _: &rpc::server::Connection,
    ) -> Option<Box<Future<Item = Self::Response, Error = Never>>> {
        match request {
            RpcRequest::Edit { ranges, text } => {
                Some(Box::new(self.buffer.borrow().edit(ranges, text).then(
                    |maybe_text| match maybe_text {
                        Ok(text) => {
                            future::ok(RpcResponse::Edited(String::from_utf16(&text).unwrap()))
                        }
                        _ => panic!("Error while editing"),
                    },
                )))
            }
            RpcRequest::AddSelectionSet { ranges } => {
                let cloned = ranges.clone();
                Some(Box::new(
                    self.buffer
                        .borrow()
                        .add_selection_set(ranges)
                        .then(|maybe_added| match maybe_added {
                            Ok(selection_set_id) => future::ok(RpcResponse::UpdatedSelectionSet(
                                selection_set_id,
                                cloned,
                            )),
                            _ => panic!("Error while adding selection"),
                        }),
                ))
            }
            RpcRequest::ReplaceSelectionSet {
                selection_set_id,
                ranges,
            } => {
                let cloned = ranges.clone();
                Some(Box::new(
                    self.buffer
                        .borrow()
                        .replace_selection_set(selection_set_id, ranges)
                        .then(move |maybe_replaced| match maybe_replaced {
                            Ok(_) => future::ok(RpcResponse::UpdatedSelectionSet(
                                selection_set_id,
                                cloned,
                            )),
                            _ => panic!("Error while updating selection"),
                        }),
                ))
            }
            RpcRequest::RemoveSelectionSet(selection_set_id) => Some(Box::new(
                self.buffer
                    .borrow()
                    .remove_selection_set(selection_set_id)
                    .then(move |maybe_removed| match maybe_removed {
                        Ok(_) => future::ok(RpcResponse::RemovedSelectionSet(selection_set_id)),
                        _ => panic!("Error while removing selection"),
                    }),
            )),
            RpcRequest::Save => {
                Some(Box::new(self.buffer.borrow().save().then(
                    |maybe_saved| match maybe_saved {
                        Ok(_) => future::ok(RpcResponse::Saved),
                        _ => panic!("Error while saving"),
                    },
                )))
            }
        }
    }
}

pub trait BufferTrait {
    fn save(&self) -> Box<Future<Item = (), Error = io::Error>>;

    fn text(&self) -> Box<Future<Item = Vec<u16>, Error = io::Error>>;

    fn edit(
        &self,
        ranges: Vec<Range<Point>>,
        text: String,
    ) -> Box<Future<Item = Vec<u16>, Error = io::Error>>;

    fn add_selection_set(
        &self,
        ranges: Vec<Range<Point>>,
    ) -> Box<Future<Item = SelectionSetId, Error = io::Error>>;

    fn replace_selection_set(
        &self,
        selection_set_id: SelectionSetId,
        ranges: Vec<Range<Point>>,
    ) -> Box<Future<Item = (), Error = io::Error>>;

    fn remove_selection_set(
        &self,
        selection_set_id: SelectionSetId,
    ) -> Box<Future<Item = (), Error = io::Error>>;
}

pub enum BufferEntry {
    Local(Rc<RefCell<LocalBuffer>>),
    Remote(Rc<RefCell<RemoteBuffer>>),
}

impl BufferEntry {
    pub fn buffer(&self) -> Rc<RefCell<BufferTrait>> {
        match self {
            BufferEntry::Local(buffer) => buffer.clone(),
            BufferEntry::Remote(buffer) => buffer.clone(),
        }
    }
}

pub struct LocalBuffer {
    id: BufferId,
    tree: Rc<fs::LocalTree>,
    file: Box<fs::File>,
    updates: NotifyCell<()>,
}

pub struct RemoteBuffer {
    service: rpc::client::Service<BufferService>,
}

impl LocalBuffer {
    pub fn new(id: BufferId, tree: Rc<fs::LocalTree>, file: Box<fs::File>) -> LocalBuffer {
        LocalBuffer {
            id,
            tree,
            file,
            updates: NotifyCell::new(()),
        }
    }

    pub fn id(&self) -> BufferId {
        self.id
    }

    pub fn file_id(&self) -> fs::FileId {
        self.file.id()
    }

    pub fn updates(&self) -> Box<Stream<Item = (), Error = ()>> {
        Box::new(self.updates.observe().select(self.tree.as_tree().updates()))
    }

    pub fn max_point(&self) -> Point {
        self.tree.max_point(self.id)
    }

    pub fn clip_point(&self, original: Point) -> Point {
        self.tree.clip_point(self.id, original)
    }

    pub fn longest_row(&self) -> u32 {
        self.tree.longest_row(self.id)
    }

    pub fn line(&self, row: u32) -> Vec<u16> {
        self.tree.line(self.id, row)
    }

    pub fn len_for_row(&self, row: u32) -> u32 {
        self.tree.len_for_row(self.id, row)
    }

    pub fn iter_at_point(&self, point: Point) -> Box<Iterator<Item = u16>> {
        self.tree.iter_at_point(self.id, point)
    }

    pub fn backward_iter_at_point(&self, point: Point) -> Box<Iterator<Item = u16>> {
        self.tree.backward_iter_at_point(self.id, point)
    }

    pub fn len(&self) -> usize {
        self.tree.len(self.id)
    }

    pub fn local_selections(
        &self,
    ) -> Result<HashMap<SelectionSetId, Vec<Range<Point>>>, io::Error> {
        self.selection_ranges().map(|ranges| ranges.local)
    }

    pub fn mutate_selections<F>(&self, set_id: SelectionSetId, f: F) -> Result<(), io::Error>
    where
        F: FnOnce(&LocalBuffer, &mut Vec<Range<Point>>) -> Vec<Range<Point>>,
    {
        let selections = self.merge_selections(&mut f(&self, &mut self.selections(set_id).clone()));
        self.replace_selection_set(set_id, selections).wait()
    }

    fn merge_selections(&self, selections: &mut Vec<Range<Point>>) -> Vec<Range<Point>> {
        let mut new_selections = Vec::with_capacity(selections.len());
        {
            let mut old_selections = selections.drain(..);
            if let Some(mut prev_selection) = old_selections.next() {
                for selection in old_selections {
                    if prev_selection.end.cmp(&selection.start) >= Ordering::Equal {
                        if selection.end.cmp(&prev_selection.end) > Ordering::Equal {
                            prev_selection.end = selection.end;
                        }
                    } else {
                        new_selections.push(mem::replace(&mut prev_selection, selection));
                    }
                }
                new_selections.push(prev_selection);
            }
        }

        new_selections
    }

    pub fn selections(&self, selection_set_id: SelectionSetId) -> Vec<Range<Point>> {
        self.local_selections()
            .unwrap()
            .get(&selection_set_id)
            .unwrap()
            .clone()
    }

    fn selection_ranges(&self) -> Result<memo_core::BufferSelectionRanges, io::Error> {
        self.tree.selection_ranges(self.id)
    }

    pub fn remote_selections(
        &self,
    ) -> Result<HashMap<ReplicaId, Vec<Vec<Range<Point>>>>, io::Error> {
        self.selection_ranges().map(|ranges| ranges.remote)
    }
}

impl BufferTrait for LocalBuffer {
    fn save(&self) -> Box<Future<Item = (), Error = io::Error>> {
        Box::new(self.file.write_text(self.text()))
    }

    fn text(&self) -> Box<Future<Item = Vec<u16>, Error = io::Error>> {
        Box::new(self.tree.text(self.id))
    }

    fn edit(
        &self,
        ranges: Vec<Range<Point>>,
        text: String,
    ) -> Box<Future<Item = Vec<u16>, Error = io::Error>> {
        let tree = self.tree.clone();
        let id = self.id.clone();
        let updates = self.updates.clone();
        Box::new(self.tree.edit(self.id, ranges, text).and_then(move |_| {
            updates.set(());
            tree.text(id)
        }))
    }

    fn add_selection_set(
        &self,
        ranges: Vec<Range<Point>>,
    ) -> Box<Future<Item = SelectionSetId, Error = io::Error>> {
        let updates = self.updates.clone();
        Box::new(
            self.tree
                .add_selection_set(self.id, ranges)
                .and_then(move |selection_set_id| {
                    updates.set(());
                    future::ok(selection_set_id)
                }),
        )
    }

    fn replace_selection_set(
        &self,
        selection_set_id: SelectionSetId,
        ranges: Vec<Range<Point>>,
    ) -> Box<Future<Item = (), Error = io::Error>> {
        let updates = self.updates.clone();
        Box::new(
            self.tree
                .replace_selection_set(self.id, selection_set_id, ranges)
                .and_then(move |_| future::ok(updates.set(()))),
        )
    }

    fn remove_selection_set(
        &self,
        selection_set_id: SelectionSetId,
    ) -> Box<Future<Item = (), Error = io::Error>> {
        let updates = self.updates.clone();
        Box::new(
            self.tree
                .remove_selection_set(self.id, selection_set_id)
                .and_then(move |_| future::ok(updates.set(()))),
        )
    }
}

impl RemoteBuffer {
    pub fn new(
        foreground: ForegroundExecutor,
        service: rpc::client::Service<BufferService>,
    ) -> RemoteBuffer {
        RemoteBuffer { service }
    }
}

impl BufferTrait for RemoteBuffer {
    fn save(&self) -> Box<Future<Item = (), Error = io::Error>> {
        Box::new(
            self.service
                .request(RpcRequest::Save)
                .then(move |response| match response {
                    Ok(RpcResponse::Saved) => future::ok(()),
                    _ => panic!("Error saving"),
                }),
        )
    }

    fn text(&self) -> Box<Future<Item = Vec<u16>, Error = io::Error>> {
        Box::new(future::ok(self.service.state().unwrap().text))
    }

    fn edit(
        &self,
        ranges: Vec<Range<Point>>,
        text: String,
    ) -> Box<Future<Item = Vec<u16>, Error = io::Error>> {
        let service = self.service.clone();
        Box::new(
            self.service
                .request(RpcRequest::Edit { ranges, text })
                .then(move |response| match response {
                    Ok(RpcResponse::Edited(text)) => {
                        let mut state = service.state().unwrap();

                        let text = text.encode_utf16().collect::<Vec<u16>>();
                        state.text = text.clone();

                        future::ok(text)
                    }
                    _ => panic!("Error editing"),
                }),
        )
    }

    fn add_selection_set(
        &self,
        ranges: Vec<Range<Point>>,
    ) -> Box<Future<Item = SelectionSetId, Error = io::Error>> {
        let service = self.service.clone();
        Box::new(
            self.service
                .request(RpcRequest::AddSelectionSet { ranges })
                .then(move |response| match response {
                    Ok(RpcResponse::UpdatedSelectionSet(selection_set_id, ranges)) => {
                        let mut state = service.state().unwrap();

                        state.local_selections.insert(selection_set_id, ranges);

                        future::ok(selection_set_id)
                    }
                    _ => panic!("Error editing"),
                }),
        )
    }

    fn replace_selection_set(
        &self,
        selection_set_id: SelectionSetId,
        ranges: Vec<Range<Point>>,
    ) -> Box<Future<Item = (), Error = io::Error>> {
        let service = self.service.clone();
        Box::new(
            self.service
                .request(RpcRequest::ReplaceSelectionSet {
                    selection_set_id,
                    ranges,
                })
                .then(move |response| match response {
                    Ok(RpcResponse::UpdatedSelectionSet(selection_set_id, ranges)) => {
                        let mut state = service.state().unwrap();

                        state.local_selections.insert(selection_set_id, ranges);

                        future::ok(())
                    }
                    _ => panic!("Error editing"),
                }),
        )
    }

    fn remove_selection_set(
        &self,
        selection_set_id: SelectionSetId,
    ) -> Box<Future<Item = (), Error = io::Error>> {
        let service = self.service.clone();
        Box::new(
            self.service
                .request(RpcRequest::RemoveSelectionSet(selection_set_id))
                .then(move |response| match response {
                    Ok(RpcResponse::RemovedSelectionSet(selection_set_id)) => {
                        let mut state = service.state().unwrap();

                        state.local_selections.remove(&selection_set_id);

                        future::ok(())
                    }
                    _ => panic!("Error editing"),
                }),
        )
    }
}

// #[cfg(test)]
// pub mod tests {
//     extern crate rand;

//     use self::rand::{Rng, SeedableRng, StdRng};
//     use super::*;
//     use cross_platform;
//     use fs::{tests::TestTree, LocalTree};
//     use rpc;
//     use std::time::Duration;
//     use tokio_core::reactor;
//     use IntoShared;

//     #[test]
//     fn test_edit() {
//         let mut buffer = Buffer::new(build_base_buffer_id());
//         buffer.edit(&[0..0], "abc");
//         assert_eq!(buffer.to_string(), "abc");
//         buffer.edit(&[3..3], "def");
//         assert_eq!(buffer.to_string(), "abcdef");
//         buffer.edit(&[0..0], "ghi");
//         assert_eq!(buffer.to_string(), "ghiabcdef");
//         buffer.edit(&[5..5], "jkl");
//         assert_eq!(buffer.to_string(), "ghiabjklcdef");
//         buffer.edit(&[6..7], "");
//         assert_eq!(buffer.to_string(), "ghiabjlcdef");
//         buffer.edit(&[4..9], "mno");
//         assert_eq!(buffer.to_string(), "ghiamnoef");
//     }

//     #[test]
//     fn test_random_edits() {
//         let buffer_id = build_base_buffer_id();

//         for seed in 0..100 {
//             println!("{:?}", seed);
//             let mut rng = StdRng::from_seed(&[seed]);

//             let mut buffer = Buffer::new(buffer_id);
//             let mut reference_string = String::new();

//             for _i in 0..10 {
//                 let mut old_ranges: Vec<Range<usize>> = Vec::new();
//                 for _ in 0..5 {
//                     let last_end = old_ranges.last().map_or(0, |last_range| last_range.end + 1);
//                     if last_end > buffer.len() {
//                         break;
//                     }
//                     let end = rng.gen_range::<usize>(last_end, buffer.len() + 1);
//                     let start = rng.gen_range::<usize>(last_end, end + 1);
//                     old_ranges.push(start..end);
//                 }
//                 let new_text = RandomCharIter(rng)
//                     .take(rng.gen_range(0, 10))
//                     .collect::<String>();

//                 buffer.edit(&old_ranges, new_text.as_str());
//                 for old_range in old_ranges.iter().rev() {
//                     reference_string = [
//                         &reference_string[0..old_range.start],
//                         new_text.as_str(),
//                         &reference_string[old_range.end..],
//                     ]
//                     .concat();
//                 }
//                 assert_eq!(buffer.to_string(), reference_string);
//             }
//         }
//     }

//     #[test]
//     fn test_len_for_row() {
//         let mut buffer = Buffer::new(build_base_buffer_id());
//         buffer.edit(&[0..0], "abcd\nefg\nhij");
//         buffer.edit(&[12..12], "kl\nmno");
//         buffer.edit(&[18..18], "\npqrs\n");
//         buffer.edit(&[18..21], "\nPQ");

//         assert_eq!(buffer.len_for_row(0), Ok(4));
//         assert_eq!(buffer.len_for_row(1), Ok(3));
//         assert_eq!(buffer.len_for_row(2), Ok(5));
//         assert_eq!(buffer.len_for_row(3), Ok(3));
//         assert_eq!(buffer.len_for_row(4), Ok(4));
//         assert_eq!(buffer.len_for_row(5), Ok(0));
//         assert_eq!(buffer.len_for_row(6), Err(Error::OffsetOutOfRange));
//     }

//     #[test]
//     fn test_longest_row() {
//         let mut buffer = Buffer::new(build_base_buffer_id());
//         assert_eq!(buffer.longest_row(), 0);
//         buffer.edit(&[0..0], "abcd\nefg\nhij");
//         assert_eq!(buffer.longest_row(), 0);
//         buffer.edit(&[12..12], "kl\nmno");
//         assert_eq!(buffer.longest_row(), 2);
//         buffer.edit(&[18..18], "\npqrs");
//         assert_eq!(buffer.longest_row(), 2);
//         buffer.edit(&[10..12], "");
//         assert_eq!(buffer.longest_row(), 0);
//         buffer.edit(&[24..24], "tuv");
//         assert_eq!(buffer.longest_row(), 4);
//     }

//     #[test]
//     fn iter_starting_at_point() {
//         let mut buffer = Buffer::new(build_base_buffer_id());
//         buffer.edit(&[0..0], "abcd\nefgh\nij");
//         buffer.edit(&[12..12], "kl\nmno");
//         buffer.edit(&[18..18], "\npqrs");
//         buffer.edit(&[18..21], "\nPQ");

//         let iter = buffer.iter_starting_at_point(Point::new(0, 0));
//         assert_eq!(
//             String::from_utf16_lossy(&iter.collect::<Vec<u16>>()),
//             "abcd\nefgh\nijkl\nmno\nPQrs"
//         );

//         let iter = buffer.iter_starting_at_point(Point::new(1, 0));
//         assert_eq!(
//             String::from_utf16_lossy(&iter.collect::<Vec<u16>>()),
//             "efgh\nijkl\nmno\nPQrs"
//         );

//         let iter = buffer.iter_starting_at_point(Point::new(2, 0));
//         assert_eq!(
//             String::from_utf16_lossy(&iter.collect::<Vec<u16>>()),
//             "ijkl\nmno\nPQrs"
//         );

//         let iter = buffer.iter_starting_at_point(Point::new(3, 0));
//         assert_eq!(
//             String::from_utf16_lossy(&iter.collect::<Vec<u16>>()),
//             "mno\nPQrs"
//         );

//         let iter = buffer.iter_starting_at_point(Point::new(4, 0));
//         assert_eq!(
//             String::from_utf16_lossy(&iter.collect::<Vec<u16>>()),
//             "PQrs"
//         );

//         let iter = buffer.iter_starting_at_point(Point::new(5, 0));
//         assert_eq!(String::from_utf16_lossy(&iter.collect::<Vec<u16>>()), "");

//         // Regression test:
//         let mut buffer = Buffer::new(build_base_buffer_id());
//         buffer.edit(&[0..0], "[workspace]\nmembers = [\n    \"xray_core\",\n    \"xray_server\",\n    \"xray_cli\",\n    \"xray_wasm\",\n]\n");
//         buffer.edit(&[60..60], "\n");

//         let iter = buffer.iter_starting_at_point(Point::new(6, 0));
//         assert_eq!(
//             String::from_utf16_lossy(&iter.collect::<Vec<u16>>()),
//             "    \"xray_wasm\",\n]\n"
//         );
//     }

//     #[test]
//     fn backward_iter_starting_at_point() {
//         let mut buffer = Buffer::new(build_base_buffer_id());
//         buffer.edit(&[0..0], "abcd\nefgh\nij");
//         buffer.edit(&[12..12], "kl\nmno");
//         buffer.edit(&[18..18], "\npqrs");
//         buffer.edit(&[18..21], "\nPQ");

//         let iter = buffer.backward_iter_starting_at_point(Point::new(0, 0));
//         assert_eq!(String::from_utf16_lossy(&iter.collect::<Vec<u16>>()), "");

//         let iter = buffer.backward_iter_starting_at_point(Point::new(0, 3));
//         assert_eq!(String::from_utf16_lossy(&iter.collect::<Vec<u16>>()), "cba");

//         let iter = buffer.backward_iter_starting_at_point(Point::new(1, 4));
//         assert_eq!(
//             String::from_utf16_lossy(&iter.collect::<Vec<u16>>()),
//             "hgfe\ndcba"
//         );

//         let iter = buffer.backward_iter_starting_at_point(Point::new(3, 2));
//         assert_eq!(
//             String::from_utf16_lossy(&iter.collect::<Vec<u16>>()),
//             "nm\nlkji\nhgfe\ndcba"
//         );

//         let iter = buffer.backward_iter_starting_at_point(Point::new(4, 4));
//         assert_eq!(
//             String::from_utf16_lossy(&iter.collect::<Vec<u16>>()),
//             "srQP\nonm\nlkji\nhgfe\ndcba"
//         );

//         let iter = buffer.backward_iter_starting_at_point(Point::new(5, 0));
//         assert_eq!(
//             String::from_utf16_lossy(&iter.collect::<Vec<u16>>()),
//             "srQP\nonm\nlkji\nhgfe\ndcba"
//         );
//     }

//     #[test]
//     fn test_point_for_offset() {
//         let text = Text::from("abc\ndefgh\nijklm\nopq");
//         assert_eq!(text.point_for_offset(0), Ok(Point { row: 0, column: 0 }));
//         assert_eq!(text.point_for_offset(1), Ok(Point { row: 0, column: 1 }));
//         assert_eq!(text.point_for_offset(2), Ok(Point { row: 0, column: 2 }));
//         assert_eq!(text.point_for_offset(3), Ok(Point { row: 0, column: 3 }));
//         assert_eq!(text.point_for_offset(4), Ok(Point { row: 1, column: 0 }));
//         assert_eq!(text.point_for_offset(5), Ok(Point { row: 1, column: 1 }));
//         assert_eq!(text.point_for_offset(9), Ok(Point { row: 1, column: 5 }));
//         assert_eq!(text.point_for_offset(10), Ok(Point { row: 2, column: 0 }));
//         assert_eq!(text.point_for_offset(14), Ok(Point { row: 2, column: 4 }));
//         assert_eq!(text.point_for_offset(15), Ok(Point { row: 2, column: 5 }));
//         assert_eq!(text.point_for_offset(16), Ok(Point { row: 3, column: 0 }));
//         assert_eq!(text.point_for_offset(17), Ok(Point { row: 3, column: 1 }));
//         assert_eq!(text.point_for_offset(19), Ok(Point { row: 3, column: 3 }));
//         assert_eq!(text.point_for_offset(20), Err(Error::OffsetOutOfRange));

//         let text = Text::from("abc");
//         assert_eq!(text.point_for_offset(0), Ok(Point { row: 0, column: 0 }));
//         assert_eq!(text.point_for_offset(1), Ok(Point { row: 0, column: 1 }));
//         assert_eq!(text.point_for_offset(2), Ok(Point { row: 0, column: 2 }));
//         assert_eq!(text.point_for_offset(3), Ok(Point { row: 0, column: 3 }));
//         assert_eq!(text.point_for_offset(4), Err(Error::OffsetOutOfRange));
//     }

//     #[test]
//     fn test_offset_for_point() {
//         let text = Text::from("abc\ndefgh");
//         assert_eq!(text.offset_for_point(Point { row: 0, column: 0 }), Ok(0));
//         assert_eq!(text.offset_for_point(Point { row: 0, column: 1 }), Ok(1));
//         assert_eq!(text.offset_for_point(Point { row: 0, column: 2 }), Ok(2));
//         assert_eq!(text.offset_for_point(Point { row: 0, column: 3 }), Ok(3));
//         assert_eq!(
//             text.offset_for_point(Point { row: 0, column: 4 }),
//             Err(Error::OffsetOutOfRange)
//         );
//         assert_eq!(text.offset_for_point(Point { row: 1, column: 0 }), Ok(4));
//         assert_eq!(text.offset_for_point(Point { row: 1, column: 1 }), Ok(5));
//         assert_eq!(text.offset_for_point(Point { row: 1, column: 5 }), Ok(9));
//         assert_eq!(
//             text.offset_for_point(Point { row: 1, column: 6 }),
//             Err(Error::OffsetOutOfRange)
//         );

//         let text = Text::from("abc");
//         assert_eq!(text.offset_for_point(Point { row: 0, column: 0 }), Ok(0));
//         assert_eq!(text.offset_for_point(Point { row: 0, column: 1 }), Ok(1));
//         assert_eq!(text.offset_for_point(Point { row: 0, column: 2 }), Ok(2));
//         assert_eq!(text.offset_for_point(Point { row: 0, column: 3 }), Ok(3));
//         assert_eq!(
//             text.offset_for_point(Point { row: 0, column: 4 }),
//             Err(Error::OffsetOutOfRange)
//         );
//     }

//     #[test]
//     fn test_longest_row_in_range() {
//         for seed in 0..100 {
//             println!("{:?}", seed);
//             let mut rng = StdRng::from_seed(&[seed]);
//             let string = RandomCharIter(rng)
//                 .take(rng.gen_range(1, 10))
//                 .collect::<String>();
//             let text = Text::from(string.as_ref());

//             for _i in 0..10 {
//                 let end = rng.gen_range(1, string.len() + 1);
//                 let start = rng.gen_range(0, end);

//                 let mut cur_row = string[0..start].chars().filter(|c| *c == '\n').count() as u32;
//                 let mut cur_row_len = 0;
//                 let mut expected_longest_row = cur_row;
//                 let mut expected_longest_row_len = cur_row_len;
//                 for ch in string[start..end].chars() {
//                     if ch == '\n' {
//                         if cur_row_len > expected_longest_row_len {
//                             expected_longest_row = cur_row;
//                             expected_longest_row_len = cur_row_len;
//                         }
//                         cur_row += 1;
//                         cur_row_len = 0;
//                     } else {
//                         cur_row_len += 1;
//                     }
//                 }
//                 if cur_row_len > expected_longest_row_len {
//                     expected_longest_row = cur_row;
//                     expected_longest_row_len = cur_row_len;
//                 }

//                 assert_eq!(
//                     text.longest_row_in_range(start..end),
//                     Ok((expected_longest_row, expected_longest_row_len))
//                 );
//             }
//         }
//     }

//     #[test]
//     fn fragment_ids() {
//         for seed in 0..10 {
//             use self::rand::{Rng, SeedableRng, StdRng};
//             let mut rng = StdRng::from_seed(&[seed]);

//             let mut ids = vec![FragmentId(Arc::new(vec![0])), FragmentId(Arc::new(vec![4]))];
//             for _i in 0..100 {
//                 let index = rng.gen_range::<usize>(1, ids.len());

//                 let left = ids[index - 1].clone();
//                 let right = ids[index].clone();
//                 ids.insert(index, FragmentId::between_with_max(&left, &right, 4));

//                 let mut sorted_ids = ids.clone();
//                 sorted_ids.sort();
//                 assert_eq!(ids, sorted_ids);
//             }
//         }
//     }

//     #[test]
//     fn test_anchors() {
//         let mut buffer = Buffer::new(build_base_buffer_id());
//         buffer.edit(&[0..0], "abc");
//         let left_anchor = buffer.anchor_before_offset(2).unwrap();
//         let right_anchor = buffer.anchor_after_offset(2).unwrap();

//         buffer.edit(&[1..1], "def\n");
//         assert_eq!(buffer.to_string(), "adef\nbc");
//         assert_eq!(buffer.offset_for_anchor(&left_anchor).unwrap(), 6);
//         assert_eq!(buffer.offset_for_anchor(&right_anchor).unwrap(), 6);
//         assert_eq!(
//             buffer.point_for_anchor(&left_anchor).unwrap(),
//             Point { row: 1, column: 1 }
//         );
//         assert_eq!(
//             buffer.point_for_anchor(&right_anchor).unwrap(),
//             Point { row: 1, column: 1 }
//         );

//         buffer.edit(&[2..3], "");
//         assert_eq!(buffer.to_string(), "adf\nbc");
//         assert_eq!(buffer.offset_for_anchor(&left_anchor).unwrap(), 5);
//         assert_eq!(buffer.offset_for_anchor(&right_anchor).unwrap(), 5);
//         assert_eq!(
//             buffer.point_for_anchor(&left_anchor).unwrap(),
//             Point { row: 1, column: 1 }
//         );
//         assert_eq!(
//             buffer.point_for_anchor(&right_anchor).unwrap(),
//             Point { row: 1, column: 1 }
//         );

//         buffer.edit(&[5..5], "ghi\n");
//         assert_eq!(buffer.to_string(), "adf\nbghi\nc");
//         assert_eq!(buffer.offset_for_anchor(&left_anchor).unwrap(), 5);
//         assert_eq!(buffer.offset_for_anchor(&right_anchor).unwrap(), 9);
//         assert_eq!(
//             buffer.point_for_anchor(&left_anchor).unwrap(),
//             Point { row: 1, column: 1 }
//         );
//         assert_eq!(
//             buffer.point_for_anchor(&right_anchor).unwrap(),
//             Point { row: 2, column: 0 }
//         );

//         buffer.edit(&[7..9], "");
//         assert_eq!(buffer.to_string(), "adf\nbghc");
//         assert_eq!(buffer.offset_for_anchor(&left_anchor).unwrap(), 5);
//         assert_eq!(buffer.offset_for_anchor(&right_anchor).unwrap(), 7);
//         assert_eq!(
//             buffer.point_for_anchor(&left_anchor).unwrap(),
//             Point { row: 1, column: 1 }
//         );
//         assert_eq!(
//             buffer.point_for_anchor(&right_anchor).unwrap(),
//             Point { row: 1, column: 3 }
//         );

//         // Ensure anchoring to a point is equivalent to anchoring to an offset.
//         assert_eq!(
//             buffer.anchor_before_point(Point { row: 0, column: 0 }),
//             buffer.anchor_before_offset(0)
//         );
//         assert_eq!(
//             buffer.anchor_before_point(Point { row: 0, column: 1 }),
//             buffer.anchor_before_offset(1)
//         );
//         assert_eq!(
//             buffer.anchor_before_point(Point { row: 0, column: 2 }),
//             buffer.anchor_before_offset(2)
//         );
//         assert_eq!(
//             buffer.anchor_before_point(Point { row: 0, column: 3 }),
//             buffer.anchor_before_offset(3)
//         );
//         assert_eq!(
//             buffer.anchor_before_point(Point { row: 1, column: 0 }),
//             buffer.anchor_before_offset(4)
//         );
//         assert_eq!(
//             buffer.anchor_before_point(Point { row: 1, column: 1 }),
//             buffer.anchor_before_offset(5)
//         );
//         assert_eq!(
//             buffer.anchor_before_point(Point { row: 1, column: 2 }),
//             buffer.anchor_before_offset(6)
//         );
//         assert_eq!(
//             buffer.anchor_before_point(Point { row: 1, column: 3 }),
//             buffer.anchor_before_offset(7)
//         );
//         assert_eq!(
//             buffer.anchor_before_point(Point { row: 1, column: 4 }),
//             buffer.anchor_before_offset(8)
//         );

//         // Comparison between anchors.
//         let anchor_at_offset_0 = buffer.anchor_before_offset(0).unwrap();
//         let anchor_at_offset_1 = buffer.anchor_before_offset(1).unwrap();
//         let anchor_at_offset_2 = buffer.anchor_before_offset(2).unwrap();

//         assert_eq!(
//             buffer.cmp_anchors(&anchor_at_offset_0, &anchor_at_offset_0),
//             Ok(Ordering::Equal)
//         );
//         assert_eq!(
//             buffer.cmp_anchors(&anchor_at_offset_1, &anchor_at_offset_1),
//             Ok(Ordering::Equal)
//         );
//         assert_eq!(
//             buffer.cmp_anchors(&anchor_at_offset_2, &anchor_at_offset_2),
//             Ok(Ordering::Equal)
//         );

//         assert_eq!(
//             buffer.cmp_anchors(&anchor_at_offset_0, &anchor_at_offset_1),
//             Ok(Ordering::Less)
//         );
//         assert_eq!(
//             buffer.cmp_anchors(&anchor_at_offset_1, &anchor_at_offset_2),
//             Ok(Ordering::Less)
//         );
//         assert_eq!(
//             buffer.cmp_anchors(&anchor_at_offset_0, &anchor_at_offset_2),
//             Ok(Ordering::Less)
//         );

//         assert_eq!(
//             buffer.cmp_anchors(&anchor_at_offset_1, &anchor_at_offset_0),
//             Ok(Ordering::Greater)
//         );
//         assert_eq!(
//             buffer.cmp_anchors(&anchor_at_offset_2, &anchor_at_offset_1),
//             Ok(Ordering::Greater)
//         );
//         assert_eq!(
//             buffer.cmp_anchors(&anchor_at_offset_2, &anchor_at_offset_0),
//             Ok(Ordering::Greater)
//         );
//     }

//     #[test]
//     fn anchors_at_start_and_end() {
//         let mut buffer = Buffer::new(build_base_buffer_id());
//         let before_start_anchor = buffer.anchor_before_offset(0).unwrap();
//         let after_end_anchor = buffer.anchor_after_offset(0).unwrap();

//         buffer.edit(&[0..0], "abc");
//         assert_eq!(buffer.to_string(), "abc");
//         assert_eq!(buffer.offset_for_anchor(&before_start_anchor).unwrap(), 0);
//         assert_eq!(buffer.offset_for_anchor(&after_end_anchor).unwrap(), 3);

//         let after_start_anchor = buffer.anchor_after_offset(0).unwrap();
//         let before_end_anchor = buffer.anchor_before_offset(3).unwrap();

//         buffer.edit(&[3..3], "def");
//         buffer.edit(&[0..0], "ghi");
//         assert_eq!(buffer.to_string(), "ghiabcdef");
//         assert_eq!(buffer.offset_for_anchor(&before_start_anchor).unwrap(), 0);
//         assert_eq!(buffer.offset_for_anchor(&after_start_anchor).unwrap(), 3);
//         assert_eq!(buffer.offset_for_anchor(&before_end_anchor).unwrap(), 6);
//         assert_eq!(buffer.offset_for_anchor(&after_end_anchor).unwrap(), 9);
//     }

//     #[test]
//     fn test_clip_point() {
//         let mut buffer = Buffer::new(build_base_buffer_id());
//         buffer.edit(&[0..0], "abcdefghi");

//         let point = buffer.clip_point(Point::new(0, 0));
//         assert_eq!(point.row, 0);
//         assert_eq!(point.column, 0);

//         let point = buffer.clip_point(Point::new(0, 2));
//         assert_eq!(point.row, 0);
//         assert_eq!(point.column, 2);

//         let point = buffer.clip_point(Point::new(1, 12));
//         assert_eq!(point.row, 0);
//         assert_eq!(point.column, 9);
//     }

//     #[test]
//     fn test_snapshot() {
//         let mut buffer = Buffer::new(build_base_buffer_id());
//         buffer.edit(&[0..0], "abcdefghi");
//         buffer.edit(&[3..6], "DEF");

//         let snapshot = buffer.snapshot();
//         assert_eq!(buffer.to_string(), String::from("abcDEFghi"));
//         assert_eq!(snapshot.to_string(), String::from("abcDEFghi"));

//         buffer.edit(&[0..1], "A");
//         buffer.edit(&[8..9], "I");
//         assert_eq!(buffer.to_string(), String::from("AbcDEFghI"));
//         assert_eq!(snapshot.to_string(), String::from("abcDEFghi"));
//     }

//     #[test]
//     fn test_random_concurrent_edits() {
//         for seed in 0..100 {
//             println!("{:?}", seed);
//             let mut rng = StdRng::from_seed(&[seed]);

//             let site_range = 0..5;
//             let mut buffers = Vec::new();
//             let mut queues = Vec::new();
//             for i in site_range.clone() {
//                 let mut buffer = Buffer::new(build_base_buffer_id());
//                 buffer.replica_id = i + 1;
//                 buffers.push(buffer);
//                 queues.push(Vec::new());
//             }

//             let mut edit_count = 10;
//             loop {
//                 let replica_index = rng.gen_range::<usize>(site_range.start, site_range.end);
//                 let buffer = &mut buffers[replica_index];
//                 if edit_count > 0 && rng.gen() {
//                     let mut old_ranges: Vec<Range<usize>> = Vec::new();
//                     for _ in 0..5 {
//                         let last_end = old_ranges.last().map_or(0, |last_range| last_range.end + 1);
//                         if last_end > buffer.len() {
//                             break;
//                         }
//                         let end = rng.gen_range::<usize>(last_end, buffer.len() + 1);
//                         let start = rng.gen_range::<usize>(last_end, end + 1);
//                         old_ranges.push(start..end);
//                     }
//                     let new_text = RandomCharIter(rng)
//                         .take(rng.gen_range(0, 10))
//                         .collect::<String>();

//                     for op in buffer.edit(&old_ranges, new_text.as_str()) {
//                         for (index, queue) in queues.iter_mut().enumerate() {
//                             if index != replica_index {
//                                 queue.push(op.clone());
//                             }
//                         }
//                     }

//                     edit_count -= 1;
//                 } else if !queues[replica_index].is_empty() {
//                     buffer
//                         .integrate_op(queues[replica_index].remove(0))
//                         .unwrap();
//                 }

//                 if edit_count == 0 && queues.iter().all(|q| q.is_empty()) {
//                     break;
//                 }
//             }

//             for buffer in &buffers[1..] {
//                 assert_eq!(buffer.to_string(), buffers[0].to_string());
//             }
//         }
//     }

//     #[test]
//     fn test_edit_replication() {
//         let local_buffer = Buffer::new(build_base_buffer_id()).into_shared();
//         local_buffer.borrow_mut().edit(&[0..0], "abcdef");
//         local_buffer.borrow_mut().edit(&[2..4], "ghi");

//         let mut reactor = reactor::Core::new().unwrap();
//         let foreground = Rc::new(reactor.handle());
//         let client_1 =
//             rpc::tests::connect(&mut reactor, super::rpc::Service::new(local_buffer.clone()));
//         let remote_buffer_1 = Buffer::remote(foreground.clone(), client_1).unwrap();
//         let client_2 =
//             rpc::tests::connect(&mut reactor, super::rpc::Service::new(local_buffer.clone()));
//         let remote_buffer_2 = Buffer::remote(foreground, client_2).unwrap();
//         assert_eq!(
//             remote_buffer_1.borrow().to_string(),
//             local_buffer.borrow().to_string()
//         );
//         assert_eq!(
//             remote_buffer_2.borrow().to_string(),
//             local_buffer.borrow().to_string()
//         );

//         local_buffer.borrow_mut().edit(&[3..6], "jk");
//         remote_buffer_1.borrow_mut().edit(&[7..7], "lmn");
//         let anchor = remote_buffer_1.borrow().anchor_before_offset(8).unwrap();

//         let mut remaining_tries = 10;
//         while remote_buffer_1.borrow().to_string() != local_buffer.borrow().to_string()
//             || remote_buffer_2.borrow().to_string() != local_buffer.borrow().to_string()
//         {
//             remaining_tries -= 1;
//             assert!(
//                 remaining_tries > 0,
//                 "Ran out of patience waiting for buffers to converge"
//             );
//             reactor.turn(Some(Duration::from_millis(0)));
//         }

//         assert_eq!(local_buffer.borrow().offset_for_anchor(&anchor).unwrap(), 7);
//         assert_eq!(
//             remote_buffer_1.borrow().offset_for_anchor(&anchor).unwrap(),
//             7
//         );
//         assert_eq!(
//             remote_buffer_2.borrow().offset_for_anchor(&anchor).unwrap(),
//             7
//         );
//     }

//     #[test]
//     fn test_selection_replication() {
//         use stream_ext::StreamExt;

//         let mut buffer_1 = Buffer::new(build_base_buffer_id());
//         buffer_1.edit(&[0..0], "abcdef");
//         let sels = vec![empty_selection(&buffer_1, 1), empty_selection(&buffer_1, 3)];
//         buffer_1.add_selection_set(0, sels);
//         let sels = vec![empty_selection(&buffer_1, 2), empty_selection(&buffer_1, 4)];
//         let buffer_1_set_id = buffer_1.add_selection_set(0, sels);
//         let buffer_1 = buffer_1.into_shared();

//         let mut reactor = reactor::Core::new().unwrap();
//         let foreground = Rc::new(reactor.handle());
//         let buffer_2 = Buffer::remote(
//             foreground.clone(),
//             rpc::tests::connect(&mut reactor, super::rpc::Service::new(buffer_1.clone())),
//         )
//         .unwrap();
//         assert_eq!(selections(&buffer_1), selections(&buffer_2));

//         let buffer_3 = Buffer::remote(
//             foreground,
//             rpc::tests::connect(&mut reactor, super::rpc::Service::new(buffer_1.clone())),
//         )
//         .unwrap();
//         assert_eq!(selections(&buffer_1), selections(&buffer_3));

//         let mut buffer_1_updates = buffer_1.borrow().updates();
//         let mut buffer_2_updates = buffer_2.borrow().updates();
//         let mut buffer_3_updates = buffer_3.borrow().updates();

//         buffer_1
//             .borrow_mut()
//             .mutate_selections(buffer_1_set_id, |buffer, selections| {
//                 for selection in selections {
//                     selection.start = buffer
//                         .anchor_before_offset(
//                             buffer.offset_for_anchor(&selection.start).unwrap() + 1,
//                         )
//                         .unwrap();
//                 }
//             })
//             .unwrap();
//         buffer_2_updates.wait_next(&mut reactor).unwrap();
//         assert_eq!(selections(&buffer_1), selections(&buffer_3));
//         buffer_3_updates.wait_next(&mut reactor).unwrap();
//         assert_eq!(selections(&buffer_1), selections(&buffer_3));

//         buffer_1
//             .borrow_mut()
//             .remove_selection_set(buffer_1_set_id)
//             .unwrap();
//         buffer_1_updates.wait_next(&mut reactor).unwrap();

//         buffer_2_updates.wait_next(&mut reactor).unwrap();
//         assert_eq!(selections(&buffer_1), selections(&buffer_2));
//         buffer_3_updates.wait_next(&mut reactor).unwrap();
//         assert_eq!(selections(&buffer_1), selections(&buffer_3));

//         let sels = vec![empty_selection(&buffer_2.borrow(), 1)];
//         let buffer_2_set_id = buffer_2.borrow_mut().add_selection_set(0, sels);
//         buffer_2_updates.wait_next(&mut reactor).unwrap();

//         buffer_1_updates.wait_next(&mut reactor).unwrap();
//         assert_eq!(selections(&buffer_1), selections(&buffer_2));
//         buffer_3_updates.wait_next(&mut reactor).unwrap();
//         assert_eq!(selections(&buffer_1), selections(&buffer_3));

//         buffer_2
//             .borrow_mut()
//             .mutate_selections(buffer_2_set_id, |buffer, selections| {
//                 for selection in selections {
//                     selection.start = buffer
//                         .anchor_before_offset(
//                             buffer.offset_for_anchor(&selection.start).unwrap() + 1,
//                         )
//                         .unwrap();
//                 }
//             })
//             .unwrap();

//         buffer_1_updates.wait_next(&mut reactor).unwrap();
//         assert_eq!(selections(&buffer_2), selections(&buffer_1));
//         buffer_3_updates.wait_next(&mut reactor).unwrap();
//         assert_eq!(selections(&buffer_2), selections(&buffer_3));

//         buffer_2
//             .borrow_mut()
//             .remove_selection_set(buffer_2_set_id)
//             .unwrap();
//         buffer_2_updates.wait_next(&mut reactor).unwrap();

//         buffer_1_updates.wait_next(&mut reactor).unwrap();
//         assert_eq!(selections(&buffer_1), selections(&buffer_2));
//         buffer_3_updates.wait_next(&mut reactor).unwrap();
//         assert_eq!(selections(&buffer_1), selections(&buffer_3));

//         drop(buffer_3);
//         buffer_1_updates.wait_next(&mut reactor).unwrap();
//         for (replica_id, _, _) in selections(&buffer_1) {
//             assert_eq!(buffer_1.borrow().replica_id, replica_id);
//         }
//     }

//     struct RandomCharIter<T: Rng>(T);

//     impl<T: Rng> Iterator for RandomCharIter<T> {
//         type Item = char;

//         fn next(&mut self) -> Option<Self::Item> {
//             if self.0.gen_weighted_bool(5) {
//                 Some('\n')
//             } else {
//                 Some(self.0.gen_range(b'a', b'z' + 1).into())
//             }
//         }
//     }

//     fn selections(buffer: &Rc<RefCell<Buffer>>) -> Vec<(ReplicaId, SelectionSetId, Selection)> {
//         let buffer = buffer.borrow();

//         let mut selections = Vec::new();
//         for ((replica_id, set_id), selection_set) in &buffer.selections {
//             for selection in selection_set.selections.iter() {
//                 selections.push((*replica_id, *set_id, selection.clone()));
//             }
//         }
//         selections.sort_by(|a, b| match a.0.cmp(&b.0) {
//             Ordering::Equal => a.1.cmp(&b.1),
//             comparison @ _ => comparison,
//         });

//         selections
//     }

//     fn empty_selection(buffer: &Buffer, offset: usize) -> Selection {
//         let anchor = buffer.anchor_before_offset(offset).unwrap();
//         Selection {
//             start: anchor.clone(),
//             end: anchor,
//             reversed: false,
//             goal_column: None,
//         }
//     }

//     fn build_tree() -> TestTree {
//         TestTree::from_json(
//             "/Users/someone/tree",
//             json!({
//                 "root-1": {
//                     "file-1": null,
//                     "subdir-1": {
//                         "file-1": null,
//                         "file-2": null,
//                     }
//                 },
//                 "root-2": {
//                     "subdir-2": {
//                         "file-3": null,
//                         "file-4": null,
//                     }
//                 }
//             }),
//         )
//     }

//     pub fn build_base_buffer_id() -> BufferId {
//         let tree = build_tree();
//         tree.open_buffer(&cross_platform::Path::from("root-1/file-1"))
//             .wait()
//             .unwrap()
//     }
// }
