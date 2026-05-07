use async_trait::async_trait;
use merkur_core::{ConsolidationReport, Consolidator, Memory, MerkurResult};

pub struct NoopConsolidator;

#[async_trait]
impl Consolidator for NoopConsolidator {
    async fn consolidate(&self, memories: &[Memory]) -> MerkurResult<ConsolidationReport> {
        let mut report = ConsolidationReport::empty();
        report.memories_processed = memories.len();
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use merkur_core::{Memory, MemoryLevel};

    fn make_memory(id: &str, content: &str) -> Memory {
        let now = chrono::Utc::now();
        Memory {
            id: id.into(),
            content: content.into(),
            abstract_: None,
            category: "general".into(),
            weight: 1.0,
            level: MemoryLevel::Full,
            pending_consolidation: true,
            embedding: None,
            metadata: Default::default(),
            context: Default::default(),
            created_at: now,
            updated_at: now,
            accessed_at: now,
            access_count: 0,
        }
    }

    #[tokio::test]
    async fn test_noop_consolidates_all() {
        let c = NoopConsolidator;
        let memories = vec![make_memory("m1", "hello"), make_memory("m2", "world")];
        let report = c.consolidate(&memories).await.unwrap();
        assert_eq!(report.memories_processed, 2);
        assert_eq!(report.edges_created, 0);
        assert_eq!(report.errors, 0);
        assert!(report.new_abstracts.is_empty());
        assert!(report.new_edges.is_empty());
    }

    #[tokio::test]
    async fn test_noop_empty_list() {
        let c = NoopConsolidator;
        let report = c.consolidate(&[]).await.unwrap();
        assert_eq!(report.memories_processed, 0);
    }
}
