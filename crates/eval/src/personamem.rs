//! PersonaMem benchmark support (bowen-upenn/PersonaMem, MIT license).
//!
//! Differences from LoCoMo: questions are multiple-choice (exact-match
//! scoring against `correct_answer` — no judge LLM), and each question is
//! asked "in-situ" at `end_index` within a shared context, so the runner
//! replays each context into one namespace in ascending checkpoint order.
//!
//! Context messages have no timestamps; ingested content is
//! `[#<msg index>] <role>: <content>`, and `system` messages are skipped
//! (per-session scenario boilerplate, not user history).

use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum PmError {
    #[error("invalid questions CSV: {0}")]
    Csv(#[from] csv::Error),

    #[error("invalid contexts JSONL: {0}")]
    Json(#[from] serde_json::Error),

    #[error("bad field in questions CSV: {0}")]
    Field(String),
}

#[derive(Debug)]
pub struct PmQuestion {
    pub question_id: String,
    pub persona_id: String,
    pub question_type: String,
    pub topic: String,
    pub question: String,
    pub correct_letter: char,
    pub options: Vec<String>,
    pub context_id: String,
    /// In-situ cutoff: the question may only see `messages[..=end_index]`.
    pub end_index: usize,
}

#[derive(Debug)]
pub struct PmContext {
    pub id: String,
    pub messages: Vec<PmMessage>,
}

#[derive(Debug, serde::Deserialize)]
pub struct PmMessage {
    pub role: String,
    pub content: String,
}

pub fn parse_questions_csv(input: &str) -> Result<Vec<PmQuestion>, PmError> {
    let mut rdr = csv::Reader::from_reader(input.as_bytes());
    let mut out = Vec::new();
    for rec in rdr.records() {
        let rec = rec?;
        let get = |i: usize| rec.get(i).unwrap_or_default().to_string();
        let options_raw = get(12);
        let options = parse_options(&options_raw)?;
        let correct_letter = letter_of(&get(11))
            .ok_or_else(|| PmError::Field(format!("bad correct_answer: {}", get(11))))?;
        let end_index: usize = get(14)
            .parse()
            .map_err(|_| PmError::Field(format!("bad end_index: {}", get(14))))?;
        out.push(PmQuestion {
            persona_id: get(0),
            question_id: get(1),
            question_type: get(2),
            topic: get(3),
            question: get(10),
            correct_letter,
            options,
            context_id: get(13),
            end_index,
        });
    }
    Ok(out)
}

pub fn parse_contexts_jsonl(input: &str) -> Result<HashMap<String, PmContext>, PmError> {
    let mut out = HashMap::new();
    for line in input.lines().filter(|l| !l.trim().is_empty()) {
        let map: HashMap<String, Vec<PmMessage>> = serde_json::from_str(line)?;
        for (id, messages) in map {
            out.insert(id.clone(), PmContext { id, messages });
        }
    }
    Ok(out)
}

/// The dataset mixes JSON lists and Python-literal lists (single quotes) in
/// `all_options` — try JSON first, then a small Python-string-list parser
/// handling both quote styles and backslash escapes.
pub fn parse_options(raw: &str) -> Result<Vec<String>, PmError> {
    if let Ok(v) = serde_json::from_str::<Vec<String>>(raw) {
        return Ok(v);
    }
    let t = raw.trim();
    let inner = t
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| PmError::Field(format!("all_options not a list: {}", &raw[..raw.len().min(60)])))?;
    let mut out = Vec::new();
    let mut chars = inner.chars().peekable();
    loop {
        // Skip whitespace and commas between items.
        while matches!(chars.peek(), Some(',') | Some(' ') | Some('\n') | Some('\t')) {
            chars.next();
        }
        let Some(&quote) = chars.peek() else { break };
        if quote != '\'' && quote != '"' {
            return Err(PmError::Field(format!("unexpected char in options list: {quote}")));
        }
        chars.next();
        let mut item = String::new();
        loop {
            match chars.next() {
                Some('\\') => {
                    if let Some(c) = chars.next() {
                        item.push(c);
                    }
                }
                Some(c) if c == quote => break,
                Some(c) => item.push(c),
                None => return Err(PmError::Field("unterminated option string".into())),
            }
        }
        out.push(item);
    }
    Ok(out)
}

/// `(c)` → `c` (case-insensitive). Anything else is a data error.
pub fn letter_of(correct_answer: &str) -> Option<char> {
    let t = correct_answer.trim().trim_start_matches('(').trim_end_matches(')');
    let mut chars = t.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => Some(c.to_ascii_lowercase()),
        _ => None,
    }
}

/// Parse the model's chosen option letter. Accepts `(c)`, `The answer is
/// (c).`, a bare `C`, or `c) ...`; letters beyond the option count are
/// rejected. `None` = unparseable (scored as wrong).
pub fn parse_choice(raw: &str, n_options: usize) -> Option<char> {
    let valid = |c: char| (c as usize) < ('a' as usize + n_options);
    let lower = raw.to_lowercase();
    // Strongest signal first: a parenthesized letter anywhere.
    for (i, _) in lower.match_indices('(') {
        let mut chars = lower[i + 1..].chars();
        if let (Some(c), Some(')')) = (chars.next(), chars.next())
            && c.is_ascii_lowercase()
            && valid(c)
        {
            return Some(c);
        }
    }
    // Bare-letter forms: "a", "A.", "b)".
    let t = lower.trim();
    let mut chars = t.chars();
    if let Some(c) = chars.next()
        && c.is_ascii_lowercase()
        && valid(c)
        && matches!(chars.next(), None | Some('.') | Some(')'))
    {
        return Some(c);
    }
    None
}

/// Multiple-choice prompt: pick the response that best fits the user's
/// CURRENT profile per the retrieved memories. No abstention — a non-answer
/// scores as wrong anyway, so the model must commit to a letter.
pub fn build_mc_prompt(
    question: &str,
    options: &[String],
    memories: &[String],
) -> (String, String) {
    let system = "You are selecting the best assistant response. Judge each option against the \
        user's CURRENT (latest) preferences and facts as shown in the retrieved memories — when \
        memories conflict, the most recent statement wins. Reply with exactly one letter in \
        parentheses, e.g. \"(b)\"."
        .to_string();
    let mut user = String::from("Memories:\n");
    for m in memories {
        user.push_str("- ");
        user.push_str(m);
        user.push('\n');
    }
    user.push_str("\nThe user says: ");
    user.push_str(question);
    user.push_str("\n\nCandidate responses:\n");
    for o in options {
        user.push_str(o);
        user.push('\n');
    }
    user.push_str("\nWhich response best fits the user right now? Reply with the letter only.");
    (system, user)
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUESTIONS_CSV: &str = concat!(
        "persona_id,question_id,question_type,topic,context_length_in_tokens,context_length_in_letters,",
        "distance_to_ref_in_blocks,distance_to_ref_in_tokens,num_irrelevant_tokens,",
        "distance_to_ref_proportion_in_context,user_question_or_message,correct_answer,all_options,",
        "shared_context_id,end_index_in_shared_context\n",
        "0,q-1,recall_user_shared_facts,music,100,500,1,50,0,50%,\"I like jazz, what do you recommend?\",(b),",
        "\"[\"\"(a) Rock is best, you said so\"\", \"\"(b) A jazz club, since you love jazz, and not rock\"\", \"\"(c) Classical\"\", \"\"(d) Pop\"\"]\",ctxA,10\n",
        "0,q-2,track_full_preference_evolution,music,100,500,1,50,0,50%,What should I listen to now?,(d),",
        "\"[\"\"(a) Jazz\"\", \"\"(b) Rock\"\", \"\"(c) Pop\"\", \"\"(d) Ambient, matching your latest taste\"\"]\",ctxA,20\n",
    );

    const CONTEXTS_JSONL: &str = concat!(
        r#"{"ctxA": [{"role":"system","content":"You are a music assistant."},"#,
        r#"{"role":"user","content":"I used to love rock."},"#,
        r#"{"role":"assistant","content":"Noted, rock it is."}]}"#,
        "\n",
        r#"{"ctxB": [{"role":"user","content":"hello"}]}"#,
        "\n",
    );

    #[test]
    fn parses_python_style_single_quoted_options() {
        // 303/589 real rows are Python-literal lists, not JSON.
        let raw = r#"['(a) It\'s fine', '(b) "quoted" ok', '(c) third', '(d) fourth']"#;
        let opts = parse_options(raw).unwrap();
        assert_eq!(opts.len(), 4);
        assert_eq!(opts[0], "(a) It's fine");
        assert_eq!(opts[1], "(b) \"quoted\" ok");
    }

    #[test]
    fn parse_options_prefers_json_then_falls_back() {
        let json = r#"["(a) one", "(b) two"]"#;
        assert_eq!(parse_options(json).unwrap().len(), 2);
        let py = r#"['(a) one', '(b) two']"#;
        assert_eq!(parse_options(py).unwrap().len(), 2);
    }

    #[test]
    fn parses_questions_csv_with_quoted_options() {
        let qs = parse_questions_csv(QUESTIONS_CSV).unwrap();
        assert_eq!(qs.len(), 2);
        let q = &qs[0];
        assert_eq!(q.question_id, "q-1");
        assert_eq!(q.question_type, "recall_user_shared_facts");
        assert_eq!(q.correct_letter, 'b');
        assert_eq!(q.options.len(), 4);
        assert!(q.options[1].contains("jazz club"));
        assert_eq!(q.context_id, "ctxA");
        assert_eq!(q.end_index, 10);
        assert_eq!(qs[1].correct_letter, 'd');
    }

    #[test]
    fn parses_contexts_jsonl_keyed_by_id() {
        let ctxs = parse_contexts_jsonl(CONTEXTS_JSONL).unwrap();
        assert_eq!(ctxs.len(), 2);
        let a = &ctxs["ctxA"];
        assert_eq!(a.messages.len(), 3);
        assert_eq!(a.messages[1].role, "user");
        assert_eq!(a.messages[1].content, "I used to love rock.");
    }

    #[test]
    fn letter_of_parses_parenthesized_answer() {
        assert_eq!(letter_of("(c)"), Some('c'));
        assert_eq!(letter_of("(A)"), Some('a'));
        assert_eq!(letter_of("bad"), None);
    }

    #[test]
    fn parse_choice_accepts_common_judge_free_formats() {
        assert_eq!(parse_choice("(b)", 4), Some('b'));
        assert_eq!(parse_choice("The best answer is (c).", 4), Some('c'));
        assert_eq!(parse_choice("A", 4), Some('a'));
        assert_eq!(parse_choice("b) because...", 4), Some('b'));
        assert_eq!(parse_choice("I choose nothing", 4), None);
        // Out-of-range letter must not match.
        assert_eq!(parse_choice("(e)", 4), None);
    }

    #[test]
    fn mc_prompt_frames_response_selection_and_forces_a_letter() {
        let (system, user) = build_mc_prompt(
            "I like jazz, what do you recommend?",
            &["(a) Rock".into(), "(b) A jazz club".into()],
            &["[#5] user: I love jazz".into()],
        );
        assert!(system.contains("current") || system.contains("latest"));
        assert!(user.contains("I like jazz, what do you recommend?"));
        assert!(user.contains("(a) Rock"));
        assert!(user.contains("(b) A jazz club"));
        assert!(user.contains("[#5] user: I love jazz"));
    }
}
