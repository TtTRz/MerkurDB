use async_trait::async_trait;
use merkur_core::{
    ConsolidationReport, Consolidator, EdgeType, Memory, MerkurError, MerkurResult, NewEdge,
};
use serde::Deserialize;
use std::collections::HashSet;
use std::time::Duration;
use tracing::warn;

pub struct LlmConsolidator {
    base_url: String,
    model: String,
    client: reqwest::Client,
}

impl LlmConsolidator {
    pub fn new(base_url: String, model: String) -> MerkurResult<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| MerkurError::Consolidation(format!("Failed to build HTTP client: {e}")))?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            client,
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

#[async_trait]
impl Consolidator for LlmConsolidator {
    async fn consolidate(&self, memories: &[Memory]) -> MerkurResult<ConsolidationReport> {
        if memories.is_empty() {
            return Ok(ConsolidationReport::empty());
        }

        let prompt = build_prompt(memories);

        let resp = self
            .client
            .post(format!("{}/api/generate", self.base_url))
            .json(&serde_json::json!({
                "model": &self.model,
                "prompt": &prompt,
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

        let response_text = body["response"].as_str().ok_or_else(|| {
            MerkurError::Consolidation("LLM response missing 'response' field".into())
        })?;

        let cleaned = extract_json_object(response_text);
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
}

/// Build a JSON-safe prompt by serializing each memory through `serde_json`,
/// avoiding hand-rolled escaping bugs around backslashes and Unicode controls.
fn build_prompt(memories: &[Memory]) -> String {
    let items: Vec<serde_json::Value> = memories
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id,
                "content": m.content,
            })
        })
        .collect();
    let items_json = serde_json::to_string(&items).unwrap_or_else(|_| "[]".into());

    format!(
        r#"You are a memory consolidation agent. Given a list of memories, produce:
1. An abstract (concise 1-2 sentence summary) for each memory.
2. Edges between semantically related memories (same entities, cause-effect, temporal sequence).

Use ONLY the ids from the input list. Do not invent ids.

Memories: {items_json}

Respond with JSON only:
{{"memories":[{{"id":"...","abstract":"..."}}],"edges":[{{"source_id":"...","target_id":"...","relation":"...","weight":0.8}}]}}"#
    )
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
}
