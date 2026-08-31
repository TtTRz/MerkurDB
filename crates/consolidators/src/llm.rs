use async_trait::async_trait;
use merkur_core::{
    Adjudication, AdjudicationAction, ConsolidationReport, Consolidator, EdgeType, Memory,
    MerkurError, MerkurResult, NewEdge, ScoredMemory,
};
use serde::Deserialize;
use std::collections::HashSet;
use std::time::Duration;
use tracing::warn;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmBackend {
    Ollama,
    OpenAI,
}

pub struct LlmConsolidator {
    base_url: String,
    model: String,
    client: reqwest::Client,
    backend: LlmBackend,
}

impl LlmConsolidator {
    pub fn new(base_url: String, model: String, backend: LlmBackend) -> MerkurResult<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| MerkurError::Consolidation(format!("Failed to build HTTP client: {e}")))?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            client,
            backend,
        })
    }

    async fn call_llm(&self, prompt: &str) -> MerkurResult<String> {
        match self.backend {
            LlmBackend::Ollama => self.call_ollama(prompt).await,
            LlmBackend::OpenAI => self.call_openai(prompt).await,
        }
    }

    async fn call_ollama(&self, prompt: &str) -> MerkurResult<String> {
        let resp = self
            .client
            .post(format!("{}/api/generate", self.base_url))
            .json(&serde_json::json!({
                "model": &self.model,
                "prompt": prompt,
                "stream": false,
                "format": "json",
            }))
            .send()
            .await
            .map_err(|e| MerkurError::Consolidation(format!("LLM request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(MerkurError::Consolidation(format!(
                "LLM returned {status}: {body}"
            )));
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| MerkurError::Consolidation(format!("Failed to parse LLM body: {e}")))?;

        body["response"].as_str().map(str::to_owned).ok_or_else(|| {
            MerkurError::Consolidation("LLM response missing 'response' field".into())
        })
    }

    async fn call_openai(&self, prompt: &str) -> MerkurResult<String> {
        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "model": &self.model,
                "messages": [{"role": "user", "content": prompt}],
                "response_format": {"type": "json_object"},
            }))
            .send()
            .await
            .map_err(|e| MerkurError::Consolidation(format!("LLM request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(MerkurError::Consolidation(format!(
                "LLM returned {status}: {body}"
            )));
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| MerkurError::Consolidation(format!("Failed to parse LLM body: {e}")))?;

        body["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| {
                MerkurError::Consolidation("LLM response missing choices[0].message.content".into())
            })
    }
}

#[derive(Debug, Deserialize)]
struct LlmResponse {
    #[serde(default)]
    memories: Vec<AbstractResult>,
    #[serde(default)]
    edges: Vec<EdgeResult>,
}

#[derive(Debug, Deserialize)]
struct AbstractResult {
    id: String,
    #[serde(rename = "abstract")]
    abstract_: String,
    /// Consolidator-assessed salience in [0, 1]; missing or out-of-range
    /// values fall back to the neutral prior at application time.
    importance: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct EdgeResult {
    source_id: String,
    target_id: String,
    relation: Option<String>,
    weight: Option<f64>,
}

/// Extract the first JSON object substring from arbitrary LLM output. Handles
/// the common patterns of leading "Here is the JSON:" prose, markdown fences,
/// and trailing commentary. Returns the original string when no plausible
/// object is found so that `serde_json` can produce a structured error.
fn extract_json_object(s: &str) -> &str {
    let trimmed = s.trim();
    // Strip fenced markdown code block if present.
    let stripped = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(str::trim_start)
        .unwrap_or(trimmed);
    let stripped = stripped
        .strip_suffix("```")
        .map(str::trim_end)
        .unwrap_or(stripped);

    if let (Some(start), Some(end)) = (stripped.find('{'), stripped.rfind('}'))
        && end >= start
    {
        return &stripped[start..=end];
    }
    stripped
}

#[derive(Debug, Deserialize)]
struct AdjudicationJson {
    action: Option<String>,
    target_id: Option<String>,
    reason: Option<String>,
}

/// Parse an adjudication verdict from LLM output. Anything unparseable, any
/// hallucinated target id, and any UPDATE/DELETE left without a valid target
/// all collapse to the safe default (`Add`, no target) — a governance miss
/// must never mutate memory.
pub(crate) fn parse_adjudication(
    raw: &str,
    pending_id: &str,
    candidate_ids: &HashSet<&str>,
) -> Adjudication {
    let cleaned = extract_json_object(raw);
    let parsed: AdjudicationJson = match serde_json::from_str(cleaned) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "adjudication output unparseable; defaulting to ADD");
            return Adjudication::default();
        }
    };
    let action = match parsed
        .action
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("update") => AdjudicationAction::Update,
        Some("delete") => AdjudicationAction::Delete,
        Some("noop") => AdjudicationAction::Noop,
        _ => AdjudicationAction::Add,
    };
    let target_id = match parsed.target_id {
        Some(t) if t == pending_id || candidate_ids.contains(t.as_str()) => Some(t),
        Some(t) => {
            warn!(target = %t, "LLM hallucinated adjudication target; dropping");
            None
        }
        None => None,
    };
    let (action, target_id) = match (&action, &target_id) {
        (AdjudicationAction::Update, None) | (AdjudicationAction::Delete, None) => {
            (AdjudicationAction::Add, None)
        }
        _ => (action, target_id),
    };
    Adjudication {
        action,
        target_id,
        reason: parsed.reason.unwrap_or_default(),
    }
}

/// Build the adjudication prompt: one pending memory vs its nearest
/// neighbors (with similarity evidence the scheduler will re-check against
/// the configured floor).
fn build_adjudication_prompt(
    pending: &Memory,
    candidates: &[ScoredMemory],
) -> MerkurResult<String> {
    let pending_json = serde_json::to_string(&serde_json::json!({
        "id": pending.id,
        "content": pending.content,
    }))
    .map_err(|e| MerkurError::Consolidation(format!("encode pending memory: {e}")))?;
    let cands: Vec<serde_json::Value> = candidates
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "content": c.content,
                "similarity": (c.score * 1000.0).round() / 1000.0,
            })
        })
        .collect();
    let cands_json = serde_json::to_string(&cands)
        .map_err(|e| MerkurError::Consolidation(format!("encode candidates: {e}")))?;

    Ok(format!(
        r#"You are a memory governance judge. A newly written memory may relate to existing memories.

New memory: {pending_json}
Existing neighbors (most similar first): {cands_json}

Decide the relationship:
- ADD: the new memory is a genuinely new fact (the default when unsure).
- UPDATE: the new memory restates or refines one existing memory — name it in target_id; it will take over the new content.
- DELETE: a contradiction — name the loser in target_id: the existing memory if the new fact supersedes it, or the new memory's own id if the new write itself is wrong.
- NOOP: the same fact is already recorded; nothing to do.

Use ONLY ids shown above (or the new memory's own id for DELETE). Do not invent ids.

Respond with JSON only:
{{"action":"ADD|UPDATE|DELETE|NOOP","target_id":"...or null...","reason":"one sentence"}}"#
    ))
}

#[async_trait]
impl Consolidator for LlmConsolidator {
    async fn consolidate(&self, memories: &[Memory]) -> MerkurResult<ConsolidationReport> {
        if memories.is_empty() {
            return Ok(ConsolidationReport::empty());
        }

        let prompt = build_prompt(memories)?;
        let response_text = self.call_llm(&prompt).await?;
        let cleaned = extract_json_object(&response_text);
        let parsed: LlmResponse = serde_json::from_str(cleaned).map_err(|e| {
            MerkurError::Consolidation(format!("Failed to parse LLM JSON output: {e}"))
        })?;

        // Build the set of input ids so we can drop hallucinated references.
        let valid_ids: HashSet<&str> = memories.iter().map(|m| m.id.as_str()).collect();

        let mut report = ConsolidationReport::empty();
        report.memories_processed = memories.len();

        for m in &parsed.memories {
            if !valid_ids.contains(m.id.as_str()) {
                warn!(
                    id = m.id.as_str(),
                    "LLM hallucinated abstract for unknown memory id; dropping"
                );
                report.errors += 1;
                continue;
            }
            report
                .new_abstracts
                .insert(m.id.clone(), m.abstract_.clone());
            if let Some(imp) = m.importance {
                if (0.0..=1.0).contains(&imp) {
                    report.new_importance.insert(m.id.clone(), imp);
                } else {
                    warn!(
                        id = m.id.as_str(),
                        importance = imp,
                        "LLM proposed out-of-range importance; keeping prior"
                    );
                }
            }
        }

        for e in &parsed.edges {
            if !valid_ids.contains(e.source_id.as_str())
                || !valid_ids.contains(e.target_id.as_str())
            {
                warn!(
                    src = e.source_id.as_str(),
                    dst = e.target_id.as_str(),
                    "LLM proposed edge between unknown ids; dropping"
                );
                report.errors += 1;
                continue;
            }
            if e.source_id == e.target_id {
                report.errors += 1;
                continue;
            }
            report.new_edges.push(NewEdge {
                source_id: e.source_id.clone(),
                target_id: e.target_id.clone(),
                weight: e.weight,
                relation: e.relation.clone(),
                edge_type: EdgeType::Auto,
            });
        }

        // edges_created is populated by the scheduler after actual insertion
        // succeeds, not from what the LLM merely proposed.
        report.edges_created = 0;

        Ok(report)
    }

    async fn adjudicate(
        &self,
        pending: &Memory,
        candidates: &[ScoredMemory],
    ) -> MerkurResult<Adjudication> {
        if candidates.is_empty() {
            return Ok(Adjudication::default());
        }
        let prompt = build_adjudication_prompt(pending, candidates)?;
        let raw = self.call_llm(&prompt).await?;
        let candidate_ids: HashSet<&str> = candidates.iter().map(|c| c.id.as_str()).collect();
        Ok(parse_adjudication(&raw, &pending.id, &candidate_ids))
    }
}

/// Build a JSON-safe prompt by serializing each memory through `serde_json`,
/// avoiding hand-rolled escaping bugs around backslashes and Unicode controls.
///
/// Returns an error if serialization fails — in practice that only happens
/// when a `Memory.content` contains non-UTF8 bytes smuggled in through a
/// custom upstream, which should be surfaced rather than silently replaced
/// with an empty array.
fn build_prompt(memories: &[Memory]) -> MerkurResult<String> {
    let items: Vec<serde_json::Value> = memories
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id,
                "content": m.content,
            })
        })
        .collect();
    let items_json = serde_json::to_string(&items)
        .map_err(|e| MerkurError::Consolidation(format!("encode prompt memories: {e}")))?;

    Ok(format!(
        r#"You are a memory consolidation agent. Given a list of memories, produce:
1. An abstract (concise 1-2 sentence summary) for each memory.
2. Edges between semantically related memories (same entities, cause-effect, temporal sequence).

Use ONLY the ids from the input list. Do not invent ids.

Memories: {items_json}

3. An importance score in [0, 1] for each memory: how central this fact is to the
   user's long-term goals (1 = core identity/preference, 0 = ephemeral trivia).

Respond with JSON only:
{{"memories":[{{"id":"...","abstract":"...","importance":0.7}}],"edges":[{{"source_id":"...","target_id":"...","relation":"...","weight":0.8}}]}}"#
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_json_object_plain() {
        let s = r#"{"a":1}"#;
        assert_eq!(extract_json_object(s), s);
    }

    #[test]
    fn test_extract_json_object_with_fence() {
        let s = "```json\n{\"a\":1}\n```";
        assert_eq!(extract_json_object(s), "{\"a\":1}");
    }

    #[test]
    fn test_extract_json_object_with_prose() {
        let s = "Here is the result:\n{\"a\":1}\nThanks";
        assert_eq!(extract_json_object(s), "{\"a\":1}");
    }

    fn test_memory(id: &str, content: &str) -> Memory {
        let now = chrono::Utc::now();
        Memory {
            id: id.into(),
            content: content.into(),
            abstract_: None,
            category: "general".into(),
            weight: 1.0,
            level: merkur_core::MemoryLevel::Full,
            pending_consolidation: true,
            embedding: None,
            metadata: Default::default(),
            context: Default::default(),
            created_at: now,
            updated_at: now,
            accessed_at: now,
            access_count: 0,
            namespace: merkur_core::DEFAULT_NAMESPACE.to_string(),
            importance: merkur_core::NEUTRAL_IMPORTANCE,
            valid_at: chrono::Utc::now(),
            invalid_at: None,
        }
    }

    #[test]
    fn test_prompt_requests_importance() {
        let m = test_memory("m1", "user prefers rust");
        let prompt = build_prompt(&[m]).unwrap();
        assert!(prompt.contains("importance"));
        assert!(prompt.contains("[0, 1]"));
    }

    // ---------- adjudication parsing (P1-7 write governance) ----------

    #[test]
    fn adjudication_parses_update_verdict() {
        let candidates: HashSet<&str> = ["mem_x"].into_iter().collect();
        let raw = r#"{"action":"UPDATE","target_id":"mem_x","reason":"same fact rephrased"}"#;
        let v = parse_adjudication(raw, "mem_p", &candidates);
        assert_eq!(v.action, merkur_core::AdjudicationAction::Update);
        assert_eq!(v.target_id.as_deref(), Some("mem_x"));
        assert_eq!(v.reason, "same fact rephrased");
    }

    #[test]
    fn adjudication_hallucinated_target_collapses_to_add() {
        let candidates: HashSet<&str> = ["mem_x"].into_iter().collect();
        let raw = r#"{"action":"DELETE","target_id":"mem_hallucinated","reason":"invented"}"#;
        let v = parse_adjudication(raw, "mem_p", &candidates);
        assert_eq!(v.action, merkur_core::AdjudicationAction::Add);
        assert_eq!(v.target_id, None);
    }

    #[test]
    fn adjudication_delete_may_target_the_pending_memory_itself() {
        let candidates: HashSet<&str> = ["mem_x"].into_iter().collect();
        let raw = r#"{"action":"delete","target_id":"mem_p","reason":"new write is wrong"}"#;
        let v = parse_adjudication(raw, "mem_p", &candidates);
        assert_eq!(v.action, merkur_core::AdjudicationAction::Delete);
        assert_eq!(v.target_id.as_deref(), Some("mem_p"));
    }

    #[test]
    fn adjudication_garbage_output_is_safe_default() {
        let candidates: HashSet<&str> = HashSet::new();
        let v = parse_adjudication("not json at all", "mem_p", &candidates);
        assert_eq!(v.action, merkur_core::AdjudicationAction::Add);
    }

    #[test]
    fn adjudication_update_without_target_collapses_to_add() {
        let candidates: HashSet<&str> = ["mem_x"].into_iter().collect();
        let v = parse_adjudication(r#"{"action":"UPDATE"}"#, "mem_p", &candidates);
        assert_eq!(v.action, merkur_core::AdjudicationAction::Add);
    }

    #[test]
    fn adjudication_prompt_lists_candidates_with_similarity() {
        let pending = test_memory("mem_p", "the region is now us-west");
        let cand = merkur_core::ScoredMemory {
            id: "mem_x".into(),
            content: "the region is us-east".into(),
            abstract_: None,
            score: 0.87,
            weight: 1.0,
            level: merkur_core::MemoryLevel::Full,
            category: "general".into(),
            context: Default::default(),
            created_at: chrono::Utc::now(),
            namespace: merkur_core::DEFAULT_NAMESPACE.to_string(),
            importance: merkur_core::NEUTRAL_IMPORTANCE,
        };
        let prompt = build_adjudication_prompt(&pending, std::slice::from_ref(&cand)).unwrap();
        assert!(prompt.contains("mem_x"));
        assert!(prompt.contains("us-east"));
        assert!(prompt.contains("0.87"));
    }
}