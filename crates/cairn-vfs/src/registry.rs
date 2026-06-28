//! The [`VfsRegistry`]: holds connected backend instances behind `Arc<dyn Vfs>`.

use crate::vfs::Vfs;
use cairn_types::ConnectionId;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// A registry of connected backends, keyed by [`ConnectionId`].
///
/// The effect runner resolves a `ConnectionId` to an `Arc<dyn Vfs>` here before performing I/O. The
/// registry is cheap to clone (`Arc` inside) and safe to share across tasks.
#[derive(Clone, Default)]
pub struct VfsRegistry {
    inner: Arc<RwLock<HashMap<ConnectionId, Arc<dyn Vfs>>>>,
}

impl VfsRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) a backend for a connection.
    pub async fn insert(&self, id: ConnectionId, backend: Arc<dyn Vfs>) {
        self.inner.write().await.insert(id, backend);
    }

    /// Look up a backend by connection id.
    pub async fn get(&self, id: ConnectionId) -> Option<Arc<dyn Vfs>> {
        self.inner.read().await.get(&id).cloned()
    }

    /// Remove a backend, returning it if present.
    pub async fn remove(&self, id: ConnectionId) -> Option<Arc<dyn Vfs>> {
        self.inner.write().await.remove(&id)
    }

    /// The number of registered connections.
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    /// Whether the registry is empty.
    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockVfs;

    #[tokio::test]
    async fn insert_get_remove() {
        let reg = VfsRegistry::new();
        assert!(reg.is_empty().await);
        let id = ConnectionId(1);
        reg.insert(id, Arc::new(MockVfs::new(id))).await;
        assert_eq!(reg.len().await, 1);
        assert!(reg.get(id).await.is_some());
        assert!(reg.remove(id).await.is_some());
        assert!(reg.is_empty().await);
    }
}
