//! LoCoMo dataset parsing (`locomo10.json` from snap-research/locomo).
//!
//! Layout quirks handled here:
//! - `conversation` is a flat map with interleaved `session_N` /
//!   `session_N_date_time` keys; serde_json sorts map keys alphabetically, so
//!   sessions must be re-ordered numerically (`session_10` > `session_2`).
//! - Category-5 (adversarial) QA carry `adversarial_answer` instead of
//!   `answer`; a handful of QA omit `evidence`; some answers are bare ints.

use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum DatasetError {
    #[error("invalid dataset JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug)]
pub struct Dataset {
    pub conversations: Vec<Conversation>,
}

#[derive(Debug)]
pub struct Conversation {
    pub sample_id: String,
    pub sessions: Vec<Session>,
    pub qa: Vec<Qa>,
}

#[derive(Debug)]
pub struct Session {
    pub index: u32,
    pub date_time: String,
    pub turns: Vec<Turn>,
}

#[derive(Debug)]
pub struct Turn {
    pub dia_id: String,
    pub speaker: String,
    pub text: String,
}

#[derive(Debug)]
pub struct Qa {
    pub question: String,
    pub answer: Option<String>,
    pub adversarial_answer: Option<String>,
    pub evidence: Vec<String>,
    pub category: u32,
}

impl Qa {
    pub fn is_adversarial(&self) -> bool {
        self.category == 5
    }

    /// Answer a judge should grade against: the real answer for normal QA,
    /// the trap answer for adversarial QA.
    pub fn golden_answer(&self) -> Option<String> {
        self.answer.clone().or_else(|| self.adversarial_answer.clone())
    }
}

impl Session {
    /// `1:56 pm on 8 May, 2023` → `8 May, 2023`. Sessions without the
    /// `" on "` separator keep their raw `date_time`.
    pub fn date_part(&self) -> String {
        self.date_time
            .rsplit(" on ")
            .next()
            .unwrap_or(&self.date_time)
            .to_string()
    }
}

impl Dataset {
    pub fn from_json_str(s: &str) -> Result<Self, DatasetError> {
        let raw: Vec<RawConversation> = serde_json::from_str(s)?;
        Ok(Self {
            conversations: raw.into_iter().map(Conversation::from).collect(),
        })
    }

    pub fn from_file(path: &std::path::Path) -> Result<Self, DatasetError> {
        let s = std::fs::read_to_string(path)
            .map_err(|e| DatasetError::Json(serde::de::Error::custom(e.to_string())))?;
        Self::from_json_str(&s)
    }
}

#[derive(Deserialize)]
struct RawConversation {
    sample_id: String,
    conversation: HashMap<String, serde_json::Value>,
    #[serde(default)]
    qa: Vec<RawQa>,
}

#[derive(Deserialize)]
struct RawQa {
    question: String,
    #[serde(default)]
    answer: Option<serde_json::Value>,
    #[serde(default)]
    adversarial_answer: Option<String>,
    #[serde(default)]
    evidence: Vec<String>,
    category: u32,
}

#[derive(Deserialize)]
struct RawTurn {
    speaker: String,
    dia_id: String,
    text: String,
}

impl From<RawConversation> for Conversation {
    fn from(raw: RawConversation) -> Self {
        let mut turns_by_idx: Vec<(u32, Vec<Turn>)> = Vec::new();
        let mut dt_by_idx: HashMap<u32, String> = HashMap::new();
        for (key, value) in &raw.conversation {
            let Some(rest) = key.strip_prefix("session_") else {
                continue;
            };
            if let Some(idx) = rest.strip_suffix("_date_time") {
                if let (Ok(n), Some(s)) = (idx.parse::<u32>(), value.as_str()) {
                    dt_by_idx.insert(n, s.to_string());
                }
            } else if let Ok(n) = rest.parse::<u32>() {
                let turns: Vec<RawTurn> =
                    serde_json::from_value(value.clone()).unwrap_or_default();
                turns_by_idx.push((
                    n,
                    turns
                        .into_iter()
                        .map(|t| Turn {
                            dia_id: t.dia_id,
                            speaker: t.speaker,
                            text: t.text,
                        })
                        .collect(),
                ));
            }
        }
        turns_by_idx.sort_by_key(|(n, _)| *n);
        let sessions = turns_by_idx
            .into_iter()
            .map(|(n, turns)| Session {
                index: n,
                date_time: dt_by_idx.remove(&n).unwrap_or_default(),
                turns,
            })
            .collect();
        let qa = raw
            .qa
            .into_iter()
            .map(|q| Qa {
                question: q.question,
                answer: q.answer.and_then(value_to_string),
                adversarial_answer: q.adversarial_answer,
                evidence: q.evidence,
                category: q.category,
            })
            .collect();
        Self {
            sample_id: raw.sample_id,
            sessions,
            qa,
        }
    }
}

fn value_to_string(v: serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"[
        {
            "sample_id": "conv-test",
            "conversation": {
                "speaker_a": "Caroline",
                "speaker_b": "Melanie",
                "session_2_date_time": "3:00 pm on 9 May, 2023",
                "session_2": [
                    {"speaker": "Melanie", "dia_id": "D2:1", "text": "second session"}
                ],
                "session_1": [
                    {"speaker": "Caroline", "dia_id": "D1:1", "text": "hello"},
                    {"speaker": "Melanie", "dia_id": "D1:2", "text": "hi there"}
                ],
                "session_1_date_time": "1:56 pm on 8 May, 2023",
                "session_10": [
                    {"speaker": "Caroline", "dia_id": "D10:1", "text": "tenth session"}
                ],
                "session_10_date_time": "2:00 pm on 1 June, 2023"
            },
            "qa": [
                {"question": "When did X happen?", "answer": "8 May 2023", "evidence": ["D1:1"], "category": 2},
                {"question": "What year?", "answer": 2023, "evidence": ["D1:1", "D1:2"], "category": 1},
                {"question": "Trap question?", "adversarial_answer": "a lie", "evidence": ["D2:1"], "category": 5},
                {"question": "No evidence here", "answer": "whatever", "category": 4}
            ]
        }
    ]"#;

    #[test]
    fn parses_sessions_in_numeric_order() {
        let ds = Dataset::from_json_str(FIXTURE).unwrap();
        let conv = &ds.conversations[0];
        assert_eq!(conv.sample_id, "conv-test");
        let idx: Vec<u32> = conv.sessions.iter().map(|s| s.index).collect();
        assert_eq!(idx, vec![1, 2, 10]);
        assert_eq!(conv.sessions[0].turns.len(), 2);
        assert_eq!(conv.sessions[0].turns[0].dia_id, "D1:1");
        assert_eq!(conv.sessions[0].turns[0].speaker, "Caroline");
    }

    #[test]
    fn extracts_date_part_from_session_datetime() {
        let ds = Dataset::from_json_str(FIXTURE).unwrap();
        let conv = &ds.conversations[0];
        assert_eq!(conv.sessions[0].date_part(), "8 May, 2023");
        assert_eq!(conv.sessions[2].date_part(), "1 June, 2023");
    }

    #[test]
    fn normalizes_int_answers_to_strings() {
        let ds = Dataset::from_json_str(FIXTURE).unwrap();
        let qa = &ds.conversations[0].qa[1];
        assert_eq!(qa.answer.as_deref(), Some("2023"));
    }

    #[test]
    fn adversarial_qa_uses_adversarial_answer_as_golden() {
        let ds = Dataset::from_json_str(FIXTURE).unwrap();
        let qa = &ds.conversations[0].qa[2];
        assert!(qa.is_adversarial());
        assert_eq!(qa.answer, None);
        assert_eq!(qa.golden_answer(), Some("a lie".to_string()));
        let normal = &ds.conversations[0].qa[0];
        assert!(!normal.is_adversarial());
        assert_eq!(normal.golden_answer(), Some("8 May 2023".to_string()));
    }

    #[test]
    fn missing_evidence_defaults_to_empty() {
        let ds = Dataset::from_json_str(FIXTURE).unwrap();
        let qa = &ds.conversations[0].qa[3];
        assert!(qa.evidence.is_empty());
    }
}
