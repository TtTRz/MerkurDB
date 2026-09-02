//! LLM access for the QA track: answer generation and grading.
//!
//! The chat model sits behind a trait so tests run on a mock; the real
//! implementation talks to any OpenAI-compatible `/chat/completions`
//! endpoint. Prompts follow the mem0-style LoCoMo evaluation: answers must
//! be grounded in the retrieved memories only (abstention allowed — the
//! adversarial category depends on it), and the judge grades semantic
//! equivalence rather than exact match.

use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error("chat request failed: {0}")]
    Http(String),

    #[error("unexpected chat response shape: {0}")]
    Malformed(String),

    #[error("mock chat ran out of queued responses")]
    MockExhausted,
}

pub type ChatResult<T> = Result<T, ChatError>;

#[async_trait]
pub trait ChatModel: Send + Sync {
    async fn chat(&self, system: &str, user: &str) -> ChatResult<String>;
}

// ── Verdict ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Correct,
    Incorrect,
}

/// Parse a judge reply. "incorrect" contains "correct", so the negative is
/// checked first; anything unrecognized is `None` (caller counts it as a
/// judge parse failure).
pub fn parse_verdict(raw: &str) -> Option<Verdict> {
    let t = raw.trim().to_lowercase();
    if t.contains("incorrect") || t.contains("not correct") {
        Some(Verdict::Incorrect)
    } else if t.contains("correct") {
        Some(Verdict::Correct)
    } else {
        None
    }
}

// ── Prompts ──

/// Answer-prompt style. `Baseline` is the smoke/baseline-validated wording;
/// `Aggregate` targets the category-1 failure mode found in the full run:
/// enumeration questions need facts combined across memories, and partial
/// information must be surfaced instead of abstained away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum AnswerStyle {
    #[default]
    Baseline,
    Aggregate,
    /// Aggregate + premise guard: at top-30 depth every adversarial question
    /// has *related* memories, so "no memory is relevant" never fires; the
    /// guard keys abstention on the question's presupposition instead.
    Guarded,
}

/// Answer a LoCoMo question from retrieved memory contents (already carrying
/// their `[date] speaker:` prefixes). Abstention is explicitly allowed —
/// category-5 questions are unanswerable by construction.
pub fn build_answer_prompt(question: &str, memories: &[String]) -> (String, String) {
    build_answer_prompt_styled(question, memories, AnswerStyle::Baseline)
}

pub fn build_answer_prompt_styled(
    question: &str,
    memories: &[String],
    style: AnswerStyle,
) -> (String, String) {
    let system = match style {
        AnswerStyle::Baseline => {
            "You are a question-answering assistant. Answer using ONLY the provided \
        conversation memories. If the memories do not contain the answer, say you don't know \
        instead of guessing."
                .to_string()
        }
        AnswerStyle::Aggregate => {
            "You are a question-answering assistant. Answer using ONLY the provided \
        conversation memories. Combine all relevant facts across the memories into one complete \
        answer — when the question asks about activities, preferences, places, or people, list \
        every item mentioned. If the memories contain partial information, answer with what is \
        stated. Say you don't know ONLY when no memory mentions anything relevant."
                .to_string()
        }
        AnswerStyle::Guarded => {
            "You are a question-answering assistant. Answer using ONLY the provided \
        conversation memories. Combine all relevant facts across the memories into one complete \
        answer — when the question asks about activities, preferences, places, or people, list \
        every item mentioned. If the memories contain partial information, answer with what is \
        stated. However: if the question presupposes a fact or event that the memories do not \
        establish, say you don't know — related topics appearing in the memories do NOT count \
        as establishing it."
                .to_string()
        }
    };
    let mut user = String::from("Memories:\n");
    for m in memories {
        user.push_str("- ");
        user.push_str(m);
        user.push('\n');
    }
    user.push_str("\nQuestion: ");
    user.push_str(question);
    user.push_str("\nAnswer concisely (a few words or one short sentence).");
    (system, user)
}

/// Grade a prediction. Normal QA: semantic equivalence with the golden
/// answer. Adversarial QA (category 5): the golden is the trap answer a
/// hallucinating system would give; the prediction is correct only when it
/// declines to answer.
pub fn build_judge_prompt(
    question: &str,
    golden: &str,
    prediction: &str,
    adversarial: bool,
) -> (String, String) {
    let system = "You are a strict grading assistant. Reply with exactly one word: \
        \"correct\" or \"incorrect\"."
        .to_string();
    let user = if adversarial {
        format!(
            "The question below is not answerable from the user's conversation history. \
            The provided gold answer is a plausible-sounding trap that a hallucinating \
            system would invent.\n\
            Reply \"correct\" ONLY if the prediction declines to answer, states the \
            information is not available, or otherwise avoids committing to a specific \
            answer. Reply \"incorrect\" if it asserts any specific answer — especially \
            the trap answer.\n\n\
            Question: {question}\nTrap answer: {golden}\nPrediction: {prediction}"
        )
    } else {
        format!(
            "Grade whether the prediction correctly answers the question, compared against \
            the gold answer. Wording, formatting, and date-format differences are fine; \
            the meaning must match. Reply \"correct\" if it matches, \"incorrect\" if it \
            does not.\n\n\
            Question: {question}\nGold answer: {golden}\nPrediction: {prediction}"
        )
    };
    (system, user)
}

// ── Mock ──

pub struct MockChat {
    responses: Mutex<VecDeque<String>>,
    seen: Mutex<Vec<(String, String)>>,
    /// When set, replies come from the handler instead of the queue —
    /// deterministic under concurrent callers, where queue pop order is not.
    handler: Option<fn(&str, &str) -> String>,
}

impl MockChat {
    pub fn new(responses: Vec<String>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            seen: Mutex::new(Vec::new()),
            handler: None,
        }
    }

    pub fn with_handler(handler: fn(&str, &str) -> String) -> Self {
        Self {
            responses: Mutex::new(VecDeque::new()),
            seen: Mutex::new(Vec::new()),
            handler: Some(handler),
        }
    }

    pub fn seen(&self) -> Vec<(String, String)> {
        self.seen.lock().unwrap().clone()
    }
}

#[async_trait]
impl ChatModel for MockChat {
    async fn chat(&self, system: &str, user: &str) -> ChatResult<String> {
        self.seen
            .lock()
            .unwrap()
            .push((system.to_string(), user.to_string()));
        if let Some(handler) = self.handler {
            return Ok(handler(system, user));
        }
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(ChatError::MockExhausted)
    }
}

// ── OpenAI-compatible ──

/// Talks to any OpenAI-compatible chat endpoint. `base_url` is the service
/// ROOT (no `/v1`) — the `/v1/chat/completions` path is appended here,
/// matching the server-side embedder's convention.
pub struct OpenAiChat {
    base_url: String,
    api_key: Option<String>,
    model: String,
    client: reqwest::Client,
}

impl OpenAiChat {
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            model: model.into(),
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl ChatModel for OpenAiChat {
    async fn chat(&self, system: &str, user: &str) -> ChatResult<String> {
        let body = serde_json::json!({
            "model": self.model,
            "temperature": 0,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        });
        let mut req = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp: serde_json::Value = req
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| ChatError::Http(e.to_string()))?
            .json()
            .await
            .map_err(|e| ChatError::Http(e.to_string()))?;
        resp["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| ChatError::Malformed(resp.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── verdict parsing ──

    #[test]
    fn verdict_parses_plain_words() {
        assert_eq!(parse_verdict("correct"), Some(Verdict::Correct));
        assert_eq!(parse_verdict("Correct."), Some(Verdict::Correct));
        assert_eq!(parse_verdict("incorrect"), Some(Verdict::Incorrect));
        assert_eq!(parse_verdict("INCORRECT"), Some(Verdict::Incorrect));
    }

    #[test]
    fn verdict_prefers_incorrect_when_both_substrings_present() {
        // "incorrect" contains "correct"; the negative must win.
        assert_eq!(
            parse_verdict("incorrect, the dates differ"),
            Some(Verdict::Incorrect)
        );
        assert_eq!(parse_verdict("not correct"), Some(Verdict::Incorrect));
    }

    #[test]
    fn verdict_garbage_is_none() {
        assert_eq!(parse_verdict("I cannot decide"), None);
        assert_eq!(parse_verdict(""), None);
    }

    // ── answer prompt styles ──

    #[test]
    fn baseline_style_is_byte_identical_to_legacy_prompt() {
        let memories = vec!["[8 May, 2023] Caroline: hello".to_string()];
        assert_eq!(
            build_answer_prompt_styled("Q?", &memories, AnswerStyle::Baseline),
            build_answer_prompt("Q?", &memories)
        );
    }

    #[test]
    fn aggregate_style_demands_combining_and_discourages_false_abstention() {
        let memories = vec!["[8 May, 2023] Caroline: I do pottery.".to_string()];
        let (system, user) =
            build_answer_prompt_styled("What activities?", &memories, AnswerStyle::Aggregate);
        // Aggregation instruction: combine facts across memories, lists ok.
        assert!(system.contains("combine") || system.contains("all relevant facts"));
        // Partial info must be surfaced, not abstained away.
        assert!(system.contains("partial"));
        // Abstention still allowed when nothing is relevant (cat-5 safety).
        assert!(system.contains("don't know") || system.contains("do not know"));
        // Question and memories still grounded in the user message.
        assert!(user.contains("What activities?"));
        assert!(user.contains("I do pottery."));
    }

    #[test]
    fn guarded_style_combines_aggregation_with_premise_check() {
        let memories = vec!["[8 May, 2023] Caroline: I do pottery.".to_string()];
        let (system, _user) =
            build_answer_prompt_styled("What camera?", &memories, AnswerStyle::Guarded);
        // Keeps the aggregation instruction...
        assert!(system.contains("combine") || system.contains("all relevant facts"));
        // ...but adds the premise guard the Aggregate style lacked: at top-30
        // depth every adversarial question has *related* memories, so
        // "nothing relevant" never triggers. The guard keys on the question's
        // presupposition instead.
        assert!(system.contains("presupposes") || system.contains("assumes"));
        assert!(system.contains("don't know") || system.contains("do not know"));
    }

    #[test]
    fn answer_prompt_grounds_in_memories_and_allows_abstention() {
        let (system, user) = build_answer_prompt(
            "When did Caroline join the support group?",
            &["[8 May, 2023] Caroline: I joined a LGBTQ support group.".to_string()],
        );
        assert!(system.contains("don't know") || system.contains("do not know"));
        assert!(user.contains("[8 May, 2023] Caroline: I joined a LGBTQ support group."));
        assert!(user.contains("When did Caroline join the support group?"));
    }

    // ── judge prompt ──

    #[test]
    fn judge_prompt_normal_grades_against_golden() {
        let (_system, user) = build_judge_prompt("Q?", "8 May 2023", "May 8, 2023", false);
        assert!(user.contains("8 May 2023"));
        assert!(user.contains("May 8, 2023"));
        assert!(user.contains("correct") && user.contains("incorrect"));
    }

    #[test]
    fn judge_prompt_adversarial_scores_abstention_as_correct() {
        let (_system, user) = build_judge_prompt("Trap?", "a lie", "I don't know", true);
        // The judge must know the golden is a trap and refusal is the win.
        assert!(user.contains("a lie"));
        assert!(user.contains("not answerable") || user.contains("cannot be answered"));
    }

    // ── mock chat ──

    #[tokio::test]
    async fn mock_chat_replays_responses_and_records_prompts() {
        let mock = MockChat::new(vec!["first".into(), "second".into()]);
        assert_eq!(mock.chat("s1", "u1").await.unwrap(), "first");
        assert_eq!(mock.chat("s2", "u2").await.unwrap(), "second");
        let seen = mock.seen();
        assert_eq!(
            seen,
            vec![
                ("s1".to_string(), "u1".to_string()),
                ("s2".to_string(), "u2".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn mock_chat_errors_when_responses_run_out() {
        let mock = MockChat::new(vec![]);
        assert!(mock.chat("s", "u").await.is_err());
    }

    // ── openai-compatible client ──

    #[tokio::test]
    async fn openai_chat_posts_to_v1_chat_completions_with_bearer() {
        use std::sync::{Arc, Mutex};
        type Seen = Arc<Mutex<Vec<(String, Option<String>, serde_json::Value)>>>;
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(move |req: axum::extract::Request| {
                let seen = seen2.clone();
                async move {
                    let auth = req
                        .headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    let body = axum::body::to_bytes(req.into_body(), usize::MAX)
                        .await
                        .unwrap();
                    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    seen.lock().unwrap().push(("POST".to_string(), auth, json));
                    axum::Json(serde_json::json!({
                        "choices": [{"message": {"content": "pong"}}]
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let chat = OpenAiChat::new(format!("http://{addr}"), Some("sk-test".into()), "judge-1");
        let reply = chat.chat("sys", "usr").await.unwrap();
        assert_eq!(reply, "pong");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].1.as_deref(), Some("Bearer sk-test"));
        assert_eq!(seen[0].2["model"], "judge-1");
        assert_eq!(seen[0].2["messages"][0]["content"], "sys");
        assert_eq!(seen[0].2["messages"][1]["content"], "usr");
    }
}
