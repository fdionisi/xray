use std::cell::RefCell;
use std::char::decode_utf16;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::thread;

use futures::{self, Future, IntoFuture, Stream};
use memo_core::WorkTree;
use parking_lot::Mutex;
use uuid;
use xray_core::buffer::BufferId;
use xray_core::cross_platform;
use xray_core::fs as xray_fs;
use xray_core::notify_cell::NotifyCell;

use database;
use git;

pub struct Tree {
    path: cross_platform::Path,
    work_tree: WorkTree,
    root: xray_fs::Entry,
    database: Rc<RefCell<database::Database>>,
    updates: NotifyCell<()>,
    populated: NotifyCell<bool>,
}

pub struct File {
    id: xray_fs::FileId,
    file: Arc<Mutex<fs::File>>,
}

impl Tree {
    pub fn new<T: Into<PathBuf>>(
        path: T,
        git: Rc<git::GitProvider>,
        database: Rc<RefCell<database::Database>>,
    ) -> Result<Self, &'static str> {
        let path = path.into();
        let file_name = OsString::from(path.file_name().ok_or("Path must have a filename")?);
        let root = xray_fs::Entry::dir(file_name.into(), false, false);

        let updates = NotifyCell::new(());

        let start_ops = database.borrow().operations();
        let (work_tree, ops) =
            WorkTree::new(uuid::Uuid::new_v4(), Some(git.head()), start_ops, git, None).unwrap();

        let populated = NotifyCell::new(false);
        Self::populate(
            ops,
            &work_tree,
            root.clone(),
            database.clone(),
            updates.clone(),
            populated.clone(),
        );

        Ok(Self {
            path: cross_platform::Path::from(path.into_os_string()),
            root,
            work_tree,
            database,
            updates,
            populated,
        })
    }

    pub fn digest_operation(&self, envelope: memo_core::OperationEnvelope) {
        self.database.borrow_mut().add(envelope);
        self.updates.set(());
    }

    fn populate(
        ops: Box<(dyn Stream<Error = memo_core::Error, Item = memo_core::OperationEnvelope>)>,
        work_tree: &WorkTree,
        root: xray_fs::Entry,
        database: Rc<RefCell<database::Database>>,
        updates: NotifyCell<()>,
        populated: NotifyCell<bool>,
    ) {
        // TODO: make asyncrhonous
        let _ = ops
            .wait()
            .map(|envelope| database.borrow_mut().add(envelope.unwrap()))
            .collect::<Vec<()>>();

        let mut stack = vec![root];

        work_tree.with_cursor(|cursor| loop {
            let entry = cursor.entry().unwrap();

            if entry.status != memo_core::FileStatus::Removed {
                stack.truncate(entry.depth);
                match entry.file_type {
                    memo_core::FileType::Directory => {
                        let dir = xray_fs::Entry::dir(
                            entry.name.as_os_str().into(),
                            false,
                            !entry.visible,
                        );
                        stack.last_mut().unwrap().insert(dir.clone()).unwrap();
                        stack.push(dir);
                    }
                    memo_core::FileType::Text => {
                        let file = xray_fs::Entry::file(
                            entry.name.as_os_str().into(),
                            false,
                            !entry.visible,
                        );
                        stack.last_mut().unwrap().insert(file).unwrap();
                    }
                }
                updates.set(())
            }

            if !cursor.next(true) {
                break;
            }
        });

        populated.set(true);
    }
}

impl xray_fs::Tree for Tree {
    fn root(&self) -> xray_fs::Entry {
        self.root.clone()
    }

    fn updates(&self) -> Box<Stream<Item = (), Error = ()>> {
        Box::new(self.updates.observe())
    }
}

impl xray_fs::LocalTree for Tree {
    fn path(&self) -> &cross_platform::Path {
        &self.path
    }

    fn populated(&self) -> Box<Future<Item = (), Error = ()>> {
        Box::new(futures::future::ok(()))
    }

    fn as_tree(&self) -> &xray_fs::Tree {
        self
    }

    fn open_file(
        &self,
        relative_path: &cross_platform::Path,
    ) -> Box<Future<Item = Box<xray_fs::File>, Error = io::Error>> {
        let mut path = self.path.clone();
        path.push_path(relative_path);
        let path = path.to_path_buf();

        let (tx, rx) = futures::sync::oneshot::channel();

        thread::spawn(|| {
            fn open(path: PathBuf) -> Result<File, io::Error> {
                Ok(File::new(
                    fs::OpenOptions::new().read(true).write(true).open(path)?,
                )?)
            }

            let _ = tx.send(open(path));
        });

        Box::new(
            rx.then(|result| result.expect("Sender should not be dropped"))
                .map(|file| Box::new(file) as Box<xray_fs::File>),
        )
    }

    fn open_buffer(
        &self,
        path: &cross_platform::Path,
    ) -> Box<Future<Item = memo_core::BufferId, Error = io::Error>> {
        Box::new(
            self.work_tree
                .open_text_file(path.to_path_buf())
                .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("{}", err))),
        )
    }

    fn max_point(&self, buffer_id: BufferId) -> memo_core::Point {
        self.work_tree.max_point(buffer_id).unwrap()
    }

    fn clip_point(&self, buffer_id: BufferId, original: memo_core::Point) -> memo_core::Point {
        self.work_tree.clip_point(buffer_id, original).unwrap()
    }

    fn longest_row(&self, buffer_id: BufferId) -> u32 {
        self.work_tree.longest_row(buffer_id).unwrap()
    }

    fn line(&self, buffer_id: BufferId, row: u32) -> Vec<u16> {
        self.work_tree.line(buffer_id, row).unwrap()
    }

    fn len_for_row(&self, buffer_id: BufferId, row: u32) -> u32 {
        self.work_tree.len_for_row(buffer_id, row).unwrap()
    }

    fn iter_at_point(
        &self,
        buffer_id: BufferId,
        point: memo_core::Point,
    ) -> Box<Iterator<Item = u16>> {
        Box::new(self.work_tree.iter_at_point(buffer_id, point).unwrap())
    }

    fn backward_iter_at_point(
        &self,
        buffer_id: BufferId,
        point: memo_core::Point,
    ) -> Box<Iterator<Item = u16>> {
        Box::new(
            self.work_tree
                .iter_at_point(buffer_id, point)
                .unwrap()
                .rev(),
        )
    }

    fn len(&self, buffer_id: BufferId) -> usize {
        self.work_tree.len(buffer_id).unwrap()
    }

    fn text(&self, buffer_id: BufferId) -> Box<Future<Item = Vec<u16>, Error = io::Error>> {
        Box::new(
            self.work_tree
                .text(buffer_id)
                .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("{}", err)))
                .map(|iter| iter.collect::<Vec<u16>>())
                .into_future(),
        )
    }

    fn edit(
        &self,
        buffer_id: BufferId,
        ranges: Vec<Range<memo_core::Point>>,
        text: String,
    ) -> Box<Future<Item = (), Error = io::Error>> {
        Box::new(
            self.work_tree
                .edit_2d(buffer_id, ranges, text)
                .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("{}", err)))
                .map(|envelope| self.digest_operation(envelope))
                .into_future(),
        )
    }

    fn add_selection_set(
        &self,
        buffer_id: BufferId,
        ranges: Vec<Range<memo_core::Point>>,
    ) -> Box<Future<Item = memo_core::LocalSelectionSetId, Error = io::Error>> {
        Box::new(
            self.work_tree
                .add_selection_set(buffer_id, ranges)
                .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("{}", err)))
                .map(|(set_id, envelope)| {
                    self.digest_operation(envelope);
                    set_id
                })
                .into_future(),
        )
    }

    fn replace_selection_set(
        &self,
        buffer_id: BufferId,
        selection_set_id: memo_core::LocalSelectionSetId,
        ranges: Vec<Range<memo_core::Point>>,
    ) -> Box<Future<Item = (), Error = io::Error>> {
        Box::new(
            self.work_tree
                .replace_selection_set(buffer_id, selection_set_id, ranges)
                .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("{}", err)))
                .map(|envelope| self.digest_operation(envelope))
                .into_future(),
        )
    }

    fn remove_selection_set(
        &self,
        buffer_id: BufferId,
        selection_set_id: memo_core::LocalSelectionSetId,
    ) -> Box<Future<Item = (), Error = io::Error>> {
        Box::new(
            self.work_tree
                .remove_selection_set(buffer_id, selection_set_id)
                .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("{}", err)))
                .map(|envelope| self.digest_operation(envelope))
                .into_future(),
        )
    }

    fn selection_ranges(
        &self,
        buffer_id: BufferId,
    ) -> Result<memo_core::BufferSelectionRanges, io::Error> {
        self.work_tree
            .selection_ranges(buffer_id)
            .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("{}", err)))
    }
}

impl File {
    fn new(file: fs::File) -> Result<File, io::Error> {
        Ok(File {
            id: file.metadata()?.ino(),
            file: Arc::new(Mutex::new(file)),
        })
    }
}

impl xray_fs::File for File {
    fn id(&self) -> xray_fs::FileId {
        self.id
    }

    fn read(&self) -> Box<Future<Item = String, Error = io::Error>> {
        let (tx, rx) = futures::sync::oneshot::channel();
        let file = self.file.clone();
        thread::spawn(move || {
            fn read(file: &fs::File) -> Result<String, io::Error> {
                let mut buf_reader = io::BufReader::new(file);
                let mut contents = String::new();
                buf_reader.read_to_string(&mut contents)?;
                Ok(contents)
            }

            let _ = tx.send(read(&file.lock()));
        });

        Box::new(rx.then(|result| result.expect("Sender should not be dropped")))
    }

    fn write_text(
        &self,
        text: Box<Future<Item = Vec<u16>, Error = io::Error>>,
    ) -> Box<Future<Item = (), Error = io::Error>> {
        let (tx, rx) = futures::sync::oneshot::channel();
        let file = self.file.clone();
        let text = text.wait().unwrap();
        thread::spawn(move || {
            fn write(file: &mut fs::File, text: Vec<u16>) -> Result<(), io::Error> {
                let mut size = 0_u64;
                {
                    let mut buf_writer = io::BufWriter::new(&mut *file);
                    buf_writer.seek(SeekFrom::Start(0))?;
                    for character in decode_utf16(text) {
                        let character = character.map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "buffer did not contain valid UTF-8",
                            )
                        })?;
                        let mut encode_buf = [0_u8; 4];
                        let encoded_char = character.encode_utf8(&mut encode_buf);
                        buf_writer.write(encoded_char.as_bytes())?;
                        size += encoded_char.len() as u64;
                    }
                }
                file.set_len(size)?;
                Ok(())
            }

            let _ = tx.send(write(&mut file.lock(), text));
        });
        Box::new(rx.then(|result| result.expect("Sender should not be dropped")))
    }
}
