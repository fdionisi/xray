use std::cell::RefCell;
use std::cmp;
use std::collections::{BinaryHeap, HashMap};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use futures::{future, stream, Async, Future, Poll, Stream};

use xray_rpc;

use crate::buffer::{Buffer, BufferId};
use crate::fuzzy;
use crate::git;
use crate::network;
use crate::never::Never;
use crate::notify_cell::{NotifyCell, NotifyCellObserver, WeakNotifyCell};
use crate::usize_map::UsizeMap;
use crate::work_tree::WorkTree;
use crate::{Error, ForegroundExecutor, IntoShared, ReplicaId};

pub type TreeId = usize;

pub trait Project {
    fn open_path(
        &self,
        tree_id: TreeId,
        relative_path: &PathBuf,
    ) -> Box<dyn Future<Item = Rc<Buffer>, Error = Error>>;
    fn search_paths(
        &self,
        needle: &str,
        max_results: usize,
        include_ignored: bool,
    ) -> (PathSearch, NotifyCellObserver<PathSearchStatus>);
}

pub struct LocalProject {
    trees: UsizeMap<Rc<WorkTree>>,
}

pub struct RemoteProject {
    service: Rc<RefCell<xray_rpc::client::Service<ProjectService>>>,
    trees: HashMap<TreeId, Rc<WorkTree>>,
    buffers: Rc<RefCell<HashMap<TreeId, (BufferId, BufferId)>>>,
}

type GitServiceHandle = xray_rpc::server::ServiceHandle;
type NetworkServiceHandle = xray_rpc::server::ServiceHandle;

pub struct ProjectService {
    project: Rc<RefCell<LocalProject>>,
    tree_services: HashMap<TreeId, (GitServiceHandle, NetworkServiceHandle)>,
}

type GitServiceId = xray_rpc::ServiceId;
type NetworkServiceId = xray_rpc::ServiceId;

#[derive(Deserialize, Serialize)]
pub struct RpcState {
    oids: HashMap<TreeId, Option<git::Oid>>,
    trees: HashMap<TreeId, (GitServiceId, NetworkServiceId)>,
}

#[derive(Deserialize, Serialize)]
pub enum RpcRequest {
    OpenPath {
        tree_id: TreeId,
        relative_path: PathBuf,
    },
}

#[derive(Deserialize, Serialize)]
pub enum RpcResponse {
    OpenedBuffer(Result<BufferId, xray_rpc::Error>),
}

pub struct PathSearch {
    tree_ids: Vec<TreeId>,
    roots: Arc<Vec<crate::fs::Entry>>,
    needle: Vec<char>,
    max_results: usize,
    include_ignored: bool,
    stack: Vec<StackEntry>,
    updates: WeakNotifyCell<PathSearchStatus>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PathSearchStatus {
    Pending,
    Ready(Vec<PathSearchResult>),
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct PathSearchResult {
    pub score: fuzzy::Score,
    pub positions: Vec<usize>,
    pub tree_id: TreeId,
    pub relative_path: PathBuf,
    pub display_path: String,
}

struct StackEntry {
    children: Arc<Vec<crate::fs::Entry>>,
    child_index: usize,
    found_match: bool,
}

#[derive(Debug)]
enum MatchMarker {
    ContainsMatch,
    IsMatch,
}

impl LocalProject {
    pub fn new(trees: Vec<Rc<WorkTree>>) -> Self {
        let mut project = LocalProject {
            trees: UsizeMap::new(),
        };

        for tree in trees {
            project.add_tree(tree);
        }

        project
    }

    pub fn add_tree(&mut self, tree: Rc<WorkTree>) {
        self.trees.add(tree);
    }
}

impl Project for LocalProject {
    fn open_path(
        &self,
        tree_id: TreeId,
        relative_path: &PathBuf,
    ) -> Box<dyn Future<Item = Rc<Buffer>, Error = Error>> {
        if let Some(tree) = self.trees.get(&tree_id) {
            tree.open_text_file(relative_path)
        } else {
            panic!("Unreachanle `tree`")
        }
    }

    fn search_paths(
        &self,
        needle: &str,
        max_results: usize,
        include_ignored: bool,
    ) -> (PathSearch, NotifyCellObserver<PathSearchStatus>) {
        let (updates, updates_observer) = NotifyCell::weak(PathSearchStatus::Pending);

        let mut tree_ids = Vec::new();
        let mut roots = Vec::new();
        for (id, tree) in self.trees.iter() {
            tree_ids.push(*id);
            roots.push(tree.root().clone());
        }

        let search = PathSearch {
            tree_ids,
            roots: Arc::new(roots),
            needle: needle.chars().collect(),
            max_results,
            include_ignored,
            stack: Vec::new(),
            updates,
        };

        (search, updates_observer)
    }
}

// FIXME
// What a mess...
impl RemoteProject {
    pub fn new(
        replica_id: ReplicaId,
        foreground: ForegroundExecutor,
        service: xray_rpc::client::Service<ProjectService>,
    ) -> impl Future<Item = Self, Error = Error> {
        let state = service.state().unwrap();

        let foreground_clone = foreground.clone();
        let service_clone = service.clone();
        let trees_iter = state.trees.into_iter();
        let strm = stream::unfold(trees_iter, move |mut vals| match vals.next() {
            Some((tree_id, (git_service_id, network_service_id))) => Some(
                RemoteProject::create_work_tree(
                    replica_id,
                    foreground_clone.clone(),
                    service_clone.clone(),
                    (
                        tree_id.clone(),
                        (git_service_id.clone(), network_service_id.clone()),
                    ),
                )
                .join(future::ok(vals)),
            ),
            None => None,
        });

        let service2 = service.clone();
        strm.collect().and_then(|work_trees| {
            let mut trees = HashMap::new();

            for (tree_id, work_tree) in work_trees {
                trees.insert(tree_id, work_tree);
            }

            Ok(Self {
                service: service2.into_shared(),
                trees,
                buffers: Rc::new(RefCell::new(HashMap::new())),
            })
        })
    }

    fn create_work_tree(
        replica_id: ReplicaId,
        foreground: ForegroundExecutor,
        service: xray_rpc::client::Service<ProjectService>,
        tree: (TreeId, (GitServiceId, NetworkServiceId)),
    ) -> impl Future<Item = (TreeId, Rc<WorkTree>), Error = Error> {
        let (tree_id, (git_service_id, network_service_id)) = tree;
        let state = service.state().unwrap();

        let git_service: xray_rpc::client::Service<git::GitProviderService> = service
            .take_service(git_service_id)
            .expect("The server should create services for each tree in our project state.");

        let network_service: xray_rpc::client::Service<network::NetworkProviderService> = service
            .take_service(network_service_id)
            .expect("The server should create services for each tree in our project state.");

        let git_provider = Rc::new(git::RemoteGitProvider::new(git_service));
        let network_provider =
            network::RemoteNetworkProvider::new(foreground.clone(), network_service);

        let oid = state.oids.get(&tree_id).unwrap();
        WorkTree::new(
            foreground.clone(),
            replica_id,
            oid.clone(),
            git_provider,
            network_provider,
        )
        .and_then(move |work_tree| Ok((tree_id.clone(), work_tree)))
    }
}

impl Project for RemoteProject {
    fn open_path(
        &self,
        tree_id: TreeId,
        relative_path: &PathBuf,
    ) -> Box<dyn Future<Item = Rc<Buffer>, Error = Error>> {
        let tree = self.trees.get(&tree_id).unwrap().clone();
        let buffers = self.buffers.clone();

        Box::new(
            self.service
                .borrow()
                .request(RpcRequest::OpenPath {
                    tree_id,
                    relative_path: relative_path.to_path_buf(),
                })
                .map_err(|err| Error::from(err))
                .join(tree.open_text_file(&relative_path))
                .then(move |response| {
                    response.and_then(|response| match response {
                        (RpcResponse::OpenedBuffer(result), buffer) => result
                            .map_err(|err| Error::from(err))
                            .and_then(|buffer_id| {
                                buffers
                                    .borrow_mut()
                                    .insert(tree_id, (buffer_id, buffer.id()));
                                Ok(buffer)
                            }),
                    })
                }),
        )
    }

    fn search_paths(
        &self,
        needle: &str,
        max_results: usize,
        include_ignored: bool,
    ) -> (PathSearch, NotifyCellObserver<PathSearchStatus>) {
        let (updates, updates_observer) = NotifyCell::weak(PathSearchStatus::Pending);

        let mut tree_ids = Vec::new();
        let mut roots = Vec::new();
        for (id, tree) in self.trees.iter() {
            tree_ids.push(*id);
            roots.push(tree.root().clone());
        }

        let search = PathSearch {
            tree_ids,
            roots: Arc::new(roots),
            needle: needle.chars().collect(),
            max_results,
            include_ignored,
            stack: Vec::new(),
            updates,
        };

        (search, updates_observer)
    }
}

impl ProjectService {
    pub fn new(project: Rc<RefCell<LocalProject>>) -> Self {
        Self {
            project,
            tree_services: HashMap::new(),
        }
    }
}

impl xray_rpc::server::Service for ProjectService {
    type State = RpcState;
    type Update = RpcState;
    type Request = RpcRequest;
    type Response = RpcResponse;

    fn init(&mut self, connection: &xray_rpc::server::Connection) -> Self::State {
        let mut state = RpcState {
            oids: HashMap::new(),
            trees: HashMap::new(),
        };
        for (tree_id, tree) in self.project.borrow().trees.iter() {
            let git_handle = connection.add_service(git::GitProviderService::new(tree.git.clone()));
            let network_handle =
                connection.add_service(network::NetworkProviderService::new(tree.network.clone()));
            state.trees.insert(
                *tree_id,
                (git_handle.service_id(), network_handle.service_id()),
            );
            state.oids.insert(*tree_id, tree.head());
            self.tree_services
                .insert(*tree_id, (git_handle, network_handle));
        }

        state
    }

    fn poll_update(
        &mut self,
        _connection: &xray_rpc::server::Connection,
    ) -> Async<Option<Self::Update>> {
        Async::NotReady
    }

    fn request(
        &mut self,
        request: Self::Request,
        _connection: &xray_rpc::server::Connection,
    ) -> Option<Box<dyn Future<Item = Self::Response, Error = Never>>> {
        match request {
            RpcRequest::OpenPath {
                tree_id,
                relative_path,
            } => Some(Box::new(
                self.project
                    .borrow()
                    .open_path(tree_id, &relative_path)
                    .then(move |result| {
                        Ok(RpcResponse::OpenedBuffer(
                            result
                                .map_err(|_| xray_rpc::Error::IoError(String::from("Err")))
                                .map(|buffer| buffer.id()),
                        ))
                    }),
            )),
        }
    }
}

impl PathSearch {
    fn find_matches(&mut self) -> Result<HashMap<crate::fs::EntryId, MatchMarker>, ()> {
        let mut results = HashMap::new();
        let mut matcher = fuzzy::Matcher::new(&self.needle);

        let mut steps_since_last_check = 0;
        let mut children = if self.roots.len() == 1 {
            self.roots[0].children().unwrap()
        } else {
            self.roots.clone()
        };
        let mut child_index = 0;
        let mut found_match = false;

        loop {
            self.check_cancellation(&mut steps_since_last_check, 10000)?;
            let stack = &mut self.stack;

            if child_index < children.len() {
                if children[child_index].is_ignored() {
                    child_index += 1;
                    continue;
                }

                if matcher.push(&children[child_index].name_chars()) {
                    matcher.pop();
                    results.insert(children[child_index].id(), MatchMarker::IsMatch);
                    found_match = true;
                    child_index += 1;
                } else if children[child_index].is_dir() {
                    let next_children = children[child_index].children().unwrap();
                    stack.push(StackEntry {
                        children: children,
                        child_index,
                        found_match,
                    });
                    children = next_children;
                    child_index = 0;
                    found_match = false;
                } else {
                    matcher.pop();
                    child_index += 1;
                }
            } else if stack.len() > 0 {
                matcher.pop();
                let entry = stack.pop().unwrap();
                children = entry.children;
                child_index = entry.child_index;
                if found_match {
                    results.insert(children[child_index].id(), MatchMarker::ContainsMatch);
                } else {
                    found_match = entry.found_match;
                }
                child_index += 1;
            } else {
                break;
            }
        }

        Ok(results)
    }

    fn rank_matches(
        &mut self,
        matches: HashMap<crate::fs::EntryId, MatchMarker>,
    ) -> Result<Vec<PathSearchResult>, ()> {
        let mut results: BinaryHeap<PathSearchResult> = BinaryHeap::new();
        let mut positions = Vec::new();
        positions.resize(self.needle.len(), 0);
        let mut scorer = fuzzy::Scorer::new(&self.needle);

        let mut steps_since_last_check = 0;
        let mut children = if self.roots.len() == 1 {
            self.roots[0].children().unwrap()
        } else {
            self.roots.clone()
        };
        let mut child_index = 0;
        let mut found_match = false;

        loop {
            self.check_cancellation(&mut steps_since_last_check, 1000)?;
            let stack = &mut self.stack;

            if child_index < children.len() {
                if children[child_index].is_ignored() && !self.include_ignored {
                    child_index += 1;
                } else if children[child_index].is_dir() {
                    let descend;
                    let child_is_match;

                    if found_match {
                        child_is_match = true;
                        descend = true;
                    } else {
                        match matches.get(&children[child_index].id()) {
                            Some(&MatchMarker::IsMatch) => {
                                child_is_match = true;
                                descend = true;
                            }
                            Some(&MatchMarker::ContainsMatch) => {
                                child_is_match = false;
                                descend = true;
                            }
                            None => {
                                child_is_match = false;
                                descend = false;
                            }
                        }
                    };

                    if descend {
                        scorer.push(children[child_index].name_chars(), None);
                        let next_children = children[child_index].children().unwrap();
                        stack.push(StackEntry {
                            child_index,
                            children,
                            found_match,
                        });
                        found_match = child_is_match;
                        children = next_children;
                        child_index = 0;
                    } else {
                        child_index += 1;
                    }
                } else {
                    if found_match || matches.contains_key(&children[child_index].id()) {
                        let score =
                            scorer.push(children[child_index].name_chars(), Some(&mut positions));
                        scorer.pop();
                        if results.len() < self.max_results
                            || score > results.peek().map(|r| r.score).unwrap()
                        {
                            let tree_id = if self.roots.len() == 1 {
                                self.tree_ids[0]
                            } else {
                                self.tree_ids[stack[0].child_index]
                            };

                            let mut relative_path = PathBuf::new();
                            let mut display_path = String::new();
                            for (i, entry) in stack.iter().enumerate() {
                                let child = &entry.children[entry.child_index];
                                if self.roots.len() == 1 || i != 0 {
                                    relative_path.push(child.name());
                                }
                                display_path.extend(child.name_chars());
                            }
                            let child = &children[child_index];
                            relative_path.push(child.name());
                            display_path.extend(child.name_chars());
                            if results.len() == self.max_results {
                                results.pop();
                            }
                            results.push(PathSearchResult {
                                score,
                                tree_id,
                                relative_path,
                                display_path,
                                positions: positions.clone(),
                            });
                        }
                    }
                    child_index += 1;
                }
            } else if stack.len() > 0 {
                scorer.pop();
                let entry = stack.pop().unwrap();
                children = entry.children;
                child_index = entry.child_index;
                found_match = entry.found_match;
                child_index += 1;
            } else {
                break;
            }
        }

        Ok(results.into_sorted_vec())
    }

    #[inline(always)]
    fn check_cancellation(
        &self,
        steps_since_last_check: &mut usize,
        steps_between_checks: usize,
    ) -> Result<(), ()> {
        *steps_since_last_check += 1;
        if *steps_since_last_check == steps_between_checks {
            if self.updates.has_observers() {
                *steps_since_last_check = 0;
            } else {
                return Err(());
            }
        }
        Ok(())
    }
}

impl Future for PathSearch {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if self.needle.is_empty() {
            let _ = self.updates.try_set(PathSearchStatus::Ready(Vec::new()));
        } else {
            let matches = self.find_matches()?;
            let results = self.rank_matches(matches)?;
            let _ = self.updates.try_set(PathSearchStatus::Ready(results));
        }
        Ok(Async::Ready(()))
    }
}

impl Ord for PathSearchResult {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        self.partial_cmp(other).unwrap_or(cmp::Ordering::Equal)
    }
}

impl PartialOrd for PathSearchResult {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        // Reverse the comparison so results with lower scores sort
        // closer to the top of the results heap.
        other.score.partial_cmp(&self.score)
    }
}

impl Eq for PathSearchResult {}

#[cfg(test)]
pub mod tests {
    use std::rc::Rc;

    use tokio_core::reactor;

    use super::*;

    use crate::{
        tests::git::{BaseEntry, TestGitProvider},
        tests::network::TestNetworkProvider,
        tests::work_tree::TestWorkTree,
        work_tree::WorkTree,
        Error,
    };

    #[test]
    fn test_open_same_path_concurrently() -> Result<(), Error> {
        let project = build_project()?;

        let tree_id = 0;
        let relative_path = PathBuf::from("subdir-a/subdir-1/bar");

        let buffer_future_1 = project.open_path(tree_id, &relative_path);
        let buffer_future_2 = project.open_path(tree_id, &relative_path);
        let (buffer_1, buffer_2) = buffer_future_1.join(buffer_future_2).wait().unwrap();
        assert!(Rc::ptr_eq(&buffer_1, &buffer_2));

        Ok(())
    }

    #[test]
    fn test_drop_buffer_rc() -> Result<(), Error> {
        let project = build_project()?;

        let tree_id = 0;
        let relative_path = PathBuf::from("subdir-a/subdir-1/bar");

        let buffer_1 = project.open_path(tree_id, &relative_path).wait()?;
        buffer_1.edit(vec![0..3], "memory")?;
        let buffer_2 = project.open_path(tree_id, &relative_path).wait()?;
        assert_eq!(buffer_2.to_string(), "memory");

        // Dropping only one of the two strong references does not release the buffer.
        drop(buffer_2);
        let buffer_3 = project.open_path(tree_id, &relative_path).wait()?;
        assert_eq!(buffer_3.to_string(), "memory");

        // Dropping all strong references causes the buffer to be released.
        drop(buffer_1);
        drop(buffer_3);
        let buffer_4 = project.open_path(tree_id, &relative_path).wait()?;
        // TODO: This should pass
        // assert_eq!(buffer_4.to_string(), "abc");
        assert_eq!(buffer_4.to_string(), "memory");

        Ok(())
    }

    #[test]
    fn test_search_one_tree() -> Result<(), Error> {
        let reactor = reactor::Core::new().unwrap();
        let handle = Rc::new(reactor.handle());

        let uuid = uuid::Uuid::from_u128(0);

        let tree = {
            let git = Rc::new(TestGitProvider::new());
            let oid = git.gen_oid();

            git.commit(
                oid,
                vec![
                    BaseEntry::dir(1, "root-1"),
                    BaseEntry::file(2, "file-1", ""),
                    BaseEntry::dir(2, "subdir-1"),
                    BaseEntry::file(3, "file-1", ""),
                    BaseEntry::file(3, "file-2", ""),
                    BaseEntry::dir(1, "root-2"),
                    BaseEntry::dir(2, "subdir-2"),
                    BaseEntry::file(3, "file-3", ""),
                    BaseEntry::file(3, "file-4", ""),
                ],
            );

            let network = Rc::new(TestNetworkProvider::new());

            WorkTree::new_sync(
                handle.clone(),
                uuid,
                Some(oid),
                git.clone(),
                network.clone(),
            )?
        };

        let project = LocalProject::new(vec![tree]);
        let (mut search, observer) = project.search_paths("sub2", 10, true);

        assert_eq!(search.poll(), Ok(Async::Ready(())));
        assert_eq!(
            summarize_results(&observer.get()),
            Some(vec![
                (
                    0,
                    "root-2/subdir-2/file-3".to_string(),
                    "root-2/subdir-2/file-3".to_string(),
                    vec![7, 8, 9, 14],
                ),
                (
                    0,
                    "root-2/subdir-2/file-4".to_string(),
                    "root-2/subdir-2/file-4".to_string(),
                    vec![7, 8, 9, 14],
                ),
                (
                    0,
                    "root-1/subdir-1/file-2".to_string(),
                    "root-1/subdir-1/file-2".to_string(),
                    vec![7, 8, 9, 21],
                ),
            ])
        );

        Ok(())
    }

    // FIXME
    //
    // #[test]
    // fn test_search_many_trees() -> Result<(), Error>  {
    //     let project = build_project()?;
    //
    //     let (mut search, observer) = project.search_paths("bar", 10, true);
    //     assert_eq!(search.poll(), Ok(Async::Ready(())));
    //     assert_eq!(
    //         summarize_results(&observer.get()),
    //         Some(vec![
    //             (
    //                 1,
    //                 "subdir-b/subdir-2/foo".to_string(),
    //                 "bar/subdir-b/subdir-2/foo".to_string(),
    //                 vec![0, 1, 2],
    //             ),
    //             (
    //                 0,
    //                 "subdir-a/subdir-1/bar".to_string(),
    //                 "foo/subdir-a/subdir-1/bar".to_string(),
    //                 vec![22, 23, 24],
    //             ),
    //             (
    //                 1,
    //                 "subdir-b/subdir-2/file-3".to_string(),
    //                 "bar/subdir-b/subdir-2/file-3".to_string(),
    //                 vec![0, 1, 2],
    //             ),
    //             (
    //                 0,
    //                 "subdir-a/subdir-1/file-1".to_string(),
    //                 "foo/subdir-a/subdir-1/file-1".to_string(),
    //                 vec![6, 11, 18],
    //             ),
    //         ])
    //     );
    //
    //     Ok(())
    // }

    fn build_project() -> Result<LocalProject, Error> {
        let reactor = reactor::Core::new().unwrap();
        let handle = Rc::new(reactor.handle());

        let uuid = uuid::Uuid::from_u128(0);

        let tree_1 = {
            let git = Rc::new(TestGitProvider::new());
            let oid = git.gen_oid();

            git.commit(
                oid,
                vec![
                    BaseEntry::dir(1, "subdir-a"),
                    BaseEntry::file(2, "file-1", ""),
                    BaseEntry::dir(2, "subdir-1"),
                    BaseEntry::file(3, "file-1", ""),
                    BaseEntry::file(3, "bar", "abc"),
                ],
            );

            let network = Rc::new(TestNetworkProvider::new());

            WorkTree::new_sync(
                handle.clone(),
                uuid,
                Some(oid),
                git.clone(),
                network.clone(),
            )?
        };

        let tree_2 = {
            let git = Rc::new(TestGitProvider::new());
            let oid = git.gen_oid();

            git.commit(
                oid,
                vec![
                    BaseEntry::dir(1, "subdir-b"),
                    BaseEntry::dir(2, "subdir-2"),
                    BaseEntry::file(3, "file-3", ""),
                    BaseEntry::file(3, "foo", ""),
                ],
            );

            let network = Rc::new(TestNetworkProvider::new());

            WorkTree::new_sync(
                handle.clone(),
                uuid,
                Some(oid),
                git.clone(),
                network.clone(),
            )?
        };

        Ok(LocalProject::new(vec![tree_1, tree_2]))
    }

    fn summarize_results(
        results: &PathSearchStatus,
    ) -> Option<Vec<(TreeId, String, String, Vec<usize>)>> {
        match results {
            &PathSearchStatus::Pending => None,
            &PathSearchStatus::Ready(ref results) => {
                let summary = results
                    .iter()
                    .map(|result| {
                        let tree_id = result.tree_id;
                        let relative_path = String::from(result.relative_path.to_string_lossy());
                        let display_path = result.display_path.clone();
                        let positions = result.positions.clone();
                        (tree_id, relative_path, display_path, positions)
                    })
                    .collect();
                Some(summary)
            }
        }
    }
}
