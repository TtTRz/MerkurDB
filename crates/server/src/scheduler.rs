use merkur_core::{ConsolidationReport, Consolidator, Forgetter, LevelAction, Storage};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, error, info};

pub struct Scheduler {
    storage: Arc<dyn Storage>,
    consolidator: Arc<dyn Consolidator>,
    forgetter: Arc<dyn Forgetter>,
    consolidation_interval: Duration,
    consolidation_batch_size: usize,
    forgetting_interval: Duration,
    forgetting_batch_size: usize,
    archive_days: i32,
}

impl Scheduler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        storage: Arc<dyn Storage>,
        consolidator: Arc<dyn Consolidator>,
        forgetter: Arc<dyn Forgetter>,
        consolidation_interval: Duration,
        consolidation_batch_size: usize,
        forgetting_interval: Duration,
        forgetting_batch_size: usize,
        archive_days: i32,
    ) -> Self {
        Self {
            storage,
            consolidator,
            forgetter,
            consolidation_interval,
            consolidation_batch_size,
            forgetting_interval,
            forgetting_batch_size,
            archive_days,
        }
    }

    /// Run until the shutdown channel fires. The current tick is allowed to
    /// finish before exiting so we don't truncate a half-written consolidation.
    pub async fn run(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        let mut consolidate_ticker = tokio::time::interval(self.consolidation_interval);
        let mut forget_ticker = tokio::time::interval(self.forgetting_interval);
        consolidate_ticker.reset_after(Duration::from_secs(5));

        loop {
            tokio::select! {
                _ = consolidate_ticker.tick() => {
                    self.run_consolidation().await;
                }
                _ = forget_ticker.tick() => {
                    self.run_forgetting().await;
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("Scheduler received shutdown");
                        break;
                    }
                }
            }
        }
    }

    pub async fn run_consolidation_once(
        storage: &(dyn Storage + Send + Sync),
        consolidator: &(dyn Consolidator + Send + Sync),
        batch_size: usize,
    ) -> ConsolidationReport {
        let pending = match storage.list_pending(batch_size).await {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to list pending memories: {e}");
                return ConsolidationReport::empty();
            }
        };

        if pending.is_empty() {
            debug!("No pending memories to consolidate");
            return ConsolidationReport::empty();
        }

        info!("Consolidating {} pending memories", pending.len());

        let started_at = chrono::Utc::now();
        let mut report = match consolidator.consolidate(&pending).await {
            Ok(r) => r,
            Err(e) => {
                error!("Consolidation failed: {e}");
                return ConsolidationReport::empty();
            }
        };

        for (id, abstract_) in &report.new_abstracts {
            if let Err(e) = storage.insert_context_tag(id, "abstract", abstract_).await {
                error!("Failed to update abstract for {id}: {e}");
                report.errors += 1;
            }
        }

        // Track edges actually inserted vs proposed; this is what gets returned
        // to the client and persisted to the consolidate_log table.
        let mut actually_created = 0;
        for edge in &report.new_edges {
            match storage.insert_edge(edge).await {
                Ok(()) => actually_created += 1,
                Err(e) => {
                    error!(
                        "Failed to create edge {}->{}: {e}",
                        edge.source_id, edge.target_id
                    );
                    report.errors += 1;
                }
            }
        }
        report.edges_created = actually_created;

        let ids: Vec<String> = pending.iter().map(|m| m.id.clone()).collect();
        if let Err(e) = storage.mark_consolidated(&ids).await {
            error!("Failed to mark consolidated: {e}");
            report.errors += 1;
        }

        let finished_at = chrono::Utc::now();
        if let Err(e) = storage
            .log_consolidation(started_at, finished_at, &report)
            .await
        {
            error!("Failed to log consolidation: {e}");
        }

        info!(
            "Consolidation complete: {} processed, {} edges, {} errors",
            report.memories_processed, report.edges_created, report.errors
        );

        report
    }

    async fn run_consolidation(&self) {
        Self::run_consolidation_once(
            &*self.storage,
            &*self.consolidator,
            self.consolidation_batch_size,
        )
        .await;
    }

    pub async fn run_forgetting_once(
        storage: &(dyn Storage + Send + Sync),
        forgetter: &(dyn Forgetter + Send + Sync),
        batch_size: usize,
        archive_days: i32,
    ) -> (usize, usize, usize) {
        let memories = match storage.list_for_forgetting(batch_size).await {
            Ok(m) => m,
            Err(e) => {
                error!("Failed to list memories for forgetting: {e}");
                return (0, 0, 0);
            }
        };

        if memories.is_empty() {
            return (0, 0, 0);
        }

        let now = chrono::Utc::now();
        let mut archived = 0;
        let mut downgraded = 0;

        for memory in &memories {
            let action = forgetter.decide(memory, now);
            match action {
                LevelAction::Archive => {
                    if let Err(e) = storage.update_level(&memory.id, -1).await {
                        error!("Failed to archive {}: {e}", memory.id);
                    } else {
                        archived += 1;
                    }
                }
                LevelAction::Downgrade(level) => {
                    if let Err(e) = storage.update_level(&memory.id, level.to_i32()).await {
                        error!("Failed to downgrade {}: {e}", memory.id);
                    } else {
                        downgraded += 1;
                        debug!("Downgraded {} to {:?}", memory.id, level);
                    }
                }
                LevelAction::Keep => {}
            }
        }

        if archived > 0 || downgraded > 0 {
            info!(
                "Forgetting tick: archived={}, downgraded={}",
                archived, downgraded
            );
        }

        let cleaned = storage
            .delete_archived_older_than(archive_days)
            .await
            .unwrap_or(0);
        if cleaned > 0 {
            info!("Cleaned up {cleaned} archived memories");
        }

        (archived, downgraded, cleaned)
    }

    async fn run_forgetting(&self) {
        Self::run_forgetting_once(
            &*self.storage,
            &*self.forgetter,
            self.forgetting_batch_size,
            self.archive_days,
        )
        .await;
    }
}
