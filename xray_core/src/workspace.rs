use std::cell::{Ref, RefCell, RefMut};
use std::ops::Range;
use std::rc::Rc;

use futures::Future;
use xray_rpc::{self, client, server};

use crate::buffer::{BufferId, Point};
use crate::never::Never;
use crate::project::{LocalProject, Project, ProjectService, RemoteProject};
use crate::{Error, ForegroundExecutor, IntoShared, UserId};

pub trait Workspace {
    fn user_id(&self) -> UserId;
    fn project(&self) -> Ref<dyn Project>;
    fn project_mut(&self) -> RefMut<dyn Project>;
}

pub struct LocalWorkspace {
    next_user_id: UserId,
    user_id: UserId,
    project: Rc<RefCell<LocalProject>>,
}

pub struct RemoteWorkspace {
    user_id: UserId,
    project: Rc<RefCell<RemoteProject>>,
}

pub struct WorkspaceService {
    workspace: Rc<RefCell<LocalWorkspace>>,
}

#[derive(Serialize, Deserialize)]
pub struct ServiceState {
    user_id: UserId,
    project: xray_rpc::ServiceId,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Anchor {
    buffer_id: BufferId,
    range: Range<Point>,
}

impl LocalWorkspace {
    pub fn new(project: LocalProject) -> Self {
        Self {
            user_id: 0,
            next_user_id: 1,
            project: project.into_shared(),
        }
    }
}

impl Workspace for LocalWorkspace {
    fn user_id(&self) -> UserId {
        self.user_id
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
        foreground: ForegroundExecutor,
        service: client::Service<WorkspaceService>,
    ) -> impl Future<Item = Option<Self>, Error = Error> {
        let state = service.state().unwrap();
        let user_id = state.user_id;
        RemoteProject::new(
            foreground.clone(),
            service.take_service(state.project).unwrap(),
        )
        .and_then(move |project| {
            Ok(Some(Self {
                user_id,
                project: project.into_shared(),
            }))
        })
    }
}

impl Workspace for RemoteWorkspace {
    fn user_id(&self) -> UserId {
        self.user_id
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
        let mut workspace = self.workspace.borrow_mut();
        let user_id = workspace.next_user_id;
        workspace.next_user_id += 1;
        ServiceState {
            user_id,
            project: connection
                .add_service(ProjectService::new(workspace.project.clone()))
                .service_id(),
        }
    }
}
