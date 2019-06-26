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

use futures::{self, Future, Stream};
use parking_lot::Mutex;
use xray_core::cross_platform;
use xray_core::fs as xray_fs;
use xray_core::git as xray_git;
use xray_core::work_tree;
use xray_core::notify_cell::NotifyCell;
use xray_core::storage::Storage;
use xray_core::ReplicaId;

use database;
use git;

pub struct Tree {
    path: cross_platform::Path,
    root: xray_fs::Entry,
    database: Rc<database::Database>,
    work_tree: Rc<RefCell<work_tree::WorkTree>>,
    git_provider: Rc<xray_git::GitProvider>,
    updates: NotifyCell<()>,
    populated: NotifyCell<bool>,
}

pub struct File {
    id: xray_fs::FileId,
    file: Arc<Mutex<fs::File>>,
}

impl Tree {
    pub fn new<T: Into<PathBuf>>(
        foreground: xray_core::ForegroundExecutor,
        path: T,
        replica_id: ReplicaId,
        git: Rc<git::GitProvider>,
        database: Rc<database::Database>,
    ) -> Result<Self, &'static str> {
        let path = path.into();
        let file_name = OsString::from(path.file_name().ok_or("Path must have a filename")?);
        let root = xray_fs::Entry::dir(file_name.into(), false, false);
        let updates = NotifyCell::new(());
        let populated = NotifyCell::new(false);

        let work_tree =
            work_tree::WorkTree::new(foreground.clone(), database.clone(), replica_id, Some(git.head()), git.clone());

        Self::populate(
            &work_tree,
            root.clone(),
            updates.clone(),
            populated.clone(),
        );

        let work_tree = Rc::new(RefCell::new(work_tree));
        
        {
            database.add_tree(work_tree.clone());
        }

        Ok(Self {
            path: cross_platform::Path::from(path.into_os_string()),
            root,
            updates,
            populated,
            work_tree,
            database,
            git_provider: git,
        })
    }

    fn populate(
        work_tree: &work_tree::WorkTree,
        root: xray_fs::Entry,
        updates: NotifyCell<()>,
        populated: NotifyCell<bool>,
    ) {
        let mut stack = vec![root];
        let work_tree = work_tree.inner();
        work_tree.borrow().with_cursor(|cursor| loop {
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

    fn work_tree(&self) -> Rc<RefCell<work_tree::WorkTree>> {
        self.work_tree.clone()
    }

    fn open_buffer(
        &self,
        path: PathBuf,
    ) -> Box<Future<Item = memo_core::BufferId, Error = io::Error>> {
        Box::new(
            self.work_tree
                .borrow()
                .open_text_file(path)
                .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("{}", err))),
        )
    }
}

impl xray_fs::LocalTree for Tree {
    fn path(&self) -> &cross_platform::Path {
        &self.path
    }

    fn populated(&self) -> Box<Future<Item = (), Error = ()>> {
        Box::new(
            self.populated
                .observe()
                .skip_while(|p| Ok(!p))
                .into_future()
                .then(|_| Ok(())),
        )
    }

    fn as_tree(&self) -> &xray_fs::Tree {
        self
    }

    fn git_provider(&self) -> Rc<xray_git::GitProvider> {
        self.git_provider.clone()
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

    fn write_u16_chars(&self, text: Vec<u16>) -> Box<Future<Item = (), Error = io::Error>> {
        let (tx, rx) = futures::sync::oneshot::channel();
        let file = self.file.clone();
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
