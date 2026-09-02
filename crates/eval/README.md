# merkur-eval — benchmark harness

Evaluates MerkurDB on two public memory benchmarks over the live HTTP serving
path — the same path real clients use — with per-question JSONL dumps so every
number is auditable.

- [LoCoMo](https://github.com/snap-research/locomo) (CC BY-NC 4.0, research
  only): 10 conversations, 5,882 dialog turns, 1,986 QA with evidence
  annotations. Two tracks: **retrieval recall** (LLM-free, scored against
  evidence `dia_id`s) and **judge-graded QA**.
- [PersonaMem](https://huggingface.co/datasets/bowen-upenn/PersonaMem) (MIT):
  589 multiple-choice questions over 37 shared contexts, asked in-situ — each
  context replays into one namespace in ascending `end_index` order and a
  question is answered only against turns visible at its checkpoint (no
  future-turn leakage). Exact-match scoring, no judge LLM.

Datasets are downloaded, not committed (gitignored under `crates/eval/data/`).

## Setup

```bash
scripts/fetch_locomo.sh            # locomo10.json
# PersonaMem 32k: download questions_32k.csv + shared_contexts_32k.jsonl from
# the HuggingFace page into crates/eval/data/
cargo +1.97.0 build --release -p merkur-server --features openai -p merkur-eval
```

Both scripts want OpenAI-compatible endpoints. BASE_URLs are service roots
(no `/v1`); the vector dimension is probed automatically at server boot.

```bash
export MERKUR_EVAL_EMBED_BASE_URL=...   # server-side embeddings
export MERKUR_EVAL_EMBED_API_KEY=...
export MERKUR_EVAL_EMBED_MODEL=...
export MERKUR_EVAL_CHAT_BASE_URL=...    # eval-side answer + judge (LoCoMo qa)
export MERKUR_EVAL_CHAT_API_KEY=...
export MERKUR_EVAL_CHAT_MODEL=...
```

## Running

```bash
scripts/run_locomo.sh              # ingest -> recall -> qa (throwaway server)
scripts/run_personamem.sh          # in-situ replay -> MC answers -> report
scripts/sweep_fusion.sh            # P1-5 fusion sweep: persistent corpus,
                                   # restart per config, recall-only
```

Knobs (env): `MERKUR_EVAL_LIMIT` (retrieval depth, 10), `MERKUR_EVAL_JOBS`
(concurrency, 8), `MERKUR_EVAL_ANSWER_STYLE` (`baseline` | `aggregate` |
`guarded` — `guarded` is the recommended reporting style: aggregate facts
across memories, but abstain when the question presupposes an unestablished
fact), `MERKUR_EVAL_CONV` (one LoCoMo conversation), `MERKUR_EVAL_PM_*` for
PersonaMem (`LIMIT` 30, `JOBS` 8, `CONTEXT_JOBS` 4, `CONTEXT` prefix filter,
`TAG` report suffix).

The CLI also runs standalone against any live server (`merkur-eval
stats|ingest|recall|qa|pm-run --server ... --token ...`), with per-question
`--dump` JSONL for failure analysis.

## Methodology notes

- **Ingest** writes one memory per dialog turn: LoCoMo content is
  `[<session date>] <speaker>: <text>` with `dia_id` in `context` (search
  results return `context`, not `metadata`); PersonaMem content is
  `[#<msg index>] <role>: <content>` and `system` messages are skipped.
- **Recall** searches with `score_threshold=0` — ungated, so the metric
  reflects ranking quality, not the production threshold. Per question:
  `coverage = |evidence ∩ retrieved| / |evidence|`, `hit = coverage > 0`.
- **QA (LoCoMo)** retrieves top-k, answers with a grounded prompt, then a
  judge grades semantic equivalence; adversarial (category 5) questions carry
  a trap answer and score abstention as correct. Unparseable judge replies
  count as incorrect but are tallied separately.
- **Resilience**: searches retry with backoff (3 attempts); exhausted
  questions are counted under `errors` and excluded from scoring rather than
  aborting the run.

## Tests

`cargo +1.97.0 test -p merkur-eval` — pure logic (parsing, scoring, prompts,
choice/verdict parsing) is unit-tested; HTTP orchestration is tested against
an in-process axum stub server. No live LLM or MerkurDB server needed.
