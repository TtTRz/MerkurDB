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
    purge_invalidated_days: i32,
    adjudication_floor: f64,
    adjudication_candidates: usize,
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
        purge_invalidated_days: i32,
        adjudication_floor: f64,
        adjudication_candidates: usize,
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
            purge_invalidated_days,
            adjudication_floor,
            adjudication_candidates,
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
        adjudication_floor: f64,
        adjudication_candidates: usize,
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

        // Only mark memories whose abstracts were actually persisted.
        let mut consolidated_ids: Vec<String> = Vec::new();
        for (id, abstract_) in &report.new_abstracts {
            match storage.update_abstract(id, abstract_).await {
                Ok(()) => consolidated_ids.push(id.clone()),
                Err(e) => {
                    error!("Failed to update abstract for {id}: {e}");
                    report.errors += 1;
                }
            }
        }

        // Importance follows the same persistence contract as abstracts:
        // only assessments for memories whose abstract landed are applied.
        for id in &consolidated_ids {
            if let Some(importance) = report.new_importance.get(id)
                && let Err(e) = storage.update_importance(id, *importance).await
            {
                error!("Failed to update importance for {id}: {e}");
                report.errors += 1;
            }
        }

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

        if !consolidated_ids.is_empty()
            && let Err(e) = storage.mark_consolidated(&consolidated_ids).await
        {
            error!("Failed to mark consolidated: {e}");
            report.errors += 1;
        }

        // Write governance (P1-7): adjudicate each pending memory against its
        // nearest neighbors in the same bucket. UPDATE/DELETE are destructive,
        // so an LLM verdict alone is never enough — it executes only when the
        // pair's cosine similarity clears `adjudication_floor`.
        if adjudication_candidates > 0 {
            let pending_ids: Vec<String> = pending.iter().map(|m| m.id.clone()).collect();
            let embeddings = match storage.get_embeddings(&pending_ids).await {
                Ok(e) => e,
                Err(e) => {
                    error!("Failed to fetch embeddings for adjudication: {e}");
                    Default::default()
                }
            };
            for memory in &pending {
                let Some(embedding) = embeddings.get(&memory.id) else {
                    continue;
                };
                let hits = match storage
                    .vector_search_ns(embedding, &memory.namespace, adjudication_candidates + 1)
                    .await
                {
                    Ok(h) => h,
                    Err(e) => {
                        error!("Adjudication candidate search failed for {}: {e}", memory.id);
                        continue;
                    }
                };
                let candidates: Vec<_> = hits
                    .into_iter()
                    .filter(|h| h.id != memory.id)
                    .take(adjudication_candidates)
                    .collect();
                if candidates.is_empty() {
                    continue;
                }
                let verdict = match consolidator.adjudicate(memory, &candidates).await {
                    Ok(v) => v,
                    Err(e) => {
                        error!("Adjudication failed for {}: {e}", memory.id);
                        report.errors += 1;
                        continue;
                    }
                };
                let score_of = |id: &str| {
                    candidates.iter().find(|c| c.id == id).map(|c| c.score)
                };
                match verdict.action {
                    merkur_core::AdjudicationAction::Add | merkur_core::AdjudicationAction::Noop => {}
                    merkur_core::AdjudicationAction::Update => {
                        let Some(target) = verdict.target_id.as_deref() else {
                            continue;
                        };
                        if target == memory.id {
                            // An UPDATE pointing at the pending memory itself
                            // is meaningless; the parser should have dropped it.
                            continue;
                        }
                        match score_of(target) {
                            Some(sim) if sim >= adjudication_floor => {
                                // Absorb: the target takes the new content
                                // (keeping its learned salience and edges);
                                // the pending row is invalidated with a
                                // pointer for audit.
                                match storage
                                    .update_memory(target, &memory.content, Some(embedding))
                                    .await
                                {
                                    Ok(()) => {
                                        match storage
                                            .invalidate_memory(&memory.id, Some(target))
                                            .await
                                        {
                                            Ok(()) => {
                                                report.absorptions += 1;
                                                debug!(
                                                    absorbed = %memory.id,
                                                    into = %target,
                                                    sim,
                                                    reason = %verdict.reason,
                                                    "absorbed pending memory into existing one"
                                                );
                                            }
                                            Err(e) => {
                                                error!("Failed to invalidate {}: {e}", memory.id);
                                                report.errors += 1;
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        error!("Failed to absorb into {target}: {e}");
                                        report.errors += 1;
                                    }
                                }
                            }
                            other => {
                                debug!(
                                    id = %memory.id,
                                    target,
                                    similarity = ?other,
                                    floor = adjudication_floor,
                                    "UPDATE verdict below similarity floor; skipped"
                                );
                            }
                        }
                    }
                    merkur_core::AdjudicationAction::Delete => {
                        let Some(target) = verdict.target_id.as_deref() else {
                            continue;
                        };
                        // Evidence for the pair: the target's own similarity,
                        // or — when the pending memory itself loses — the top
                        // candidate's (the contradiction partner).
                        let evidence = if target == memory.id {
                            candidates.first().map(|c| c.score)
                        } else {
                            score_of(target)
                        };
                        match evidence {
                            Some(sim) if sim >= adjudication_floor => {
                                match storage.invalidate_memory(target, None).await {
                                    Ok(()) => {
                                        report.invalidations += 1;
                                        debug!(
                                            target,
                                            sim,
                                            reason = %verdict.reason,
                                            "invalidated memory per DELETE verdict"
                                        );
                                    }
                                    Err(e) => {
                                        error!("Failed to invalidate {target}: {e}");
                                        report.errors += 1;
                                    }
                                }
                            }
                            other => {
                                debug!(
                                    id = %memory.id,
                                    target,
                                    similarity = ?other,
                                    floor = adjudication_floor,
                                    "DELETE verdict below similarity floor; skipped"
                                );
                            }
                        }
                    }
                }
            }
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
            self.adjudication_floor,
            self.adjudication_candidates,
        )
        .await;
    }

    pub async fn run_forgetting_once(
        storage: &(dyn Storage + Send + Sync),
        forgetter: &(dyn Forgetter + Send + Sync),
        batch_size: usize,
        archive_days: i32,
        purge_invalidated_days: i32,
    ) -> (usize, usize, usize, usize, usize) {
        let now = chrono::Utc::now();
        let mut archived = 0;
        let mut downgraded = 0;
        let mut upgraded = 0;

        let memories = match storage.list_for_forgetting(batch_size).await {
            Ok(m) => m,
            Err(e) => {
                error!("Failed to list memories for forgetting: {e}");
                Vec::new()
            }
        };

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
                LevelAction::Upgrade(level) => {
                    if let Err(e) = storage.update_level(&memory.id, level.to_i32()).await {
                        error!("Failed to upgrade {}: {e}", memory.id);
                    } else {
                        upgraded += 1;
                        debug!("Upgraded {} to {:?}", memory.id, level);
                    }
                }
                LevelAction::Keep => {}
            }
        }

        if archived > 0 || downgraded > 0 || upgraded > 0 {
            info!(
                "Forgetting tick: archived={}, downgraded={}, upgraded={}",
                archived, downgraded, upgraded
            );
        }

        let cleaned = storage
            .delete_archived_older_than(archive_days)
            .await
            .unwrap_or(0);
        if cleaned > 0 {
            info!("Cleaned up {cleaned} archived memories");
        }

        // Write-governance retention (P1-7): hard-delete rows whose
        // soft-invalidation is older than the audit window. Runs even when
        // the forgetting candidate list is empty — invalidated rows are
        // excluded from that list by definition.
        let purged = storage
            .purge_invalidated_older_than(purge_invalidated_days)
            .await
            .unwrap_or(0);
        if purged > 0 {
            info!("Purged {purged} invalidated memories");
        }

        (archived, downgraded, upgraded, cleaned, purged)
    }

    async fn run_forgetting(&self) {
        Self::run_forgetting_once(
            &*self.storage,
            &*self.forgetter,
            self.forgetting_batch_size,
            self.archive_days,
            self.purge_invalidated_days,
        )
        .await;
    }
}
