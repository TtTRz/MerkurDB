# merkur-eval — LoCoMo benchmark harness

Evaluates MerkurDB on the [LoCoMo](https://github.com/snap-research/locomo)
long-conversational-memory benchmark (10 conversations, 5,882 dialog turns,
1,986 QA). Dataset is CC BY-NC 4.0 (research use only) and is **not**
committed to the repo.

## Setup

```bash
scripts/fetch_locomo.sh            # downloads crates/eval/data/locomo10.json
cargo +1.97.0 build --release -p merkur-server --features openai -p merkur-eval
```

## One-command run

```bash
export MERKUR_EVAL_EMBED_BASE_URL=https://api.openai.com   # server-side embeddings
export MERKUR_EVAL_EMBED_API_KEY=sk-...
export MERKUR_EVAL_EMBED_MODEL=text-embedding-3-small
export MERKUR_EVAL_CHAT_BASE_URL=https://api.openai.com    # eval-side judge/answer
export MERKUR_EVAL_CHAT_API_KEY=sk-...
export MERKUR_EVAL_CHAT_MODEL=gpt-4o-mini
scripts/run_locomo.sh
```

The script boots a throwaway server (temp sqlite, noop consolidator,
consolidation/forgetting ticks parked at 24 h so the offline pipeline does
not mutate the corpus mid-measurement), runs all three stages, copies the
JSON reports into `crates/eval/data/`, and tears everything down.

## Stages (runnable individually)

```bash
merkur-eval stats                                  # dataset shape, no server
merkur-eval ingest  --server URL --token T         # one memory per turn
merkur-eval recall  --limit 10 --mode hybrid       # LLM-free
merkur-eval qa      --limit 10                     # needs MERKUR_EVAL_CHAT_*
```

Global flags: `--data`, `--server`, `--token`, `--conv <sample_id>` (restrict
to one conversation; sample ids are `conv-26`, `conv-30`, ... `conv-50`).

## Methodology

**Ingest.** One memory per dialog turn, namespace = `sample_id`. Content is
`[<session date>] <speaker>: <text>` so both BM25 and vector channels see
the temporal anchor. `dia_id` rides in `context` (search results carry
`context`, not `metadata`), which lets recall map hits to evidence without
N+1 lookups.

**Recall (LLM-free).** For each QA, hybrid search with
`score_threshold=0` (ungated — recall measures ranking, not the production
threshold). Per question: `coverage = |evidence ∩ retrieved| / |evidence|`,
`hit = coverage > 0`. Reported per category and overall. Questions without
evidence annotations (4 of 1,986) are skipped and counted. This is the
metric P1-5 composite-weight tuning iterates on.

**QA (judged).** Retrieved top-k contents → grounded answer prompt
(abstention explicitly allowed) → judge grades semantic equivalence against
the golden answer. Category 5 (adversarial, 446 questions) has no real
answer — its `adversarial_answer` is the hallucination trap, and the judge
scores abstention as correct. Judge replies that fail to parse count as
incorrect but are tallied separately (`parse_failures`).

Categories: 1 single-hop (282) · 2 multi-hop (321) · 3 temporal (96) ·
4 open-domain (841) · 5 adversarial (446).

## Tests

Pure logic (parsing, scoring, prompts, verdicts) is unit-tested; HTTP
orchestration is tested against an in-process axum stub server
(`cargo +1.97.0 test -p merkur-eval`). No live LLM or MerkurDB server is
needed for the test suite.
