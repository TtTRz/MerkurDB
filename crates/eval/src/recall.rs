//! Retrieval-recall scoring: did search return the dialog turns LoCoMo marks
//! as a question's evidence? LLM-free and deterministic — this is the metric
//! P1-5 weight tuning iterates on.
//!
//! Per question: `coverage = |evidence ∩ retrieved| / |evidence|` and
//! `hit = coverage > 0`. Questions without evidence are skipped (counted).
//! Aggregates are reported per category and overall.

use std::collections::{BTreeMap, HashSet};

#[derive(Debug, serde::Serialize)]
pub struct RecallQuestion {
    pub category: u32,
    pub question: String,
    pub evidence: Vec<String>,
    pub retrieved: Vec<String>,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct RecallReport {
    pub questions: usize,
    pub hits: usize,
    pub coverage_sum: f64,
    pub skipped_no_evidence: usize,
    pub per_category: Vec<CategoryStat>,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct CategoryStat {
    pub category: u32,
    pub questions: usize,
    pub hits: usize,
    pub coverage_sum: f64,
}

impl RecallReport {
    pub fn hit_rate(&self) -> f64 {
        if self.questions == 0 {
            0.0
        } else {
            self.hits as f64 / self.questions as f64
        }
    }

    pub fn mean_coverage(&self) -> f64 {
        if self.questions == 0 {
            0.0
        } else {
            self.coverage_sum / self.questions as f64
        }
    }
}

impl CategoryStat {
    pub fn mean_coverage(&self) -> f64 {
        if self.questions == 0 {
            0.0
        } else {
            self.coverage_sum / self.questions as f64
        }
    }
}

pub fn coverage_and_hit(evidence: &[String], retrieved: &[String]) -> (f64, bool) {
    let got: HashSet<&str> = retrieved.iter().map(String::as_str).collect();
    let found = evidence.iter().filter(|e| got.contains(e.as_str())).count();
    let cov = found as f64 / evidence.len() as f64;
    (cov, found > 0)
}

pub fn score_recall(questions: &[RecallQuestion]) -> RecallReport {
    let mut report = RecallReport::default();
    let mut cats: BTreeMap<u32, CategoryStat> = BTreeMap::new();
    for q in questions {
        if q.evidence.is_empty() {
            report.skipped_no_evidence += 1;
            continue;
        }
        let (cov, hit) = coverage_and_hit(&q.evidence, &q.retrieved);
        report.questions += 1;
        report.hits += hit as usize;
        report.coverage_sum += cov;
        let stat = cats.entry(q.category).or_insert_with(|| CategoryStat {
            category: q.category,
            ..Default::default()
        });
        stat.questions += 1;
        stat.hits += hit as usize;
        stat.coverage_sum += cov;
    }
    report.per_category = cats.into_values().collect();
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(category: u32, evidence: &[&str], retrieved: &[&str]) -> RecallQuestion {
        RecallQuestion {
            category,
            question: "q".to_string(),
            evidence: evidence.iter().map(|s| s.to_string()).collect(),
            retrieved: retrieved.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn coverage_is_intersection_over_evidence() {
        // 2 of 3 evidence turns retrieved -> 2/3 coverage, and a hit.
        let (cov, hit) = coverage_and_hit(
            &["D1:1".into(), "D1:2".into(), "D1:3".into()],
            &["D1:1".into(), "D1:2".into(), "D9:9".into()],
        );
        assert!((cov - 2.0 / 3.0).abs() < 1e-9);
        assert!(hit);
    }

    #[test]
    fn zero_overlap_is_a_miss() {
        let (cov, hit) = coverage_and_hit(&["D1:1".into()], &["D2:2".into()]);
        assert_eq!(cov, 0.0);
        assert!(!hit);
    }

    #[test]
    fn aggregates_per_category_and_overall() {
        let questions = vec![
            q(1, &["D1:1"], &["D1:1"]),         // hit, cov 1.0
            q(1, &["D1:2"], &["D9:9"]),         // miss, cov 0.0
            q(2, &["D2:1", "D2:2"], &["D2:1"]), // hit, cov 0.5
            q(3, &[], &["D3:1"]),               // no evidence -> skipped
        ];
        let report = score_recall(&questions);
        assert_eq!(report.skipped_no_evidence, 1);
        assert_eq!(report.questions, 3);
        assert_eq!(report.hits, 2);
        assert!((report.hit_rate() - 2.0 / 3.0).abs() < 1e-9);
        assert!((report.mean_coverage() - (1.0 + 0.0 + 0.5) / 3.0).abs() < 1e-9);

        let cat1 = report
            .per_category
            .iter()
            .find(|c| c.category == 1)
            .unwrap();
        assert_eq!(cat1.questions, 2);
        assert_eq!(cat1.hits, 1);
        assert!((cat1.mean_coverage() - 0.5).abs() < 1e-9);
        let cat2 = report
            .per_category
            .iter()
            .find(|c| c.category == 2)
            .unwrap();
        assert!((cat2.mean_coverage() - 0.5).abs() < 1e-9);
        // Skipped question contributes no category row.
        assert!(report.per_category.iter().all(|c| c.category != 3));
    }
}
