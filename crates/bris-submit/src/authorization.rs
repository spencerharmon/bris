//! Per-root upload authorization and drain coordination. Authorization is
//! deliberately not restored from disk; destination bindings are.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::queue::QueueError;

/// A persistent upload generation, independent for manual and automatic work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadScope {
    /// Opaque generation supplied by the platform configuration owner.
    pub scope: String,
    /// Whether authorization came from the automatic-upload toggle.
    pub automatic: bool,
}

/// Cancellation wakes the actual HTTP future, including during response reads.
#[derive(Debug)]
pub struct Cancellation(watch::Sender<bool>);

impl Default for Cancellation {
    fn default() -> Self {
        Self(watch::channel(false).0)
    }
}

impl Cancellation {
    /// Cancel this operation. Safe to call from another thread.
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    /// Whether this operation was cancelled.
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    #[cfg(feature = "http")]
    pub(crate) async fn cancelled(&self) {
        let mut rx = self.0.subscribe();
        loop {
            if *rx.borrow_and_update() {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct PersistentAuthorization {
    pub manual: Option<String>,
    pub automatic: Option<String>,
    pub bindings: Vec<Binding>,
    #[serde(default)]
    pub revoked: Vec<UploadScope>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Binding {
    pub scope: UploadScope,
    pub fingerprint: String,
}

#[derive(Debug, Default)]
pub(crate) struct Authorization {
    pub manual: Option<String>,
    pub automatic: Option<String>,
    pub active: Vec<(Option<UploadScope>, Weak<Cancellation>)>,
}

impl Authorization {
    pub(crate) fn permits(&self, scope: &UploadScope) -> bool {
        let configured = if scope.automatic {
            &self.automatic
        } else {
            &self.manual
        };
        configured.as_deref() == Some(scope.scope.as_str()) && !scope.scope.is_empty()
    }

    pub(crate) fn cancel_all(&mut self) {
        self.active.retain(|(_, token)| {
            if let Some(token) = token.upgrade() {
                token.cancel();
                true
            } else {
                false
            }
        });
    }
}

#[derive(Debug, Default)]
pub(crate) struct Coordinator {
    // Never hold this mutex across network I/O or waiting for the drain lock.
    pub authorization: Mutex<Authorization>,
    pub drain: Mutex<()>,
}

pub(crate) fn coordinator(root: &Path) -> Result<Arc<Coordinator>, QueueError> {
    static ROOTS: OnceLock<Mutex<HashMap<PathBuf, Weak<Coordinator>>>> = OnceLock::new();
    let mut roots = ROOTS
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| QueueError::Poisoned)?;
    roots.retain(|_, value| value.strong_count() > 0);
    if let Some(shared) = roots.get(root).and_then(Weak::upgrade) {
        return Ok(shared);
    }
    let shared = Arc::new(Coordinator::default());
    roots.insert(root.to_owned(), Arc::downgrade(&shared));
    Ok(shared)
}
