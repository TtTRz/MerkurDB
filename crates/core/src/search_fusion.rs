//! Hybrid search fusion: FTS5 BM25 x vector cosine via Reciprocal Rank Fusion.

use crate::{MerkurResult, ScoredMemory, Storage};

/// Blend of the three relevance channels in the composite score.
///
/// Deliberately configurable rather than hardcoded: these defaults are a
/// conservative, *untuned* starting point (documented as such) — real weights
/// belong to a future public evaluation pass (P2-10).
#[derive(Debug, Clone, Copy)]
pub struct ScoreWeights {
    /// Share of the RRF-fused retrieval relevance.
    pub search: f64,
    /// Share of the stored (un-decayed) memory weight — how strongly the
    /// system was asked to remember this, independent of time.
    pub weight: f64,
    /// Share of the Consolidator-assessed importance.
    pub importance: f64,
}

impl Default for ScoreWeights {
    fn default() -> Self {
        Self {
            search: 0.5,
            weight: 0.2,
            importance: 0.3,
        }
    }
}

/// Composite retrieval score in [0, 1] under the default weights.
///
/// All three inputs are pre-normalized to a [0, 1] scale:
/// `fused` is the RRF-normalized relevance (1.0 = rank-1 in both channels),
/// `weight` is the raw stored weight clamped to [0, 1] (decay is the
/// forgetting curve's business, not retrieval's),
/// `importance` is the Consolidator's 0–1 assessment.
pub fn composite_score(fused: f64, weight: f64, importance: f64) -> f64 {
    composite_score_with(&ScoreWeights::default(), fused, weight, importance)
}

/// Composite score under explicit weights (P1-5: configurable via
/// `retrieval.fusion` in server config).
pub fn composite_score_with(w: &ScoreWeights, fused: f64, weight: f64, importance: f64) -> f64 {
    w.search * fused + w.weight * weight.clamp(0.0, 1.0) + w.importance * importance.clamp(0.0, 1.0)
}

/// Fusion tuning knobs (P1-5). Defaults reproduce the pre-P1-5 behavior
/// exactly: symmetric channels, k=60, 0.5/0.2/0.3 composite.
#[derive(Debug, Clone, Copy)]
pub struct FusionParams {
    pub rrf_k: f64,
    /// Relative share of the BM25 channel in the fused score.
    pub bm25_weight: f64,
    /// Relative share of the vector channel in the fused score.
    pub vector_weight: f64,
    pub score: ScoreWeights,
}

impl Default for FusionParams {
    fn default() -> Self {
        Self {
            rrf_k: DEFAULT_RRF_K,
            bm25_weight: 1.0,
            vector_weight: 1.0,
            score: ScoreWeights::default(),
        }
    }
}

/// Default RRF smoothing constant. `k = 60` is the value used across the
/// retrieval literature (and by mem0 / LangChain hybrid retrievers): it damps
/// the influence of rank position so a single channel cannot dominate purely
/// through steep internal score gradients.
pub const DEFAULT_RRF_K: f64 = 60.0;

/// Escape a raw user query into a safe FTS5 MATCH expression.
///
/// The entire input becomes one quoted phrase, which strips every operator
/// meaning (`AND`, `OR`, `NEAR`, `(`, `)`, `-`, `^`, `:`, `*`). Inner double
/// quotes are doubled per FTS5 string-literal rules. An empty input maps to an
/// empty output so callers can short-circuit.
pub fn escape_fts_query(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    format!("\"{}\"", raw.replace('"', "\"\""))
}

/// Whether the BM25 channel can contribute for this query at all.
///
/// The trigram tokenizer cannot index any term shorter than three characters,
/// so a trimmed query below that length would match nothing: callers skip the
/// BM25 round-trip and let the vector channel stand alone.
pub fn is_bm25_viable(raw: &str) -> bool {
    raw.trim().chars().count() >= 3
}

/// Reciprocal Rank Fusion over two ranked candidate lists.
///
/// Each list is `(id, channel_score)` pairs already ordered best-first; only
/// positions matter here, scores are ignored. Fused score for id *d*:
///
/// ```text
/// raw(d) = sum over channels of 1 / (k + rank(d) + 1)      // rank is 0-based
/// fused(d) = raw(d) / (channels / (k + 1))                 // theoretical max
/// ```
///
/// Normalizing by the fixed theoretical maximum — both channels rank-1 — keeps
/// scores in (0, 1] with stable semantics across requests (a `score_threshold`
/// means the same thing regardless of batch size). A memory found in a single
/// channel peaks at ~0.5; dual-channel hits rank strictly higher. Ties break
/// by ascending id so ordering is reproducible. Output is truncated to
/// `limit`.
pub fn rrf_fuse(
    bm25: &[(String, f64)],
    vector: &[(String, f64)],
    k: f64,
    limit: usize,
) -> Vec<(String, f64)> {
    rrf_fuse_weighted(bm25, vector, k, 1.0, 1.0, limit)
}

/// Weighted RRF: each channel contributes `weight / (k + rank + 1)`.
/// Normalizing by the weighted theoretical maximum — top rank in both
/// channels — keeps the (0, 1] score semantics of [`rrf_fuse`]. Zero weight
/// disables a channel's influence (its ids score 0 but are not removed).
#[allow(clippy::too_many_arguments)]
pub fn rrf_fuse_weighted(
    bm25: &[(String, f64)],
    vector: &[(String, f64)],
    k: f64,
    bm25_weight: f64,
    vector_weight: f64,
    limit: usize,
) -> Vec<(String, f64)> {
    let mut fused: std::collections::HashMap<&str, f64> = std::collections::HashMap::new();
    for (rank, (id, _)) in bm25.iter().enumerate() {
        *fused.entry(id.as_str()).or_insert(0.0) += bm25_weight / (k + rank as f64 + 1.0);
    }
    for (rank, (id, _)) in vector.iter().enumerate() {
        *fused.entry(id.as_str()).or_insert(0.0) += vector_weight / (k + rank as f64 + 1.0);
    }

    let weight_sum = bm25_weight + vector_weight;
    if limit == 0 || fused.is_empty() || weight_sum <= 0.0 {
        return Vec::new();
    }

    let max = weight_sum / (k + 1.0);
    let mut out: Vec<(String, f64)> = fused
        .into_iter()
        .map(|(id, s)| (id.to_string(), s / max))
        .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out.truncate(limit);
    out
}

/// Approximate token count for budget accounting.
///
/// Deliberately the industry-default `chars / 4` heuristic rather than a real
/// tokenizer: pulling in tiktoken would saddle MerkurDB's zero-dependency
/// binary with a tokenizer model. Callers that need exact accounting can
/// override via `chars_per_token` in config (documented as approximate).
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// Maximal-Marginal-Relevance style dedup: drop lower-scored items whose
/// content is near-identical to an already-kept higher-scored one.
///
/// Similarity is a cheap Jaccard over whitespace-split word sets — adequate
/// for catching verbatim/near-verbatim duplicates that hybrid retrieval
/// surfaces when the same fact was written twice. `threshold` in [0, 1].
pub fn mmr_dedup(items: &mut Vec<ScoredMemory>, threshold: f64) -> Vec<ScoredMemory> {
    fn word_set(s: &str) -> std::collections::HashSet<String> {
        s.split_whitespace().map(|w| w.to_string()).collect()
    }
    fn jaccard(
        a: &std::collections::HashSet<String>,
        b: &std::collections::HashSet<String>,
    ) -> f64 {
        if a.is_empty() || b.is_empty() {
            return 0.0;
        }
        let inter = a.intersection(b).count() as f64;
        let union = a.union(b).count() as f64;
        inter / union
    }

    // Highest score first so the best representative of each cluster wins.
    items.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut kept: Vec<ScoredMemory> = Vec::new();
    let mut kept_sets: Vec<std::collections::HashSet<String>> = Vec::new();
    'outer: for item in items.drain(..) {
        let cand = word_set(&item.content);
        for existing in &kept_sets {
            if jaccard(&cand, existing) >= threshold {
                continue 'outer;
            }
        }
        kept_sets.push(word_set(&item.content));
        kept.push(item);
    }
    kept
}

/// Greedy bin-packing: walk items best-first, keep whatever still fits the
/// remaining token budget, count the rest as dropped.
///
/// Greedy (not knapsack) on purpose: context assembly is latency-sensitive,
/// the marginal gain of optimal packing is negligible for memory-sized items,
/// and best-first greedy is deterministic and explainable.
pub fn greedy_pack(items: &[ScoredMemory], token_budget: usize) -> (Vec<ScoredMemory>, usize) {
    let mut packed = Vec::new();
    let mut dropped = 0;
    let mut remaining = token_budget;
    for item in items {
        let cost = estimate_tokens(&item.content);
        if cost <= remaining {
            remaining -= cost;
            packed.push(item.clone());
        } else {
            dropped += 1;
        }
    }
    (packed, dropped)
}

/// Run both retrieval channels and return their RRF-fused ranking as fully
/// populated records (`score` carries the composite value).
///
/// Thin orchestration shared by every search entry point (REST handler and
/// MCP tool) so they cannot drift apart. Oversamples each channel at
/// `limit * 2` before fusing — RRF needs more input ranks than the requested
/// output to keep tail quality. A single channel failing degrades to the
/// other one instead of failing the whole recall. Candidates that surfaced
/// only through BM25 are fetched by id; vanished ids are skipped, and a
/// transient hydration failure is logged and skipped rather than sinking the
/// recall.
///
/// `relevance_floor` gates on the **fused retrieval relevance** — the same
/// semantic `score_threshold` has against raw cosine in fast mode. It must
/// not gate on the composite score: the composite's structural floor
/// (`weight` + `importance` shares = 0.35 for a fresh memory at default
/// weights) sits above the default threshold and would silently disable it.
/// Gating before hydration also skips pointless `get_memory` round-trips for
/// candidates that would be filtered out anyway. Callers that want no gate
/// (context assembly, debugging CLIs) pass `0.0`.
pub async fn hybrid_recall(
    storage: &dyn Storage,
    query_vec: &[f32],
    raw_query: &str,
    namespace: &str,
    limit: usize,
    relevance_floor: f64,
    fusion: &FusionParams,
) -> MerkurResult<Vec<ScoredMemory>> {
    let oversample = limit.saturating_mul(2).max(limit);

    let vec_hits = match storage.vector_search_ns(query_vec, namespace, oversample).await {
        Ok(hits) => hits,
        Err(e) => {
            tracing::warn!(error = %e, "vector channel failed; falling back to BM25 only");
            Vec::new()
        }
    };

    let bm25 = if is_bm25_viable(raw_query) {
        match storage.text_search(raw_query, namespace, oversample).await {
            Ok(hits) => hits,
            Err(e) => {
                tracing::warn!(error = %e, "BM25 channel failed; falling back to vector only");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    let vec_ranked: Vec<(String, f64)> =
        vec_hits.iter().map(|s| (s.id.clone(), s.score)).collect();
    let fused = rrf_fuse_weighted(
        &bm25,
        &vec_ranked,
        fusion.rrf_k,
        fusion.bm25_weight,
        fusion.vector_weight,
        limit,
    );

    let mut by_id: std::collections::HashMap<String, ScoredMemory> = vec_hits
        .into_iter()
        .map(|m| (m.id.clone(), m))
        .collect();

    let mut out = Vec::with_capacity(fused.len());
    for (id, fused_score) in fused {
        if fused_score < relevance_floor {
            continue;
        }
        let mut memory = match by_id.remove(&id) {
            Some(m) => m,
            None => match storage.get_memory(&id).await {
                Ok(Some(m)) => scored_from_memory(m, fused_score),
                Ok(None) => continue,
                Err(e) => {
                    // The channel that surfaced this id succeeded; a transient
                    // hydration failure degrades to the remaining hits.
                    tracing::warn!(id = %id, error = %e, "hydrating BM25-only hit failed; skipping");
                    continue;
                }
            },
        };
        // P1-5: the externally visible score is the composite of retrieval
        // relevance, stored weight, and Consolidator importance. RRF rank
        // alone decided *which* ids made it here; this decides their order.
        memory.score =
            composite_score_with(&fusion.score, fused_score, memory.weight, memory.importance);
        out.push(memory);
    }
    // Composite, not RRF order, defines the final ranking.
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    Ok(out)
}

/// Project a stored [`crate::Memory`] into a [`ScoredMemory`] under an
/// externally computed score.
fn scored_from_memory(m: crate::Memory, score: f64) -> ScoredMemory {
    ScoredMemory {
        id: m.id,
        content: m.content,
        abstract_: m.abstract_,
        score,
        weight: m.weight,
        level: m.level,
        category: m.category,
        context: m.context,
        created_at: m.created_at,
        namespace: m.namespace,
        importance: m.importance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MerkurError;

    fn mk_scored(id: &str, content: &str, score: f64) -> ScoredMemory {
        ScoredMemory {
            id: id.into(),
            content: content.into(),
            abstract_: None,
            score,
            weight: 1.0,
            level: crate::MemoryLevel::Full,
            category: "general".into(),
            context: Default::default(),
            created_at: chrono::Utc::now(),
            namespace: crate::DEFAULT_NAMESPACE.to_string(),
            importance: crate::NEUTRAL_IMPORTANCE,
        }
    }

    // ---------- escape_fts_query ----------

    #[test]
    fn escape_wraps_plain_text_in_phrase() {
        assert_eq!(escape_fts_query("hello world"), "\"hello world\"");
    }

    #[test]
    fn escape_doubles_inner_quotes() {
        assert_eq!(escape_fts_query("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn escape_neutralizes_fts_operators() {
        // AND/OR/NEAR/-/^/: lose their operator meaning inside a quoted phrase.
        let escaped = escape_fts_query("rust AND (memory) OR -v8^");
        assert_eq!(escaped, "\"rust AND (memory) OR -v8^\"");
        // Must remain a single quoted expression.
        assert!(escaped.starts_with('"') && escaped.ends_with('"'));
    }

    #[test]
    fn escape_preserves_cjk() {
        assert_eq!(escape_fts_query("用户喜欢 Rust"), "\"用户喜欢 Rust\"");
    }

    #[test]
    fn escape_empty_yields_empty() {
        assert_eq!(escape_fts_query(""), "");
    }

    // ---------- is_bm25_viable ----------

    #[test]
    fn viable_requires_three_chars_after_trim() {
        assert!(!is_bm25_viable(""));
        assert!(!is_bm25_viable("ab"));
        assert!(!is_bm25_viable("  a  "));
        assert!(is_bm25_viable("abc"));
        assert!(is_bm25_viable("gc算法"));
    }

    // ---------- rrf_fuse ----------

    const K: f64 = DEFAULT_RRF_K;

    #[test]
    fn fuse_top_in_both_channels_normalizes_to_one() {
        let bm25 = vec![("a".to_string(), 3.0)];
        let vec = vec![("a".to_string(), 0.99)];
        let out = rrf_fuse(&bm25, &vec, K, 10);
        assert_eq!(out.len(), 1);
        let (id, score) = &out[0];
        assert_eq!(id, "a");
        let expected = (1.0 / (K + 1.0) * 2.0) / (2.0 / (K + 1.0));
        assert!((score - expected).abs() < 1e-9);
        assert!((*score - 1.0).abs() < 1e-9);
    }

    #[test]
    fn fuse_single_channel_hit_caps_below_one() {
        let bm25 = vec![];
        let vec = vec![("x".to_string(), 0.8)];
        let out = rrf_fuse(&bm25, &vec, K, 10);
        // rank-1 in only one channel: (1/(k+1)) / (2/(k+1)) = 0.5 exactly.
        assert!((out[0].1 - 0.5).abs() < 1e-9);
    }

    #[test]
    fn fuse_orders_dual_hits_above_single_hits() {
        let bm25 = vec![
            ("m1".to_string(), 5.0),
            ("b_only".to_string(), 4.0),
        ];
        let vec = vec![
            ("m1".to_string(), 0.9),
            ("v_only".to_string(), 0.5),
        ];
        let out = rrf_fuse(&bm25, &vec, K, 10);
        assert_eq!(out[0].0, "m1", "dual-channel top must win");
        let singles = &out[1..];
        assert_eq!(singles.len(), 2);
        // Each remaining id peaked at rank-2 of exactly one channel -> tie.
        assert!((singles[0].1 - singles[1].1).abs() < 1e-9);
    }

    #[test]
    fn fuse_breaks_equal_scores_by_id_ascending() {
        // Each id is rank-1 in exactly one channel -> identical fused scores.
        let bm25 = vec![("z_only".to_string(), 9.0)];
        let vec = vec![("a_only".to_string(), 0.7)];
        let out = rrf_fuse(&bm25, &vec, K, 10);
        assert!((out[0].1 - out[1].1).abs() < 1e-9);
        assert_eq!(out[0].0, "a_only");
        assert_eq!(out[1].0, "z_only");
    }

    #[test]
    fn fuse_respects_limit() {
        let bm25: Vec<(String, f64)> = (0..50).map(|i| (format!("m{i}"), 100.0 - i as f64)).collect();
        let vec: Vec<(String, f64)> = (0..30).map(|i| (format!("m{i}"), 0.5)).collect();
        let out = rrf_fuse(&bm25, &vec, K, 10);
        assert_eq!(out.len(), 10);
    }

    #[test]
    fn fuse_both_empty_yields_empty() {
        assert!(rrf_fuse(&[], &[], K, 10).is_empty());
    }

    // ---------- rrf_fuse_weighted / FusionParams (P1-5) ----------

    #[test]
    fn weighted_fuse_with_unit_weights_matches_rrf_fuse() {
        let bm25 = vec![("a".to_string(), 3.0), ("b".to_string(), 2.0)];
        let vec = vec![("a".to_string(), 0.9), ("c".to_string(), 0.8)];
        let plain = rrf_fuse(&bm25, &vec, K, 10);
        let weighted = rrf_fuse_weighted(&bm25, &vec, K, 1.0, 1.0, 10);
        assert_eq!(plain, weighted);
    }

    #[test]
    fn weighted_fuse_boosts_favored_channel() {
        // b_only ranks 1st in BM25, v_only ranks 1st in vector; symmetric
        // weights tie them, doubling bm25's share must flip the order.
        let bm25 = vec![("b_only".to_string(), 1.0)];
        let vec = vec![("v_only".to_string(), 1.0)];
        let tie = rrf_fuse_weighted(&bm25, &vec, K, 1.0, 1.0, 10);
        assert!((tie[0].1 - tie[1].1).abs() < 1e-9);
        let boosted = rrf_fuse_weighted(&bm25, &vec, K, 2.0, 1.0, 10);
        assert_eq!(boosted[0].0, "b_only");
        assert!(boosted[0].1 > boosted[1].1);
        let flipped = rrf_fuse_weighted(&bm25, &vec, K, 1.0, 2.0, 10);
        assert_eq!(flipped[0].0, "v_only");
    }

    #[test]
    fn weighted_fuse_keeps_normalized_score_semantics() {
        // Top in both channels under any positive weights still scores 1.0.
        let bm25 = vec![("a".to_string(), 1.0)];
        let vec = vec![("a".to_string(), 1.0)];
        let out = rrf_fuse_weighted(&bm25, &vec, K, 3.0, 0.5, 10);
        assert!((out[0].1 - 1.0).abs() < 1e-9);
    }

    #[test]
    fn composite_score_with_custom_weights() {
        // Pure-relevance configuration: score == fused, ignoring salience.
        let w = ScoreWeights { search: 1.0, weight: 0.0, importance: 0.0 };
        assert!((composite_score_with(&w, 0.7, 1.0, 0.5) - 0.7).abs() < 1e-9);
        // Default weights match the legacy function exactly.
        assert_eq!(
            composite_score(0.6, 0.9, 0.4),
            composite_score_with(&ScoreWeights::default(), 0.6, 0.9, 0.4)
        );
    }

    #[test]
    fn fusion_params_default_matches_legacy_behavior() {
        let p = FusionParams::default();
        assert_eq!(p.rrf_k, DEFAULT_RRF_K);
        assert_eq!(p.bm25_weight, 1.0);
        assert_eq!(p.vector_weight, 1.0);
    }

    // ---------- composite_score ----------

    #[test]
    fn composite_weights_relevance_weight_importance() {
        // ws=0.5 / wr=0.2 / wi=0.3, all normalized to unit ceiling.
        let s = composite_score(1.0, 1.0, 1.0);
        assert!((s - 1.0).abs() < 1e-9);

        // Dropping the retrieval channel leaves wr+wi behind.
        assert!((composite_score(0.0, 1.0, 1.0) - 0.5).abs() < 1e-9);

        // A perfect fused hit on an unremarkable memory.
        assert!((composite_score(1.0, 0.5, 0.5) - (0.5 + 0.1 + 0.15)).abs() < 1e-9);
    }

    #[test]
    fn composite_is_monotonic_in_each_input() {
        let base = composite_score(0.5, 0.5, 0.5);
        assert!(composite_score(0.6, 0.5, 0.5) > base);
        assert!(composite_score(0.5, 0.6, 0.5) > base);
        assert!(composite_score(0.5, 0.5, 0.6) > base);
    }

    // ---------- MMR + context packing (P1-6) ----------

    #[test]
    fn estimate_tokens_is_chars_over_four() {
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2); // ceil
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn mmr_dedups_near_identical_content() {
        // Two near-duplicates: the lower-scored one must be dropped.
        let mut items = vec![
            mk_scored("a", "the quick brown fox jumps", 0.9),
            mk_scored("b", "the quick brown fox jumps over", 0.7),
            mk_scored("c", "completely different content", 0.5),
        ];
        let kept = mmr_dedup(&mut items, 0.8);
        let ids: Vec<&str> = kept.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"a"), "highest-scored must survive");
        assert!(ids.contains(&"c"));
        assert!(!ids.contains(&"b"), "near-duplicate of a must be dropped");
    }

    #[test]
    fn greedy_pack_respects_token_budget() {
        let items = vec![
            mk_scored("big", &"x".repeat(400), 0.9),   // ~100 tokens
            mk_scored("mid", &"y".repeat(200), 0.8),   // ~50 tokens
            mk_scored("small", &"z".repeat(40), 0.7),  // ~10 tokens
        ];
        let (packed, dropped) = greedy_pack(&items, 60);
        let ids: Vec<&str> = packed.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"mid") && ids.contains(&"small"));
        assert_eq!(dropped, 1, "the 100-token item must not fit a 60-token budget");
    }

    #[test]
    fn greedy_pack_keeps_everything_when_budget_allows() {
        let items = vec![
            mk_scored("a", "short", 0.9),
            mk_scored("b", "also short", 0.8),
        ];
        let (packed, dropped) = greedy_pack(&items, 1000);
        assert_eq!(packed.len(), 2);
        assert_eq!(dropped, 0);
    }

    // ---------- hybrid_recall orchestration ----------

    /// Minimal Storage double exercising the recall surface: one vector hit
    /// (`v1`), configurable BM25 hits, switchable `get_memory` failure, and a
    /// call counter proving gated candidates never reach hydration. Every
    /// unrelated trait method is `unimplemented!` on purpose.
    struct StubStorage {
        bm25_hits: Vec<(String, f64)>,
        hydration_fails: bool,
        get_memory_calls: std::sync::atomic::AtomicUsize,
    }

    impl StubStorage {
        fn new(bm25_hits: Vec<(String, f64)>, hydration_fails: bool) -> Self {
            Self {
                bm25_hits,
                hydration_fails,
                get_memory_calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    fn hydrated_memory(id: &str) -> crate::Memory {
        crate::Memory {
            id: id.into(),
            content: format!("hydrated {id}"),
            abstract_: None,
            category: "general".into(),
            weight: 1.0,
            level: crate::MemoryLevel::Full,
            pending_consolidation: false,
            embedding: None,
            metadata: Default::default(),
            context: Default::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            accessed_at: chrono::Utc::now(),
            access_count: 0,
            namespace: crate::DEFAULT_NAMESPACE.to_string(),
            importance: crate::NEUTRAL_IMPORTANCE,
            valid_at: chrono::Utc::now(),
            invalid_at: None,
        }
    }

    #[async_trait::async_trait]
    impl Storage for StubStorage {
        async fn insert_memory(&self, _: &crate::NewMemory) -> MerkurResult<String> {
            unimplemented!()
        }
        async fn insert_memory_dedup(
            &self,
            _: &crate::NewMemory,
            _: f64,
        ) -> MerkurResult<String> {
            unimplemented!()
        }
        async fn update_memory(&self, _: &str, _: &str, _: Option<&[f32]>) -> MerkurResult<()> {
            unimplemented!()
        }
        async fn get_memory(&self, id: &str) -> MerkurResult<Option<crate::Memory>> {
            self.get_memory_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.hydration_fails {
                return Err(MerkurError::Storage("transient boom".into()));
            }
            Ok(Some(hydrated_memory(id)))
        }
        async fn delete_memory(&self, _: &str) -> MerkurResult<()> {
            unimplemented!()
        }
        async fn invalidate_memory(&self, _: &str, _: Option<&str>) -> MerkurResult<()> {
            unimplemented!()
        }
        async fn purge_invalidated_older_than(&self, _: i32) -> MerkurResult<usize> {
            unimplemented!()
        }
        async fn vector_search(
            &self,
            _: &[f32],
            _: usize,
        ) -> MerkurResult<Vec<ScoredMemory>> {
            unimplemented!()
        }
        async fn vector_search_ns(
            &self,
            _: &[f32],
            _: &str,
            _: usize,
        ) -> MerkurResult<Vec<ScoredMemory>> {
            Ok(vec![mk_scored("v1", "vector hit", 0.9)])
        }
        async fn record_access(&self, _: &[String]) -> MerkurResult<()> {
            unimplemented!()
        }
        async fn get_embeddings(
            &self,
            _: &[String],
        ) -> MerkurResult<std::collections::HashMap<String, Vec<f32>>> {
            unimplemented!()
        }
        async fn text_search(
            &self,
            _: &str,
            _: &str,
            _: usize,
        ) -> MerkurResult<Vec<(String, f64)>> {
            Ok(self.bm25_hits.clone())
        }
        async fn insert_edge(&self, _: &crate::NewEdge) -> MerkurResult<()> {
            unimplemented!()
        }
        async fn get_edges(&self, _: &str) -> MerkurResult<Vec<crate::Edge>> {
            unimplemented!()
        }
        async fn get_edges_batch(
            &self,
            _: &[String],
        ) -> MerkurResult<std::collections::HashMap<String, Vec<crate::Edge>>> {
            unimplemented!()
        }
        async fn bfs_expand(
            &self,
            _: &[String],
            _: usize,
            _: usize,
        ) -> MerkurResult<Vec<ScoredMemory>> {
            unimplemented!()
        }
        async fn bfs_expand_ns(
            &self,
            _: &[String],
            _: &str,
            _: usize,
            _: usize,
        ) -> MerkurResult<Vec<ScoredMemory>> {
            unimplemented!()
        }
        async fn insert_context_tag(&self, _: &str, _: &str, _: &str) -> MerkurResult<()> {
            unimplemented!()
        }
        async fn search_by_context(
            &self,
            _: &std::collections::HashMap<String, String>,
        ) -> MerkurResult<Vec<String>> {
            unimplemented!()
        }
        async fn list_pending(&self, _: usize) -> MerkurResult<Vec<crate::Memory>> {
            unimplemented!()
        }
        async fn list_for_forgetting(&self, _: usize) -> MerkurResult<Vec<crate::Memory>> {
            unimplemented!()
        }
        async fn mark_consolidated(&self, _: &[String]) -> MerkurResult<()> {
            unimplemented!()
        }
        async fn update_level(&self, _: &str, _: i32) -> MerkurResult<()> {
            unimplemented!()
        }
        async fn update_abstract(&self, _: &str, _: &str) -> MerkurResult<()> {
            unimplemented!()
        }
        async fn update_importance(&self, _: &str, _: f64) -> MerkurResult<()> {
            unimplemented!()
        }
        async fn delete_archived_older_than(&self, _: i32) -> MerkurResult<usize> {
            unimplemented!()
        }
        async fn log_consolidation(
            &self,
            _: chrono::DateTime<chrono::Utc>,
            _: chrono::DateTime<chrono::Utc>,
            _: &crate::ConsolidationReport,
        ) -> MerkurResult<()> {
            unimplemented!()
        }
        async fn get_consolidation_log(
            &self,
            _: usize,
        ) -> MerkurResult<Vec<crate::ConsolidationLogEntry>> {
            unimplemented!()
        }
        async fn stats(&self) -> MerkurResult<crate::StorageStats> {
            unimplemented!()
        }
        async fn memory_exists(&self, _: &str) -> MerkurResult<bool> {
            unimplemented!()
        }
        async fn memory_exists_batch(
            &self,
            _: &[String],
        ) -> MerkurResult<std::collections::HashSet<String>> {
            unimplemented!()
        }
    }

    /// A BM25-only candidate whose hydration fails must be skipped with a
    /// warning, not fail the whole recall — the vector channel's hits remain
    /// perfectly usable.
    #[tokio::test]
    async fn hybrid_recall_skips_hits_whose_hydration_fails() {
        let storage = StubStorage::new(vec![("b1".to_string(), 1.0)], true);
        let out = hybrid_recall(&storage, &[1.0], "some query", "default", 10, 0.0, &FusionParams::default())
            .await
            .expect("a hydration failure must degrade, not fail the recall");
        let ids: Vec<&str> = out.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["v1"]);
    }

    /// The threshold gates fused retrieval relevance — as it gates cosine in
    /// `fast` mode — not the composite score (whose structural floor would
    /// make the gate a no-op). `v1` is rank-1 in both channels (fused 1.0);
    /// `b1` is BM25 rank-2 only (fused ≈ 0.49) and must be gated by 0.6
    /// *before* hydration is even attempted.
    #[tokio::test]
    async fn hybrid_recall_gates_on_fused_relevance_before_hydration() {
        let storage = StubStorage::new(
            vec![("v1".to_string(), 2.0), ("b1".to_string(), 1.0)],
            false,
        );
        let out = hybrid_recall(&storage, &[1.0], "some query", "default", 10, 0.6, &FusionParams::default())
            .await
            .unwrap();
        let ids: Vec<&str> = out.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["v1"], "BM25-rank-2-only hit must be gated by 0.6");
        assert_eq!(
            storage
                .get_memory_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "gated candidates must never reach hydration"
        );
    }
}