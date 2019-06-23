use database;
use futures::{self, Future, Stream};
use git;
use memo_core::WorkTree;
use parking_lot::Mutex;
use std::cell::RefCell;
use std::char::decode_utf16;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::thread;
use uuid;
use xray_core::buffer::BufferSnapshot;
use xray_core::cross_platform;
use xray_core::fs as xray_fs;
use xray_core::notify_cell::NotifyCell;

pub struct Tree {
    path: cross_platform::Path,
    work_tree: WorkTree,
    database: Rc<RefCell<database::Database>>,
    updates: NotifyCell<()>,
    populated: NotifyCell<bool>,
}

pub struct FileProvider;

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
        Self::populate(ops, database.clone(), populated.clone());

        Ok(Self {
            path: cross_platform::Path::from(path.into_os_string()),
            work_tree,
            database,
            updates,
            populated,
        })
    }

    fn populate(
        ops: Box<(dyn Stream<Error = memo_core::Error, Item = memo_core::OperationEnvelope>)>,
        database: Rc<RefCell<database::Database>>,
        populated: NotifyCell<bool>,
    ) {
        // TODO: make asyncrhonous
        let _ = ops
            .wait()
            .map(|envelope| database.borrow_mut().add(envelope.unwrap()))
            .collect::<Vec<()>>();

        populated.set(true);
    }
}

impl xray_fs::Tree for Tree {
    fn root(&self) -> xray_fs::Entry {
        let root_file_name = OsString::from(
            self.path
                .to_path_buf()
                .file_name()
                .expect("Path must have a filename"),
        );

        let root = xray_fs::Entry::dir(root_file_name.into(), false, false);
        let mut stack = vec![root];

        self.work_tree.with_cursor(|cursor| loop {
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
            }

            if !cursor.next(true) {
                break;
            }
        });

        let root = stack.first().unwrap();

        root.clone()
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
}

impl FileProvider {
    pub fn new() -> Self {
        FileProvider
    }
}

impl xray_fs::FileProvider for FileProvider {
    fn open(
        &self,
        path: &cross_platform::Path,
    ) -> Box<Future<Item = Box<xray_fs::File>, Error = io::Error>> {
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

    fn write_snapshot(
        &self,
        snapshot: BufferSnapshot,
    ) -> Box<Future<Item = (), Error = io::Error>> {
        let (tx, rx) = futures::sync::oneshot::channel();
        let file = self.file.clone();
        thread::spawn(move || {
            fn write(file: &mut fs::File, snapshot: BufferSnapshot) -> Result<(), io::Error> {
                let mut size = 0_u64;
                {
                    let mut buf_writer = io::BufWriter::new(&mut *file);
                    buf_writer.seek(SeekFrom::Start(0))?;
                    for character in snapshot
                        .iter()
                        .flat_map(|c| decode_utf16(c.iter().cloned()))
                    {
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

            let _ = tx.send(write(&mut file.lock(), snapshot));
        });
        Box::new(rx.then(|result| result.expect("Sender should not be dropped")))
    }
}
