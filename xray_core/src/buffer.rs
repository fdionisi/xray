use std::cell::RefCell;
use std::cmp::{self, Ordering};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;

use fs;
use futures::{future, unsync, Future, Stream};
use notify_cell::{NotifyCell, NotifyCellObserver};
use rpc::{client, Error as RpcError};
use work_tree::WorkTree;
use ForegroundExecutor;
use IntoShared;
use ReplicaId;

pub use memo_core::{
    BufferId, BufferSelectionRanges as SelectionRanges, LocalSelectionSetId as SelectionSetId,
    Operation, OperationEnvelope, Point,
};

#[derive(Eq, PartialEq, Debug, Serialize, Deserialize)]
pub enum Error {
    OffsetOutOfRange,
    InvalidAnchor,
    InvalidOperation,
    SelectionSetNotFound,
    IoError(String),
    RpcError(RpcError),
}

pub struct Buffer {
    id: BufferId,
    work_tree: Rc<RefCell<WorkTree>>,
    client: Option<client::Service<rpc::Service>>,
    operation_txs: RefCell<Vec<unsync::mpsc::UnboundedSender<Operation>>>,
    updates: Rc<NotifyCell<()>>,
    file: Option<Box<fs::File>>,
}

pub mod rpc {
    use super::{Buffer, BufferId, Operation};
    use futures::{Async, Future, Stream};
    use never::Never;
    use rpc;
    use serde::{Deserializer, Serializer};
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Serialize, Deserialize)]
    pub struct State {
        pub(super) id: BufferId,
    }

    #[derive(Serialize, Deserialize)]
    pub enum Request {
        Operation(
            #[serde(serialize_with = "serialize_op", deserialize_with = "deserialize_op")]
            Operation,
        ),
        Save,
    }

    #[derive(Serialize, Deserialize)]
    pub enum Response {
        Operation,
        Saved,
        Error(super::Error),
    }

    #[derive(Serialize, Deserialize)]
    pub enum Update {
        Operation(
            #[serde(serialize_with = "serialize_op", deserialize_with = "deserialize_op")]
            Operation,
        ),
    }

    pub struct Service {
        outgoing_ops: Box<Stream<Item = Operation, Error = ()>>,
        buffer: Rc<RefCell<Buffer>>,
    }

    impl Service {
        pub fn new(buffer: Rc<RefCell<Buffer>>) -> Self {
            let outgoing_ops = buffer.borrow_mut().outgoing_ops();

            Self {
                outgoing_ops: Box::new(outgoing_ops),
                buffer,
            }
        }

        fn poll_outgoing_op(&mut self) -> Async<Option<Update>> {
            self.outgoing_ops
                .poll()
                .expect("Receiving on a channel cannot produce an error")
                .map(|option| option.map(|update| Update::Operation(update)))
        }
    }

    impl rpc::server::Service for Service {
        type State = State;
        type Update = Update;
        type Request = Request;
        type Response = Response;

        fn init(&mut self, _: &rpc::server::Connection) -> Self::State {
            let buffer = self.buffer.borrow_mut();
            State { id: buffer.id }
        }

        fn poll_update(&mut self, _: &rpc::server::Connection) -> Async<Option<Self::Update>> {
            self.poll_outgoing_op()
        }

        fn request(
            &mut self,
            request: Self::Request,
            _connection: &rpc::server::Connection,
        ) -> Option<Box<Future<Item = Self::Response, Error = Never>>> {
            match request {
                Request::Operation(op) => {
                    let mut buffer = self.buffer.borrow_mut();
                    buffer.broadcast_op(&op);
                    Some(Box::new(buffer.integrate_op(op).then(
                        |response| match response {
                            Ok(_) => Ok(Response::Operation),
                            Err(error) => Ok(Response::Error(error)),
                        },
                    )))
                }
                Request::Save => {
                    Some(Box::new(self.buffer.borrow().save().then(
                        |result| match result {
                            Ok(_) => Ok(Response::Saved),
                            Err(error) => Ok(Response::Error(error)),
                        },
                    )))
                }
            }
        }
    }

    // impl Drop for Service {
    //     fn drop(&mut self) {
    //         self.buffer
    //             .borrow_mut()
    //             .remove_remote_selection_sets(self.replica_id);
    //     }
    // }

    struct BytesVisitor;

    impl<'de> serde::de::Visitor<'de> for BytesVisitor {
        type Value = Vec<u8>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(formatter, "Dunno")
        }

        fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(v.to_vec())
        }
    }

    fn serialize_op<S: Serializer>(op: &Operation, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(&op.serialize())
    }

    fn deserialize_op<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Operation, D::Error> {
        let bytes = deserializer.deserialize_bytes(BytesVisitor)?;
        let op = Operation::deserialize(&bytes).unwrap();
        Ok(op.unwrap())
    }
}

impl Buffer {
    pub fn new(id: BufferId, work_tree: Rc<RefCell<WorkTree>>) -> Self {
        Self {
            id,
            work_tree,
            client: None,
            operation_txs: RefCell::new(Vec::new()),
            updates: Rc::new(NotifyCell::new(())),
            file: None,
        }
    }

    pub fn remote(
        foreground: ForegroundExecutor,
        work_tree: Rc<RefCell<WorkTree>>,
        client: client::Service<rpc::Service>,
    ) -> Result<Rc<RefCell<Buffer>>, RpcError> {
        let state = client.state()?;
        let incoming_updates = client.updates()?;

        let buffer = Buffer {
            id: state.id,
            work_tree,
            client: Some(client),
            operation_txs: RefCell::new(Vec::new()),
            updates: Rc::new(NotifyCell::new(())),
            file: None,
        }
        .into_shared();

        let buffer_weak = Rc::downgrade(&buffer);
        foreground
            .execute(Box::new(incoming_updates.for_each(move |update| {
                if let Some(buffer) = buffer_weak.upgrade() {
                    let mut buffer = buffer.borrow_mut();
                    match update {
                        rpc::Update::Operation(operation) => {
                            Box::new(buffer.integrate_op(operation).then(|result| match result {
                                Ok(_) => Ok(()),
                                _ => Err(()),
                            })) as Box<Future<Item = (), Error = ()>>
                        }
                    }
                } else {
                    Box::new(future::ok(())) as Box<Future<Item = (), Error = ()>>
                }
            })))
            .unwrap();

        Ok(buffer)
    }

    pub fn id(&self) -> BufferId {
        self.id
    }

    pub fn file_id(&self) -> Option<fs::FileId> {
        self.file.as_ref().map(|file| file.id())
    }

    pub fn set_file(&mut self, file: Box<fs::File>) {
        self.file = Some(file);
    }

    pub fn save(&self) -> Option<Box<Future<Item = (), Error = Error>>> {
        use std::error;

        if let Some(ref client) = self.client {
            Some(Box::new(client.request(rpc::Request::Save).then(
                |response| match response {
                    Ok(rpc::Response::Saved) => Ok(()),
                    Ok(rpc::Response::Error(error)) => Err(error),
                    Ok(rpc::Response::Operation) => Err(Error::InvalidOperation),
                    Err(error) => Err(Error::RpcError(error)),
                },
            )))
        } else {
            self.file.as_ref().map(|file| {
                Box::new(
                    file.write_u16_chars(self.to_u16_chars()).map_err(|error| {
                        Error::IoError(error::Error::description(&error).to_owned())
                    }),
                ) as Box<Future<Item = (), Error = Error>>
            })
        }
    }

    pub fn len(&self) -> usize {
        self.work_tree.borrow().len(self.id)
    }

    pub fn len_for_row(&self, row: u32) -> u32 {
        self.work_tree.borrow().len_for_row(self.id, row)
    }

    pub fn longest_row(&self) -> u32 {
        self.work_tree.borrow().longest_row(self.id)
    }

    pub fn max_point(&self) -> Point {
        self.work_tree.borrow().max_point(self.id)
    }

    pub fn clip_point(&self, original: Point) -> Point {
        return cmp::max(cmp::min(original, self.max_point()), Point::new(0, 0));
    }

    pub fn line(&self, row: u32) -> Vec<u16> {
        self.work_tree.borrow().line(self.id, row)
    }

    pub fn to_u16_chars(&self) -> Vec<u16> {
        self.work_tree.borrow().to_u16_chars(self.id)
    }

    #[cfg(test)]
    pub fn to_string(&self) -> String {
        String::from_utf16_lossy(self.to_u16_chars().as_slice())
    }

    pub fn iter_at_point(&self, point: Point) -> impl Iterator<Item = u16> {
        self.work_tree.borrow().iter_at_point(self.id, point)
    }

    pub fn backward_iter_at_point(&self, point: Point) -> impl Iterator<Item = u16> {
        self.work_tree
            .borrow()
            .backward_iter_at_point(self.id, point)
    }

    pub fn edit<I>(&self, old_ranges: I, new_text: &str)
    where
        I: IntoIterator<Item = Range<Point>>,
    {
        self.work_tree.borrow().edit(self.id, old_ranges, new_text);

        self.updates.set(());
    }

    pub fn add_selection_set<I>(&self, ranges: I) -> SelectionSetId
    where
        I: IntoIterator<Item = Range<Point>>,
    {
        let selection_set_id = self.work_tree.borrow().add_selection_set(self.id, ranges);

        self.updates.set(());

        selection_set_id
    }

    pub fn replace_selection_set<I>(&self, selection_set_id: SelectionSetId, ranges: I)
    where
        I: IntoIterator<Item = Range<Point>>,
    {
        self.work_tree
            .borrow()
            .replace_selection_set(self.id, selection_set_id, ranges);

        self.updates.set(())
    }

    pub fn remove_selection_set(&self, selection_set_id: SelectionSetId) {
        self.work_tree
            .borrow()
            .remove_selection_set(self.id, selection_set_id);

        self.updates.set(())
    }

    fn selection_ranges(&self) -> SelectionRanges {
        self.work_tree.borrow().selection_ranges(self.id)
    }

    fn local_selections(&self) -> HashMap<SelectionSetId, Vec<Range<Point>>> {
        self.selection_ranges().local
    }

    pub fn selections(&self, selection_set_id: SelectionSetId) -> Vec<Range<Point>> {
        self.local_selections()
            .get(&selection_set_id)
            .unwrap()
            .clone()
    }

    pub fn insert_selections<F>(&self, set_id: SelectionSetId, f: F)
    where
        F: FnOnce(&Buffer, &Vec<Range<Point>>) -> Vec<Range<Point>>,
    {
        self.mutate_selections(set_id, |buffer, old_selections| {
            let mut new_selections = f(buffer, old_selections);
            new_selections.sort_unstable_by(|a, b| a.start.cmp(&b.start));

            let mut selections = Vec::with_capacity(old_selections.len() + new_selections.len());
            {
                let mut old_selections = old_selections.drain(..).peekable();
                let mut new_selections = new_selections.drain(..).peekable();
                loop {
                    if old_selections.peek().is_some() {
                        if new_selections.peek().is_some() {
                            match old_selections
                                .peek()
                                .unwrap()
                                .start
                                .cmp(&new_selections.peek().unwrap().start)
                            {
                                Ordering::Less => {
                                    selections.push(old_selections.next().unwrap());
                                }
                                Ordering::Equal => {
                                    selections.push(old_selections.next().unwrap());
                                    selections.push(new_selections.next().unwrap());
                                }
                                Ordering::Greater => {
                                    selections.push(new_selections.next().unwrap());
                                }
                            }
                        } else {
                            selections.push(old_selections.next().unwrap());
                        }
                    } else if new_selections.peek().is_some() {
                        selections.push(new_selections.next().unwrap());
                    } else {
                        break;
                    }
                }
            }

            selections
        })
    }

    pub fn mutate_selections<F>(&self, set_id: SelectionSetId, f: F)
    where
        F: FnOnce(&Buffer, &mut Vec<Range<Point>>) -> Vec<Range<Point>>,
    {
        self.replace_selection_set(set_id, f(&self, &mut self.selections(set_id).clone()));
    }

    pub fn remote_selections(&self) -> HashMap<ReplicaId, Vec<Vec<Range<Point>>>> {
        self.selection_ranges().remote
    }

    pub fn updates(&self) -> Box<Stream<Item = (), Error = ()>> {
        Box::new(
            self.work_tree
                .borrow()
                .buffer_updates(self.id)
                .select(self.updates.observe()),
        )
    }

    fn broadcast_op(&self, op: &Operation) {
        let mut operation_txs = self.operation_txs.borrow_mut();
        for i in (0..operation_txs.len()).rev() {
            if operation_txs[i].unbounded_send(op.clone()).is_err() {
                operation_txs.swap_remove(i);
            }
        }

        if let Some(ref client) = self.client {
            client.request(rpc::Request::Operation(op.clone()));
        }
    }

    fn integrate_op(&self, op: Operation) -> Box<Future<Item = (), Error = Error>> {
        let updates = self.updates.clone();

        Box::new(future::ok(self.work_tree.borrow().apply_ops(vec![op])))
    }

    fn outgoing_ops(&self) -> unsync::mpsc::UnboundedReceiver<Operation> {
        let mut operation_txs = self.operation_txs.borrow_mut();
        let (tx, rx) = unsync::mpsc::unbounded();
        operation_txs.push(tx);
        rx
    }
}

// #[cfg(test)]
// mod tests {
//     extern crate rand;

//     use self::rand::{Rng, SeedableRng, StdRng};
//     use super::*;
//     use rpc;
//     use std::time::Duration;
//     use tokio_core::reactor;
//     use IntoShared;

//     #[test]
//     fn test_edit() {
//         let mut buffer = Buffer::new(0);
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
//         for seed in 0..100 {
//             println!("{:?}", seed);
//             let mut rng = StdRng::from_seed(&[seed]);

//             let mut buffer = Buffer::new(0);
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
//                     ].concat();
//                 }
//                 assert_eq!(buffer.to_string(), reference_string);
//             }
//         }
//     }

//     #[test]
//     fn test_len_for_row() {
//         let mut buffer = Buffer::new(0);
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
//         let mut buffer = Buffer::new(0);
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
//         let mut buffer = Buffer::new(0);
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
//         let mut buffer = Buffer::new(0);
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
//         let mut buffer = Buffer::new(0);
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
//         let mut buffer = Buffer::new(0);
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
//         let mut buffer = Buffer::new(0);
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
//         let mut buffer = Buffer::new(0);
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
//         let mut buffer = Buffer::new(0);
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
//                 let mut buffer = Buffer::new(0);
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
//         let local_buffer = Buffer::new(0).into_shared();
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

//         let mut buffer_1 = Buffer::new(0);
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
//         ).unwrap();
//         assert_eq!(selections(&buffer_1), selections(&buffer_2));

//         let buffer_3 = Buffer::remote(
//             foreground,
//             rpc::tests::connect(&mut reactor, super::rpc::Service::new(buffer_1.clone())),
//         ).unwrap();
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
// }
