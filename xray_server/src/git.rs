use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use futures::{future, stream, Future, Stream};
use git2::{ObjectType, Repository, Tree};
use xray_core::fs::{DirEntry, FileType};
use xray_core::git::{self, Oid};

pub struct GitProvider {
    pub path: PathBuf,
    repo: Rc<Repository>,
}

impl GitProvider {
    pub fn new<P: AsRef<Path>>(path: P) -> GitProvider {
        let repo = Rc::new(Repository::open(&path).expect("Can't open repository"));

        GitProvider {
            path: path.as_ref().to_path_buf(),
            repo,
        }
    }

    pub fn head(&self) -> Oid {
        let raw_oid = self
            .repo
            .head()
            .expect("Can't retrieve head")
            .target()
            .expect("Not direct reference");

        let mut oid: [u8; 20] = Default::default();
        oid.copy_from_slice(&raw_oid.as_bytes()[0..20]);

        oid
    }
}

impl git::GitProvider for GitProvider {
    fn base_entries(&self, oid: Oid) -> Box<dyn Stream<Item = DirEntry, Error = io::Error>> {
        Box::new(stream::iter_ok(
            git2::Oid::from_bytes(&oid)
                .and_then(|oid| self.repo.find_commit(oid))
                .and_then(|commit| commit.tree())
                .and_then(|tree| Ok(visit_dirs(&self.repo, tree, 1)))
                .unwrap(),
        ))
    }

    fn base_text(
        &self,
        oid: Oid,
        path: &Path,
    ) -> Box<dyn Future<Item = String, Error = io::Error>> {
        Box::new(future::ok(
            git2::Oid::from_bytes(&oid)
                .and_then(|oid| self.repo.find_commit(oid))
                .and_then(|commit| commit.tree())
                .and_then(|tree| tree.get_path(path))
                .and_then(|entry| self.repo.find_blob(entry.id()))
                .and_then(|blob| {
                    let content = blob.content();
                    let base_text = String::from_utf8(content.to_vec());
                    Ok(base_text.unwrap())
                })
                .map_err(|_| std::io::Error::from(std::io::ErrorKind::Other))
                .unwrap(),
        ))
    }
}

fn visit_dirs(repo: &Repository, tree: Tree, depth: usize) -> Vec<DirEntry> {
    tree.iter()
        .map(|entry| {
            let name = entry.name().unwrap().to_string();

            let name = OsString::from(name.to_string());
            match entry.kind() {
                Some(ObjectType::Blob) => vec![DirEntry {
                    depth,
                    name,
                    file_type: FileType::Text,
                }],
                Some(ObjectType::Tree) => vec![
                    vec![DirEntry {
                        depth,
                        name,
                        file_type: FileType::Directory,
                    }],
                    visit_dirs(repo, repo.find_tree(entry.id()).unwrap(), depth + 1),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<DirEntry>>(),
                _ => panic!("not supported"),
            }
        })
        .flatten()
        .collect::<Vec<DirEntry>>()
}
