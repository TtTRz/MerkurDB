//! IO orchestration: push LoCoMo conversations into a running MerkurDB
//! server, then measure retrieval recall and judge-graded QA accuracy over
//! HTTP — the same serving path real clients use.
//!
//! Turn ingest format: content is `[<session date>] <speaker>: <text>` so
//! both BM25 and vector channels see the temporal anchor; `dia_id` rides in
//! `context` (which search results return, unlike `metadata`) so recall can
//! map hits back to LoCoMo evidence without N+1 lookups.

use crate::dataset::Conversation;
use crate::llm::{
    AnswerStyle, ChatModel, Verdict, build_answer_prompt_styled, build_judge_prompt, parse_verdict,
};
use crate::recall::RecallQuestion;
use futures::StreamExt as _;
use merkur_client::{ClientError, HttpMerkurClient, MerkurClient, SearchParams};
use merkur_core::WriteItem;
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Client(#[from] ClientError),

    #[error(transparent)]
    Chat(#[from] crate::llm::ChatError),
}

pub type RunResult<T> = Result<T, RunError>;

// ── ingest ──

#[derive(Debug, Default)]
pub struct IngestSummary {
    pub turns_written: usize,
    pub batches: usize,
}

/// Write one conversation into the client's namespace, one memory per dialog
/// turn, batching at `batch_size`.
pub async fn ingest_conversation(
    client: &HttpMerkurClient,
    conv: &Conversation,
    batch_size: usize,
) -> RunResult<IngestSummary> {
    let mut items: Vec<WriteItem> = Vec::new();
    for session in &conv.sessions {
        let date = session.date_part();
        for turn in &session.turns {
            items.push(WriteItem {
                content: format!("[{date}] {}: {}", turn.speaker, turn.text),
                context: Some(HashMap::from([
                    ("dia_id".to_string(), turn.dia_id.clone()),
                    ("speaker".to_string(), turn.speaker.clone()),
                    ("session".to_string(), session.index.to_string()),
                ])),
                metadata: Some(HashMap::from([
                    ("dia_id".to_string(), serde_json::json!(turn.dia_id)),
                    ("session_date".to_string(), serde_json::json!(date)),
                    (
                        "session_index".to_string(),
                        serde_json::json!(session.index),
                    ),
                ])),
            });
        }
    }
    let mut summary = IngestSummary::default();
    for chunk in items.chunks(batch_size.max(1)) {
        let resp = client.write_batch(chunk).await?;
        summary.turns_written += resp.count;
        summary.batches += 1;
    }
    Ok(summary)
}

// ── recall ──

#[derive(Debug, Default)]
pub struct RecallRun {
    pub questions: Vec<RecallQuestion>,
    /// Questions whose search failed even after retries; excluded from
    /// scoring but counted so a flaky run is visible.
    pub errors: usize,
}

/// One transient failure must not kill a multi-hour benchmark run: retry the
/// search with backoff (3 attempts), then let the caller record-and-continue.
async fn search_with_retry(
    client: &HttpMerkurClient,
    query: &str,
    params: &SearchParams,
) -> RunResult<merkur_client::SearchResponse> {
    let mut delay = std::time::Duration::from_millis(200);
    let mut last_err: Option<RunError> = None;
    for attempt in 0..3 {
        match client.search(query, params).await {
            Ok(resp) => return Ok(resp),
            Err(e) => {
                last_err = Some(e.into());
                if attempt < 2 {
                    tokio::time::sleep(delay).await;
                    delay *= 4;
                }
            }
        }
    }
    Err(last_err.expect("retry loop always sets an error"))
}

fn ungated_params(limit: usize, mode: &str) -> SearchParams {
    SearchParams {
        mode: Some(mode.to_string()),
        limit: Some(limit),
        score_threshold: Some(0.0),
        ..Default::default()
    }
}

/// One recall query: search and map hits to `dia_id`s.
async fn recall_one(
    client: &HttpMerkurClient,
    params: &SearchParams,
    qa: &crate::dataset::Qa,
) -> RunResult<RecallQuestion> {
    let resp = search_with_retry(client, &qa.question, params).await?;
    let retrieved = resp
        .results
        .iter()
        .filter_map(|m| m.context.get("dia_id").cloned())
        .collect();
    Ok(RecallQuestion {
        category: qa.category,
        question: qa.question.clone(),
        evidence: qa.evidence.clone(),
        retrieved,
    })
}

/// Search every QA of the conversation and collect the `dia_id`s of the hits.
/// Retrieval is ungated (`score_threshold = 0`) so recall reflects ranking
/// quality, not the production threshold.
pub async fn recall_conversation(
    client: &HttpMerkurClient,
    conv: &Conversation,
    limit: usize,
    mode: &str,
) -> RunResult<RecallRun> {
    let params = ungated_params(limit, mode);
    let mut run = RecallRun::default();
    let total = conv.qa.len();
    for (i, qa) in conv.qa.iter().enumerate() {
        if i > 0 && i % 25 == 0 {
            eprintln!("[recall] {} {}/{}", conv.sample_id, i, total);
        }
        match recall_one(client, &params, qa).await {
            Ok(q) => run.questions.push(q),
            Err(_) => run.errors += 1,
        }
    }
    Ok(run)
}

/// Concurrent recall: `concurrency` in-flight searches. Identical scoring
/// semantics to the serial path; only the load profile differs.
pub async fn recall_conversation_concurrent(
    client: &HttpMerkurClient,
    conv: &Conversation,
    limit: usize,
    mode: &str,
    concurrency: usize,
) -> RunResult<RecallRun> {
    let params = ungated_params(limit, mode);
    let total = conv.qa.len();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let results: Vec<RunResult<RecallQuestion>> = futures::stream::iter(conv.qa.iter())
        .map(|qa| {
            let done = done.clone();
            let params = &params; // capture a Copy reference, not the owned value
            async move {
                let r = recall_one(client, params, qa).await;
                let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if n.is_multiple_of(100) {
                    eprintln!("[recall] {} {}/{}", conv.sample_id, n, total);
                }
                r
            }
        })
        .buffer_unordered(concurrency.max(1))
        .collect()
        .await;
    let mut run = RecallRun::default();
    for r in results {
        match r {
            Ok(q) => run.questions.push(q),
            Err(_) => run.errors += 1,
        }
    }
    Ok(run)
}

// ── qa ──

#[derive(Debug, serde::Serialize)]
pub struct QaRecord {
    pub category: u32,
    /// `None` = the judge's reply could not be parsed; counted as incorrect
    /// but tracked separately so a flaky judge is visible in the report.
    pub verdict: Option<Verdict>,
    /// Per-question detail for failure analysis (JSONL dump).
    pub question: String,
    pub golden: String,
    pub prediction: String,
    pub judge_raw: String,
}

#[derive(Debug, Default)]
pub struct QaRun {
    pub records: Vec<QaRecord>,
    pub skipped_no_golden: usize,
    /// Questions lost to search/chat failures after retries; excluded from
    /// accuracy but counted so a flaky run is visible.
    pub errors: usize,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct QaReport {
    pub questions: usize,
    pub correct: usize,
    pub parse_failures: usize,
    pub per_category: Vec<QaCategoryStat>,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct QaCategoryStat {
    pub category: u32,
    pub questions: usize,
    pub correct: usize,
}

impl QaReport {
    pub fn accuracy(&self) -> f64 {
        if self.questions == 0 {
            0.0
        } else {
            self.correct as f64 / self.questions as f64
        }
    }
}

pub fn score_qa(records: &[QaRecord]) -> QaReport {
    let mut report = QaReport::default();
    let mut cats: BTreeMap<u32, QaCategoryStat> = BTreeMap::new();
    for r in records {
        report.questions += 1;
        let correct = r.verdict == Some(Verdict::Correct);
        if r.verdict.is_none() {
            report.parse_failures += 1;
        }
        report.correct += correct as usize;
        let stat = cats.entry(r.category).or_insert_with(|| QaCategoryStat {
            category: r.category,
            ..Default::default()
        });
        stat.questions += 1;
        stat.correct += correct as usize;
    }
    report.per_category = cats.into_values().collect();
    report
}

/// For each QA: retrieve top-`limit` memories, generate an answer grounded in
/// them, then grade it against the golden (trap answers for adversarial QA).
pub async fn qa_conversation(
    client: &HttpMerkurClient,
    chat: &dyn ChatModel,
    conv: &Conversation,
    limit: usize,
    mode: &str,
    style: AnswerStyle,
) -> RunResult<QaRun> {
    let params = ungated_params(limit, mode);
    let mut run = QaRun::default();
    let total = conv.qa.len();
    for (i, qa) in conv.qa.iter().enumerate() {
        if i > 0 && i % 10 == 0 {
            let done = run.records.len();
            let correct = run
                .records
                .iter()
                .filter(|r| r.verdict == Some(Verdict::Correct))
                .count();
            eprintln!(
                "[qa] {} {}/{} (accuracy so far: {:.1}%, errors: {})",
                conv.sample_id,
                i,
                total,
                if done == 0 {
                    0.0
                } else {
                    correct as f64 / done as f64 * 100.0
                },
                run.errors
            );
        }
        match qa_one(client, chat, &params, qa, style).await {
            Ok(Some(record)) => run.records.push(record),
            Ok(None) => run.skipped_no_golden += 1,
            Err(_) => run.errors += 1,
        }
    }
    Ok(run)
}

/// One QA item end to end: retrieve -> answer -> judge. `Ok(None)` means the
/// question has no golden answer and is skipped.
async fn qa_one(
    client: &HttpMerkurClient,
    chat: &dyn ChatModel,
    params: &SearchParams,
    qa: &crate::dataset::Qa,
    style: AnswerStyle,
) -> RunResult<Option<QaRecord>> {
    let Some(golden) = qa.golden_answer() else {
        return Ok(None);
    };
    let resp = search_with_retry(client, &qa.question, params).await?;
    let memories: Vec<String> = resp.results.iter().map(|m| m.content.clone()).collect();
    let (sys, user) = build_answer_prompt_styled(&qa.question, &memories, style);
    let prediction = chat.chat(&sys, &user).await?;
    let (jsys, juser) = build_judge_prompt(&qa.question, &golden, &prediction, qa.is_adversarial());
    let raw = chat.chat(&jsys, &juser).await?;
    Ok(Some(QaRecord {
        category: qa.category,
        verdict: parse_verdict(&raw),
        question: qa.question.clone(),
        golden: golden.clone(),
        prediction,
        judge_raw: raw,
    }))
}

/// Concurrent QA: `concurrency` questions in flight, each still running the
/// full retrieve->answer->judge chain. Identical scoring semantics to the
/// serial path; only wall-clock and server/provider load differ.
pub async fn qa_conversation_concurrent(
    client: &HttpMerkurClient,
    chat: &dyn ChatModel,
    conv: &Conversation,
    limit: usize,
    mode: &str,
    concurrency: usize,
    style: AnswerStyle,
) -> RunResult<QaRun> {
    let params = ungated_params(limit, mode);
    let total = conv.qa.len();
    let progress = std::sync::Arc::new(std::sync::Mutex::new((0usize, 0usize, 0usize)));
    let results: Vec<RunResult<Option<QaRecord>>> = futures::stream::iter(conv.qa.iter())
        .map(|qa| {
            let progress = progress.clone();
            let params = &params; // capture a Copy reference, not the owned value
            async move {
                let r = qa_one(client, chat, params, qa, style).await;
                let mut p = progress.lock().unwrap();
                p.0 += 1; // done
                match &r {
                    Ok(Some(rec)) if rec.verdict == Some(Verdict::Correct) => p.1 += 1,
                    Ok(Some(_)) | Ok(None) => {}
                    Err(_) => p.2 += 1,
                }
                if p.0.is_multiple_of(25) {
                    eprintln!(
                        "[qa] {} {}/{} (accuracy so far: {:.1}%, errors: {})",
                        conv.sample_id,
                        p.0,
                        total,
                        p.1 as f64 / p.0.max(1) as f64 * 100.0,
                        p.2
                    );
                }
                r
            }
        })
        .buffer_unordered(concurrency.max(1))
        .collect()
        .await;
    let mut run = QaRun::default();
    for r in results {
        match r {
            Ok(Some(record)) => run.records.push(record),
            Ok(None) => run.skipped_no_golden += 1,
            Err(_) => run.errors += 1,
        }
    }
    Ok(run)
}

// ── personamem ──

#[derive(Debug, serde::Serialize)]
pub struct PmRecord {
    pub question_id: String,
    pub question_type: String,
    pub correct: bool,
    pub chosen: Option<char>,
    pub correct_letter: char,
    pub question: String,
    pub prediction_raw: String,
}

#[derive(Debug, Default)]
pub struct PmRun {
    pub records: Vec<PmRecord>,
    /// Questions lost to search/chat failures after retries.
    pub errors: usize,
    pub turns_written: usize,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct PmReport {
    pub questions: usize,
    pub correct: usize,
    pub parse_failures: usize,
    pub per_type: Vec<PmTypeStat>,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct PmTypeStat {
    pub question_type: String,
    pub questions: usize,
    pub correct: usize,
}

impl PmReport {
    pub fn accuracy(&self) -> f64 {
        if self.questions == 0 {
            0.0
        } else {
            self.correct as f64 / self.questions as f64
        }
    }
}

pub fn score_pm(records: &[PmRecord]) -> PmReport {
    let mut report = PmReport::default();
    let mut types: BTreeMap<String, PmTypeStat> = BTreeMap::new();
    for r in records {
        report.questions += 1;
        report.correct += r.correct as usize;
        report.parse_failures += r.chosen.is_none() as usize;
        let stat = types
            .entry(r.question_type.clone())
            .or_insert_with(|| PmTypeStat {
                question_type: r.question_type.clone(),
                ..Default::default()
            });
        stat.questions += 1;
        stat.correct += r.correct as usize;
    }
    report.per_type = types.into_values().collect();
    report
}

/// Replay one shared context in-situ: walk checkpoints (unique `end_index`es)
/// in ascending order, ingesting only the newly visible user/assistant
/// messages, then answer that checkpoint's questions against the accumulated
/// memories — no future leakage, minimal writes.
pub async fn pm_run_context(
    client: &HttpMerkurClient,
    chat: &dyn ChatModel,
    ctx: &crate::personamem::PmContext,
    questions: &[&crate::personamem::PmQuestion],
    limit: usize,
    mode: &str,
    jobs: usize,
) -> RunResult<PmRun> {
    use crate::personamem::{build_mc_prompt, parse_choice};

    let mut run = PmRun::default();
    let params = ungated_params(limit, mode);

    // Ascending unique checkpoints.
    let mut ends: Vec<usize> = questions.iter().map(|q| q.end_index).collect();
    ends.sort_unstable();
    ends.dedup();

    let mut prev: Option<usize> = None;
    for end in ends {
        // Ingest messages (prev, end], skipping system boilerplate; content
        // carries the true message index for traceability.
        let start = prev.map(|p| p + 1).unwrap_or(0);
        let mut items: Vec<WriteItem> = Vec::new();
        for (i, m) in ctx.messages.iter().enumerate().take(end + 1).skip(start) {
            if m.role == "system" {
                continue;
            }
            items.push(WriteItem {
                content: format!("[#{i}] {}: {}", m.role, m.content),
                context: Some(HashMap::from([
                    ("msg_index".to_string(), i.to_string()),
                    ("role".to_string(), m.role.clone()),
                ])),
                metadata: None,
            });
        }
        for chunk in items.chunks(100) {
            let resp = client.write_batch(chunk).await?;
            run.turns_written += resp.count;
        }
        prev = Some(end);

        // Answer this checkpoint's questions concurrently.
        let checkpoint_qs: Vec<&crate::personamem::PmQuestion> = questions
            .iter()
            .copied()
            .filter(|q| q.end_index == end)
            .collect();
        let n_checkpoint = checkpoint_qs.len();
        let results: Vec<RunResult<PmRecord>> = futures::stream::iter(checkpoint_qs)
            .map(|q| {
                let params = &params;
                async move {
                    let resp = search_with_retry(client, &q.question, params).await?;
                    let memories: Vec<String> =
                        resp.results.iter().map(|m| m.content.clone()).collect();
                    let (sys, user) = build_mc_prompt(&q.question, &q.options, &memories);
                    let raw = chat.chat(&sys, &user).await?;
                    let chosen = parse_choice(&raw, q.options.len());
                    Ok(PmRecord {
                        question_id: q.question_id.clone(),
                        question_type: q.question_type.clone(),
                        correct: chosen == Some(q.correct_letter),
                        chosen,
                        correct_letter: q.correct_letter,
                        question: q.question.clone(),
                        prediction_raw: raw,
                    })
                }
            })
            .buffer_unordered(jobs.max(1))
            .collect()
            .await;
        for r in results {
            match r {
                Ok(rec) => run.records.push(rec),
                Err(_) => run.errors += 1,
            }
        }
        eprintln!(
            "[pm] {} checkpoint end={} (answered {}, errors: {})",
            ctx.id, end, n_checkpoint, run.errors
        );
    }
    Ok(run)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::{Conversation, Qa, Session, Turn};
    use crate::llm::{MockChat, Verdict};
    use axum::response::IntoResponse;
    use std::sync::{Arc, Mutex};

    fn conv_fixture() -> Conversation {
        Conversation {
            sample_id: "conv-test".into(),
            sessions: vec![
                Session {
                    index: 1,
                    date_time: "1:56 pm on 8 May, 2023".into(),
                    turns: vec![
                        Turn {
                            dia_id: "D1:1".into(),
                            speaker: "Caroline".into(),
                            text: "hello".into(),
                        },
                        Turn {
                            dia_id: "D1:2".into(),
                            speaker: "Melanie".into(),
                            text: "hi there".into(),
                        },
                    ],
                },
                Session {
                    index: 2,
                    date_time: "3:00 pm on 9 May, 2023".into(),
                    turns: vec![Turn {
                        dia_id: "D2:1".into(),
                        speaker: "Caroline".into(),
                        text: "bye".into(),
                    }],
                },
            ],
            qa: vec![
                Qa {
                    question: "who said hello?".into(),
                    answer: Some("Caroline".into()),
                    adversarial_answer: None,
                    evidence: vec!["D1:1".into(), "D1:3".into()],
                    category: 1,
                },
                Qa {
                    question: "trap question?".into(),
                    answer: None,
                    adversarial_answer: Some("a lie".into()),
                    evidence: vec![],
                    category: 5,
                },
            ],
        }
    }

    // ── stub MerkurDB server ──

    #[derive(Default)]
    struct StubState {
        write_batches: Vec<(Option<String>, serde_json::Value)>,
        search_queries: Vec<(Option<String>, String)>,
        /// When > 0, /v1/search replies 500 this many times before serving.
        search_failures_remaining: usize,
    }

    fn scored_memory_json(id: &str, dia_id: &str, content: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "content": content,
            "abstract": null,
            "score": 0.9,
            "weight": 1.0,
            "level": "full",
            "category": "general",
            "context": {"dia_id": dia_id},
            "created_at": "2026-01-01T00:00:00+00:00",
            "namespace": "conv-test",
            "importance": 0.5
        })
    }

    async fn spawn_stub(state: Arc<Mutex<StubState>>) -> String {
        let write_state = state.clone();
        let search_state = state.clone();
        let app = axum::Router::new()
            .route(
                "/v1/write-batch",
                axum::routing::post(move |req: axum::extract::Request| {
                    let state = write_state.clone();
                    async move {
                        let ns = req
                            .headers()
                            .get("x-merkur-namespace")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned);
                        let body = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
                        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        let count = json["items"].as_array().unwrap().len();
                        let ids: Vec<String> = (0..count).map(|i| format!("mem_{i}")).collect();
                        state.lock().unwrap().write_batches.push((ns, json));
                        axum::Json(serde_json::json!({
                            "ids": ids, "count": count, "requested": count, "errors": null
                        }))
                    }
                }),
            )
            .route(
                "/v1/search",
                axum::routing::get(move |req: axum::extract::Request| {
                    let state = search_state.clone();
                    async move {
                        let ns = req
                            .headers()
                            .get("x-merkur-namespace")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned);
                        let uri = req.uri().to_string();
                        {
                            let mut locked = state.lock().unwrap();
                            locked.search_queries.push((ns, uri));
                            if locked.search_failures_remaining > 0 {
                                locked.search_failures_remaining -= 1;
                                let body = axum::Json(serde_json::json!({
                                    "error": {"code": "INTERNAL", "message": "boom"}
                                }));
                                return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, body)
                                    .into_response();
                            }
                        }
                        axum::Json(serde_json::json!({
                            "mode": "hybrid",
                            "results": [
                                scored_memory_json("mem_1", "D1:1", "[8 May, 2023] Caroline: hello"),
                                scored_memory_json("mem_2", "D1:2", "[8 May, 2023] Melanie: hi there"),
                            ],
                            "total": 2,
                            "time_ms": 1,
                            "graph": null
                        }))
                        .into_response()
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    // ── ingest ──

    #[tokio::test]
    async fn ingest_writes_dated_prefixed_turns_with_dia_id_context() {
        let state = Arc::new(Mutex::new(StubState::default()));
        let base = spawn_stub(state.clone()).await;
        let client = HttpMerkurClient::new(&base)
            .unwrap()
            .with_namespace("conv-test");
        let summary = ingest_conversation(&client, &conv_fixture(), 100)
            .await
            .unwrap();

        assert_eq!(summary.turns_written, 3);
        assert_eq!(summary.batches, 1);
        let locked = state.lock().unwrap();
        let (ns, body) = &locked.write_batches[0];
        assert_eq!(ns.as_deref(), Some("conv-test"));
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["content"], "[8 May, 2023] Caroline: hello");
        assert_eq!(items[0]["context"]["dia_id"], "D1:1");
        assert_eq!(items[0]["metadata"]["session_date"], "8 May, 2023");
        assert_eq!(items[2]["content"], "[9 May, 2023] Caroline: bye");
    }

    #[tokio::test]
    async fn ingest_splits_at_batch_size() {
        let state = Arc::new(Mutex::new(StubState::default()));
        let base = spawn_stub(state.clone()).await;
        let client = HttpMerkurClient::new(&base)
            .unwrap()
            .with_namespace("conv-test");
        let summary = ingest_conversation(&client, &conv_fixture(), 2)
            .await
            .unwrap();
        assert_eq!(summary.batches, 2);
        assert_eq!(summary.turns_written, 3);
    }

    // ── recall ──

    #[tokio::test]
    async fn recall_maps_hits_back_to_dia_ids() {
        let state = Arc::new(Mutex::new(StubState::default()));
        let base = spawn_stub(state.clone()).await;
        let client = HttpMerkurClient::new(&base)
            .unwrap()
            .with_namespace("conv-test");
        let run = recall_conversation(&client, &conv_fixture(), 10, "hybrid")
            .await
            .unwrap();

        // Both QA produce a RecallQuestion; the no-evidence one is filtered by
        // score_recall, not by the runner.
        assert_eq!(run.questions.len(), 2);
        assert_eq!(run.errors, 0);
        assert_eq!(
            run.questions[0].retrieved,
            vec!["D1:1".to_string(), "D1:2".to_string()]
        );
        assert_eq!(run.questions[0].category, 1);
        // Search must run ungated (threshold 0) with the requested mode.
        let locked = state.lock().unwrap();
        assert!(locked.search_queries[0].1.contains("score_threshold=0"));
        assert!(locked.search_queries[0].1.contains("mode=hybrid"));
        let report = crate::recall::score_recall(&run.questions);
        assert_eq!(report.questions, 1); // cat-5 question has no evidence
        assert_eq!(report.hits, 1);
        assert!((report.mean_coverage() - 0.5).abs() < 1e-9);
    }

    #[tokio::test]
    async fn recall_retries_transient_failures_then_succeeds() {
        let state = Arc::new(Mutex::new(StubState {
            search_failures_remaining: 2,
            ..Default::default()
        }));
        let base = spawn_stub(state.clone()).await;
        let client = HttpMerkurClient::new(&base)
            .unwrap()
            .with_namespace("conv-test");
        let run = recall_conversation(&client, &conv_fixture(), 10, "hybrid")
            .await
            .unwrap();

        assert_eq!(run.errors, 0);
        assert_eq!(run.questions.len(), 2);
        // First query: fail, fail, ok (3 calls); second: ok (1 call).
        assert_eq!(state.lock().unwrap().search_queries.len(), 4);
    }

    #[tokio::test]
    async fn recall_records_error_and_continues_when_search_keeps_failing() {
        let state = Arc::new(Mutex::new(StubState {
            search_failures_remaining: 999,
            ..Default::default()
        }));
        let base = spawn_stub(state.clone()).await;
        let client = HttpMerkurClient::new(&base)
            .unwrap()
            .with_namespace("conv-test");
        let run = recall_conversation(&client, &conv_fixture(), 10, "hybrid")
            .await
            .unwrap();

        assert_eq!(run.questions.len(), 0);
        assert_eq!(run.errors, 2);
        // 3 attempts per question, then give up and move on.
        assert_eq!(state.lock().unwrap().search_queries.len(), 6);
    }

    // ── qa ──

    #[test]
    fn score_qa_counts_parse_failures_as_incorrect_but_separate() {
        let rec = |category, verdict| QaRecord {
            category,
            verdict,
            question: "q".into(),
            golden: "g".into(),
            prediction: "p".into(),
            judge_raw: "j".into(),
        };
        let records = vec![
            rec(1, Some(Verdict::Correct)),
            rec(1, Some(Verdict::Incorrect)),
            rec(2, None),
        ];
        let report = score_qa(&records);
        assert_eq!(report.questions, 3);
        assert_eq!(report.correct, 1);
        assert_eq!(report.parse_failures, 1);
        assert!((report.accuracy() - 1.0 / 3.0).abs() < 1e-9);
        let cat1 = report
            .per_category
            .iter()
            .find(|c| c.category == 1)
            .unwrap();
        assert_eq!(cat1.questions, 2);
        assert_eq!(cat1.correct, 1);
    }

    #[test]
    fn qa_record_serializes_for_jsonl_dump() {
        let rec = QaRecord {
            category: 5,
            verdict: Some(Verdict::Correct),
            question: "trap?".into(),
            golden: "a lie".into(),
            prediction: "I don't know".into(),
            judge_raw: "correct".into(),
        };
        let v = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["verdict"], "correct");
        assert_eq!(v["question"], "trap?");
        assert_eq!(v["golden"], "a lie");
        assert_eq!(v["prediction"], "I don't know");
    }

    #[tokio::test]
    async fn qa_runner_answers_then_judges_and_skips_missing_golden() {
        let state = Arc::new(Mutex::new(StubState::default()));
        let base = spawn_stub(state.clone()).await;
        let client = HttpMerkurClient::new(&base)
            .unwrap()
            .with_namespace("conv-test");
        // First chat call = answer generation, second = judge verdict.
        let chat = MockChat::new(vec![
            "Caroline".into(),
            "correct".into(),
            "I refuse".into(),
            "correct".into(),
        ]);
        let run = qa_conversation(
            &client,
            &chat,
            &conv_fixture(),
            10,
            "hybrid",
            AnswerStyle::Baseline,
        )
        .await
        .unwrap();

        assert_eq!(run.skipped_no_golden, 0);
        assert_eq!(run.errors, 0);
        assert_eq!(run.records.len(), 2);
        assert_eq!(run.records[0].verdict, Some(Verdict::Correct));
        // The adversarial question's golden is its trap answer.
        let seen = chat.seen();
        assert_eq!(seen.len(), 4);
        assert!(seen[1].1.contains("Caroline")); // judge saw the golden
        assert!(seen[3].1.contains("a lie")); // adversarial judge saw the trap
    }

    #[tokio::test]
    async fn qa_records_error_and_continues_when_search_keeps_failing() {
        let state = Arc::new(Mutex::new(StubState {
            search_failures_remaining: 999,
            ..Default::default()
        }));
        let base = spawn_stub(state.clone()).await;
        let client = HttpMerkurClient::new(&base)
            .unwrap()
            .with_namespace("conv-test");
        let chat = MockChat::new(vec![]);
        let run = qa_conversation(
            &client,
            &chat,
            &conv_fixture(),
            10,
            "hybrid",
            AnswerStyle::Baseline,
        )
        .await
        .unwrap();

        assert_eq!(run.records.len(), 0);
        assert_eq!(run.errors, 2);
        assert!(chat.seen().is_empty()); // never reached answer generation
    }

    #[tokio::test]
    async fn qa_records_error_and_continues_when_chat_fails() {
        let state = Arc::new(Mutex::new(StubState::default()));
        let base = spawn_stub(state.clone()).await;
        let client = HttpMerkurClient::new(&base)
            .unwrap()
            .with_namespace("conv-test");
        // Only enough mock responses for the first question (answer + judge);
        // the second question's answer call runs dry -> error, not panic.
        let chat = MockChat::new(vec!["Caroline".into(), "correct".into()]);
        let run = qa_conversation(
            &client,
            &chat,
            &conv_fixture(),
            10,
            "hybrid",
            AnswerStyle::Baseline,
        )
        .await
        .unwrap();

        assert_eq!(run.records.len(), 1);
        assert_eq!(run.errors, 1);
    }

    // ── concurrent variants ──

    /// Handler-mode mock: answer prompts get a canned answer, judge prompts
    /// get a verdict — deterministic regardless of concurrent call order.
    fn handler_chat(_system: &str, user: &str) -> String {
        if user.contains("Gold answer") || user.contains("Trap answer") {
            "correct".to_string()
        } else {
            "mock answer".to_string()
        }
    }

    fn conv_with_ungolden_qa() -> Conversation {
        let mut conv = conv_fixture();
        conv.qa.push(Qa {
            question: "ungolden".into(),
            answer: None,
            adversarial_answer: None,
            evidence: vec![],
            category: 4,
        });
        conv
    }

    #[tokio::test]
    async fn qa_concurrent_processes_all_questions() {
        let state = Arc::new(Mutex::new(StubState::default()));
        let base = spawn_stub(state.clone()).await;
        let client = HttpMerkurClient::new(&base)
            .unwrap()
            .with_namespace("conv-test");
        let chat = MockChat::with_handler(handler_chat);
        let run = qa_conversation_concurrent(
            &client,
            &chat,
            &conv_with_ungolden_qa(),
            10,
            "hybrid",
            3,
            AnswerStyle::Baseline,
        )
        .await
        .unwrap();

        assert_eq!(run.records.len(), 2);
        assert!(
            run.records
                .iter()
                .all(|r| r.verdict == Some(Verdict::Correct))
        );
        assert_eq!(run.skipped_no_golden, 1);
        assert_eq!(run.errors, 0);
        // 2 judged questions x (answer + judge) calls.
        assert_eq!(chat.seen().len(), 4);
    }

    #[tokio::test]
    async fn recall_concurrent_collects_all_questions() {
        let state = Arc::new(Mutex::new(StubState::default()));
        let base = spawn_stub(state.clone()).await;
        let client = HttpMerkurClient::new(&base)
            .unwrap()
            .with_namespace("conv-test");
        let run = recall_conversation_concurrent(&client, &conv_fixture(), 10, "hybrid", 4)
            .await
            .unwrap();

        assert_eq!(run.questions.len(), 2);
        assert_eq!(run.errors, 0);
        assert!(run.questions.iter().all(|q| q.retrieved.len() == 2));
    }

    #[tokio::test]
    async fn qa_aggregate_style_reaches_answer_prompt() {
        let state = Arc::new(Mutex::new(StubState::default()));
        let base = spawn_stub(state.clone()).await;
        let client = HttpMerkurClient::new(&base)
            .unwrap()
            .with_namespace("conv-test");
        let chat = MockChat::new(vec![
            "ans".into(),
            "correct".into(),
            "ans".into(),
            "correct".into(),
        ]);
        qa_conversation(
            &client,
            &chat,
            &conv_fixture(),
            10,
            "hybrid",
            AnswerStyle::Aggregate,
        )
        .await
        .unwrap();
        let seen = chat.seen();
        // First call is the answer generation; its system prompt must carry
        // the aggregation instructions.
        assert!(seen[0].0.contains("Combine all relevant facts"));
        // Judge prompt is unaffected by answer style.
        assert!(seen[1].0.contains("strict grading"));
    }

    // ── personamem in-situ replay ──

    fn pm_fixture() -> (
        crate::personamem::PmContext,
        Vec<crate::personamem::PmQuestion>,
    ) {
        use crate::personamem::{PmContext, PmMessage, PmQuestion};
        let ctx = PmContext {
            id: "ctxA".into(),
            messages: vec![
                PmMessage {
                    role: "system".into(),
                    content: "scenario boilerplate".into(),
                },
                PmMessage {
                    role: "user".into(),
                    content: "I love jazz".into(),
                },
                PmMessage {
                    role: "assistant".into(),
                    content: "noted".into(),
                },
                PmMessage {
                    role: "user".into(),
                    content: "now I prefer ambient".into(),
                },
            ],
        };
        let q = |id: &str, end: usize, correct: char| PmQuestion {
            question_id: id.into(),
            persona_id: "0".into(),
            question_type: "recall_user_shared_facts".into(),
            topic: "music".into(),
            question: format!("question {id}"),
            correct_letter: correct,
            options: vec![
                "(a) x".into(),
                "(b) y".into(),
                "(c) z".into(),
                "(d) w".into(),
            ],
            context_id: "ctxA".into(),
            end_index: end,
        };
        (ctx, vec![q("q1", 2, 'b'), q("q2", 3, 'c')])
    }

    fn pm_handler(_system: &str, _user: &str) -> String {
        // Always "(b)": q1 (end=2, correct=b) scores right, q2 (end=3,
        // correct=c) scores wrong.
        "(b)".into()
    }

    #[tokio::test]
    async fn pm_replays_checkpoints_incrementally_and_scores_choices() {
        let state = Arc::new(Mutex::new(StubState::default()));
        let base = spawn_stub(state.clone()).await;
        let client = HttpMerkurClient::new(&base)
            .unwrap()
            .with_namespace("pm-ctxA");
        let (ctx, qs) = pm_fixture();
        let refs: Vec<&crate::personamem::PmQuestion> = qs.iter().collect();
        let chat = MockChat::with_handler(pm_handler);
        let run = pm_run_context(&client, &chat, &ctx, &refs, 10, "hybrid", 2)
            .await
            .unwrap();

        assert_eq!(run.records.len(), 2);
        assert_eq!(run.errors, 0);
        // Checkpoint 1 (end=2): 2 non-system messages; checkpoint 2 adds 1.
        let locked = state.lock().unwrap();
        let batches = &locked.write_batches;
        assert_eq!(batches.len(), 2);
        let b0 = batches[0].1["items"].as_array().unwrap();
        assert_eq!(b0.len(), 2);
        assert_eq!(b0[0]["content"], "[#1] user: I love jazz");
        assert_eq!(b0[1]["content"], "[#2] assistant: noted");
        let b1 = batches[1].1["items"].as_array().unwrap();
        assert_eq!(b1.len(), 1);
        assert_eq!(b1[0]["content"], "[#3] user: now I prefer ambient");
        // One search per question.
        assert_eq!(locked.search_queries.len(), 2);
        drop(locked);

        assert!(run.records[0].correct);
        assert!(!run.records[1].correct);
        assert_eq!(run.records[1].chosen, Some('b'));

        let report = score_pm(&run.records);
        assert_eq!(report.questions, 2);
        assert_eq!(report.correct, 1);
        assert_eq!(report.parse_failures, 0);
        assert!((report.accuracy() - 0.5).abs() < 1e-9);
        let t = report
            .per_type
            .iter()
            .find(|t| t.question_type == "recall_user_shared_facts")
            .unwrap();
        assert_eq!(t.questions, 2);
        assert_eq!(t.correct, 1);
    }

    #[test]
    fn score_pm_counts_unparseable_choice_as_wrong_but_separate() {
        let rec = |chosen| PmRecord {
            question_id: "q".into(),
            question_type: "t".into(),
            correct: chosen == Some('b'),
            chosen,
            correct_letter: 'b',
            question: "q".into(),
            prediction_raw: "raw".into(),
        };
        let report = score_pm(&[rec(Some('b')), rec(Some('a')), rec(None)]);
        assert_eq!(report.questions, 3);
        assert_eq!(report.correct, 1);
        assert_eq!(report.parse_failures, 1);
    }
}
