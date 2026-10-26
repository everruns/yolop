//! Reopen Yolop's existing workspace as an explicit Engine Environment.
//!
//! The logical head is the session's workspace, whose physical Git worktree
//! can be repointed by Yolop's worktree controller. Linked agents share it.
//! Workspace snapshot/restore remains the checkpoint controller's operation.
use async_trait::async_trait;
use everruns::hosting::EnvironmentFactory;
use everruns_contracts::typed_id::{SessionId, WorkspaceId};
use everruns_core::SessionFileSystem;
use everruns_core::host::{
    Environment, Workspace, WorkspaceBackend, WorkspaceBackendId, WorkspaceBinding,
    WorkspaceCheckpoint, WorkspaceDescriptor, WorkspaceDiff, WorkspaceError, WorkspaceHeadAccess,
    WorkspaceHeadDescriptor, WorkspaceHeadId, WorkspaceHeadRequest, WorkspaceHeadResource,
    WorkspaceHeadStatus,
};
use std::collections::BTreeMap;
use std::sync::Arc;

pub struct YolopEnvironment {
    workspace: Workspace,
    binding: WorkspaceBinding,
}

impl YolopEnvironment {
    pub fn new(
        session: SessionId,
        files: Arc<dyn SessionFileSystem>,
    ) -> Result<Self, WorkspaceError> {
        let descriptor = WorkspaceDescriptor {
            id: WorkspaceId::from_uuid(session.uuid()),
            name: "Yolop workspace".into(),
            metadata: BTreeMap::new(),
        };
        let binding = WorkspaceBinding {
            provider_id: WorkspaceBackendId::new("yolop.workspace.v1")?,
            workspace_id: descriptor.id,
            head_id: WorkspaceHeadId::from_uuid(session.uuid()),
            access: WorkspaceHeadAccess::Shared,
            payload: session.to_string().into_bytes(),
        };
        let backend = Arc::new(YolopWorkspace {
            descriptor: descriptor.clone(),
            binding: binding.clone(),
            files,
        });
        Ok(Self {
            workspace: Workspace::from_descriptor(backend, descriptor),
            binding,
        })
    }
}

#[async_trait]
impl EnvironmentFactory for YolopEnvironment {
    async fn open(&self, _: SessionId) -> everruns_contracts::Result<Environment> {
        self.workspace
            .reopen(&self.binding)
            .await
            .map(Environment::new)
            .map_err(|error| everruns_contracts::AgentLoopError::config(error.to_string()))
    }
}

struct YolopWorkspace {
    descriptor: WorkspaceDescriptor,
    binding: WorkspaceBinding,
    files: Arc<dyn SessionFileSystem>,
}

impl YolopWorkspace {
    fn validate(&self, binding: &WorkspaceBinding) -> Result<(), WorkspaceError> {
        if binding != &self.binding {
            return Err(WorkspaceError::BindingMismatch);
        }
        Ok(())
    }
    fn resource(&self) -> WorkspaceHeadResource {
        WorkspaceHeadResource {
            workspace: self.descriptor.clone(),
            head: WorkspaceHeadDescriptor {
                id: self.binding.head_id,
                name: "session".into(),
                base: None,
                access: WorkspaceHeadAccess::Shared,
                metadata: BTreeMap::new(),
            },
            binding: self.binding.clone(),
            file_system: self.files.clone(),
        }
    }
    fn product_operation() -> WorkspaceError {
        WorkspaceError::InvalidRequest(
            "this workspace is managed by Yolop's worktree and checkpoint controls".into(),
        )
    }
}

#[async_trait]
impl WorkspaceBackend for YolopWorkspace {
    fn id(&self) -> WorkspaceBackendId {
        self.binding.provider_id.clone()
    }
    async fn open_workspace(&self, locator: &str) -> Result<WorkspaceDescriptor, WorkspaceError> {
        if locator.as_bytes() != self.binding.payload {
            return Err(WorkspaceError::NotFound);
        }
        Ok(self.descriptor.clone())
    }
    async fn open_workspace_from_binding(
        &self,
        binding: &WorkspaceBinding,
    ) -> Result<WorkspaceDescriptor, WorkspaceError> {
        self.validate(binding)?;
        Ok(self.descriptor.clone())
    }
    async fn create_head(
        &self,
        _: &WorkspaceDescriptor,
        _: WorkspaceHeadRequest,
    ) -> Result<WorkspaceHeadResource, WorkspaceError> {
        Err(Self::product_operation())
    }
    async fn reopen_head(
        &self,
        binding: &WorkspaceBinding,
    ) -> Result<WorkspaceHeadResource, WorkspaceError> {
        self.validate(binding)?;
        Ok(self.resource())
    }
    async fn checkpoint(
        &self,
        binding: &WorkspaceBinding,
    ) -> Result<WorkspaceCheckpoint, WorkspaceError> {
        self.validate(binding)?;
        Err(Self::product_operation())
    }
    async fn status(
        &self,
        binding: &WorkspaceBinding,
    ) -> Result<WorkspaceHeadStatus, WorkspaceError> {
        self.validate(binding)?;
        Err(Self::product_operation())
    }
    async fn diff(&self, binding: &WorkspaceBinding) -> Result<WorkspaceDiff, WorkspaceError> {
        self.validate(binding)?;
        Err(Self::product_operation())
    }
    async fn archive(&self, binding: &WorkspaceBinding) -> Result<(), WorkspaceError> {
        self.validate(binding)?;
        Err(Self::product_operation())
    }
    async fn destroy(&self, binding: &WorkspaceBinding) -> Result<(), WorkspaceError> {
        self.validate(binding)?;
        Err(Self::product_operation())
    }
}
