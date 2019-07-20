use std::cmp;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;

use futures::Stream;
pub use memo_core::{
    BufferId, BufferSelectionRanges as SelectionRanges, LocalSelectionSetId as SelectionSetId,
    Point,
};

use crate::change_observer::ChangeObserver;
use crate::notify_cell::NotifyCell;
use crate::work_tree::BufferHandler;
use crate::Error;

pub struct Buffer {
    id: BufferId,
    handle: Rc<BufferHandler>,
    observer: Rc<ChangeObserver>,
    updates: NotifyCell<()>,
}

impl Buffer {
    pub(crate) fn new(
        id: BufferId,
        handle: Rc<BufferHandler>,
        observer: Rc<ChangeObserver>,
    ) -> Self {

        Self {
            id,
            handle,
            observer,
            updates: NotifyCell::new(()),
        }
    }

    pub fn id(&self) -> BufferId {
        self.id
    }

    pub fn edit_2d<T: AsRef<str>>(
        &self,
        old_ranges: Vec<Range<Point>>,
        new_text: T,
    ) -> Result<(), Error> {
        self.handle
            .edit_2d(self.id, old_ranges, new_text.as_ref())?;

        Ok(self.updates.set(()))
    }

    pub fn edit<T: AsRef<str>>(
        &self,
        old_ranges: Vec<Range<usize>>,
        new_text: T,
    ) -> Result<(), Error> {
        self.handle.edit(self.id, old_ranges, new_text.as_ref())?;

        Ok(self.updates.set(()))
    }

    pub fn add_selection_set(&self, ranges: Vec<Range<Point>>) -> Result<SelectionSetId, Error> {
        let set_id = self.handle.add_selection_set(self.id, ranges)?;

        self.updates.set(());

        Ok(set_id)
    }

    pub fn replace_selection_set(
        &self,
        set_id: SelectionSetId,
        ranges: Vec<Range<Point>>,
    ) -> Result<(), Error> {
        self.handle.replace_selection_set(self.id, set_id, ranges)?;

        Ok(self.updates.set(()))
    }

    pub fn remove_selection_set(&self, set_id: SelectionSetId) -> Result<(), Error> {
        self.handle.remove_selection_set(self.id, set_id)
    }

    pub fn mutate_selections<F>(&self, set_id: SelectionSetId, f: F) -> Result<(), Error>
    where
        F: FnOnce(&Buffer, &mut Vec<Range<Point>>) -> Vec<Range<Point>>,
    {
        self.replace_selection_set(set_id, f(&self, &mut self.selections(set_id)?))
    }

    pub fn path(&self) -> Option<PathBuf> {
        self.handle.path(self.id)
    }

    pub fn text(&self) -> Result<Vec<u16>, Error> {
        self.handle.text(self.id)
    }

    #[cfg(test)]
    pub fn to_string(&self) -> String {
        String::from_utf16_lossy(self.text().unwrap().as_slice())
    }

    pub fn selection_ranges(&self) -> Result<SelectionRanges, Error> {
        self.handle.selection_ranges(self.id)
    }

    pub fn buffer_deferred_ops_len(&self) -> Result<usize, Error> {
        self.handle.buffer_deferred_ops_len(self.id)
    }

    pub fn updates(&self) -> Box<dyn Stream<Item = (), Error = ()>> {
        let observer = self.updates.observe();
        Box::new(observer.select(self.observer.updates(self.id)))
    }

    pub fn len(&self) -> Result<usize, Error> {
        self.handle.len(self.id)
    }

    pub fn len_for_row(&self, row: u32) -> Result<u32, Error> {
        self.handle.len_for_row(self.id, row)
    }

    pub fn longest_row(&self) -> Result<u32, Error> {
        self.handle.longest_row(self.id)
    }

    pub fn max_point(&self) -> Result<Point, Error> {
        self.handle.max_point(self.id)
    }

    pub fn clip_point(&self, original: Point) -> Result<Point, Error> {
        let max_point = self.max_point()?;
        Ok(cmp::max(cmp::min(original, max_point), Point::new(0, 0)))
    }

    pub fn line(&self, row: u32) -> Result<Vec<u16>, Error> {
        self.handle.line(self.id, row)
    }

    pub fn iter_at_point(&self, point: Point) -> Result<impl Iterator<Item = u16>, Error> {
        self.handle.iter_at_point(self.id, point)
    }

    pub fn backward_iter_at_point(&self, point: Point) -> Result<impl Iterator<Item = u16>, Error> {
        self.handle.backward_iter_at_point(self.id, point)
    }

    pub fn selections(&self, selection_set_id: SelectionSetId) -> Result<Vec<Range<Point>>, Error> {
        if let Some(ranges) = self.selection_ranges()?.local.get(&selection_set_id) {
            Ok(ranges.clone())
        } else {
            Err(Error::from(memo_core::Error::InvalidLocalSelectionSet(
                selection_set_id,
            )))
        }
    }
}

#[cfg(test)]
pub mod tests {
    use std::ops::Range;
    use std::path::PathBuf;
    use std::rc::Rc;

    use futures::Future;
    use rand::{Rng, SeedableRng, StdRng};

    use super::{Buffer, Point};

    use crate::{
        work_tree::{tests::TestWorkTree, WorkTree},
        Error,
    };

    #[test]
    fn test_edit() -> Result<(), Error> {
        let buffer = Buffer::basic();
        buffer.edit(vec![0..0], "abc")?;
        assert_eq!(buffer.to_string(), "abc");
        buffer.edit(vec![3..3], "def")?;
        assert_eq!(buffer.to_string(), "abcdef");
        buffer.edit(vec![0..0], "ghi")?;
        assert_eq!(buffer.to_string(), "ghiabcdef");
        buffer.edit(vec![5..5], "jkl")?;
        assert_eq!(buffer.to_string(), "ghiabjklcdef");
        buffer.edit(vec![6..7], "")?;
        assert_eq!(buffer.to_string(), "ghiabjlcdef");
        buffer.edit(vec![4..9], "mno")?;
        assert_eq!(buffer.to_string(), "ghiamnoef");

        Ok(())
    }

    #[test]
    fn test_random_edits() -> Result<(), Error> {
        for seed in 0..100 {
            println!("{:?}", seed);
            let mut rng = StdRng::from_seed(&[seed]);

            let buffer = Buffer::basic();
            let mut reference_string = String::new();

            for _i in 0..10 {
                let mut old_ranges: Vec<Range<usize>> = Vec::new();
                for _ in 0..5 {
                    let last_end = old_ranges.last().map_or(0, |last_range| last_range.end + 1);
                    if last_end > buffer.len()? {
                        break;
                    }
                    let end = rng.gen_range::<usize>(last_end, buffer.len()? + 1);
                    let start = rng.gen_range::<usize>(last_end, end + 1);
                    old_ranges.push(start..end);
                }
                let new_text = RandomCharIter(rng)
                    .take(rng.gen_range(0, 10))
                    .collect::<String>();

                let string = String::from(new_text);
                buffer.edit(old_ranges.clone(), string.as_str())?;
                for old_range in old_ranges.iter().rev() {
                    reference_string = [
                        &reference_string[0..old_range.start],
                        string.as_str(),
                        &reference_string[old_range.end..],
                    ]
                    .concat();
                }
                assert_eq!(buffer.to_string(), reference_string);
            }
        }

        Ok(())
    }

    #[test]
    fn test_len_for_row() -> Result<(), Error> {
        let buffer = Buffer::basic();
        buffer.edit(vec![0..0], "abcd\nefg\nhij")?;
        buffer.edit(vec![12..12], "kl\nmno")?;
        buffer.edit(vec![18..18], "\npqrs\n")?;
        buffer.edit(vec![18..21], "\nPQ")?;

        assert_eq!(buffer.len_for_row(0), Ok(4));
        assert_eq!(buffer.len_for_row(1), Ok(3));
        assert_eq!(buffer.len_for_row(2), Ok(5));
        assert_eq!(buffer.len_for_row(3), Ok(3));
        assert_eq!(buffer.len_for_row(4), Ok(4));
        assert_eq!(buffer.len_for_row(5), Ok(0));
        assert_eq!(
            buffer.len_for_row(6),
            Err(Error::from(memo_core::Error::OffsetOutOfRange))
        );

        Ok(())
    }

    #[test]
    fn test_longest_row() -> Result<(), Error> {
        let buffer = Buffer::basic();
        assert_eq!(buffer.longest_row(), Ok(0));
        buffer.edit(vec![0..0], "abcd\nefg\nhij")?;
        assert_eq!(buffer.longest_row(), Ok(0));
        buffer.edit(vec![12..12], "kl\nmno")?;
        assert_eq!(buffer.longest_row(), Ok(2));
        buffer.edit(vec![18..18], "\npqrs")?;
        assert_eq!(buffer.longest_row(), Ok(2));
        buffer.edit(vec![10..12], "")?;
        assert_eq!(buffer.longest_row(), Ok(0));
        buffer.edit(vec![24..24], "tuv")?;
        assert_eq!(buffer.longest_row(), Ok(4));

        Ok(())
    }

    #[test]
    fn iter_at_point() -> Result<(), Error> {
        let buffer = Buffer::basic();
        buffer.edit(vec![0..0], "abcd\nefgh\nij")?;
        buffer.edit(vec![12..12], "kl\nmno")?;
        buffer.edit(vec![18..18], "\npqrs")?;
        buffer.edit(vec![18..21], "\nPQ")?;

        let iter = buffer.iter_at_point(Point::new(0, 0))?;
        assert_eq!(
            String::from_utf16_lossy(&iter.collect::<Vec<u16>>()),
            "abcd\nefgh\nijkl\nmno\nPQrs"
        );

        let iter = buffer.iter_at_point(Point::new(1, 0))?;
        assert_eq!(
            String::from_utf16_lossy(&iter.collect::<Vec<u16>>()),
            "efgh\nijkl\nmno\nPQrs"
        );

        let iter = buffer.iter_at_point(Point::new(2, 0))?;
        assert_eq!(
            String::from_utf16_lossy(&iter.collect::<Vec<u16>>()),
            "ijkl\nmno\nPQrs"
        );

        let iter = buffer.iter_at_point(Point::new(3, 0))?;
        assert_eq!(
            String::from_utf16_lossy(&iter.collect::<Vec<u16>>()),
            "mno\nPQrs"
        );

        let iter = buffer.iter_at_point(Point::new(4, 0))?;
        assert_eq!(
            String::from_utf16_lossy(&iter.collect::<Vec<u16>>()),
            "PQrs"
        );

        let iter = buffer.iter_at_point(Point::new(5, 0))?;
        assert_eq!(String::from_utf16_lossy(&iter.collect::<Vec<u16>>()), "");

        // Regression test:
        let buffer = Buffer::basic();
        buffer.edit(vec![0..0], "[workspace]\nmembers = [\n    \"xray_core\",\n    \"xray_server\",\n    \"xray_cli\",\n    \"xray_wasm\",\n]\n")?;
        buffer.edit(vec![60..60], "\n")?;

        let iter = buffer.iter_at_point(Point::new(6, 0))?;
        assert_eq!(
            String::from_utf16_lossy(&iter.collect::<Vec<u16>>()),
            "    \"xray_wasm\",\n]\n"
        );

        Ok(())
    }

    #[test]
    fn backward_iter_at_point() -> Result<(), Error> {
        let buffer = Buffer::basic();
        buffer.edit(vec![0..0], "abcd\nefgh\nij")?;
        buffer.edit(vec![12..12], "kl\nmno")?;
        buffer.edit(vec![18..18], "\npqrs")?;
        buffer.edit(vec![18..21], "\nPQ")?;

        let iter = buffer.backward_iter_at_point(Point::new(0, 0))?;
        assert_eq!(String::from_utf16_lossy(&iter.collect::<Vec<u16>>()), "");

        let iter = buffer.backward_iter_at_point(Point::new(0, 3))?;
        assert_eq!(String::from_utf16_lossy(&iter.collect::<Vec<u16>>()), "cba");

        let iter = buffer.backward_iter_at_point(Point::new(1, 4))?;
        assert_eq!(
            String::from_utf16_lossy(&iter.collect::<Vec<u16>>()),
            "hgfe\ndcba"
        );

        let iter = buffer.backward_iter_at_point(Point::new(3, 2))?;
        assert_eq!(
            String::from_utf16_lossy(&iter.collect::<Vec<u16>>()),
            "nm\nlkji\nhgfe\ndcba"
        );

        let iter = buffer.backward_iter_at_point(Point::new(4, 4))?;
        assert_eq!(
            String::from_utf16_lossy(&iter.collect::<Vec<u16>>()),
            "srQP\nonm\nlkji\nhgfe\ndcba"
        );

        let iter = buffer.backward_iter_at_point(Point::new(5, 0))?;
        assert_eq!(
            String::from_utf16_lossy(&iter.collect::<Vec<u16>>()),
            "srQP\nonm\nlkji\nhgfe\ndcba"
        );

        Ok(())
    }

    #[test]
    fn test_clip_point() -> Result<(), Error> {
        let buffer = Buffer::basic();
        buffer.edit(vec![0..0], "abcdefghi")?;

        let point = buffer.clip_point(Point::new(0, 0))?;
        assert_eq!(point.row, 0);
        assert_eq!(point.column, 0);

        let point = buffer.clip_point(Point::new(0, 2))?;
        assert_eq!(point.row, 0);
        assert_eq!(point.column, 2);

        let point = buffer.clip_point(Point::new(1, 12))?;
        assert_eq!(point.row, 0);
        assert_eq!(point.column, 9);

        Ok(())
    }

    #[test]
    fn test_text() -> Result<(), Error> {
        let buffer = Buffer::basic();
        buffer.edit(vec![0..0], "abcdefghi")?;
        buffer.edit(vec![3..6], "DEF")?;

        assert_eq!(buffer.to_string(), String::from("abcDEFghi"));
        assert_eq!(
            String::from_utf16_lossy(buffer.text()?.as_slice()),
            String::from("abcDEFghi")
        );

        buffer.edit(vec![0..1], "A")?;
        buffer.edit(vec![8..9], "I")?;
        assert_eq!(buffer.to_string(), String::from("AbcDEFghI"));
        assert_eq!(
            String::from_utf16_lossy(buffer.text()?.as_slice()),
            String::from("AbcDEFghI")
        );

        Ok(())
    }

    // #[test]
    // fn test_random_concurrent_edits() -> Result<(), Error> {
    //     for seed in 0..100 {
    //         println!("{:?}", seed);
    //         let mut rng = StdRng::from_seed(&[seed]);
    //
    //         let site_range = 0..5;
    //         let mut buffers = Vec::new();
    //         let mut queues = Vec::new();
    //         for i in site_range.clone() {
    //             let mut buffer = Buffer::new(0);
    //             buffer.replica_id = i + 1;
    //             buffers.push(buffer);
    //             queues.push(Vec::new());
    //         }
    //
    //         let mut edit_count = 10;
    //         loop {
    //             let replica_index = rng.gen_range::<usize>(site_range.start, site_range.end);
    //             let buffer = &mut buffers[replica_index];
    //             if edit_count > 0 && rng.gen() {
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
    //
    //                 for op in buffer.edit(&old_ranges, new_text.as_str()) {
    //                     for (index, queue) in queues.iter_mut().enumerate() {
    //                         if index != replica_index {
    //                             queue.push(op.clone());
    //                         }
    //                     }
    //                 }
    //
    //                 edit_count -= 1;
    //             } else if !queues[replica_index].is_empty() {
    //                 buffer
    //                     .integrate_op(queues[replica_index].remove(0))
    //                     .unwrap();
    //             }
    //
    //             if edit_count == 0 && queues.iter().all(|q| q.is_empty()) {
    //                 break;
    //             }
    //         }
    //
    //         for buffer in &buffers[1..] {
    //             assert_eq!(buffer.to_string(), buffers[0].to_string());
    //         }
    //     }
    //     Ok(())
    // }

    // #[test]
    // fn test_edit_replication() {
    //     let local_buffer = Buffer::new(0).into_shared();
    //     local_buffer.borrow_mut().edit(&[0..0], "abcdef");
    //     local_buffer.borrow_mut().edit(&[2..4], "ghi");
    //
    //     let mut reactor = reactor::Core::new().unwrap();
    //     let foreground = Rc::new(reactor.handle());
    //     let client_1 =
    //         rpc::tests::connect(&mut reactor, super::rpc::Service::new(local_buffer.clone()));
    //     let remote_buffer_1 = Buffer::remote(foreground.clone(), client_1).unwrap();
    //     let client_2 =
    //         rpc::tests::connect(&mut reactor, super::rpc::Service::new(local_buffer.clone()));
    //     let remote_buffer_2 = Buffer::remote(foreground, client_2).unwrap();
    //     assert_eq!(
    //         remote_buffer_1.borrow().to_string(),
    //         local_buffer.borrow().to_string()
    //     );
    //     assert_eq!(
    //         remote_buffer_2.borrow().to_string(),
    //         local_buffer.borrow().to_string()
    //     );
    //
    //     local_buffer.borrow_mut().edit(&[3..6], "jk");
    //     remote_buffer_1.borrow_mut().edit(&[7..7], "lmn");
    //     let anchor = remote_buffer_1.borrow().anchor_before_offset(8).unwrap();
    //
    //     let mut remaining_tries = 10;
    //     while remote_buffer_1.borrow().to_string() != local_buffer.borrow().to_string()
    //         || remote_buffer_2.borrow().to_string() != local_buffer.borrow().to_string()
    //     {
    //         remaining_tries -= 1;
    //         assert!(
    //             remaining_tries > 0,
    //             "Ran out of patience waiting for buffers to converge"
    //         );
    //         reactor.turn(Some(Duration::from_millis(0)));
    //     }
    //
    //     assert_eq!(local_buffer.borrow().offset_for_anchor(&anchor).unwrap(), 7);
    //     assert_eq!(
    //         remote_buffer_1.borrow().offset_for_anchor(&anchor).unwrap(),
    //         7
    //     );
    //     assert_eq!(
    //         remote_buffer_2.borrow().offset_for_anchor(&anchor).unwrap(),
    //         7
    //     );
    // }
    //
    // #[test]
    // fn test_selection_replication() {
    //     use stream_ext::StreamExt;
    //
    //     let mut buffer_1 = Buffer::new(0);
    //     buffer_1.edit(&[0..0], "abcdef");
    //     let sels = vec![empty_selection(&buffer_1, 1), empty_selection(&buffer_1, 3)];
    //     buffer_1.add_selection_set(0, sels);
    //     let sels = vec![empty_selection(&buffer_1, 2), empty_selection(&buffer_1, 4)];
    //     let buffer_1_set_id = buffer_1.add_selection_set(0, sels);
    //     let buffer_1 = buffer_1.into_shared();
    //
    //     let mut reactor = reactor::Core::new().unwrap();
    //     let foreground = Rc::new(reactor.handle());
    //     let buffer_2 = Buffer::remote(
    //         foreground.clone(),
    //         rpc::tests::connect(&mut reactor, super::rpc::Service::new(buffer_1.clone())),
    //     ).unwrap();
    //     assert_eq!(selections(&buffer_1), selections(&buffer_2));
    //
    //     let buffer_3 = Buffer::remote(
    //         foreground,
    //         rpc::tests::connect(&mut reactor, super::rpc::Service::new(buffer_1.clone())),
    //     ).unwrap();
    //     assert_eq!(selections(&buffer_1), selections(&buffer_3));
    //
    //     let mut buffer_1_updates = buffer_1.borrow().updates();
    //     let mut buffer_2_updates = buffer_2.borrow().updates();
    //     let mut buffer_3_updates = buffer_3.borrow().updates();
    //
    //     buffer_1
    //         .borrow_mut()
    //         .mutate_selections(buffer_1_set_id, |buffer, selections| {
    //             for selection in selections {
    //                 selection.start = buffer
    //                     .anchor_before_offset(
    //                         buffer.offset_for_anchor(&selection.start).unwrap() + 1,
    //                     )
    //                     .unwrap();
    //             }
    //         })
    //         .unwrap();
    //     buffer_2_updates.wait_next(&mut reactor).unwrap();
    //     assert_eq!(selections(&buffer_1), selections(&buffer_3));
    //     buffer_3_updates.wait_next(&mut reactor).unwrap();
    //     assert_eq!(selections(&buffer_1), selections(&buffer_3));
    //
    //     buffer_1
    //         .borrow_mut()
    //         .remove_selection_set(buffer_1_set_id)
    //         .unwrap();
    //     buffer_1_updates.wait_next(&mut reactor).unwrap();
    //
    //     buffer_2_updates.wait_next(&mut reactor).unwrap();
    //     assert_eq!(selections(&buffer_1), selections(&buffer_2));
    //     buffer_3_updates.wait_next(&mut reactor).unwrap();
    //     assert_eq!(selections(&buffer_1), selections(&buffer_3));
    //
    //     let sels = vec![empty_selection(&buffer_2.borrow(), 1)];
    //     let buffer_2_set_id = buffer_2.borrow_mut().add_selection_set(0, sels);
    //     buffer_2_updates.wait_next(&mut reactor).unwrap();
    //
    //     buffer_1_updates.wait_next(&mut reactor).unwrap();
    //     assert_eq!(selections(&buffer_1), selections(&buffer_2));
    //     buffer_3_updates.wait_next(&mut reactor).unwrap();
    //     assert_eq!(selections(&buffer_1), selections(&buffer_3));
    //
    //     buffer_2
    //         .borrow_mut()
    //         .mutate_selections(buffer_2_set_id, |buffer, selections| {
    //             for selection in selections {
    //                 selection.start = buffer
    //                     .anchor_before_offset(
    //                         buffer.offset_for_anchor(&selection.start).unwrap() + 1,
    //                     )
    //                     .unwrap();
    //             }
    //         })
    //         .unwrap();
    //
    //     buffer_1_updates.wait_next(&mut reactor).unwrap();
    //     assert_eq!(selections(&buffer_2), selections(&buffer_1));
    //     buffer_3_updates.wait_next(&mut reactor).unwrap();
    //     assert_eq!(selections(&buffer_2), selections(&buffer_3));
    //
    //     buffer_2
    //         .borrow_mut()
    //         .remove_selection_set(buffer_2_set_id)
    //         .unwrap();
    //     buffer_2_updates.wait_next(&mut reactor).unwrap();
    //
    //     buffer_1_updates.wait_next(&mut reactor).unwrap();
    //     assert_eq!(selections(&buffer_1), selections(&buffer_2));
    //     buffer_3_updates.wait_next(&mut reactor).unwrap();
    //     assert_eq!(selections(&buffer_1), selections(&buffer_3));
    //
    //     drop(buffer_3);
    //     buffer_1_updates.wait_next(&mut reactor).unwrap();
    //     for (replica_id, _, _) in selections(&buffer_1) {
    //         assert_eq!(buffer_1.borrow().replica_id, replica_id);
    //     }
    // }

    struct RandomCharIter<T: Rng>(T);

    impl<T: Rng> Iterator for RandomCharIter<T> {
        type Item = char;

        fn next(&mut self) -> Option<Self::Item> {
            if self.0.gen_weighted_bool(5) {
                Some('\n')
            } else {
                Some(self.0.gen_range(b'a', b'z' + 1).into())
            }
        }
    }

    pub trait TestBuffer {
        fn basic() -> Rc<Buffer>;
    }

    impl TestBuffer for Buffer {
        fn basic() -> Rc<Buffer> {
            let (_, _, tree, _) = WorkTree::basic(None);
            let basic_buffer = PathBuf::from("a");
            tree.open_text_file(&basic_buffer).wait().unwrap()
        }
    }
}
