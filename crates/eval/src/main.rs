//! merkur-eval: LoCoMo benchmark harness for MerkurDB.
//!
//! Stages (each idempotent enough to re-run against a fresh namespace):
//!   stats   — parse the dataset and print its shape (no server needed)
//!   ingest  — write conversations into the server, one memory per turn
//!   recall  — LLM-free retrieval recall against LoCoMo evidence annotations
//!   qa      — end-to-end answer + judge grading (needs a chat endpoint)
//!
//! Typical flow (see scripts/run_locomo.sh for full orchestration):
//!   merkur-eval ingest --server http://127.0.0.1:1934 --token ...
//!   merkur-eval recall --limit 10
//!   MERKUR_EVAL_CHAT_BASE_URL=... MERKUR_EVAL_CHAT_MODEL=... merkur-eval qa

use clap::{Parser, Subcommand};
use futures::StreamExt as _;
use merkur_client::HttpMerkurClient;
use merkur_eval::dataset::Dataset;
use merkur_eval::llm::{AnswerStyle, OpenAiChat};
use merkur_eval::recall::{RecallQuestion, score_recall};
use merkur_eval::runner::{
    QaRecord, ingest_conversation, pm_run_context, qa_conversation, qa_conversation_concurrent,
    recall_conversation, recall_conversation_concurrent, score_pm, score_qa,
};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "merkur-eval", about = "LoCoMo benchmark harness for MerkurDB")]
struct Cli {
    /// Path to locomo10.json
    #[arg(long, global = true, default_value = "crates/eval/data/locomo10.json")]
    data: PathBuf,

    /// Base URL of a running merkur-server
    #[arg(
        long,
        global = true,
        env = "MERKUR_EVAL_SERVER",
        default_value = "http://127.0.0.1:1934"
    )]
    server: String,

    /// Bearer token for the server
    #[arg(long, global = true, env = "MERKUR_EVAL_TOKEN")]
    token: Option<String>,

    /// Restrict the run to one conversation sample_id (all 10 by default)
    #[arg(long, global = true)]
    conv: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Parse the dataset and print its shape; no server required.
    Stats,
    /// Write conversations into the server (one memory per dialog turn).
    Ingest {
        #[arg(long, default_value_t = 100)]
        batch_size: usize,
    },
    /// Retrieval recall@limit against LoCoMo evidence (LLM-free).
    Recall {
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long, default_value = "hybrid")]
        mode: String,
        /// In-flight searches; 1 = serial (baseline-comparable).
        #[arg(long, default_value_t = 1)]
        jobs: usize,
        /// Write the machine-readable report to this path.
        #[arg(long)]
        json: Option<PathBuf>,
        /// Write per-question detail (question/evidence/retrieved) as JSONL.
        #[arg(long)]
        dump: Option<PathBuf>,
    },
    /// PersonaMem (multiple-choice, no judge): replay shared contexts in-situ
    /// and answer checkpoint questions against accumulated memories.
    PmRun {
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long, default_value = "hybrid")]
        mode: String,
        /// Questions in flight within each checkpoint.
        #[arg(long, default_value_t = 8)]
        jobs: usize,
        /// Restrict to one shared_context_id (prefix match).
        #[arg(long)]
        context: Option<String>,
        /// Contexts replayed concurrently (namespaces are isolated).
        #[arg(long, default_value_t = 4)]
        context_jobs: usize,
        #[arg(long, env = "MERKUR_EVAL_CHAT_BASE_URL")]
        chat_base_url: String,
        #[arg(long, env = "MERKUR_EVAL_CHAT_API_KEY")]
        chat_api_key: Option<String>,
        #[arg(long, env = "MERKUR_EVAL_CHAT_MODEL")]
        chat_model: String,
        #[arg(long)]
        json: Option<PathBuf>,
        #[arg(long)]
        dump: Option<PathBuf>,
    },
    /// End-to-end QA: retrieve, answer, judge. Needs a chat endpoint.
    Qa {
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long, default_value = "hybrid")]
        mode: String,
        /// Questions in flight; 1 = serial (baseline-comparable).
        #[arg(long, default_value_t = 1)]
        jobs: usize,
        /// Answer-prompt style (baseline = as benchmarked; aggregate =
        /// combine facts across memories, fewer false abstentions).
        #[arg(long, value_enum, default_value = "baseline")]
        answer_style: AnswerStyle,
        #[arg(long, env = "MERKUR_EVAL_CHAT_BASE_URL")]
        chat_base_url: String,
        #[arg(long, env = "MERKUR_EVAL_CHAT_API_KEY")]
        chat_api_key: Option<String>,
        #[arg(long, env = "MERKUR_EVAL_CHAT_MODEL")]
        chat_model: String,
        #[arg(long)]
        json: Option<PathBuf>,
        /// Write per-question detail (question/golden/prediction/verdict) as JSONL.
        #[arg(long)]
        dump: Option<PathBuf>,
    },
}

fn load_dataset(cli: &Cli) -> Result<Dataset, Box<dyn std::error::Error>> {
    Ok(Dataset::from_file(&cli.data)?)
}

fn base_client(cli: &Cli) -> Result<HttpMerkurClient, Box<dyn std::error::Error>> {
    // Remote embedding/chat providers can be slow to cold-start; the SDK's
    // 30 s default is too tight for a benchmark run.
    let timeout = std::time::Duration::from_secs(120);
    let client = HttpMerkurClient::with_options(&cli.server, cli.token.clone(), timeout)?;
    Ok(client)
}

fn write_jsonl<T: serde::Serialize>(
    path: &std::path::Path,
    items: &[T],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = String::new();
    for item in items {
        out.push_str(&serde_json::to_string(item)?);
        out.push('\n');
    }
    std::fs::write(path, out)?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match &cli.command {
        Command::Stats => {
            let ds = load_dataset(&cli)?;
            let turns: usize = ds
                .conversations
                .iter()
                .flat_map(|c| &c.sessions)
                .map(|s| s.turns.len())
                .sum();
            let qa: usize = ds.conversations.iter().map(|c| c.qa.len()).sum();
            println!(
                "conversations: {}\nturns: {turns}\nqa: {qa}",
                ds.conversations.len()
            );
            let mut cats: std::collections::BTreeMap<u32, usize> = Default::default();
            let mut no_evidence = 0;
            let mut no_golden = 0;
            for q in ds.conversations.iter().flat_map(|c| &c.qa) {
                *cats.entry(q.category).or_default() += 1;
                no_evidence += q.evidence.is_empty() as usize;
                no_golden += q.golden_answer().is_none() as usize;
            }
            for (cat, n) in cats {
                println!("category {cat}: {n}");
            }
            println!("qa without evidence: {no_evidence}");
            println!("qa without golden answer: {no_golden}");
        }

        Command::Ingest { batch_size } => {
            let ds = load_dataset(&cli)?;
            let client = base_client(&cli)?;
            let mut total = 0;
            for conv in selected(&ds, &cli.conv) {
                let namespaced = client.clone().with_namespace(&conv.sample_id);
                let summary = ingest_conversation(&namespaced, conv, *batch_size).await?;
                total += summary.turns_written;
                println!(
                    "{}: {} turns in {} batches",
                    conv.sample_id, summary.turns_written, summary.batches
                );
            }
            println!("total turns written: {total}");
        }

        Command::Recall {
            limit,
            mode,
            jobs,
            json,
            dump,
        } => {
            let ds = load_dataset(&cli)?;
            let client = base_client(&cli)?;
            let mut questions: Vec<RecallQuestion> = Vec::new();
            let mut errors = 0;
            for conv in selected(&ds, &cli.conv) {
                let namespaced = client.clone().with_namespace(&conv.sample_id);
                let run = if *jobs > 1 {
                    recall_conversation_concurrent(&namespaced, conv, *limit, mode, *jobs).await?
                } else {
                    recall_conversation(&namespaced, conv, *limit, mode).await?
                };
                println!(
                    "{}: {} queries, {} errors",
                    conv.sample_id,
                    run.questions.len(),
                    run.errors
                );
                questions.extend(run.questions);
                errors += run.errors;
            }
            let report = score_recall(&questions);
            println!("\nrecall@{limit} mode={mode}");
            println!(
                "{:<9} {:>9} {:>6} {:>9} {:>13}",
                "category", "questions", "hits", "hit_rate", "mean_coverage"
            );
            for c in &report.per_category {
                println!(
                    "{:<9} {:>9} {:>6} {:>9.3} {:>13.3}",
                    c.category,
                    c.questions,
                    c.hits,
                    c.hits as f64 / c.questions.max(1) as f64,
                    c.mean_coverage()
                );
            }
            println!(
                "{:<9} {:>9} {:>6} {:>9.3} {:>13.3}",
                "overall",
                report.questions,
                report.hits,
                report.hit_rate(),
                report.mean_coverage()
            );
            println!("skipped (no evidence): {}", report.skipped_no_evidence);
            println!("errors (retried out): {errors}");
            if let Some(path) = json {
                std::fs::write(path, serde_json::to_string_pretty(&report)?)?;
            }
            if let Some(path) = dump {
                write_jsonl(path, &questions)?;
            }
        }

        Command::Qa {
            limit,
            mode,
            jobs,
            answer_style,
            chat_base_url,
            chat_api_key,
            chat_model,
            json,
            dump,
        } => {
            let ds = load_dataset(&cli)?;
            let client = base_client(&cli)?;
            let chat = OpenAiChat::new(chat_base_url, chat_api_key.clone(), chat_model);
            let mut records: Vec<QaRecord> = Vec::new();
            let mut skipped = 0;
            let mut errors = 0;
            for conv in selected(&ds, &cli.conv) {
                let namespaced = client.clone().with_namespace(&conv.sample_id);
                let run = if *jobs > 1 {
                    qa_conversation_concurrent(
                        &namespaced,
                        &chat,
                        conv,
                        *limit,
                        mode,
                        *jobs,
                        *answer_style,
                    )
                    .await?
                } else {
                    qa_conversation(&namespaced, &chat, conv, *limit, mode, *answer_style).await?
                };
                println!(
                    "{}: {} judged, {} skipped, {} errors",
                    conv.sample_id,
                    run.records.len(),
                    run.skipped_no_golden,
                    run.errors
                );
                records.extend(run.records);
                skipped += run.skipped_no_golden;
                errors += run.errors;
            }
            let report = score_qa(&records);
            println!("\nqa accuracy (judge={chat_model})");
            println!(
                "{:<9} {:>9} {:>7} {:>9}",
                "category", "questions", "correct", "accuracy"
            );
            for c in &report.per_category {
                println!(
                    "{:<9} {:>9} {:>7} {:>9.3}",
                    c.category,
                    c.questions,
                    c.correct,
                    c.correct as f64 / c.questions.max(1) as f64
                );
            }
            println!(
                "{:<9} {:>9} {:>7} {:>9.3}",
                "overall",
                report.questions,
                report.correct,
                report.accuracy()
            );
            println!("judge parse failures: {}", report.parse_failures);
            println!("skipped (no golden): {skipped}");
            println!("errors (retried out): {errors}");
            if let Some(path) = json {
                std::fs::write(path, serde_json::to_string_pretty(&report)?)?;
            }
            if let Some(path) = dump {
                write_jsonl(path, &records)?;
            }
        }

        Command::PmRun {
            limit,
            mode,
            jobs,
            context,
            context_jobs,
            chat_base_url,
            chat_api_key,
            chat_model,
            json,
            dump,
        } => {
            // PersonaMem files sit next to locomo10.json in the data dir.
            let dir = cli.data.parent().unwrap_or(std::path::Path::new("."));
            let questions_csv = std::fs::read_to_string(dir.join("questions_32k.csv"))?;
            let contexts_jsonl = std::fs::read_to_string(dir.join("shared_contexts_32k.jsonl"))?;
            let questions = merkur_eval::personamem::parse_questions_csv(&questions_csv)?;
            let contexts = merkur_eval::personamem::parse_contexts_jsonl(&contexts_jsonl)?;

            // Group questions by shared context, preserving checkpoint replay.
            let mut by_context: std::collections::BTreeMap<
                String,
                Vec<&merkur_eval::personamem::PmQuestion>,
            > = Default::default();
            for q in &questions {
                by_context.entry(q.context_id.clone()).or_default().push(q);
            }

            let client = base_client(&cli)?;
            let chat = std::sync::Arc::new(OpenAiChat::new(
                chat_base_url,
                chat_api_key.clone(),
                chat_model,
            ));
            // Contexts are namespace-isolated, so replay them concurrently;
            // per-checkpoint answer fan-out stays at `jobs`.
            let ctx_units: Vec<_> = by_context
                .iter()
                .filter(|(ctx_id, _)| {
                    context
                        .as_ref()
                        .is_none_or(|f| ctx_id.starts_with(f.as_str()))
                })
                .collect();
            let runs: Vec<_> = futures::stream::iter(ctx_units)
                .map(|(ctx_id, qs)| {
                    let client = client.clone();
                    let chat = chat.clone();
                    let contexts = &contexts; // capture a Copy reference
                    async move {
                        let Some(ctx) = contexts.get(ctx_id) else {
                            eprintln!("[pm] context {ctx_id} missing from contexts file, skipped");
                            return None;
                        };
                        let ns = format!("pm-{}", &ctx_id[..16.min(ctx_id.len())]);
                        let namespaced = client.with_namespace(&ns);
                        match pm_run_context(
                            &namespaced,
                            chat.as_ref(),
                            ctx,
                            qs,
                            *limit,
                            mode,
                            *jobs,
                        )
                        .await
                        {
                            Ok(run) => {
                                println!(
                                    "{}: {} answered, {} errors ({} turns)",
                                    &ctx_id[..8.min(ctx_id.len())],
                                    run.records.len(),
                                    run.errors,
                                    run.turns_written
                                );
                                Some(run)
                            }
                            Err(e) => {
                                eprintln!(
                                    "[pm] context {} failed: {e}",
                                    &ctx_id[..8.min(ctx_id.len())]
                                );
                                None
                            }
                        }
                    }
                })
                .buffer_unordered((*context_jobs).max(1))
                .collect()
                .await;
            let context_failures = runs.iter().filter(|r| r.is_none()).count();
            let mut records = Vec::new();
            let mut errors = 0;
            let mut turns = 0;
            for run in runs.into_iter().flatten() {
                errors += run.errors;
                turns += run.turns_written;
                records.extend(run.records);
            }
            if context_failures > 0 {
                println!("context failures: {context_failures}");
            }
            let report = score_pm(&records);
            println!("\npersonamem accuracy (model={chat_model})");
            println!(
                "{:<40} {:>9} {:>7} {:>9}",
                "question_type", "questions", "correct", "accuracy"
            );
            for t in &report.per_type {
                println!(
                    "{:<40} {:>9} {:>7} {:>9.3}",
                    t.question_type,
                    t.questions,
                    t.correct,
                    t.correct as f64 / t.questions.max(1) as f64
                );
            }
            println!(
                "{:<40} {:>9} {:>7} {:>9.3}",
                "overall",
                report.questions,
                report.correct,
                report.accuracy()
            );
            println!("unparseable choices: {}", report.parse_failures);
            println!("errors (retried out): {errors}");
            println!("turns written: {turns}");
            if let Some(path) = json {
                std::fs::write(path, serde_json::to_string_pretty(&report)?)?;
            }
            if let Some(path) = dump {
                write_jsonl(path, &records)?;
            }
        }
    }
    Ok(())
}

fn selected<'a>(
    ds: &'a Dataset,
    conv: &Option<String>,
) -> impl Iterator<Item = &'a merkur_eval::dataset::Conversation> {
    ds.conversations
        .iter()
        .filter(move |c| conv.as_ref().is_none_or(|s| s == &c.sample_id))
}
