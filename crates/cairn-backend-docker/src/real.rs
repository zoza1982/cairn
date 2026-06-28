//! The real Docker engine adapter over `bollard`.
//!
//! Container and image **listing** are implemented against the Docker Engine API. Browsing a
//! container's filesystem (`list_dir`/`stat`/`read`) is done over the archive/exec APIs as the
//! integration step (validated against a live daemon per the M6 CI design); those return
//! `Unsupported` here until that lands. The full path-routing/mapping is verified via the mock.

use crate::ops::{ContainerInfo, ContainerOps, ImageInfo, RemoteEntry, RemoteMeta};
use async_trait::async_trait;
use bollard::Docker;
use cairn_types::{Caps, ContainerState};
use cairn_vfs::VfsError;

/// A [`ContainerOps`] implementation backed by a live Docker engine via `bollard`.
pub struct BollardDocker {
    docker: Docker,
}

impl BollardDocker {
    /// Connect using the platform's default Docker endpoint (socket / named pipe / env).
    ///
    /// # Errors
    /// [`VfsError::Connection`] if the engine cannot be reached.
    pub fn connect_local() -> Result<Self, VfsError> {
        Docker::connect_with_local_defaults()
            .map(|docker| Self { docker })
            .map_err(|e| VfsError::Connection(Box::new(e)))
    }
}

fn backend_err(e: impl std::fmt::Display) -> VfsError {
    VfsError::Backend {
        code: "docker".to_owned(),
        msg: e.to_string(),
        retryable: false,
    }
}

/// Map bollard's container-state enum to the cairn-types state.
fn map_state(s: Option<bollard::models::ContainerSummaryStateEnum>) -> ContainerState {
    use bollard::models::ContainerSummaryStateEnum as B;
    match s {
        Some(B::CREATED) => ContainerState::Created,
        Some(B::RUNNING) => ContainerState::Running,
        Some(B::PAUSED) => ContainerState::Paused,
        Some(B::RESTARTING) => ContainerState::Restarting,
        Some(B::EXITED) => ContainerState::Exited,
        Some(B::DEAD) => ContainerState::Dead,
        // REMOVING / STOPPING / EMPTY / None have no precise cairn equivalent.
        _ => ContainerState::Unknown,
    }
}

#[async_trait]
impl ContainerOps for BollardDocker {
    async fn list_containers(&self) -> Result<Vec<ContainerInfo>, VfsError> {
        // `all: true` so stopped/exited containers are listed too (the Engine default is
        // running-only), keeping `list` and `stat` consistent across container states.
        let opts = bollard::query_parameters::ListContainersOptions {
            all: true,
            ..Default::default()
        };
        let list = self
            .docker
            .list_containers(Some(opts))
            .await
            .map_err(backend_err)?;
        Ok(list
            .into_iter()
            .map(|c| {
                let id = c.id.unwrap_or_default();
                // Fall back to the id when a container has no names, so the entry stays
                // navigable rather than collapsing to an empty path segment.
                let name = c
                    .names
                    .unwrap_or_default()
                    .first()
                    .map(|n| n.trim_start_matches('/').to_owned())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| id.clone());
                ContainerInfo {
                    id,
                    name,
                    image: c.image.unwrap_or_default(),
                    state: map_state(c.state),
                }
            })
            .collect())
    }

    async fn list_images(&self) -> Result<Vec<ImageInfo>, VfsError> {
        let list = self
            .docker
            .list_images(None::<bollard::query_parameters::ListImagesOptions>)
            .await
            .map_err(backend_err)?;
        Ok(list
            .into_iter()
            .map(|i| ImageInfo {
                id: i.id,
                tags: i.repo_tags,
            })
            .collect())
    }

    async fn list_dir(&self, _container: &str, _path: &str) -> Result<Vec<RemoteEntry>, VfsError> {
        Err(VfsError::Unsupported(Caps::LIST))
    }

    async fn stat(&self, _container: &str, _path: &str) -> Result<RemoteMeta, VfsError> {
        Err(VfsError::Unsupported(Caps::LIST))
    }

    async fn read(&self, _container: &str, _path: &str) -> Result<Vec<u8>, VfsError> {
        Err(VfsError::Unsupported(Caps::READ))
    }
}
