#!/usr/bin/env bash
set -euo pipefail

# Disposable fixture for the show-me capture: a small service whose repository
# deletion path crosses enough components (HTTP -> registry -> workflow ->
# tokens/git) that a diagram explains it better than prose. Source-only on
# purpose — nothing here is compiled, so the capture spends its time on the
# agent rather than on rustc.

demo_dir=/tmp/yolop-show-me-orbit
session_dir=/tmp/yolop-show-me-sessions

rm -rf "$demo_dir" "$session_dir"
mkdir -p "$demo_dir/src/http" "$demo_dir/src/registry" "$demo_dir/src/workflows" "$session_dir"

cat >"$demo_dir/Cargo.toml" <<'EOF'
[package]
name = "orbit-registry"
version = "0.1.0"
edition = "2024"

[dependencies]
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.151"
EOF

cat >"$demo_dir/README.md" <<'EOF'
# Orbit Registry

Stores git repositories for the Orbit platform.

Deletion is a two-phase operation: the API tombstones the row synchronously so
reads and listings stop serving it, then a deterministic workflow performs the
physical cleanup in the background.
EOF

cat >"$demo_dir/src/main.rs" <<'EOF'
mod git;
mod http;
mod registry;
mod tokens;
mod workflows;

fn main() {
    http::serve("0.0.0.0:8080");
}
EOF

cat >"$demo_dir/src/http/mod.rs" <<'EOF'
pub mod repos;

/// Routes every request. `DELETE /namespaces/:namespace/repos/:name` is handled
/// by [`repos::delete_repo`].
pub fn serve(addr: &str) {
    println!("orbit-registry listening on {addr}");
}
EOF

cat >"$demo_dir/src/http/repos.rs" <<'EOF'
use crate::registry::shard;
use crate::workflows::delete_repo::DeleteRepoWorkflow;

pub struct Accepted {
    pub id: String,
}

/// `DELETE /namespaces/:namespace/repos/:name`
///
/// Resolves the caller's path to an exact repo id, tombstones it, and hands the
/// physical cleanup to a deterministic workflow. Returns `202 { id }` as soon as
/// the tombstone is durable — the caller never waits for storage.
pub fn delete_repo(namespace: &str, name: &str) -> Accepted {
    let repo_id = shard::resolve_repo_id(namespace, name);
    let target = shard::set_deleted_at(&repo_id);

    let workflow = DeleteRepoWorkflow::create(&format!("delete-repo-{repo_id}"));
    workflow.start(&target);

    Accepted { id: workflow.id }
}
EOF

cat >"$demo_dir/src/registry/mod.rs" <<'EOF'
pub mod shard;
EOF

cat >"$demo_dir/src/registry/shard.rs" <<'EOF'
/// A repository row on one registry shard.
pub struct DeletionTarget {
    pub repo_id: String,
    pub storage_key: String,
}

/// Resolves `namespace/name` to the exact repo id on this shard.
pub fn resolve_repo_id(namespace: &str, name: &str) -> String {
    format!("{namespace}/{name}")
}

/// Writes the tombstone. Once `deleted_at` is set the row is invisible to reads
/// and listings, which is what makes the rest of the cleanup safe to do later.
pub fn set_deleted_at(repo_id: &str) -> DeletionTarget {
    DeletionTarget {
        repo_id: repo_id.to_string(),
        storage_key: format!("repos/{repo_id}.git"),
    }
}

/// Confirms the tombstone still matches before anything is destroyed.
pub fn verify_tombstone(target: &DeletionTarget) -> bool {
    !target.repo_id.is_empty()
}

/// Removes the row for good, after every external resource is gone.
pub fn hard_delete(target: &DeletionTarget) {
    let _ = target;
}
EOF

cat >"$demo_dir/src/workflows/mod.rs" <<'EOF'
pub mod delete_repo;
EOF

cat >"$demo_dir/src/workflows/delete_repo.rs" <<'EOF'
use crate::git;
use crate::registry::shard::{self, DeletionTarget};
use crate::tokens;

/// Deterministic cleanup for one tombstoned repository. The id is derived from
/// the repo id, so a retry re-attaches to the same run instead of deleting twice.
pub struct DeleteRepoWorkflow {
    pub id: String,
}

impl DeleteRepoWorkflow {
    pub fn create(id: &str) -> Self {
        Self { id: id.to_string() }
    }

    /// Order matters: re-verify the tombstone, revoke access, drop the reverse
    /// indexes, reset storage, and only then hard-delete the row.
    pub fn start(&self, target: &DeletionTarget) {
        if !shard::verify_tombstone(target) {
            return;
        }

        tokens::delete_all_for_repo(&target.repo_id);
        tokens::remove_reverse_indexes(&target.repo_id);
        git::server::reset_storage(&target.storage_key);

        shard::hard_delete(target);
    }
}
EOF

cat >"$demo_dir/src/tokens.rs" <<'EOF'
/// Revokes every access token issued for the repository.
pub fn delete_all_for_repo(repo_id: &str) {
    let _ = repo_id;
}

/// Drops the KV reverse indexes that map tokens back to the repository.
pub fn remove_reverse_indexes(repo_id: &str) {
    let _ = repo_id;
}
EOF

mkdir -p "$demo_dir/src/git"
cat >"$demo_dir/src/git/mod.rs" <<'EOF'
pub mod server;
EOF

cat >"$demo_dir/src/git/server.rs" <<'EOF'
/// Resets the on-disk git storage backing a repository.
pub fn reset_storage(storage_key: &str) {
    let _ = storage_key;
}
EOF

(
    cd "$demo_dir"
    git init -q .
    git add -A
    git -c user.email=demo@example.com -c user.name=Demo commit -qm "orbit-registry fixture"
)
