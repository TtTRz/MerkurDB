use async_trait::async_trait;
use merkur_core::{
    ConsolidationReport, Consolidator, EdgeType, Memory, MerkurError, MerkurResult, NewEdge,
};
use serde::Deserialize;

pub struct LlmConsolidator {
    base_url: String,
    model: String,
    client: reqwest::Client,
}

impl LlmConsolidator {
    pub fn new(base_url: String, model: String) -> Self {
        Self {
            base_url,
            model,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("Failed to build HTTP client"),
        }
    }
}

#[derive(Debug, Deserialize)]
struct LlmResponse {
    memories: Vec<AbstractResult>,
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
            .map_err(|e| MerkurError::Internal(format!("LLM request failed: {e}")))?;

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| MerkurError::Internal(format!("Failed to parse LLM response: {e}")))?;

        let response_text = body["response"]
            .as_str()
            .ok_or_else(|| MerkurError::Internal("LLM response missing 'response' field".into()))?;

        let parsed: LlmResponse = serde_json::from_str(response_text)
            .map_err(|e| MerkurError::Internal(format!("Failed to parse LLM JSON output: {e}")))?;

        let mut report = ConsolidationReport::empty();
        report.memories_processed = memories.len();

        for m in &parsed.memories {
            report
                .new_abstracts
                .insert(m.id.clone(), m.abstract_.clone());
        }

        for e in &parsed.edges {
            report.new_edges.push(NewEdge {
                source_id: e.source_id.clone(),
                target_id: e.target_id.clone(),
                weight: e.weight,
                relation: e.relation.clone(),
                edge_type: EdgeType::Auto,
            });
        }
        report.edges_created = parsed.edges.len();

        Ok(report)
    }
}

fn build_prompt(memories: &[Memory]) -> String {
    let items: Vec<String> = memories
        .iter()
        .map(|m| {
            format!(
                r#"{{"id":"{}","content":"{}"}}"#,
                m.id,
                m.content.replace('"', "\\\"")
            )
        })
        .collect();

    format!(
        r#"You are a memory consolidation agent. Given a list of memories, produce:
1. An abstract (concise 1-2 sentence summary) for each memory
2. Edges between semantically related memories (e.g., same entities, cause-effect, temporal sequence)

Memories:
[{}]

Respond with JSON only:
{{"memories":[{{"id":"...","abstract":"..."}},...],"edges":[{{"source_id":"...","target_id":"...","relation":"...","weight":0.8}},...]}}"#,
        items.join(",")
    )
}
