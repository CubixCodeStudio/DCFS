//! Garbage collection: deleting a file eventually deletes its bytes.
//!
//! Unlinking a file and overwriting one both leave objects behind, because
//! objects are immutable and shared — an edit reuses the object ids of the
//! chunks it did not touch. This sweep finds objects that no live version
//! references any more and deletes them from the object store, which is what
//! removes the attachment once the Discord backend is wired in.
//!
//! Nothing is deleted until it has been dead for the retention period, so a
//! commit that is still in flight, or an operator who wants a window to undo,
//! is not raced.

use dcfs_core::ObjectId;
use dcfs_db::MetadataRepository;
use dcfs_objectstore::{ObjectLocator, ObjectStore, ObjectStoreError};
use std::sync::Arc;
use std::time::Duration;

/// What one sweep did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub objects_deleted: u64,
    pub chunk_rows_removed: u64,
    pub versions_purged: u64,
    pub nodes_purged: u64,
    pub sessions_purged: u64,
    pub sizes_corrected: u64,
}

/// How many objects one sweep will delete before yielding, so a large backlog
/// cannot hold a database connection or the object store for minutes on end.
const BATCH: i64 = 500;

/// Run one sweep.
///
/// The object store is emptied before the metadata that points at it: a crash
/// in between leaves a metadata row referring to a deleted object, which the
/// next sweep cleans up. The reverse order would leak the object forever,
/// because nothing would remember it existed.
pub async fn collect_once(
    repo: &Arc<dyn MetadataRepository>,
    store: &Arc<dyn ObjectStore>,
    retention: Duration,
) -> Result<GcReport, String> {
    // One sweeper at a time across every instance. Another one running means
    // this tick is already covered, so there is nothing to wait for.
    let Some(_sweeping) = repo.try_lock_gc().await.map_err(|e| e.to_string())? else {
        tracing::debug!("another instance is sweeping; skipping this tick");
        return Ok(GcReport::default());
    };

    let cutoff = chrono::Utc::now()
        - chrono::Duration::from_std(retention).map_err(|e| format!("bad retention: {e}"))?;
    let mut report = GcReport::default();

    let doomed = repo
        .collectable_objects(cutoff, BATCH)
        .await
        .map_err(|e| e.to_string())?;

    for object_id in doomed {
        let locator = ObjectLocator::new(ObjectId::from_uuid(object_id));
        match store.delete(&locator).await {
            // Already gone is the desired end state, so keep going and clean
            // up the metadata that still points at it.
            Ok(()) | Err(ObjectStoreError::NotFound(_)) => {}
            Err(e) => {
                // One unreachable object must not stop the sweep; the next one
                // will try again.
                tracing::warn!(%object_id, "gc: cannot delete object: {e}");
                continue;
            }
        }
        report.chunk_rows_removed += repo
            .forget_object(object_id)
            .await
            .map_err(|e| e.to_string())?;
        report.objects_deleted += 1;
    }

    report.versions_purged = repo
        .purge_empty_dead_versions(cutoff)
        .await
        .map_err(|e| e.to_string())?;
    report.nodes_purged = repo
        .purge_deleted_nodes(cutoff)
        .await
        .map_err(|e| e.to_string())?;
    // An expired session is already refused; this only stops the table growing.
    report.sessions_purged = repo
        .purge_expired_sessions(chrono::Utc::now())
        .await
        .map_err(|e| e.to_string())?;
    // A write in progress advances the file's size before it commits. If that
    // write was abandoned and has just been collected, the size has to come
    // back down to what the committed version actually holds.
    report.sizes_corrected = repo
        .reconcile_node_sizes()
        .await
        .map_err(|e| e.to_string())?;

    Ok(report)
}

/// Run [`collect_once`] forever, every `interval`.
pub fn spawn(
    repo: Arc<dyn MetadataRepository>,
    store: Arc<dyn ObjectStore>,
    retention: Duration,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            match collect_once(&repo, &store, retention).await {
                Ok(report) if report == GcReport::default() => {}
                Ok(report) => tracing::info!(
                    objects = report.objects_deleted,
                    versions = report.versions_purged,
                    nodes = report.nodes_purged,
                    sessions = report.sessions_purged,
                    sizes = report.sizes_corrected,
                    "gc swept"
                ),
                Err(e) => tracing::warn!("gc failed: {e}"),
            }
        }
    })
}
