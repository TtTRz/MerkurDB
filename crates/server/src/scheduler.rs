use merkur_core::{
    AdjudicationWrite, ConsolidationReport, Consolidator, Forgetter, LevelAction, MemorySnapshot,
    Storage,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, error, info};

pub struct Scheduler {
    state: crate::app_state::AppState,
}

impl Scheduler {
    pub fn new(state: crate::app_state::AppState) -> Self {
        Self { state }
    }

    /// Separate loops keep slow consolidation from delaying forgetting.
    /// Shutdown drains any owned job, including one started through HTTP.
    pub async fn run(self: Arc<Self>, shutdown: watch::Receiver<bool>) {
        let consolidation = async {
            let mut shutdown = shutdown.clone();
            let mut ticker = tokio::time::interval(Duration::from_secs(
                self.state.config.consolidation.interval_seconds,
            ));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            ticker.reset_after(Duration::from_secs(5));
            loop {
                if *shutdown.borrow() {
                    break;
                }
                tokio::select! {
                    biased;
                    result = shutdown.changed() => {
                        if result.is_err() || *shutdown.borrow() { break; }
                    }
                    _ = ticker.tick() => {
                        if let Some(job) = self.state.start_consolidation()
                            && let Err(e) = job.await {
                                error!("Consolidation task failed: {e}");
                            }
                    }
                }
            }
            self.state.consolidation.wait_idle().await;
        };
        let forgetting = async {
            let mut shutdown = shutdown.clone();
            let mut ticker = tokio::time::interval(Duration::from_secs(
                self.state.config.forgetting.interval_seconds,
            ));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                if *shutdown.borrow() {
                    break;
                }
                tokio::select! {
                    biased;
                    result = shutdown.changed() => {
                        if result.is_err() || *shutdown.borrow() { break; }
                    }
                    _ = ticker.tick() => { self.run_forgetting().await; }
                }
            }
        };
        tokio::join!(consolidation, forgetting);
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
                return ConsolidationReport {
                    errors: 1,
                    ..Default::default()
                };
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
                let mut report = ConsolidationReport {
                    errors: 1,
                    ..Default::default()
                };
                if let Err(e) = storage
                    .log_consolidation(started_at, chrono::Utc::now(), &report)
                    .await
                {
                    error!("Failed to log consolidation: {e}");
                    report.errors += 1;
                }
                return report;
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

        // Write governance (P1-7): adjudicate each pending memory against its
        // nearest neighbors in the same bucket. UPDATE/DELETE are destructive,
        // so an LLM verdict alone is never enough — it executes only when the
        // pair's cosine similarity clears `adjudication_floor`.
        //
        // Candidates are pre-filtered by that floor: a set with no pair above
        // it can never execute, so the adjudication call is skipped as pure
        // cost. Verdict *gathering* (candidate search + LLM call, both
        // read-only) runs concurrently; verdict *execution* (absorb /
        // invalidate writes) runs serially in pending order. Storage checks the original
        // evidence revisions atomically, including conflicts with earlier verdicts or users.
        if adjudication_candidates > 0 {
            const ADJUDICATION_CONCURRENCY: usize = 4;

            let pending_ids: Vec<String> = pending.iter().map(|m| m.id.clone()).collect();
            let embeddings = match storage.get_embeddings(&pending_ids).await {
                Ok(e) => e,
                Err(e) => {
                    error!("Failed to fetch embeddings for adjudication: {e}");
                    report.errors += 1;
                    Default::default()
                }
            };

            // (pending index, verdict, above-floor candidates, adjudication failed)
            type Gathered = (
                usize,
                merkur_core::Adjudication,
                Vec<merkur_core::ScoredMemory>,
                bool,
            );
            let embeddings = &embeddings;
            let mut gathered: Vec<Gathered> = Vec::new();
            let indexed: Vec<(usize, &merkur_core::Memory)> = pending.iter().enumerate().collect();
            for chunk in indexed.chunks(ADJUDICATION_CONCURRENCY) {
                let futs: Vec<_> = chunk
                    .iter()
                    .map(|&(index, memory)| async move {
                        // Embeddings are optional: text-only memories can finish
                        // extraction without vector adjudication. Batch fetch errors
                        // are recorded above and still prevent acknowledgement.
                        let embedding = embeddings.get(&memory.id)?;
                        let hits = match storage
                            .vector_search_ns(
                                embedding,
                                &memory.namespace,
                                adjudication_candidates + 1,
                            )
                            .await
                        {
                            Ok(h) => h,
                            Err(e) => {
                                error!(
                                    "Adjudication candidate search failed for {}: {e}",
                                    memory.id
                                );
                                return Some((
                                    index,
                                    merkur_core::Adjudication::default(),
                                    Vec::new(),
                                    true,
                                ));
                            }
                        };
                        let candidates: Vec<_> = hits
                            .into_iter()
                            .filter(|h| h.id != memory.id && h.score >= adjudication_floor)
                            .take(adjudication_candidates)
                            .collect();
                        if candidates.is_empty() {
                            return None;
                        }
                        match consolidator.adjudicate(memory, &candidates).await {
                            Ok(verdict) => Some((index, verdict, candidates, false)),
                            Err(e) => {
                                error!("Adjudication failed for {}: {e}", memory.id);
                                Some((
                                    index,
                                    merkur_core::Adjudication::default(),
                                    Vec::new(),
                                    true,
                                ))
                            }
                        }
                    })
                    .collect();
                gathered.extend(futures::future::join_all(futs).await.into_iter().flatten());
            }

            // Execute in pending order, exactly as a serial loop would.
            gathered.sort_by_key(|(index, _, _, _)| *index);
            for (index, verdict, candidates, failed) in gathered {
                if failed {
                    report.errors += 1;
                    continue;
                }
                let memory = &pending[index];
                let score_of = |id: &str| candidates.iter().find(|c| c.id == id).map(|c| c.score);
                match verdict.action {
                    merkur_core::AdjudicationAction::Add
                    | merkur_core::AdjudicationAction::Noop => {}
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
                                let candidate = candidates.iter().find(|c| c.id == target).unwrap();
                                let write = AdjudicationWrite::Absorb {
                                    pending: MemorySnapshot::from(memory),
                                    target: MemorySnapshot {
                                        id: candidate.id.clone(),
                                        namespace: candidate.namespace.clone(),
                                        content_revision: candidate.content_revision,
                                    },
                                };
                                match storage.apply_adjudication(&write).await {
                                    Ok(true) => {
                                        consolidated_ids.retain(|id| id != target);
                                        report.absorptions += 1;
                                        debug!(absorbed = %memory.id, into = target, sim,
                                            reason = %verdict.reason, "absorbed pending memory");
                                    }
                                    Ok(false) => {
                                        consolidated_ids.retain(|id| id != &memory.id);
                                        debug!(id = %memory.id, target, "stale UPDATE skipped; retry pending");
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
                        // All candidates clear the floor. Self DELETE does not
                        // identify the contradiction partner, so guard every input.
                        let evidence = if target == memory.id {
                            candidates.first().map(|c| c.score)
                        } else {
                            score_of(target)
                        };
                        match evidence {
                            Some(sim) if sim >= adjudication_floor => {
                                let candidate_snapshots = candidates
                                    .iter()
                                    .filter(|c| target == memory.id || c.id == target)
                                    .map(|c| MemorySnapshot {
                                        id: c.id.clone(),
                                        namespace: c.namespace.clone(),
                                        content_revision: c.content_revision,
                                    })
                                    .collect();
                                let write = AdjudicationWrite::Invalidate {
                                    pending: MemorySnapshot::from(memory),
                                    candidates: candidate_snapshots,
                                    target_id: target.to_string(),
                                };
                                match storage.apply_adjudication(&write).await {
                                    Ok(true) => {
                                        report.invalidations += 1;
                                        debug!(target, sim, reason = %verdict.reason,
                                            "invalidated memory per DELETE verdict");
                                    }
                                    Ok(false) => {
                                        consolidated_ids.retain(|id| id != &memory.id);
                                        debug!(id = %memory.id, target, "stale DELETE skipped; retry pending");
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

        let snapshots: Vec<_> = pending
            .iter()
            .filter(|m| consolidated_ids.contains(&m.id))
            .map(MemorySnapshot::from)
            .collect();
        // Retry failed batches without pretending partial work is settled.
        // Existing writes remain visible; this is not an exactly-once transaction.
        if report.errors == 0
            && !consolidated_ids.is_empty()
            && let Err(e) = storage.mark_consolidated_if_unchanged(&snapshots).await
        {
            error!("Failed to mark consolidated: {e}");
            report.errors += 1;
        }

        let finished_at = chrono::Utc::now();
        if let Err(e) = storage
            .log_consolidation(started_at, finished_at, &report)
            .await
        {
            error!("Failed to log consolidation: {e}");
            report.errors += 1;
        }

        info!(
            "Consolidation complete: {} processed, {} edges, {} errors",
            report.memories_processed, report.edges_created, report.errors
        );

        report
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

        match storage.cleanup_orphan_vectors().await {
            Ok(processed) if processed > 0 => info!("Processed {processed} orphan vector keys"),
            Err(e) => error!("Orphan vector cleanup failed; retained for retry: {e}"),
            _ => {}
        }

        (archived, downgraded, upgraded, cleaned, purged)
    }

    async fn run_forgetting(&self) {
        Self::run_forgetting_once(
            &*self.state.storage,
            &*self.state.forgetter,
            self.state.config.forgetting.batch_size,
            self.state.config.forgetting.archive_days,
            self.state.config.forgetting.purge_invalidated_days,
        )
        .await;
    }
}
