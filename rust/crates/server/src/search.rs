//! `ReindexSearchEntryJob` and `SearchEntries.enqueue_reindex`.

use loco_jobs::{EnqueueOptions, Job};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReindexSearchEntry {
    pub record_type: String,
    pub record_id: i64,
}

impl Job for ReindexSearchEntry {
    const KIND: &'static str = "reindex_search_entry";

    type Context = AppState;

    async fn run(self, state: AppState) -> anyhow::Result<()> {
        loco_db::search::reindex(&state.db, &self.record_type, self.record_id).await?;
        Ok(())
    }
}

/// Enqueues reindexing of the records. Failures are logged: the search index is not
/// worth failing the request for.
pub async fn reindex_later(state: &AppState, record_type: &str, ids: &[i64]) {
    for &record_id in ids {
        let job = ReindexSearchEntry {
            record_type: record_type.to_owned(),
            record_id,
        };
        if let Err(err) = loco_jobs::enqueue(&state.db, &job, EnqueueOptions::default()).await {
            tracing::error!(%err, record_type, record_id, "enqueueing a reindex failed");
        }
    }
}
