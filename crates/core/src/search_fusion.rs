//! Hybrid search fusion: FTS5 BM25 x vector cosine via Reciprocal Rank Fusion.

use crate::{MerkurError, MerkurResult, ScoredMemory, Storage};

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
    let w = ScoreWeights::default();
    w.search * fused + w.weight * weight.clamp(0.0, 1.0) + w.importance * importance.clamp(0.0, 1.0)
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
    let mut fused: std::collections::HashMap<&str, f64> = std::collections::HashMap::new();
    let channels: [&[(String, f64)]; 2] = [bm25, vector];
    for ch in channels {
        for (rank, (id, _)) in ch.iter().enumerate() {
            *fused.entry(id.as_str()).or_insert(0.0) += 1.0 / (k + rank as f64 + 1.0);
        }
    }

    if limit == 0 || fused.is_empty() {
        return Vec::new();
    }

    let max = 2.0 / (k + 1.0);
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
/// populated records (`score` carries the normalized fused value).
///
/// Thin orchestration shared by every search entry point (REST handler and
/// MCP tool) so they cannot drift apart. Oversamples each channel at
/// `limit * 2` before fusing — RRF needs more input ranks than the requested
/// output to keep tail quality. A single channel failing degrades to the
/// other one instead of failing the whole recall. Candidates that surfaced
/// only through BM25 are fetched by id; vanished ids are skipped.
pub async fn hybrid_recall(
    storage: &dyn Storage,
    query_vec: &[f32],
    raw_query: &str,
    namespace: &str,
    limit: usize,
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
    let fused = rrf_fuse(&bm25, &vec_ranked, DEFAULT_RRF_K, limit);

    let mut by_id: std::collections::HashMap<String, ScoredMemory> = vec_hits
        .into_iter()
        .map(|m| (m.id.clone(), m))
        .collect();

    let mut out = Vec::with_capacity(fused.len());
    for (id, fused_score) in fused {
        let mut memory = match by_id.remove(&id) {
            Some(m) => m,
            None => match storage.get_memory(&id).await {
                Ok(Some(m)) => scored_from_memory(m, fused_score),
                Ok(None) => continue,
                Err(MerkurError::MemoryNotFound(_)) => continue,
                Err(e) => return Err(e),
            },
        };
        // P1-5: the externally visible score is the composite of retrieval
        // relevance, stored weight, and Consolidator importance. RRF rank
        // alone decided *which* ids made it here; this decides their order.
        memory.score = composite_score(fused_score, memory.weight, memory.importance);
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
}