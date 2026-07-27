//! Live extension status, shared between the push surface and the read surface.
//!
//! A server's `status/changed` push has two consumers with different lifetimes:
//! the status bar wants the short text *now* (and only exists in the TUI), while
//! `list_extensions` wants the last-known state *later*, in any client. The
//! registry is the seam — the status sink records here on every push, and
//! `list_extensions` reads it — so `detail` and `level` survive a notification
//! the status bar renders lossily (or, in `--print`/ACP, never renders at all).
//!
//! Same shape as [`super::manager::LiveProcessRegistry`]: a cloneable handle
//! over shared state, built once in `runtime.rs` and handed to both sides.

use super::protocol::StatusChangedParams;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct StatusRegistry {
    entries: Arc<Mutex<BTreeMap<String, StatusChangedParams>>>,
}

impl StatusRegistry {
    /// Record `name`'s latest status. An empty `status` clears the entry — the
    /// protocol's "clear the field" semantics, so a cleared extension reads
    /// back as absent rather than as an empty string.
    pub fn record(&self, name: &str, params: &StatusChangedParams) {
        let mut entries = self.entries.lock().expect("extension status lock");
        if params.status.is_empty() {
            entries.remove(name);
        } else {
            entries.insert(name.to_string(), params.clone());
        }
    }

    /// The last status `name` pushed, if it has one and hasn't cleared it.
    pub fn get(&self, name: &str) -> Option<StatusChangedParams> {
        self.entries
            .lock()
            .expect("extension status lock")
            .get(name)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::protocol::StatusLevel;

    fn params(status: &str, level: StatusLevel, detail: Option<&str>) -> StatusChangedParams {
        StatusChangedParams {
            status: status.to_string(),
            level,
            detail: detail.map(str::to_string),
        }
    }

    #[test]
    fn records_the_latest_push_with_level_and_detail() {
        let registry = StatusRegistry::default();
        assert!(registry.get("cache-break").is_none());

        registry.record(
            "cache-break",
            &params("cache break on gpt-5", StatusLevel::Warn, Some("fell to 0")),
        );
        let stored = registry.get("cache-break").unwrap();
        assert_eq!(stored.status, "cache break on gpt-5");
        assert_eq!(stored.level, StatusLevel::Warn);
        assert_eq!(stored.detail.as_deref(), Some("fell to 0"));

        // A later push replaces it wholesale.
        registry.record("cache-break", &params("recovered", StatusLevel::Ok, None));
        let stored = registry.get("cache-break").unwrap();
        assert_eq!(stored.level, StatusLevel::Ok);
        assert!(stored.detail.is_none());
    }

    #[test]
    fn an_empty_status_clears_the_entry() {
        let registry = StatusRegistry::default();
        registry.record("cache-break", &params("warned", StatusLevel::Warn, None));
        registry.record("cache-break", &params("", StatusLevel::Ok, None));
        assert!(
            registry.get("cache-break").is_none(),
            "an empty status clears the field, it does not store an empty one"
        );
    }

    #[test]
    fn extensions_are_tracked_independently() {
        let registry = StatusRegistry::default();
        registry.record("cache-break", &params("warned", StatusLevel::Warn, None));
        registry.record("lsp", &params("rust-analyzer ready", StatusLevel::Ok, None));
        assert_eq!(registry.get("lsp").unwrap().status, "rust-analyzer ready");
        assert_eq!(
            registry.get("cache-break").unwrap().level,
            StatusLevel::Warn
        );
    }
}
