//! Baseline fixture authoring/replay only; no renderer logic is implemented here.
//! Usage: cargo run --offline --locked --jobs 2 -- <new-output-directory>
//! Outputs are UTF-8 bytes from public baseline helpers, without normalization.
//! ACP input construction uses the pinned SDK rather than a handwritten wire schema:
//! https://docs.rs/agent-client-protocol-schema/1.10.2/agent_client_protocol_schema/v1/index.html

use std::error::Error;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use agent_client_protocol_schema::v1::{
    AudioContent, Content, ContentBlock, ContentChunk, Cost, Diff, EmbeddedResource,
    EmbeddedResourceResource, ImageContent, MessageId, Plan, PlanEntry, PlanEntryPriority,
    PlanEntryStatus, ResourceLink, SessionUpdate, Terminal, TextContent, TextResourceContents,
    ToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    ToolKind, UsageUpdate,
};
use bitrouter_tui::{log_tail, plain};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Serialize, Deserialize)]
struct Sequence {
    format_version: u32,
    id: String,
    coverage: Vec<String>,
    steps: Vec<Step>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Step {
    Update { update: Box<SessionUpdate> },
    Checkpoint { id: String, reason: String },
}

#[derive(Serialize, Deserialize)]
struct TailInput {
    id: String,
    path: String,
    log: String,
    max_lines: usize,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Constructor {
    LineRaw,
    LineFromString,
    LineFromStr,
    SpanRaw,
    SpanStyled,
}

#[derive(Serialize, Deserialize)]
struct ConstructorInput {
    id: String,
    constructor: Constructor,
    text: String,
}

fn update(update: SessionUpdate) -> Step {
    Step::Update {
        update: Box::new(update),
    }
}

fn checkpoint(id: &str, reason: &str) -> Step {
    Step::Checkpoint {
        id: id.into(),
        reason: reason.into(),
    }
}

fn chunk(text: &str, key: Option<&str>) -> ContentChunk {
    let mut chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
    chunk.message_id = key.map(|key| MessageId::from(key.to_string()));
    chunk
}

fn user(text: &str, key: &str) -> Step {
    update(SessionUpdate::UserMessageChunk(chunk(text, Some(key))))
}

fn agent(text: &str, key: Option<&str>) -> Step {
    update(SessionUpdate::AgentMessageChunk(chunk(text, key)))
}

fn thought(text: &str, key: Option<&str>) -> Step {
    update(SessionUpdate::AgentThoughtChunk(chunk(text, key)))
}

fn text_content(text: impl Into<String>) -> ToolCallContent {
    ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(text))))
}

fn tool(call: ToolCall) -> Step {
    update(SessionUpdate::ToolCall(call))
}

fn patch(id: &str, fields: ToolCallUpdateFields) -> Step {
    update(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        ToolCallId::new(id),
        fields,
    )))
}

fn usage(amount: Option<f64>, marker: Option<Value>) -> Step {
    let mut usage = UsageUpdate::new(1500, 200000);
    usage.cost = amount.map(|amount| Cost::new(amount, "USD"));
    usage.meta = marker.map(|marker| {
        let mut meta = serde_json::Map::new();
        meta.insert(bitrouter_tui::cost::COST_PROVENANCE_META_KEY.into(), marker);
        meta
    });
    update(SessionUpdate::UsageUpdate(usage))
}

fn sequence(id: &str, coverage: &[&str], steps: Vec<Step>) -> Sequence {
    Sequence {
        format_version: 1,
        id: id.into(),
        coverage: coverage.iter().map(|item| (*item).into()).collect(),
        steps,
    }
}

fn markdown_sequence() -> Sequence {
    let prose = concat!(
        "## Workspace review\n\n",
        "**Parser** and *writer* share `config.rs`. Read [the guide](https://example.invalid/guide) ",
        "before changing settings; this deliberately long sentence crosses the eighty-column ",
        "boundary with 中文, café, e\u{301}, and 👩‍💻 intact in the recorded baseline.\n\n",
        "- Inspect files\n- Preserve output\n  - Keep nested notes\n",
        "- [x] Read config\n- [ ] Review changes\n\n",
        "> A quoted observation with **emphasis**.\n\n",
        "| Repository | Role |\n|---|---|\n",
        "| [bitrouter](/transcript-fixture/README.md) | Routes requests |\n",
        "| writer | Writes output |\n\n",
        "---\n\n",
        "```sh\n\techo '**literal** 你好👩‍💻'\n",
        "  printf '%s\\n' 'a literal command whose whitespace, quotes, and very long argument must survive wrapping unchanged'\n",
        "  echo '中文👩‍💻中文👩‍💻中文👩‍💻中文👩‍💻中文👩‍💻中文👩‍💻中文👩‍💻中文👩‍💻中文👩‍💻中文👩‍💻中文👩‍💻中文👩‍💻'\n",
        "```\n"
    );
    let oversized = format!(
        "| Repository | Detail |\n|---|---|\n| [bitrouter](/transcript-fixture/README.md) | {} |\n| writer | Keep **every** field |\n\n```sh\nprintf '**unfinished** 你好'",
        "oversized-table-token-".repeat(6)
    );
    sequence(
        "markdown-unicode",
        &[
            "Three settled turns, repeated empty reads, keyed and unkeyed chunks",
            "Literal multiline user text and trailing spaces",
            "Prose, emphasis, links, nested/task lists, blockquote, horizontal rule",
            "Fitting table and oversized-table labeled fallback",
            "CJK, composed/decomposed accents, ZWJ emoji, tabs, framed long code",
            "Unclosed code fence at settlement and inline formatting completed across chunks",
        ],
        vec![
            checkpoint("empty", "Read an empty transcript before the first turn"),
            user(
                "Review **literal input**.  \nKeep the second line.",
                "chat:prompt:1",
            ),
            agent("## Work", Some("answer:1")),
            agent(&prose[7..], None),
            checkpoint("turn-1", "First prompt settled after all chunks arrived"),
            checkpoint("turn-1-repeat", "No updates since the previous read"),
            user(
                "Show the long table and an unfinished command.",
                "chat:prompt:2",
            ),
            agent(&oversized, Some("answer:2")),
            checkpoint(
                "turn-2",
                "Second prompt settled with an unclosed code fence",
            ),
            checkpoint("turn-2-repeat", "No updates since the previous read"),
            user("Finish the review.", "chat:prompt:3"),
            agent("**All", Some("answer:3")),
            agent(" done**.\n\nNothing else changed.\n", None),
            checkpoint(
                "turn-3",
                "Third prompt settled after completing inline emphasis",
            ),
        ],
    )
}

fn tools_sequence() -> Sequence {
    let old = (1..=60)
        .map(|n| format!("setting_{n:02}=old\n"))
        .collect::<String>();
    let new = old
        .replace("setting_15=old", "setting_15=new")
        .replace("setting_45=old", "setting_45=新值");
    let output = (1..=43)
        .map(|n| {
            if n == 2 {
                format!("literal output: {}\n", "unwrapped-".repeat(12))
            } else {
                format!("check {n:02}: ok\n")
            }
        })
        .collect::<String>();
    let generated = (1..=35)
        .map(|n| format!("generated_{n:02}\n"))
        .collect::<String>();
    let plan = |done| {
        SessionUpdate::Plan(Plan::new(vec![
            PlanEntry::new(
                "Inspect the configuration",
                PlanEntryPriority::High,
                if done {
                    PlanEntryStatus::Completed
                } else {
                    PlanEntryStatus::InProgress
                },
            ),
            PlanEntry::new(
                "Apply the edit",
                PlanEntryPriority::Medium,
                if done {
                    PlanEntryStatus::Completed
                } else {
                    PlanEntryStatus::Pending
                },
            ),
            PlanEntry::new(
                "Review optional cleanup",
                PlanEntryPriority::Low,
                PlanEntryStatus::Pending,
            ),
        ]))
    };
    sequence(
        "tools-diffs-reasoning",
        &[
            "Two settled turns and tool pending/in_progress/completed/failed patches",
            "Plan replacement in first-seen order; message and tool reasoning",
            "Shell argv extraction for a Read call; literal Execute frame",
            "Forty-source-line tool cap, counted remainder, overwide literal output",
            "Separated diff hunks with omitted unchanged lines; new-file diff height cap",
            "Image, audio and resource descriptions; terminal metadata omission",
            "Unchanged diff; untitled failed tool id fallback; CRLF content",
        ],
        vec![
            user("Inspect and update the configuration.", "chat:prompt:1"),
            update(plan(false)),
            thought(
                "**Checking** the current settings before editing. ",
                Some("thought:1"),
            ),
            thought("中文 notes stay separate from the final answer.", None),
            tool(
                ToolCall::new("read:1", "Inspect configuration")
                    .kind(ToolKind::Read)
                    .status(ToolCallStatus::Pending)
                    .raw_input(json!({"command": ["/bin/bash", "-lc", "sed -n '1,4p' config.rs"]})),
            ),
            patch(
                "read:1",
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::InProgress)
                    .content(vec![text_content("temporary partial result")]),
            ),
            patch(
                "read:1",
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Completed)
                    .content(vec![
                        text_content("setting_01=old\nsetting_02=old\n"),
                        ToolCallContent::Terminal(Terminal::new("terminal-handle-must-not-appear")),
                    ]),
            ),
            tool(
                ToolCall::new("think:1", "Choose the smallest edit")
                    .kind(ToolKind::Think)
                    .status(ToolCallStatus::Completed)
                    .content(vec![text_content(
                        "Use **two** small changes.\n\n- Keep other settings\n- Explain the diff",
                    )]),
            ),
            tool(
                ToolCall::new("edit:1", "Edit config.rs")
                    .kind(ToolKind::Edit)
                    .status(ToolCallStatus::Pending),
            ),
            patch(
                "edit:1",
                ToolCallUpdateFields::new().status(ToolCallStatus::InProgress),
            ),
            patch(
                "edit:1",
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Completed)
                    .content(vec![ToolCallContent::Diff(
                        Diff::new("/transcript-fixture/project/config.rs", new).old_text(old),
                    )]),
            ),
            tool(
                ToolCall::new("execute:1", "Run checks")
                    .kind(ToolKind::Execute)
                    .status(ToolCallStatus::Pending)
                    .raw_input(json!({"cmd":"printf '%s\\n' '**literal**'; check-config --all"})),
            ),
            patch(
                "execute:1",
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Completed)
                    .content(vec![text_content(output)]),
            ),
            update(plan(true)),
            agent(
                "The two settings are updated; all **43** checks passed.",
                Some("answer:1"),
            ),
            checkpoint(
                "turn-1",
                "All tool and plan patches settled before the first emission",
            ),
            checkpoint(
                "turn-1-repeat",
                "No updates; completed cards must not be duplicated",
            ),
            user(
                "Create the report and inspect the attachments.",
                "chat:prompt:2",
            ),
            tool(
                ToolCall::new("create:1", "Create report.txt")
                    .kind(ToolKind::Edit)
                    .status(ToolCallStatus::Completed)
                    .content(vec![ToolCallContent::Diff(Diff::new(
                        "/transcript-fixture/project/report.txt",
                        generated,
                    ))]),
            ),
            tool(
                ToolCall::new("same:1", "Verify unchanged note")
                    .kind(ToolKind::Edit)
                    .status(ToolCallStatus::Completed)
                    .content(vec![ToolCallContent::Diff(
                        Diff::new("/transcript-fixture/project/note.txt", "same\n")
                            .old_text("same\n".to_string()),
                    )]),
            ),
            tool(
                ToolCall::new("fetch:1", "Inspect attachments")
                    .kind(ToolKind::Fetch)
                    .status(ToolCallStatus::Completed)
                    .content(vec![
                        ToolCallContent::Content(Content::new(ContentBlock::Image(
                            ImageContent::new("AAAA", "image/png"),
                        ))),
                        ToolCallContent::Content(Content::new(ContentBlock::Audio(
                            AudioContent::new("AAAA", "audio/wav"),
                        ))),
                        ToolCallContent::Content(Content::new(ContentBlock::ResourceLink(
                            ResourceLink::new(
                                "report",
                                "file:///transcript-fixture/project/report.txt",
                            ),
                        ))),
                        ToolCallContent::Content(Content::new(ContentBlock::Resource(
                            EmbeddedResource::new(EmbeddedResourceResource::TextResourceContents(
                                TextResourceContents::new(
                                    "resource body is not inline output",
                                    "file:///transcript-fixture/project/note.txt",
                                ),
                            )),
                        ))),
                    ]),
            ),
            tool(
                ToolCall::new("search:failed", "")
                    .kind(ToolKind::Search)
                    .status(ToolCallStatus::Pending),
            ),
            patch(
                "search:failed",
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Failed)
                    .content(vec![text_content(
                        "missing optional index\r\nretry later\r\n",
                    )]),
            ),
            agent(
                "The report is ready. The optional search **failed**.",
                Some("answer:2"),
            ),
            checkpoint("turn-2", "Second turn settled, including the failed tool"),
            checkpoint("turn-2-repeat", "No new rows after settlement"),
        ],
    )
}

fn cost_sequence() -> Sequence {
    sequence(
        "cost-omission",
        &[
            "Two settled turns with absent, harness, router, unknown and malformed provenance",
            "Usage-only checkpoints are empty and never add a cost footer",
            "Actual observed zero remains omitted, just like an absent cost",
        ],
        vec![
            usage(None, None),
            checkpoint("absent-before-turn", "Usage has no document entry"),
            user("Explain the first result.", "chat:prompt:1"),
            agent("First result: **ready**.", Some("answer:1")),
            usage(Some(1.32), None),
            checkpoint(
                "turn-1-harness-cost",
                "Settled turn with agent-reported cost; no footer",
            ),
            usage(Some(0.42), Some(json!("router"))),
            checkpoint(
                "router-cost-only",
                "Only the cumulative attributed cost changed",
            ),
            usage(Some(9.99), Some(json!("future-provenance"))),
            checkpoint(
                "unknown-cost-only",
                "Unknown cost provenance must add no document rows",
            ),
            usage(Some(9.99), Some(json!(7))),
            checkpoint(
                "malformed-cost-only",
                "Non-string provenance must add no document rows",
            ),
            usage(None, Some(json!("router"))),
            checkpoint(
                "missing-figure-only",
                "Marker without a figure must add no document rows",
            ),
            user("Explain the second result.", "chat:prompt:2"),
            agent("Second result: ready too.", Some("answer:2")),
            usage(Some(0.0), Some(json!("router"))),
            checkpoint(
                "turn-2-zero-cost",
                "Settled turn with explicitly measured zero; still no footer",
            ),
            checkpoint(
                "turn-2-repeat",
                "Repeated read after the last turn is empty",
            ),
        ],
    )
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?
        .write_all(bytes)?;
    Ok(())
}

fn json_bytes(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn capture_sequence(directory: &Path, authored: Sequence) -> Result<Value> {
    let input_name = format!("{}.input.json", authored.id);
    let expected_name = format!("{}.expected.txt", authored.id);
    write_new(&directory.join(&input_name), &json_bytes(&authored)?)?;
    // Replay serialized inputs, not the constructor objects, to prove the input format.
    let input: Sequence = serde_json::from_slice(&fs::read(directory.join(&input_name))?)?;
    let mut transcript = plain::Transcript::default();
    let mut output = Vec::new();
    let mut checkpoints = Vec::new();
    for (step_index, step) in input.steps.into_iter().enumerate() {
        match step {
            Step::Update { update } => transcript.apply(*update),
            Step::Checkpoint { id, reason } => {
                let start = output.len();
                let rows = transcript.unwritten();
                plain::write(&mut output, &rows)?;
                checkpoints.push(json!({
                    "id": id, "reason": reason, "step_index": step_index,
                    "byte_start": start, "byte_end": output.len(), "rows": rows.len(),
                    "sha256": sha256(&output[start..]),
                }));
            }
        }
    }
    write_new(&directory.join(&expected_name), &output)?;
    Ok(
        json!({"id": input.id, "driver": "plain_transcript", "input": input_name,
        "expected": expected_name, "checkpoints": checkpoints}),
    )
}

fn capture_tails(directory: &Path) -> Result<Value> {
    let long = (1..=24)
        .map(|n| format!("trace {n:02}: checking configuration\r\n"))
        .collect::<String>()
        + "agent failed: invalid setting 中文\r\n";
    let inputs = vec![
        TailInput {
            id: "long".into(),
            path: "/transcript-fixture/logs/session.log".into(),
            log: long,
            max_lines: log_tail::TAIL_LINES,
        },
        TailInput {
            id: "short".into(),
            path: "/transcript-fixture/logs/short.log".into(),
            log: "connecting\nagent failed: missing config".into(),
            max_lines: log_tail::TAIL_LINES,
        },
        TailInput {
            id: "empty".into(),
            path: "/transcript-fixture/logs/empty.log".into(),
            log: String::new(),
            max_lines: log_tail::TAIL_LINES,
        },
    ];
    let input_name = "failure-tails.input.json";
    let expected_name = "failure-tails.expected.txt";
    write_new(&directory.join(input_name), &json_bytes(&inputs)?)?;
    let replay: Vec<TailInput> = serde_json::from_slice(&fs::read(directory.join(input_name))?)?;
    let mut output = Vec::new();
    let mut captures = Vec::new();
    for input in replay {
        let start = output.len();
        let rows = log_tail::render(Path::new(&input.path), &input.log, input.max_lines);
        plain::write(&mut output, &rows)?;
        captures.push(
            json!({"id": input.id, "byte_start": start, "byte_end": output.len(),
            "rows": rows.len(), "sha256": sha256(&output[start..])}),
        );
    }
    write_new(&directory.join(expected_name), &output)?;
    Ok(
        json!({"id":"failure-tails", "driver":"log_tail_then_plain_write",
        "input":input_name, "expected":expected_name, "captures":captures}),
    )
}

fn capture_constructors(directory: &Path) -> Result<Value> {
    let mut inputs = Vec::new();
    for (name, constructor) in [
        ("line-raw", Constructor::LineRaw),
        ("line-from-string", Constructor::LineFromString),
        ("line-from-str", Constructor::LineFromStr),
        ("span-raw", Constructor::SpanRaw),
        ("span-styled", Constructor::SpanStyled),
    ] {
        for (case, text) in [
            (
                "mixed",
                "first\nsecond\r\n\n中文\t👩‍💻\u{2028}next\u{2029}end\rsolo\n",
            ),
            ("empty", ""),
            ("newlines", "\n\r\n\n"),
        ] {
            inputs.push(ConstructorInput {
                id: format!("{name}-{case}"),
                constructor,
                text: text.into(),
            });
        }
    }
    let input_name = "line-constructors.input.json";
    let expected_name = "line-constructors.expected.txt";
    write_new(&directory.join(input_name), &json_bytes(&inputs)?)?;
    let replay: Vec<ConstructorInput> =
        serde_json::from_slice(&fs::read(directory.join(input_name))?)?;
    let mut output = Vec::new();
    let mut captures = Vec::new();
    for input in replay {
        // The baseline writer requires Line<'static>. This one-shot authoring
        // process retains fifteen tiny inputs to exercise the borrowed constructor
        // itself, without reimplementing its conversion to owned spans.
        let text: &'static str = Box::leak(input.text.into_boxed_str());
        // Use the pinned dependency's actual constructors, not copied logic:
        // https://docs.rs/ratatui/0.29.0/ratatui/text/struct.Line.html#method.raw
        // https://docs.rs/ratatui/0.29.0/ratatui/text/struct.Span.html#method.raw
        let row = match input.constructor {
            Constructor::LineRaw => Line::raw(text.to_string()),
            Constructor::LineFromString => Line::from(text.to_string()),
            Constructor::LineFromStr => Line::from(text),
            Constructor::SpanRaw => Line::from(Span::raw(text.to_string())),
            Constructor::SpanStyled => Line::from(Span::styled(
                text.to_string(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
        };
        let start = output.len();
        plain::write(&mut output, &[row])?;
        captures.push(json!({
            "id": input.id, "byte_start": start, "byte_end": output.len(),
            "rows": 1, "lf_bytes": output[start..].iter().filter(|byte| **byte == b'\n').count(),
            "sha256": sha256(&output[start..]),
        }));
    }
    write_new(&directory.join(expected_name), &output)?;
    Ok(json!({
        "id": "line-constructors", "driver": "line_constructors_then_plain_write",
        "input": input_name, "expected": expected_name, "captures": captures,
    }))
}

fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let directory = args
        .next()
        .ok_or_else(|| io::Error::other("expected one new output directory"))?;
    if args.next().is_some() {
        return Err(io::Error::other("expected exactly one output directory").into());
    }
    let directory = Path::new(&directory);
    fs::create_dir(directory)?;
    let mut captures = Vec::new();
    for input in [markdown_sequence(), tools_sequence(), cost_sequence()] {
        captures.push(capture_sequence(directory, input)?);
    }
    captures.push(capture_tails(directory)?);
    captures.push(capture_constructors(directory)?);
    write_new(
        &directory.join("captures.json"),
        &json_bytes(&json!({
            "format_version": 1,
            "baseline_commit": "9956a427bd5d4316b3bf2359f88e293b158781d5",
            "piped_extent": {"width": plain::PIPED.width, "height": plain::PIPED.height},
            "encoding": "UTF-8; LF added by plain::write; no trimming or normalization",
            "ranges": "Zero-based byte offsets, inclusive start and exclusive end, into each complete expected file",
            "replay": "New Transcript per sequence. Apply update steps; call unwritten exactly at checkpoint steps and serialize using plain::write. Compare every slice, including empty ones. No finish_stream, implicit checkpoint, footer or outer wrap.",
            "captures": captures,
        }))?,
    )?;
    println!(
        "Captured three transcript sequences, three helper tails and fifteen constructor cases in {}",
        directory.display()
    );
    Ok(())
}
