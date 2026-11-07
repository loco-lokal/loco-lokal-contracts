//! `ActiveStorage::PurgeJob`: deletes blobs (and their files) nothing is attached to.

use loco_jobs::{EnqueueOptions, Job};
use loco_storage::Blob;
use serde::{Deserialize, Serialize};

use crate::state::AppState;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PurgeBlobs {
    pub blob_ids: Vec<i64>,
}

impl Job for PurgeBlobs {
    const KIND: &'static str = "purge_blobs";

    type Context = AppState;

    async fn run(self, state: AppState) -> anyhow::Result<()> {
        for id in self.blob_ids {
            if let Some(blob) = Blob::delete_if_unattached(&state.db, id).await? {
                // The row is gone either way; a leftover file is only wasted space.
                if let Err(err) = state.storage.delete(&blob).await {
                    tracing::warn!(%err, blob_id = id, "deleting a purged file failed");
                }
            }
        }
        Ok(())
    }
}

/// Enqueues a purge after the records using the blobs were deleted.
pub async fn purge_later(state: &AppState, blob_ids: Vec<i64>) -> anyhow::Result<()> {
    if !blob_ids.is_empty() {
        loco_jobs::enqueue(
            &state.db,
            &PurgeBlobs { blob_ids },
            EnqueueOptions::default(),
        )
        .await?;
    }
    Ok(())
}
