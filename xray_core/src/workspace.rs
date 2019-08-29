use std::cell::{Ref, RefCell, RefMut};
use std::ops::Range;
use std::rc::Rc;

use futures::Future;
use xray_rpc::{self, client, server};

use crate::buffer::{BufferId, Point};
use crate::never::Never;
use crate::project::{LocalProject, Project, ProjectService, RemoteProject};
use crate::{Error, ForegroundExecutor, IntoShared, ReplicaId};

pub trait Workspace {
    fn replica_id(&self) -> ReplicaId;
    fn project(&self) -> Ref<dyn Project>;
    fn project_mut(&self) -> RefMut<dyn Project>;
}

pub struct LocalWorkspace {
    replica_id: ReplicaId,
    project: Rc<RefCell<LocalProject>>,
}

pub struct RemoteWorkspace {
    replica_id: ReplicaId,
    project: Rc<RefCell<RemoteProject>>,
}

pub struct WorkspaceService {
    workspace: Rc<RefCell<LocalWorkspace>>,
}

#[derive(Serialize, Deserialize)]
pub struct ServiceState {
    project: xray_rpc::ServiceId,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Anchor {
    buffer_id: BufferId,
    range: Range<Point>,
}

impl LocalWorkspace {
    pub fn new(replica_id: ReplicaId, project: LocalProject) -> Self {
        Self {
            replica_id,
            project: project.into_shared(),
        }
    }
}

impl Workspace for LocalWorkspace {
    fn replica_id(&self) -> ReplicaId {
        self.replica_id
    }

    fn project(&self) -> Ref<dyn Project> {
        self.project.borrow()
    }

    fn project_mut(&self) -> RefMut<dyn Project> {
        self.project.borrow_mut()
    }
}

impl RemoteWorkspace {
    pub fn new(
        replica_id: ReplicaId,
        foreground: ForegroundExecutor,
        service: client::Service<WorkspaceService>,
    ) -> impl Future<Item = Option<Self>, Error = Error> {
        let state = service.state().unwrap();
        RemoteProject::new(
            replica_id,
            foreground.clone(),
            service.take_service(state.project).unwrap(),
        )
        .and_then(move |project| {
            Ok(Some(Self {
                replica_id,
                project: project.into_shared(),
            }))
        })
    }
}

impl Workspace for RemoteWorkspace {
    fn replica_id(&self) -> ReplicaId {
        self.replica_id
    }

    fn project(&self) -> Ref<dyn Project> {
        self.project.borrow()
    }

    fn project_mut(&self) -> RefMut<dyn Project> {
        self.project.borrow_mut()
    }
}

impl WorkspaceService {
    pub fn new(workspace: Rc<RefCell<LocalWorkspace>>) -> Self {
        Self { workspace }
    }
}

impl server::Service for WorkspaceService {
    type State = ServiceState;
    type Update = Never;
    type Request = Never;
    type Response = Never;

    fn init(&mut self, connection: &server::Connection) -> ServiceState {
        let workspace = self.workspace.borrow_mut();
        ServiceState {
            project: connection
                .add_service(ProjectService::new(workspace.project.clone()))
                .service_id(),
        }
    }
}
