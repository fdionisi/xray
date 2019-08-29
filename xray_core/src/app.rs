use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::io;
use std::rc::Rc;

use bytes::Bytes;
use futures::unsync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures::{future, Async, Future, IntoFuture, Stream};
use serde_json;
use xray_rpc::{self as rpc, client, server};

use crate::never::Never;
use crate::notify_cell::{NotifyCell, NotifyCellObserver};
use crate::project::LocalProject;
use crate::usize_map::UsizeMap;
use crate::views::WorkspaceView;
use crate::window::{ViewId, Window, WindowUpdateStream};
use crate::work_tree::WorkTree;
use crate::workspace::{LocalWorkspace, RemoteWorkspace, Workspace, WorkspaceService};
use crate::{BackgroundExecutor, Error, ForegroundExecutor, IntoShared, ReplicaId};

pub type WindowId = usize;
type WorkspaceId = usize;

pub struct App {
    headless: bool,
    foreground: ForegroundExecutor,
    background: BackgroundExecutor,
    commands_tx: UnboundedSender<Command>,
    commands_rx: Option<UnboundedReceiver<Command>>,
    peer_list: Rc<RefCell<PeerList>>,
    workspaces: UsizeMap<WorkspaceEntry>,
    windows: UsizeMap<Window>,
    updates: NotifyCell<()>,
}

pub enum Command {
    OpenWindow(WindowId),
}

pub struct PeerList {
    foreground: ForegroundExecutor,
    peers: HashMap<ReplicaId, Peer>,
    opened_workspaces_tx: UnboundedSender<RemoteWorkspace>,
    opened_workspaces_rx: Option<UnboundedReceiver<RemoteWorkspace>>,
    updates: NotifyCell<()>,
}

struct Peer {
    replica_id: ReplicaId,
    foreground: ForegroundExecutor,
    service: client::FullUpdateService<AppService>,
}

#[derive(Debug, PartialEq)]
struct PeerState {
    workspaces: Vec<WorkspaceDescriptor>,
}

#[derive(Debug, PartialEq)]
struct WorkspaceDescriptor {
    id: WorkspaceId,
}

struct AppService {
    app: Rc<RefCell<App>>,
    updates: NotifyCellObserver<()>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ServiceState {
    workspace_ids: Vec<WorkspaceId>,
}

#[derive(Serialize, Deserialize)]
pub enum ServiceRequest {
    OpenWorkspace(WorkspaceId),
}

#[derive(Serialize, Deserialize)]
pub enum ServiceResponse {
    OpenedWorkspace(rpc::ServiceId),
}

#[derive(Serialize, Deserialize)]
pub enum ServiceError {
    WorkspaceNotFound(WorkspaceId),
}

enum WorkspaceEntry {
    Local(Rc<RefCell<LocalWorkspace>>),
    Remote(Rc<RefCell<RemoteWorkspace>>),
}

#[derive(Debug)]
enum WorkspaceOpenError {
    NotFound(WorkspaceId),
    RpcError(rpc::Error),
}

impl App {
    pub fn new(
        headless: bool,
        foreground: ForegroundExecutor,
        background: BackgroundExecutor,
    ) -> Rc<RefCell<Self>> {
        let (commands_tx, commands_rx) = mpsc::unbounded();
        let peer_list = PeerList::new(foreground.clone()).into_shared();
        let app = App {
            headless,
            foreground: foreground.clone(),
            background,
            commands_tx,
            commands_rx: Some(commands_rx),
            peer_list: peer_list.clone(),
            workspaces: UsizeMap::new(),
            windows: UsizeMap::new(),
            updates: NotifyCell::new(()),
        }
        .into_shared();

        let app_clone = app.clone();
        foreground
            .execute(Box::new(
                peer_list
                    .borrow_mut()
                    .take_opened_workspaces()
                    .unwrap()
                    .for_each(move |workspace| {
                        let workspace = workspace.into_shared();
                        let mut app = app_clone.borrow_mut();
                        app.add_workspace(WorkspaceEntry::Remote(workspace.clone()));
                        app.open_workspace_window(workspace);
                        Ok(())
                    }),
            ))
            .unwrap();

        app
    }

    pub fn commands(&mut self) -> Option<UnboundedReceiver<Command>> {
        self.commands_rx.take()
    }

    pub fn headless(&self) -> bool {
        self.headless
    }

    pub fn open_local_workspace(&mut self, replica_id: ReplicaId, roots: Vec<Rc<WorkTree>>) {
        let workspace = LocalWorkspace::new(replica_id, LocalProject::new(roots)).into_shared();
        self.add_workspace(WorkspaceEntry::Local(workspace.clone()));
        self.open_workspace_window(workspace);
    }

    fn add_workspace(&mut self, workspace: WorkspaceEntry) {
        self.workspaces.add(workspace);
        self.updates.set(());
    }

    fn open_workspace_window<T: 'static + Workspace>(&mut self, workspace: Rc<RefCell<T>>) {
        if !self.headless {
            let mut window = Window::new(Some(self.background.clone()), 0.0);
            let workspace_view_handle = window.add_view(WorkspaceView::new(
                self.foreground.clone(),
                workspace.clone(),
            ));
            window.set_root_view(workspace_view_handle);
            let window_id = self.windows.add(window);
            if self
                .commands_tx
                .unbounded_send(Command::OpenWindow(window_id))
                .is_err()
            {
                let (commands_tx, commands_rx) = mpsc::unbounded();
                commands_tx
                    .unbounded_send(Command::OpenWindow(window_id))
                    .unwrap();
                self.commands_tx = commands_tx;
                self.commands_rx = Some(commands_rx);
            }
        }
    }

    pub fn start_window(&mut self, id: &WindowId, height: f64) -> Result<WindowUpdateStream, ()> {
        let window = self.windows.get_mut(id).ok_or(())?;
        window.set_height(height);
        Ok(window.updates())
    }

    pub fn dispatch_action(
        &mut self,
        window_id: WindowId,
        view_id: ViewId,
        action: serde_json::Value,
    ) {
        match self.windows.get_mut(&window_id) {
            Some(ref mut window) => window.dispatch_action(view_id, action),
            None => unimplemented!(),
        };
    }

    pub fn close_window(&mut self, window_id: WindowId) -> Result<(), ()> {
        self.windows.remove(&window_id).map(|_| ()).ok_or(())
    }

    pub fn connect_to_client<S>(app: Rc<RefCell<App>>, incoming: S) -> server::Connection
    where
        S: 'static + Stream<Item = Bytes, Error = io::Error>,
    {
        server::Connection::new(incoming, AppService::new(app.clone()))
    }

    pub fn connect_to_server<S>(
        &self,
        replica_id: ReplicaId,
        incoming: S,
    ) -> Box<dyn Future<Item = client::Connection, Error = rpc::Error>>
    where
        S: 'static + Stream<Item = Bytes, Error = io::Error>,
    {
        PeerList::connect_to_server(replica_id, self.peer_list.clone(), incoming)
    }
}

impl PeerList {
    fn new(foreground: ForegroundExecutor) -> Self {
        let (tx, rx) = mpsc::unbounded();
        PeerList {
            foreground,
            peers: HashMap::new(),
            opened_workspaces_tx: tx,
            opened_workspaces_rx: Some(rx),
            updates: NotifyCell::new(()),
        }
    }

    #[cfg(test)]
    fn state(&self) -> Vec<PeerState> {
        self.peers
            .iter()
            .filter_map(|(_, peer)| {
                peer.service.latest_state().ok().map(|state| PeerState {
                    workspaces: state
                        .workspace_ids
                        .iter()
                        .map(|id| WorkspaceDescriptor { id: *id })
                        .collect(),
                })
            })
            .collect()
    }

    #[cfg(test)]
    fn updates(&self) -> NotifyCellObserver<()> {
        self.updates.observe()
    }

    fn take_opened_workspaces(&mut self) -> Option<UnboundedReceiver<RemoteWorkspace>> {
        self.opened_workspaces_rx.take()
    }

    fn connect_to_server<S>(
        replica_id: ReplicaId,
        peer_list: Rc<RefCell<PeerList>>,
        incoming: S,
    ) -> Box<dyn Future<Item = client::Connection, Error = rpc::Error>>
    where
        S: 'static + Stream<Item = Bytes, Error = io::Error>,
    {
        Box::new(
            client::Connection::new(incoming).and_then(move |(connection, peer_service)| {
                let mut peer_list = peer_list.borrow_mut();
                let peer = Peer::new(replica_id, peer_list.foreground.clone(), peer_service);
                let peer_updates = peer.updates()?;
                peer_list.peers.insert(replica_id, peer);
                let peer_list_updates = peer_list.updates.clone();
                peer_list
                    .foreground
                    .execute(Box::new(peer_updates.for_each(move |_| {
                        peer_list_updates.set(());
                        Ok(())
                    })))
                    .unwrap();

                peer_list.updates.set(());

                // TODO: Eliminate this once we have a UI for the PeerList.
                peer_list.open_first_workspace(replica_id);
                Ok(connection)
            }),
        )
    }

    fn open_first_workspace(&self, replica_id: ReplicaId) {
        if let Some(peer) = self.peers.get(&replica_id) {
            let opened_workspaces_tx = self.opened_workspaces_tx.clone();
            self.foreground
                .execute(Box::new(peer.open_first_workspace().then(
                    move |result| match result {
                        Ok(Some(workspace)) => {
                            let _ = opened_workspaces_tx.unbounded_send(workspace);
                            Ok(())
                        }
                        Ok(None) => {
                            eprintln!("No workspaces on remote peer {}", replica_id);
                            Ok(())
                        }
                        Err(error) => {
                            eprintln!("Error opening remote workspace: {}", error);
                            Ok(())
                        }
                    },
                )))
                .unwrap();
        }
    }
}

impl Peer {
    fn new(
        replica_id: ReplicaId,
        foreground: ForegroundExecutor,
        service: client::Service<AppService>,
    ) -> Self {
        Self {
            replica_id,
            foreground,
            service: client::FullUpdateService::new(service),
        }
    }

    fn updates(&self) -> Result<Box<dyn Stream<Item = (), Error = ()>>, rpc::Error> {
        Ok(Box::new(self.service.updates()?.map(|_| ())))
    }

    fn open_first_workspace(
        &self,
    ) -> Box<dyn Future<Item = Option<RemoteWorkspace>, Error = WorkspaceOpenError>> {
        match self.service.latest_state() {
            Ok(state) => {
                if let Some(workspace_id) = state.workspace_ids.first() {
                    self.open_workspace(*workspace_id)
                } else {
                    Box::new(future::ok(None))
                }
            }
            Err(error) => Box::new(future::err(error.into())),
        }
    }

    fn open_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Box<dyn Future<Item = Option<RemoteWorkspace>, Error = WorkspaceOpenError>> {
        let replica_id = self.replica_id;
        let foreground = self.foreground.clone();
        let service = self.service.clone();
        Box::new(
            self.service
                .request(ServiceRequest::OpenWorkspace(workspace_id))
                .map_err(|e| e.into())
                .and_then(move |response| match response {
                    Ok(ServiceResponse::OpenedWorkspace(service_id)) => service
                        .take_service(service_id)
                        .map_err(|e| WorkspaceOpenError::from(e)),
                    Err(err) => match err {
                        ServiceError::WorkspaceNotFound(id) => {
                            Err(WorkspaceOpenError::NotFound(id))
                        }
                    },
                })
                .and_then(move |workspace_service| {
                    RemoteWorkspace::new(replica_id, foreground, workspace_service).map_err(|e| {
                        match e {
                            Error::Rpc(err) => WorkspaceOpenError::from(err),
                            _ => panic!("unknown error"),
                        }
                    })
                }),
        )
    }
}

impl AppService {
    fn new(app: Rc<RefCell<App>>) -> Self {
        let updates = app.borrow().updates.observe();
        Self { app, updates }
    }

    fn state(&self) -> ServiceState {
        ServiceState {
            workspace_ids: self.app.borrow().workspaces.keys().cloned().collect(),
        }
    }
}

impl server::Service for AppService {
    type State = ServiceState;
    type Update = ServiceState;
    type Request = ServiceRequest;
    type Response = Result<ServiceResponse, ServiceError>;

    fn init(&mut self, _connection: &server::Connection) -> Self::State {
        self.state()
    }

    fn poll_update(&mut self, _: &server::Connection) -> Async<Option<Self::Update>> {
        match self.updates.poll() {
            Ok(Async::Ready(Some(()))) => Async::Ready(Some(self.state())),
            Ok(Async::Ready(None)) | Err(_) => Async::Ready(None),
            Ok(Async::NotReady) => Async::NotReady,
        }
    }

    fn request(
        &mut self,
        request: Self::Request,
        connection: &server::Connection,
    ) -> Option<Box<dyn Future<Item = Self::Response, Error = Never>>> {
        let response = match request {
            ServiceRequest::OpenWorkspace(workspace_id) => {
                let app = self.app.borrow();
                if let Some(workspace) = app.workspaces.get(&workspace_id) {
                    match workspace {
                        &WorkspaceEntry::Local(ref workspace) => {
                            let service_handle =
                                connection.add_service(WorkspaceService::new(workspace.clone()));
                            Ok(ServiceResponse::OpenedWorkspace(
                                service_handle.service_id(),
                            ))
                        }
                        &WorkspaceEntry::Remote(_) => {
                            Err(ServiceError::WorkspaceNotFound(workspace_id))
                        }
                    }
                } else {
                    Err(ServiceError::WorkspaceNotFound(workspace_id))
                }
            }
        };

        Some(Box::new(Ok(response).into_future()))
    }
}

impl From<rpc::Error> for WorkspaceOpenError {
    fn from(error: rpc::Error) -> Self {
        WorkspaceOpenError::RpcError(error)
    }
}

impl fmt::Display for WorkspaceOpenError {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            WorkspaceOpenError::RpcError(ref error) => write!(fmt, "rpc error: {}", error),
            WorkspaceOpenError::NotFound(workspace_id) => {
                write!(fmt, "workspace not found for id {}", workspace_id)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // use std::rc::Rc;
    //
    // use futures::{unsync, Future, Sink};
    // use tokio_core::reactor;
    //
    // use crate::stream_ext::StreamExt;
    // use crate::tests::{network::TestNetworkProvider, work_tree::TestWorkTree};
    // use crate::work_tree::WorkTree;
    //
    // use super::*;

    // #[test]
    // fn test_remote_workspaces() {
    //     let mut reactor = reactor::Core::new().unwrap();
    //     let executor = Rc::new(reactor.handle());
    //
    //     let replica_id = uuid::Uuid::from_u128(0);
    //
    //     let server = App::new(true, executor.clone(), executor.clone());
    //     let client = App::new(false, executor.clone(), executor.clone());
    //     let peer_list = client.borrow().peer_list.clone();
    //
    //     let mut peer_list_updates = peer_list.borrow().updates();
    //     assert_eq!(peer_list.borrow().state(), vec![]);
    //
    //     connect(&mut reactor, server.clone(), client.clone());
    //     peer_list_updates.wait_next(&mut reactor);
    //     assert_eq!(
    //         peer_list.borrow().state(),
    //         vec![PeerState { workspaces: vec![] }]
    //     );
    //
    //     let network = Rc::new(TestNetworkProvider::new());
    //     let (_, _, work_tree, _) = WorkTree::basic(Some(network.clone()));
    //     server
    //         .borrow_mut()
    //         .open_local_workspace(replica_id, vec![work_tree]);
    //     peer_list_updates.wait_next(&mut reactor);
    //     assert_eq!(
    //         peer_list.borrow().state(),
    //         vec![PeerState {
    //             workspaces: vec![WorkspaceDescriptor { id: 0 }],
    //         }]
    //     );
    //
    //     peer_list.borrow().open_first_workspace(0);
    // }
    //
    // fn connect(reactor: &mut reactor::Core, server: Rc<RefCell<App>>, client: Rc<RefCell<App>>) {
    //     let (server_to_client_tx, server_to_client_rx) = unsync::mpsc::unbounded();
    //     let server_to_client_rx = server_to_client_rx.map_err(|_| unreachable!());
    //     let (client_to_server_tx, client_to_server_rx) = unsync::mpsc::unbounded();
    //     let client_to_server_rx = client_to_server_rx.map_err(|_| unreachable!());
    //
    //     let server_outgoing = App::connect_to_client(server, client_to_server_rx);
    //     reactor.handle().spawn(
    //         server_to_client_tx
    //             .send_all(server_outgoing.map_err(|_| unreachable!()))
    //             .then(|_| Ok(())),
    //     );
    //
    //     let client_future = client.borrow().connect_to_server(server_to_client_rx);
    //     let client_outgoing = reactor.run(client_future).unwrap();
    //     reactor.handle().spawn(
    //         client_to_server_tx
    //             .send_all(client_outgoing.map_err(|_| unreachable!()))
    //             .then(|_| Ok(())),
    //     );
    // }
}
