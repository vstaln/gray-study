//! gray-study — gray tutors YOU.
//!
//! `/study <topic>` starts a Socratic tutoring session: it records the active
//! topic in `~/.gray/study/active.json` and returns a `prompt` that puts the
//! model in tutor mode. `study_quiz` asks the user a question — interactively
//! via `host/ask` when that capability is granted, otherwise it returns the
//! question as text so the model can ask inline. `study_deck` keeps SM-2-lite
//! flashcards in `~/.gray/study/decks/<name>.json`: new cards are due
//! immediately, the interval starts at 1 day, doubles on each correct answer
//! (30-day cap), and resets on a wrong one. `prompt/context` re-injects the
//! tutor persona while a topic is active and notes how many cards are due.
//! State honors `$GRAY_HOME` (fallback `$HOME/.gray`).

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const DAY_SECS: u64 = 86_400;
const MAX_INTERVAL_DAYS: u64 = 30;
/// host/ask wait: under the host's ask-gated outer deadline (330s) so the
/// plugin degrades to text instead of surfacing the host's generic timeout.
const ASK_TTL: Duration = Duration::from_secs(300);
/// Keeps `due`/`list` payloads under the wire-reply budget.
const MAX_LIST_CARDS: usize = 20;
const MAX_FIELD_CHARS: usize = 200;

type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>;

fn manifest() -> Value {
    json!({
        "name": "study",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "tools": [
            {
                "name": "study_deck",
                "description": "Manage spaced-repetition flashcard decks (SM-2-lite). Actions: \
                 'create' a deck, 'add' a card (front/back; creates the deck if needed), 'due' to \
                 list cards past their review date, 'answer' to record a correct/wrong result, \
                 'list' to show decks (or one deck's cards). Intervals start at 1 day, double on \
                 correct answers up to 30 days, reset on wrong ones. Use during study sessions to \
                 capture facts the user should keep.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["create", "add", "due", "answer", "list"],
                            "description": "What to do."
                        },
                        "deck": {
                            "type": "string",
                            "description": "Deck name. Required for create/add/answer; optional for due/list (omit to cover every deck)."
                        },
                        "front": {"type": "string", "description": "Question side of the card (add)."},
                        "back": {"type": "string", "description": "Answer side of the card (add)."},
                        "card_id": {"description": "Card id (answer)."},
                        "correct": {"description": "Whether the user's answer was right (answer)."}
                    },
                    "required": ["action"]
                }
            },
            {
                "name": "study_quiz",
                "description": "Ask the user a quiz question. Pass 'options' to make it multiple \
                 choice; renders as an interactive picker when the host allows it, otherwise the \
                 question text comes back for you to ask inline.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "question": {"type": "string", "description": "The question to ask."},
                        "options": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Multiple-choice answers; omit for an open question."
                        }
                    },
                    "required": ["question"]
                }
            }
        ],
        "commands": ["/study"],
        "hooks": ["prompt/context"],
        "capabilities": ["host.ask"]
    })
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `~/.gray`, or `$GRAY_HOME` when set.
fn state_root() -> PathBuf {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn decks_dir(root: &Path) -> PathBuf {
    root.join("study").join("decks")
}

fn active_path(root: &Path) -> PathBuf {
    root.join("study").join("active.json")
}

/// Map a deck name to a safe filename component (no separators / traversal).
fn safe_deck_name(name: &str) -> Option<String> {
    let safe: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    (!safe.is_empty()).then_some(safe)
}

fn deck_path(root: &Path, deck: &str) -> Result<PathBuf, String> {
    let safe = safe_deck_name(deck).ok_or("missing or empty deck name")?;
    Ok(decks_dir(root).join(format!("{safe}.json")))
}

fn deck_name(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("deck")
        .to_string()
}

fn read_deck(path: &Path) -> Result<Value, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read deck {}: {e}", path.display()))?;
    let mut v: Value = serde_json::from_str(&text)
        .map_err(|e| format!("deck {} is corrupt: {e}", path.display()))?;
    if v.get("cards").and_then(Value::as_array).is_none() {
        v["cards"] = json!([]);
    }
    Ok(v)
}

fn write_deck(path: &Path, deck: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, serde_json::to_string_pretty(deck).unwrap_or_default())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn empty_deck(name: &str) -> Value {
    json!({"name": name, "cards": []})
}

/// Deck files on disk → (file-stem, path), sorted for stable output.
fn deck_files(root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(decks_dir(root)) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) == Some("json")
            && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
        {
            out.push((stem.to_string(), p));
        }
    }
    out.sort();
    out
}

fn card_is_due(card: &Value, at: u64) -> bool {
    card.get("next_review").and_then(Value::as_u64).unwrap_or(0) <= at
}

/// Every due card across every deck → (deck name, card).
fn due_cards(root: &Path, at: u64) -> Vec<(String, Value)> {
    let mut due = Vec::new();
    for (name, path) in deck_files(root) {
        let Ok(deck) = read_deck(&path) else { continue };
        for card in deck["cards"].as_array().cloned().unwrap_or_default() {
            if card_is_due(&card, at) {
                due.push((name.clone(), card));
            }
        }
    }
    due
}

fn clip(s: &str) -> String {
    s.chars().take(MAX_FIELD_CHARS).collect()
}

fn due_row(deck: &str, card: &Value) -> Value {
    json!({
        "deck": deck,
        "card_id": card["id"],
        "front": clip(card["front"].as_str().unwrap_or("")),
        "back": clip(card["back"].as_str().unwrap_or("")),
    })
}

/// Loose booleans: `correct` may arrive as bool, string, or number.
fn truthy(v: Option<&Value>) -> Option<bool> {
    match v? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.as_str() {
            "true" | "yes" | "y" | "1" | "correct" => Some(true),
            "false" | "no" | "n" | "0" | "wrong" => Some(false),
            _ => None,
        },
        Value::Number(n) => n.as_i64().map(|i| i != 0),
        _ => None,
    }
}

fn tool_deck(root: &Path, args: &Value) -> Result<String, String> {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    let deck_arg = args.get("deck").and_then(Value::as_str).unwrap_or("");
    match action {
        "create" => {
            let path = deck_path(root, deck_arg)?;
            if path.exists() {
                let d = read_deck(&path)?;
                let n = d["cards"].as_array().map(Vec::len).unwrap_or(0);
                return Ok(format!("deck '{}' already exists ({n} cards)", deck_name(&path)));
            }
            let name = deck_name(&path);
            write_deck(&path, &empty_deck(&name))?;
            Ok(format!("created deck '{name}'"))
        }
        "add" => {
            let front = args.get("front").and_then(Value::as_str).unwrap_or("").trim();
            let back = args.get("back").and_then(Value::as_str).unwrap_or("").trim();
            if front.is_empty() || back.is_empty() {
                return Err("add needs both `front` and `back`".into());
            }
            let path = deck_path(root, deck_arg)?;
            let name = deck_name(&path);
            let mut deck = if path.exists() {
                read_deck(&path)?
            } else {
                empty_deck(&name)
            };
            if deck["cards"].as_array().is_none() {
                deck["cards"] = json!([]);
            }
            let next_id = deck["cards"]
                .as_array()
                .map(|c| {
                    c.iter()
                        .filter_map(|x| x["id"].as_u64())
                        .max()
                        .unwrap_or(0)
                })
                .unwrap_or(0)
                + 1;
            deck["cards"].as_array_mut().map(|cards| {
                cards.push(json!({
                    "id": next_id,
                    "front": front,
                    "back": back,
                    "interval_days": 1,
                    // New cards are due immediately: nothing reviewed yet.
                    "next_review": now(),
                    "reviews": 0,
                    "correct": 0
                }));
            });
            write_deck(&path, &deck)?;
            Ok(format!("added card #{next_id} to '{name}' (due now)"))
        }
        "due" => {
            let at = now();
            let mut rows = Vec::new();
            if deck_arg.trim().is_empty() {
                for (name, card) in due_cards(root, at) {
                    rows.push(due_row(&name, &card));
                }
            } else {
                let path = deck_path(root, deck_arg)?;
                if !path.exists() {
                    return Err(format!("no such deck: '{deck_arg}'"));
                }
                let name = deck_name(&path);
                for c in read_deck(&path)?["cards"].as_array().cloned().unwrap_or_default() {
                    if card_is_due(&c, at) {
                        rows.push(due_row(&name, &c));
                    }
                }
            }
            let total = rows.len();
            rows.truncate(MAX_LIST_CARDS);
            Ok(json!({
                "due": rows,
                "due_count": total,
                "truncated": total > MAX_LIST_CARDS,
            })
            .to_string())
        }
        "answer" => {
            let path = deck_path(root, deck_arg)?;
            if !path.exists() {
                return Err(format!("no such deck: '{deck_arg}'"));
            }
            let card_id = args
                .get("card_id")
                .and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                })
                .ok_or("answer needs `card_id`")?;
            let correct =
                truthy(args.get("correct")).ok_or("answer needs `correct` (true/false)")?;
            let mut deck = read_deck(&path)?;
            let name = deck_name(&path);
            let Some(card) = deck["cards"]
                .as_array_mut()
                .and_then(|cards| cards.iter_mut().find(|c| c["id"].as_u64() == Some(card_id)))
            else {
                return Err(format!("no card #{card_id} in '{name}'"));
            };
            let prev = card["interval_days"].as_u64().unwrap_or(1).max(1);
            let next = if correct {
                (prev * 2).min(MAX_INTERVAL_DAYS)
            } else {
                1
            };
            card["interval_days"] = json!(next);
            card["next_review"] = json!(now() + next * DAY_SECS);
            card["reviews"] = json!(card["reviews"].as_u64().unwrap_or(0) + 1);
            if correct {
                card["correct"] = json!(card["correct"].as_u64().unwrap_or(0) + 1);
            }
            write_deck(&path, &deck)?;
            Ok(json!({
                "card_id": card_id,
                "correct": correct,
                "interval_days": next,
                "next_review_in_days": next,
            })
            .to_string())
        }
        "list" => {
            if deck_arg.trim().is_empty() {
                let at = now();
                let mut decks = Vec::new();
                for (name, path) in deck_files(root) {
                    let Ok(d) = read_deck(&path) else { continue };
                    let cards = d["cards"].as_array().cloned().unwrap_or_default();
                    let due = cards.iter().filter(|c| card_is_due(c, at)).count();
                    decks.push(json!({"name": name, "cards": cards.len(), "due": due}));
                }
                Ok(json!({"decks": decks}).to_string())
            } else {
                let path = deck_path(root, deck_arg)?;
                if !path.exists() {
                    return Err(format!("no such deck: '{deck_arg}'"));
                }
                let at = now();
                let d = read_deck(&path)?;
                let mut cards: Vec<Value> = d["cards"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c["id"],
                            "front": clip(c["front"].as_str().unwrap_or("")),
                            "interval_days": c["interval_days"],
                            "due": card_is_due(c, at),
                        })
                    })
                    .collect();
                let total = cards.len();
                cards.truncate(MAX_LIST_CARDS);
                Ok(json!({"deck": deck_name(&path), "cards": cards, "card_count": total}).to_string())
            }
        }
        other => Err(format!("unknown action '{other}' — use create|add|due|answer|list")),
    }
}

/// Pull every answer string out of a `host/ask` `result` ({answers}), in
/// whatever nesting the host used.
fn extract_answer(result: &Value) -> Option<String> {
    let a = result.get("answers")?;
    let mut out = Vec::new();
    let mut collect = |v: &Value, out: &mut Vec<String>| {
        if let Some(s) = v.as_str() {
            out.push(s.to_string());
        } else if let Some(arr) = v.as_array() {
            out.extend(arr.iter().filter_map(Value::as_str).map(str::to_string));
        }
    };
    match a {
        Value::Array(items) => {
            for it in items {
                if let Some(arr) = it.get("answers") {
                    collect(arr, &mut out);
                } else {
                    collect(it, &mut out);
                }
            }
        }
        Value::Object(m) => {
            for v in m.values() {
                collect(v, &mut out);
            }
        }
        _ => collect(a, &mut out),
    }
    (!out.is_empty()).then(|| out.join(", "))
}

/// The side of the wire that can call back into the host. Absent in tests →
/// `study_quiz` degrades to returning the question text.
struct Host {
    out: Arc<Mutex<std::io::Stdout>>,
    pending: Pending,
    counter: Arc<Mutex<u64>>,
}

impl Host {
    /// Send `host/ask` and wait for the reply routed back by the reader
    /// thread. `None` = no reply inside the TTL.
    fn ask(&self, params: Value) -> Option<Value> {
        let id = {
            let mut n = self.counter.lock().ok()?;
            *n += 1;
            format!("q{n}")
        };
        let req = json!({"id": id, "method": "host/ask", "params": params});
        let (tx, rx) = mpsc::channel();
        self.pending.lock().ok()?.insert(id, tx);
        {
            let mut o = self.out.lock().ok()?;
            writeln!(o, "{req}").ok()?;
            o.flush().ok()?;
        }
        rx.recv_timeout(ASK_TTL).ok()
    }
}

fn tool_quiz(args: &Value, host: Option<&Host>) -> Result<String, String> {
    let q = args.get("question").and_then(Value::as_str).unwrap_or("").trim();
    if q.is_empty() {
        return Err("study_quiz needs `question`".into());
    }
    let options: Vec<String> = args
        .get("options")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    // Text fallback: hand the question back so the model asks it inline.
    let degrade = |note: &str| {
        let mut t = String::from(q);
        for (i, o) in options.iter().enumerate() {
            t.push_str(&format!("\n{}) {}", (b'A' + i as u8) as char, o));
        }
        t.push_str(&format!(
            "\n\n({note} — ask this question in your reply and wait for the user's answer.)"
        ));
        t
    };
    let Some(host) = host else {
        return Ok(degrade("interactive asking is unavailable"));
    };
    let mut question = json!({"id": "quiz", "header": "Study quiz", "question": q});
    if !options.is_empty() {
        question["options"] = json!(
            options
                .iter()
                .map(|o| json!({"label": o, "description": ""}))
                .collect::<Vec<_>>()
        );
    }
    match host.ask(json!({"questions": [question], "blocking": true})) {
        Some(reply) => {
            // Capability denied / ask rejected → degrade, not error.
            if reply.get("error").is_some()
                || reply.pointer("/result/error").is_some()
            {
                return Ok(degrade("the host declined the ask"));
            }
            let result = reply.get("result").cloned().unwrap_or(Value::Null);
            match extract_answer(&result) {
                Some(a) => Ok(format!("user answered: {a}")),
                None => Ok(degrade("the host returned no answer")),
            }
        }
        None => Ok(degrade("the host did not answer in time")),
    }
}

fn tool_call(root: &Path, name: &str, args: &Value, host: Option<&Host>) -> Result<String, String> {
    match name {
        "study_deck" => tool_deck(root, args),
        "study_quiz" => tool_quiz(args, host),
        other => Err(format!("unknown tool: {other}")),
    }
}

fn active_topic(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(active_path(root)).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let topic = v.get("topic").and_then(Value::as_str)?.trim();
    (!topic.is_empty()).then(|| topic.to_string())
}

fn prompt_context(root: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(topic) = active_topic(root) {
        parts.push(format!(
            "Active study session — you are a Socratic tutor for \"{topic}\". \
             Teach by asking one question at a time, adapt to the user's \
             answers, and don't lecture. Use the study_quiz tool for quick \
             checks and study_deck for flashcards."
        ));
    }
    let due = due_cards(root, now()).len();
    if due > 0 {
        parts.push(format!(
            "{due} flashcard(s) due for review — offer a quick review round \
             (study_deck action due)."
        ));
    }
    parts.join("\n")
}

fn status_text(root: &Path) -> String {
    let decks = deck_files(root).len();
    let due = due_cards(root, now()).len();
    match active_topic(root) {
        Some(t) => format!(
            "study session active: \"{t}\" — {due} flashcard(s) due across {decks} deck(s). \
             /study end to stop · /study decks for details"
        ),
        None => format!(
            "no active study session — {due} flashcard(s) due across {decks} deck(s). \
             /study <topic> to begin · /study decks for details"
        ),
    }
}

fn decks_text(root: &Path) -> String {
    let at = now();
    let mut lines = Vec::new();
    for (name, path) in deck_files(root) {
        let Ok(d) = read_deck(&path) else { continue };
        let cards = d["cards"].as_array().cloned().unwrap_or_default();
        let due = cards.iter().filter(|c| card_is_due(c, at)).count();
        lines.push(format!("{name} — {} card(s), {due} due", cards.len()));
    }
    if lines.is_empty() {
        "no decks yet — the study_deck tool creates them".into()
    } else {
        lines.join("\n")
    }
}

/// `/study …` — `argv` excludes the command name. Returns the `command/run`
/// result payload (`{text}` to say, `{prompt}` to run as a user prompt).
fn run_command(root: &Path, argv: &[String]) -> Value {
    let sub = argv.first().map(String::as_str).unwrap_or("");
    match sub {
        "" => json!({"text": status_text(root)}),
        "end" => {
            let msg = match std::fs::remove_file(active_path(root)) {
                Ok(()) => "study session ended".to_string(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    "no active study session".to_string()
                }
                Err(e) => format!("couldn't clear session: {e}"),
            };
            json!({"text": msg})
        }
        "decks" => json!({"text": decks_text(root)}),
        _ => {
            let topic = argv.join(" ");
            let topic = topic.trim();
            if topic.is_empty() {
                return json!({"text": status_text(root)});
            }
            let rec = json!({"topic": topic, "started_at": now()});
            if let Some(p) = active_path(root).parent() {
                let _ = std::fs::create_dir_all(p);
            }
            let note = if std::fs::write(
                active_path(root),
                serde_json::to_string(&rec).unwrap_or_default(),
            )
            .is_ok()
            {
                ""
            } else {
                " (couldn't persist the session marker — persona won't survive a restart)"
            };
            json!({"prompt": format!(
                "You are now a Socratic tutor for {topic}. Teach by asking questions, not \
                 lecturing: first gauge what the user already knows, then ask one focused \
                 question at a time, adapt to their answers, and correct misconceptions \
                 gently. Use the study_quiz tool for quick comprehension checks and \
                 study_deck to build flashcards as you go. Start now: one line on what \
                 you'll cover, then your first question.{note}"
            )})
        }
    }
}

/// One numbered request → `Ok(result)` / `Err(error object)`.
fn dispatch(
    root: &Path,
    method: &str,
    params: &Value,
    host: Option<&Host>,
) -> Result<Value, Value> {
    Ok(match method {
        "plugin/manifest" => manifest(),
        "prompt/context" => json!({"text": prompt_context(root)}),
        "tool/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("args").cloned().unwrap_or(Value::Null);
            match tool_call(root, name, &args, host) {
                Ok(text) => json!({"content": text}),
                Err(text) => json!({"content": text, "is_error": true}),
            }
        }
        "command/run" => {
            let argv: Vec<String> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            run_command(root, &argv)
        }
        "plugin/shutdown" => json!({}),
        _ => return Err(json!({"code": -32601, "message": "method not found"})),
    })
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest());
        return;
    }
    let stdout = Arc::new(Mutex::new(std::io::stdout()));
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let counter = Arc::new(Mutex::new(0u64));
    let (work_tx, work_rx) = mpsc::channel::<Value>();

    // Reader thread: `host/ask` replies (string id, no method) route into
    // `pending`; numbered requests queue for the main loop; notifications
    // (no id) are dropped. Shutdown closes the channel so the loop exits.
    let reader_pending = pending.clone();
    let reader = std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v.get("method").and_then(Value::as_str) == Some("plugin/shutdown") {
                // A numbered request still gets its `{}` reply before exit.
                if v.get("id").is_some() {
                    let _ = work_tx.send(v);
                }
                break;
            }
            if v.get("method").is_none() {
                if let Some(id) = v.get("id").and_then(Value::as_str) {
                    let tx = reader_pending
                        .lock()
                        .ok()
                        .and_then(|mut p| p.remove(id));
                    if let Some(tx) = tx {
                        let _ = tx.send(v);
                    }
                }
                continue;
            }
            if work_tx.send(v).is_err() {
                break;
            }
        }
    });

    let host = Host {
        out: stdout.clone(),
        pending,
        counter,
    };
    for req in work_rx {
        let id = req.get("id").cloned().unwrap_or(json!(0));
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(Value::Null);
        let exit = method == "plugin/shutdown";
        let reply = match dispatch(&state_root(), method, &params, Some(&host)) {
            Ok(result) => json!({"id": id, "result": result}),
            Err(error) => json!({"id": id, "error": error}),
        };
        {
            let mut o = stdout.lock().expect("stdout");
            let _ = writeln!(o, "{reply}");
            let _ = o.flush();
        }
        if exit {
            break;
        }
    }
    let _ = reader.join();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gray-study-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn call_at(root: &Path, method: &str, params: Value) -> Result<Value, Value> {
        dispatch(root, method, &params, None)
    }

    fn tool(root: &Path, name: &str, args: Value) -> Value {
        call_at(root, "tool/call", json!({"name": name, "args": args})).unwrap()
    }

    #[test]
    fn manifest_declares_tools_command_hook_and_capability() {
        let m = call_at(&tmp("m"), "plugin/manifest", Value::Null).unwrap();
        assert_eq!(m["name"], "study");
        assert_eq!(m["version"], env!("CARGO_PKG_VERSION"));
        let tools: Vec<&str> = m["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert_eq!(tools, ["study_deck", "study_quiz"]);
        assert_eq!(m["commands"], json!(["/study"]));
        assert_eq!(m["hooks"], json!(["prompt/context"]));
        assert_eq!(m["capabilities"], json!(["host.ask"]));
    }

    #[test]
    fn add_creates_the_deck_and_the_card_is_due() {
        let root = tmp("add");
        let r = tool(
            &root,
            "study_deck",
            json!({"action": "add", "deck": "rust", "front": "q?", "back": "a!"}),
        );
        assert!(r["content"].as_str().unwrap().contains("card #1"));
        let due = tool(&root, "study_deck", json!({"action": "due"}));
        assert_eq!(due["content"].as_str().unwrap().contains("\"due_count\":1"), true);
        let deck: Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("study/decks/rust.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(deck["cards"][0]["interval_days"], 1);
    }

    #[test]
    fn answer_doubles_on_correct_and_resets_on_wrong() {
        let root = tmp("ans");
        tool(
            &root,
            "study_deck",
            json!({"action": "add", "deck": "d", "front": "f", "back": "b"}),
        );
        let r = tool(
            &root,
            "study_deck",
            json!({"action": "answer", "deck": "d", "card_id": 1, "correct": true}),
        );
        assert!(r["content"].as_str().unwrap().contains("\"interval_days\":2"));
        let r = tool(
            &root,
            "study_deck",
            json!({"action": "answer", "deck": "d", "card_id": 1, "correct": "wrong"}),
        );
        assert!(r["content"].as_str().unwrap().contains("\"interval_days\":1"));
        // A just-answered card is not due.
        let due = tool(&root, "study_deck", json!({"action": "due", "deck": "d"}));
        assert!(due["content"].as_str().unwrap().contains("\"due_count\":0"));
    }

    #[test]
    fn interval_caps_at_thirty_days() {
        let root = tmp("cap");
        tool(
            &root,
            "study_deck",
            json!({"action": "add", "deck": "d", "front": "f", "back": "b"}),
        );
        let mut last = String::new();
        for _ in 0..6 {
            last = tool(
                &root,
                "study_deck",
                json!({"action": "answer", "deck": "d", "card_id": 1, "correct": true}),
            )["content"]
                .as_str()
                .unwrap()
                .to_string();
        }
        // 1→2→4→8→16→32→30: the sixth correct hits the cap.
        assert!(last.contains("\"interval_days\":30"), "got: {last}");
    }

    #[test]
    fn deck_name_is_sanitized() {
        let root = tmp("safe");
        tool(
            &root,
            "study_deck",
            json!({"action": "add", "deck": "../evil", "front": "f", "back": "b"}),
        );
        assert!(root.join("study/decks/---evil.json").exists());
        assert!(!root.join("evil.json").exists());
    }

    #[test]
    fn study_topic_returns_prompt_and_marks_active() {
        let root = tmp("topic");
        let r = call_at(
            &root,
            "command/run",
            json!({"name": "/study", "argv": ["rust", "lifetimes"]}),
        )
        .unwrap();
        let prompt = r["prompt"].as_str().unwrap();
        assert!(prompt.contains("Socratic tutor for rust lifetimes"));
        assert_eq!(active_topic(&root).unwrap(), "rust lifetimes");
        // Status reports it; end clears it.
        let status = call_at(&root, "command/run", json!({"argv": []})).unwrap();
        assert!(status["text"].as_str().unwrap().contains("rust lifetimes"));
        call_at(&root, "command/run", json!({"argv": ["end"]})).unwrap();
        assert!(active_topic(&root).is_none());
    }

    #[test]
    fn context_injects_persona_and_due_count() {
        let root = tmp("ctx");
        assert_eq!(prompt_context(&root), "");
        tool(
            &root,
            "study_deck",
            json!({"action": "add", "deck": "d", "front": "f", "back": "b"}),
        );
        call_at(&root, "command/run", json!({"argv": ["biology"]})).unwrap();
        let ctx = call_at(&root, "prompt/context", json!({"session": {}})).unwrap();
        let text = ctx["text"].as_str().unwrap();
        assert!(text.contains("Socratic tutor for \"biology\""));
        assert!(text.contains("1 flashcard(s) due"));
    }

    #[test]
    fn quiz_needs_a_question_and_degrades_without_host() {
        let root = tmp("quiz");
        let r = tool(&root, "study_quiz", json!({}));
        assert_eq!(r["is_error"], true);
        let r = tool(
            &root,
            "study_quiz",
            json!({"question": "2+2?", "options": ["3", "4", "5"]}),
        );
        let text = r["content"].as_str().unwrap();
        assert!(text.contains("2+2?"));
        assert!(text.contains("B) 4"));
        assert!(r.get("is_error").is_none());
    }

    #[test]
    fn extract_answer_handles_array_and_object_shapes() {
        assert_eq!(
            extract_answer(&json!({"answers": [{"id": "quiz", "answers": ["4"]}]})).unwrap(),
            "4"
        );
        assert_eq!(
            extract_answer(&json!({"answers": {"quiz": ["a", "b"]}})).unwrap(),
            "a, b"
        );
        assert!(extract_answer(&json!({})).is_none());
    }

    #[test]
    fn unknown_tool_and_method_are_errors() {
        let root = tmp("err");
        let r = tool(&root, "nope", json!({}));
        assert_eq!(r["is_error"], true);
        assert_eq!(
            call_at(&root, "nope", Value::Null).unwrap_err()["code"],
            -32601
        );
    }

    #[test]
    fn shutdown_result_is_empty_object() {
        assert_eq!(
            call_at(&tmp("sd"), "plugin/shutdown", Value::Null).unwrap(),
            json!({})
        );
    }
}
