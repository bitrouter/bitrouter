use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use bitrouter_sdk::language_model::{
    Content, HopOutcome, ObserveHook, Phase, PipelineContext, RequestOutcome, RoutingTarget,
    StreamContext, StreamInterest, StreamPart, Tool,
};
use sha2::{Digest, Sha256};

use crate::eval::settlement::{EvalInvocation, PendingEvalDecisionStore};

const MAX_STREAM_BUFFER_BYTES: usize = 4_096;
const MAX_STREAM_TOOL_CALLS: usize = 32;
const MAX_TOOL_DEFINITIONS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedActionClass {
    ReasonOrPlan,
    InspectOrRead,
    Mutate,
    ExecuteOrTest,
    WaitOrPoll,
    AnswerOrSummarize,
    Unknown,
}

impl ObservedActionClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReasonOrPlan => "reason_or_plan",
            Self::InspectOrRead => "inspect_or_read",
            Self::Mutate => "mutate",
            Self::ExecuteOrTest => "execute_or_test",
            Self::WaitOrPoll => "wait_or_poll",
            Self::AnswerOrSummarize => "answer_or_summarize",
            Self::Unknown => "unknown",
        }
    }

    fn dominance(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::ReasonOrPlan | Self::WaitOrPoll | Self::AnswerOrSummarize => 1,
            Self::InspectOrRead => 2,
            Self::ExecuteOrTest => 3,
            Self::Mutate => 4,
        }
    }

    fn dominant(self, other: Self) -> Self {
        if other.dominance() > self.dominance() {
            other
        } else {
            self
        }
    }

    pub fn is_known(self) -> bool {
        self != Self::Unknown
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "reason_or_plan" => Some(Self::ReasonOrPlan),
            "inspect_or_read" => Some(Self::InspectOrRead),
            "mutate" => Some(Self::Mutate),
            "execute_or_test" => Some(Self::ExecuteOrTest),
            "wait_or_poll" => Some(Self::WaitOrPoll),
            "answer_or_summarize" => Some(Self::AnswerOrSummarize),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PredictionObservation {
    pub observed_action: ObservedActionClass,
}

impl PredictionObservation {
    pub fn new(observed_action: ObservedActionClass) -> Self {
        Self { observed_action }
    }

    pub fn merge(self, other: Self) -> Self {
        Self::new(self.observed_action.dominant(other.observed_action))
    }
}

#[derive(Clone)]
pub struct PredictiveResponseObserver {
    pending: PendingEvalDecisionStore,
    streams: Arc<Mutex<BTreeMap<uuid::Uuid, StreamObservationBuffer>>>,
}

impl PredictiveResponseObserver {
    pub fn new(pending: PendingEvalDecisionStore) -> Self {
        Self {
            pending,
            streams: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn initialize_stream(&self, context: &PipelineContext) {
        let Some(invocation) = context.extension::<EvalInvocation>() else {
            return;
        };
        if invocation.owner_user_id() != context.caller().user_id() {
            return;
        }
        let definitions = context
            .prompt()
            .tools
            .iter()
            .take(MAX_TOOL_DEFINITIONS)
            .map(|tool| {
                (
                    tool_name_digest(tool.name()),
                    classify_tool_definition(tool),
                )
            })
            .collect();
        self.streams
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                invocation.token(),
                StreamObservationBuffer::new(definitions),
            );
    }

    fn observe_stream_part(&self, context: &StreamContext, part: &StreamPart) {
        let Some(invocation) = context.extension::<EvalInvocation>() else {
            return;
        };
        if invocation.owner_user_id() != context.caller.user_id() {
            return;
        }
        let token = invocation.token();
        let terminal = part.is_terminal();
        let observation = {
            let mut streams = self.streams.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(buffer) = streams.get_mut(&token) else {
                return;
            };
            buffer.observe(part);
            terminal.then(|| buffer.finalize())
        };
        if let Some(observation) = observation {
            self.pending
                .observe(&invocation, context.caller.user_id(), observation);
            self.streams
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&token);
        }
    }

    fn clear_stream(&self, invocation: &EvalInvocation) {
        self.streams
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&invocation.token());
    }

    #[cfg(test)]
    pub(crate) fn buffered_request_count(&self) -> usize {
        self.streams
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

#[async_trait]
impl ObserveHook for PredictiveResponseObserver {
    async fn after_phase(&self, phase: Phase, context: &PipelineContext) {
        if phase == Phase::Route {
            self.initialize_stream(context);
        }
    }

    async fn on_hop_end(
        &self,
        context: &PipelineContext,
        _target: &RoutingTarget,
        outcome: HopOutcome<'_>,
    ) {
        if let HopOutcome::Generated(result) = outcome
            && let Some(invocation) = context.extension::<EvalInvocation>()
            && invocation.owner_user_id() == context.caller().user_id()
        {
            let observation = classify_content(&result.result.content, &context.prompt().tools);
            self.pending
                .observe(&invocation, context.caller().user_id(), observation);
            self.clear_stream(&invocation);
        }
    }

    fn stream_interest(&self) -> StreamInterest {
        StreamInterest::none()
            .with_text_delta()
            .with_tool_call_delta()
            .with_finish()
    }

    async fn on_stream_part(&self, context: &StreamContext, part: &StreamPart) {
        self.observe_stream_part(context, part);
    }

    async fn on_request_end(&self, context: &PipelineContext, _outcome: &RequestOutcome) {
        if let Some(invocation) = context.extension::<EvalInvocation>()
            && invocation.owner_user_id() == context.caller().user_id()
        {
            self.clear_stream(&invocation);
        }
    }
}

#[derive(Debug)]
struct StreamToolCall {
    id_digest: [u8; 32],
    name: String,
    arguments: String,
}

#[derive(Debug)]
struct StreamObservationBuffer {
    tool_definitions: BTreeMap<[u8; 32], ObservedActionClass>,
    calls: Vec<StreamToolCall>,
    buffered_bytes: usize,
    saw_text: bool,
    saw_tool: bool,
    dominant_tool_action: ObservedActionClass,
}

impl StreamObservationBuffer {
    fn new(tool_definitions: BTreeMap<[u8; 32], ObservedActionClass>) -> Self {
        Self {
            tool_definitions,
            calls: Vec::new(),
            buffered_bytes: 0,
            saw_text: false,
            saw_tool: false,
            dominant_tool_action: ObservedActionClass::Unknown,
        }
    }

    fn observe(&mut self, part: &StreamPart) {
        match part {
            StreamPart::TextDelta { text } => {
                self.saw_text |= !text.is_empty();
            }
            StreamPart::ToolCallDelta {
                id,
                name,
                arguments,
            } => self.observe_tool_delta(id, name.as_deref(), arguments),
            _ => {}
        }
    }

    fn observe_tool_delta(&mut self, id: &str, name: Option<&str>, arguments: &str) {
        self.saw_tool = true;
        if let Some(name) = name {
            let declared_action = classify_tool_name(name, &self.tool_definitions);
            self.dominant_tool_action = self.dominant_tool_action.dominant(declared_action);
        }
        let id_digest = tool_name_digest(id);
        let call_index = self
            .calls
            .iter()
            .position(|call| call.id_digest == id_digest)
            .or_else(|| {
                if self.calls.len() >= MAX_STREAM_TOOL_CALLS {
                    return None;
                }
                self.calls.push(StreamToolCall {
                    id_digest,
                    name: String::new(),
                    arguments: String::new(),
                });
                self.calls.len().checked_sub(1)
            });
        let Some(call_index) = call_index else {
            return;
        };
        let call = &mut self.calls[call_index];
        if let Some(name) = name {
            append_bounded(&mut call.name, name, &mut self.buffered_bytes);
        }
        append_bounded(&mut call.arguments, arguments, &mut self.buffered_bytes);
        let action = classify_tool_call(&call.name, &call.arguments, &self.tool_definitions);
        self.dominant_tool_action = self.dominant_tool_action.dominant(action);
    }

    fn finalize(&self) -> PredictionObservation {
        let observed_action = if self.saw_tool {
            self.dominant_tool_action
        } else if self.saw_text {
            ObservedActionClass::AnswerOrSummarize
        } else {
            ObservedActionClass::Unknown
        };
        PredictionObservation::new(observed_action)
    }
}

fn append_bounded(target: &mut String, fragment: &str, buffered_bytes: &mut usize) {
    let remaining = MAX_STREAM_BUFFER_BYTES.saturating_sub(*buffered_bytes);
    if remaining == 0 {
        return;
    }
    let mut end = fragment.len().min(remaining);
    while end > 0 && !fragment.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&fragment[..end]);
    *buffered_bytes = buffered_bytes.saturating_add(end);
}

fn classify_content(content: &[Content], tools: &[Tool]) -> PredictionObservation {
    let definitions = tools
        .iter()
        .take(MAX_TOOL_DEFINITIONS)
        .map(|tool| {
            (
                tool_name_digest(tool.name()),
                classify_tool_definition(tool),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut saw_text = false;
    let mut saw_reasoning = false;
    let mut saw_tool = false;
    let mut dominant_tool_action = ObservedActionClass::Unknown;
    for item in content {
        match item {
            Content::Text { text, .. } => saw_text |= !text.is_empty(),
            Content::Reasoning { text, .. } => saw_reasoning |= !text.is_empty(),
            Content::ToolCall {
                name, arguments, ..
            } => {
                saw_tool = true;
                dominant_tool_action = dominant_tool_action.dominant(classify_tool_call(
                    name,
                    arguments,
                    &definitions,
                ));
            }
            _ => {}
        }
    }
    let action = if saw_tool {
        dominant_tool_action
    } else if saw_text {
        ObservedActionClass::AnswerOrSummarize
    } else if saw_reasoning {
        ObservedActionClass::ReasonOrPlan
    } else {
        ObservedActionClass::Unknown
    };
    PredictionObservation::new(action)
}

fn classify_tool_definition(tool: &Tool) -> ObservedActionClass {
    let description = match tool {
        Tool::Function { description, .. } => description.as_deref().unwrap_or_default(),
        Tool::ProviderDefined { id, .. } => id,
    };
    classify_label(tool.name()).dominant(classify_label(description))
}

fn classify_tool_call(
    name: &str,
    arguments: &str,
    definitions: &BTreeMap<[u8; 32], ObservedActionClass>,
) -> ObservedActionClass {
    let declared_action = classify_tool_name(name, definitions);
    if declared_action != ObservedActionClass::Unknown {
        return declared_action;
    }
    if is_command_tool(name)
        && let Some(command) = command_argument(arguments)
    {
        let command_action = classify_command(&command);
        if command_action != ObservedActionClass::Unknown {
            return command_action;
        }
    }
    ObservedActionClass::Unknown
}

fn classify_tool_name(
    name: &str,
    definitions: &BTreeMap<[u8; 32], ObservedActionClass>,
) -> ObservedActionClass {
    let label_action = classify_label(name);
    if label_action != ObservedActionClass::Unknown {
        return label_action;
    }
    definitions
        .get(&tool_name_digest(name))
        .copied()
        .unwrap_or(ObservedActionClass::Unknown)
}

fn classify_label(label: &str) -> ObservedActionClass {
    let normalized = label.to_ascii_lowercase();
    if contains_any(
        &normalized,
        &[
            "apply_patch",
            "patch",
            "edit",
            "write_file",
            "write file",
            "create_file",
            "create file",
            "delete",
            "remove file",
            "rename",
            "move file",
            "modify",
            "update file",
        ],
    ) {
        return ObservedActionClass::Mutate;
    }
    if contains_any(
        &normalized,
        &["test", "check", "lint", "build", "compile", "benchmark"],
    ) {
        return ObservedActionClass::ExecuteOrTest;
    }
    if contains_any(
        &normalized,
        &[
            "read",
            "search",
            "find",
            "grep",
            "glob",
            "list",
            "view",
            "inspect",
            "query",
            "lookup",
            "open file",
        ],
    ) {
        return ObservedActionClass::InspectOrRead;
    }
    if contains_any(
        &normalized,
        &["wait", "poll", "sleep", "watch", "write_stdin"],
    ) {
        return ObservedActionClass::WaitOrPoll;
    }
    ObservedActionClass::Unknown
}

fn classify_command(command: &str) -> ObservedActionClass {
    let command = command.to_ascii_lowercase();
    if contains_any(
        &command,
        &[
            "apply_patch",
            "sed -i",
            "git add",
            "git commit",
            "mkdir ",
            "touch ",
            "rm ",
            "mv ",
            "cp ",
        ],
    ) || command.contains(" > ")
        || command.contains(" >> ")
    {
        return ObservedActionClass::Mutate;
    }
    if contains_any(
        &command,
        &[
            "cargo test",
            "cargo check",
            "cargo clippy",
            "cargo build",
            "pytest",
            "npm test",
            "pnpm test",
            "yarn test",
            "go test",
            "ctest",
        ],
    ) {
        return ObservedActionClass::ExecuteOrTest;
    }
    let trimmed = command.trim_start();
    if [
        "cat ",
        "sed ",
        "rg ",
        "grep ",
        "ls",
        "find ",
        "git diff",
        "git status",
        "git log",
    ]
    .iter()
    .any(|prefix| trimmed.starts_with(prefix))
    {
        return ObservedActionClass::InspectOrRead;
    }
    if ["sleep ", "wait ", "watch ", "tail -f "]
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
    {
        return ObservedActionClass::WaitOrPoll;
    }
    ObservedActionClass::Unknown
}

fn command_argument(arguments: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
    value.as_object()?.iter().find_map(|(key, value)| {
        matches!(key.as_str(), "cmd" | "command" | "script")
            .then(|| value.as_str().map(ToOwned::to_owned))
            .flatten()
    })
}

fn is_command_tool(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    contains_any(
        &name,
        &["bash", "shell", "terminal", "exec", "command", "process"],
    )
}

fn contains_any(value: &str, terms: &[&str]) -> bool {
    terms.iter().any(|term| value.contains(term))
}

fn tool_name_digest(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, PoisonError};

    use bitrouter_sdk::caller::CallerContext;
    use bitrouter_sdk::language_model::types::AuthScheme;
    use bitrouter_sdk::language_model::{
        ApiProtocol, Content, ExecutionResult, FinishReason, GenerateResult, GenerationParams,
        HopOutcome, ObserveHook, Phase, PipelineContext, PipelineRequest, RequestOutcome,
        RoutingTarget, StreamPart, StreamProcessor, Tool,
    };

    use super::{ObservedActionClass, PredictiveResponseObserver};
    use crate::eval::settlement::{EvalInvocation, PendingEvalDecision, PendingEvalDecisionStore};

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[tokio::test]
    async fn response_observer_classifies_nonstream_actions_with_mutation_dominance()
    -> anyhow::Result<()> {
        let cases = [
            (
                "reasoning",
                vec![Content::Reasoning {
                    text: "consider the tradeoffs".into(),
                    provider_metadata: BTreeMap::new(),
                }],
                Vec::new(),
                ObservedActionClass::ReasonOrPlan,
            ),
            (
                "answer",
                vec![Content::Text {
                    text: "done".into(),
                    provider_metadata: BTreeMap::new(),
                }],
                Vec::new(),
                ObservedActionClass::AnswerOrSummarize,
            ),
            (
                "inspect",
                vec![tool_call("read_file", r#"{"path":"src/lib.rs"}"#)],
                Vec::new(),
                ObservedActionClass::InspectOrRead,
            ),
            (
                "mutate",
                vec![tool_call("apply_patch", r#"{"patch":"private"}"#)],
                Vec::new(),
                ObservedActionClass::Mutate,
            ),
            (
                "execute",
                vec![tool_call(
                    "exec_command",
                    r#"{"cmd":"cargo test --all-features"}"#,
                )],
                Vec::new(),
                ObservedActionClass::ExecuteOrTest,
            ),
            (
                "wait",
                vec![tool_call("wait_for_event", r#"{"timeout_ms":1000}"#)],
                Vec::new(),
                ObservedActionClass::WaitOrPoll,
            ),
            (
                "unknown",
                vec![tool_call("opaque_capability", r#"{"secret":"value"}"#)],
                Vec::new(),
                ObservedActionClass::Unknown,
            ),
            (
                "mixed",
                vec![
                    tool_call("read_file", r#"{"path":"src/lib.rs"}"#),
                    tool_call("apply_patch", r#"{"patch":"private"}"#),
                ],
                Vec::new(),
                ObservedActionClass::Mutate,
            ),
            (
                "definition",
                vec![tool_call(
                    "workspace_operation",
                    r#"{"target":"src/lib.rs"}"#,
                )],
                vec![function_tool(
                    "workspace_operation",
                    "Edit or write files in the workspace",
                )],
                ObservedActionClass::Mutate,
            ),
        ];

        for (name, content, tools, expected) in cases {
            let observed = observe_nonstream(name, content, tools).await?;
            assert_eq!(observed, expected, "{name}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn response_observer_reassembles_stream_calls_without_changing_output()
    -> anyhow::Result<()> {
        let (pending, invocation) = pending("stream-fragments");
        let observer = PredictiveResponseObserver::new(pending.clone());
        let prompt = prompt(Vec::new());
        let original_prompt = prompt.clone();
        let context = pipeline_context("stream-fragments", prompt, &invocation);
        observer.after_phase(Phase::Route, &context).await;
        assert_eq!(context.prompt(), &original_prompt);
        let mut processor = StreamProcessor::new(
            Vec::new(),
            vec![Arc::new(observer.clone())],
            context.stream_context(),
        );
        let parts = vec![
            StreamPart::ToolCallDelta {
                id: "call-1".into(),
                name: Some("exec_command".into()),
                arguments: r#"{"cmd":"cargo "#.into(),
            },
            StreamPart::ToolCallDelta {
                id: "call-1".into(),
                name: None,
                arguments: r#"test --all-features"}"#.into(),
            },
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
        ];

        let mut emitted = Vec::new();
        for part in parts.clone() {
            emitted.extend(processor.process_part(part).await?);
        }

        assert_eq!(emitted, parts);
        let decision = pending
            .peek(&invocation, "local")
            .ok_or_else(|| anyhow::anyhow!("pending stream decision missing"))?;
        assert_eq!(
            decision
                .observation
                .map(|observation| observation.observed_action),
            Some(ObservedActionClass::ExecuteOrTest)
        );
        assert!(!format!("{decision:?}").contains("cargo test --all-features"));
        assert_eq!(observer.buffered_request_count(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn response_observer_bounds_stream_fragments_to_4096_bytes() -> anyhow::Result<()> {
        let (pending, invocation) = pending("stream-bound");
        let observer = PredictiveResponseObserver::new(pending.clone());
        let context = pipeline_context("stream-bound", prompt(Vec::new()), &invocation);
        observer.after_phase(Phase::Route, &context).await;
        let stream = context.stream_context();
        observer
            .on_stream_part(
                &stream,
                &StreamPart::ToolCallDelta {
                    id: "call-1".into(),
                    name: Some("exec_command".into()),
                    arguments: "x".repeat(8_192),
                },
            )
            .await;
        let buffered = observer
            .streams
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&invocation.token())
            .map(|buffer| buffer.buffered_bytes)
            .ok_or_else(|| anyhow::anyhow!("stream buffer missing"))?;
        assert!(buffered <= 4_096);

        observer
            .on_stream_part(
                &stream,
                &StreamPart::Finish {
                    reason: FinishReason::Stop,
                },
            )
            .await;

        assert_eq!(
            pending
                .peek(&invocation, "local")
                .and_then(|decision| decision.observation)
                .map(|observation| observation.observed_action),
            Some(ObservedActionClass::Unknown)
        );
        assert_eq!(observer.buffered_request_count(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn response_observer_keeps_mutation_dominance_after_thirty_two_read_calls()
    -> anyhow::Result<()> {
        let (pending, invocation) = pending("stream-call-limit");
        let observer = PredictiveResponseObserver::new(pending.clone());
        let context = pipeline_context("stream-call-limit", prompt(Vec::new()), &invocation);
        observer.after_phase(Phase::Route, &context).await;
        let stream = context.stream_context();
        for index in 0..32 {
            observer
                .on_stream_part(
                    &stream,
                    &StreamPart::ToolCallDelta {
                        id: format!("read-{index}"),
                        name: Some("read_file".into()),
                        arguments: r#"{"path":"src/lib.rs"}"#.into(),
                    },
                )
                .await;
        }
        observer
            .on_stream_part(
                &stream,
                &StreamPart::ToolCallDelta {
                    id: "mutation-after-limit".into(),
                    name: Some("apply_patch".into()),
                    arguments: r#"{"patch":"private"}"#.into(),
                },
            )
            .await;
        observer
            .on_stream_part(
                &stream,
                &StreamPart::Finish {
                    reason: FinishReason::Stop,
                },
            )
            .await;

        assert_eq!(
            pending
                .peek(&invocation, "local")
                .and_then(|decision| decision.observation)
                .map(|observation| observation.observed_action),
            Some(ObservedActionClass::Mutate)
        );
        Ok(())
    }

    #[tokio::test]
    async fn response_observer_classifies_mutation_after_raw_byte_limit() -> anyhow::Result<()> {
        let (pending, invocation) = pending("stream-byte-limit-mutation");
        let observer = PredictiveResponseObserver::new(pending.clone());
        let context = pipeline_context(
            "stream-byte-limit-mutation",
            prompt(Vec::new()),
            &invocation,
        );
        observer.after_phase(Phase::Route, &context).await;
        let stream = context.stream_context();
        observer
            .on_stream_part(
                &stream,
                &StreamPart::ToolCallDelta {
                    id: "unknown-large".into(),
                    name: Some("opaque_capability".into()),
                    arguments: "x".repeat(8_192),
                },
            )
            .await;
        observer
            .on_stream_part(
                &stream,
                &StreamPart::ToolCallDelta {
                    id: "mutation-after-bytes".into(),
                    name: Some("apply_patch".into()),
                    arguments: r#"{"patch":"private"}"#.into(),
                },
            )
            .await;
        let buffered = observer
            .streams
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&invocation.token())
            .map(|buffer| buffer.buffered_bytes)
            .ok_or_else(|| anyhow::anyhow!("stream buffer missing"))?;
        assert!(buffered <= 4_096);
        observer
            .on_stream_part(
                &stream,
                &StreamPart::Finish {
                    reason: FinishReason::Stop,
                },
            )
            .await;

        assert_eq!(
            pending
                .peek(&invocation, "local")
                .and_then(|decision| decision.observation)
                .map(|observation| observation.observed_action),
            Some(ObservedActionClass::Mutate)
        );
        Ok(())
    }

    #[tokio::test]
    async fn response_observer_teardown_clears_abandoned_stream_fragments() -> anyhow::Result<()> {
        let (pending, invocation) = pending("stream-abandoned");
        let observer = PredictiveResponseObserver::new(pending.clone());
        let context = pipeline_context("stream-abandoned", prompt(Vec::new()), &invocation);
        observer.after_phase(Phase::Route, &context).await;
        let stream = context.stream_context();
        observer
            .on_stream_part(
                &stream,
                &StreamPart::ToolCallDelta {
                    id: "private-call".into(),
                    name: Some("exec_command".into()),
                    arguments: r#"{"cmd":"private-command"}"#.into(),
                },
            )
            .await;

        observer
            .on_request_end(&context, &RequestOutcome::ClientDisconnected)
            .await;

        assert_eq!(observer.buffered_request_count(), 0);
        assert!(
            pending
                .peek(&invocation, "local")
                .and_then(|decision| decision.observation)
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn response_observer_completed_teardown_clears_unobserved_terminal_state()
    -> anyhow::Result<()> {
        let (pending, invocation) = pending("stream-completed-without-terminal");
        let observer = PredictiveResponseObserver::new(pending.clone());
        let context = pipeline_context(
            "stream-completed-without-terminal",
            prompt(Vec::new()),
            &invocation,
        );
        observer.after_phase(Phase::Route, &context).await;
        observer
            .on_stream_part(
                &context.stream_context(),
                &StreamPart::ToolCallDelta {
                    id: "removed-terminal".into(),
                    name: Some("exec_command".into()),
                    arguments: r#"{"cmd":"private-command"}"#.into(),
                },
            )
            .await;

        observer
            .on_request_end(&context, &RequestOutcome::Completed)
            .await;

        assert_eq!(observer.buffered_request_count(), 0);
        assert!(
            pending
                .peek(&invocation, "local")
                .and_then(|decision| decision.observation)
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn response_observer_isolates_same_external_id_across_invocations_and_callers()
    -> anyhow::Result<()> {
        let pending = PendingEvalDecisionStore::default();
        let first_invocation = EvalInvocation::new("owner-a");
        let second_invocation = EvalInvocation::new("owner-b");
        pending.insert(
            &first_invocation,
            pending_decision("shared-request", "decision-a"),
        );
        pending.insert(
            &second_invocation,
            pending_decision("shared-request", "decision-b"),
        );
        let observer = PredictiveResponseObserver::new(pending.clone());
        let first = pipeline_context_for(
            "shared-request",
            CallerContext::new("key-a", "owner-a"),
            prompt(Vec::new()),
            &first_invocation,
        );
        let second = pipeline_context_for(
            "shared-request",
            CallerContext::new("key-b", "owner-b"),
            prompt(Vec::new()),
            &second_invocation,
        );
        observer.after_phase(Phase::Route, &first).await;
        observer.after_phase(Phase::Route, &second).await;

        for (stream, part) in [
            (
                first.stream_context(),
                StreamPart::ToolCallDelta {
                    id: "call-a".into(),
                    name: Some("apply_patch".into()),
                    arguments: r#"{"patch":"private-a"}"#.into(),
                },
            ),
            (
                second.stream_context(),
                StreamPart::ToolCallDelta {
                    id: "call-b".into(),
                    name: Some("read_file".into()),
                    arguments: r#"{"path":"private-b"}"#.into(),
                },
            ),
        ] {
            observer.on_stream_part(&stream, &part).await;
            observer
                .on_stream_part(
                    &stream,
                    &StreamPart::Finish {
                        reason: FinishReason::Stop,
                    },
                )
                .await;
        }

        assert_eq!(
            pending
                .peek(&first_invocation, "owner-a")
                .and_then(|decision| decision.observation)
                .map(|observation| observation.observed_action),
            Some(ObservedActionClass::Mutate)
        );
        assert_eq!(
            pending
                .peek(&second_invocation, "owner-b")
                .and_then(|decision| decision.observation)
                .map(|observation| observation.observed_action),
            Some(ObservedActionClass::InspectOrRead)
        );
        assert!(pending.peek(&first_invocation, "owner-b").is_none());
        assert!(pending.take(&first_invocation, "owner-a").is_some());
        assert!(pending.peek(&second_invocation, "owner-b").is_some());
        assert_eq!(observer.buffered_request_count(), 0);
        Ok(())
    }

    async fn observe_nonstream(
        request_id: &str,
        content: Vec<Content>,
        tools: Vec<Tool>,
    ) -> anyhow::Result<ObservedActionClass> {
        let (pending, invocation) = pending(request_id);
        let observer = PredictiveResponseObserver::new(pending.clone());
        let context = pipeline_context(request_id, prompt(tools), &invocation);
        let result = ExecutionResult {
            provider_id: "provider".into(),
            model_id: "model".into(),
            account_label: None,
            result: GenerateResult {
                content,
                usage: None,
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_details: None,
                provider_metadata: BTreeMap::new(),
            },
            request_duration_ms: 1,
            upstream_duration_ms: Some(1),
            server_tool_calls: Vec::new(),
        };
        observer
            .on_hop_end(&context, &target(), HopOutcome::Generated(&result))
            .await;
        pending
            .peek(&invocation, "local")
            .and_then(|decision| decision.observation)
            .map(|observation| observation.observed_action)
            .ok_or_else(|| anyhow::anyhow!("observation missing for {request_id}"))
    }

    fn pending(request_id: &str) -> (PendingEvalDecisionStore, EvalInvocation) {
        let pending = PendingEvalDecisionStore::default();
        let invocation = EvalInvocation::new("local");
        pending.insert(
            &invocation,
            pending_decision(request_id, &format!("decision-{request_id}")),
        );
        (pending, invocation)
    }

    fn pending_decision(request_id: &str, decision_id: &str) -> PendingEvalDecision {
        PendingEvalDecision {
            request_id: request_id.into(),
            decision_id: decision_id.into(),
            policy: "auto:cost".into(),
            policy_digest: DIGEST.into(),
            request_key: "agent_trace/v2|edit|normal".into(),
            selected_tier: "economy".into(),
            baseline_tier: Some("strong".into()),
            preset: Some("auto:cost".into()),
            holdout: false,
            predicted_role: Some("implement".into()),
            predicted_action: Some("mutate".into()),
            prediction_confidence_ppm: Some(800_000),
            observation: None,
            observed_at: "2026-08-08T00:00:00Z".into(),
        }
    }

    fn pipeline_context(
        request_id: &str,
        prompt: bitrouter_sdk::language_model::Prompt,
        invocation: &EvalInvocation,
    ) -> PipelineContext {
        pipeline_context_for(request_id, CallerContext::local(), prompt, invocation)
    }

    fn pipeline_context_for(
        request_id: &str,
        caller: CallerContext,
        prompt: bitrouter_sdk::language_model::Prompt,
        invocation: &EvalInvocation,
    ) -> PipelineContext {
        let mut context = PipelineContext::new(PipelineRequest {
            request_id: request_id.into(),
            model: "model".into(),
            caller,
            headers: http::HeaderMap::new(),
            prompt,
            inbound_protocol: Some(ApiProtocol::Responses),
        });
        context.emit(invocation.clone());
        context.insert_extension(Arc::new(invocation.clone()));
        context
    }

    fn prompt(tools: Vec<Tool>) -> bitrouter_sdk::language_model::Prompt {
        bitrouter_sdk::language_model::Prompt {
            model: "model".into(),
            system: None,
            system_provider_metadata: BTreeMap::new(),
            messages: Vec::new(),
            tools,
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    fn tool_call(name: &str, arguments: &str) -> Content {
        Content::ToolCall {
            id: format!("call-{name}"),
            name: name.into(),
            arguments: arguments.into(),
            provider_executed: false,
            dynamic: false,
            provider_metadata: BTreeMap::new(),
        }
    }

    fn function_tool(name: &str, description: &str) -> Tool {
        Tool::Function {
            name: name.into(),
            description: Some(description.into()),
            parameters: serde_json::json!({"type": "object"}),
            strict: None,
            provider_metadata: BTreeMap::new(),
        }
    }

    fn target() -> RoutingTarget {
        RoutingTarget {
            provider_name: "provider".into(),
            service_id: "model".into(),
            api_base: "https://example.invalid".into(),
            api_key: String::new(),
            api_protocol: ApiProtocol::Responses,
            chat_token_limit_field: None,
            chat_supports_store: None,
            chat_supports_stream_options: None,
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: AuthScheme::XApiKey,
        }
    }
}
