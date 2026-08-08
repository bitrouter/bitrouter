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
const COMMAND_TOOL_MARKERS: [&str; 6] = ["bash", "shell", "terminal", "exec", "command", "process"];
const MUTATING_COMMAND_MARKERS: [&str; 11] = [
    "apply_patch",
    "sed -i",
    "git add",
    "git commit",
    "mkdir ",
    "touch ",
    "rm ",
    "mv ",
    "cp ",
    " > ",
    " >> ",
];

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
    command_classifier: StreamingCommandClassifier,
}

#[derive(Debug)]
struct OverflowToolCall {
    id_digest: [u8; 32],
    command_classifier: StreamingCommandClassifier,
}

#[derive(Debug)]
struct StreamObservationBuffer {
    tool_definitions: BTreeMap<[u8; 32], ObservedActionClass>,
    calls: Vec<StreamToolCall>,
    buffered_bytes: usize,
    saw_text: bool,
    saw_tool: bool,
    dominant_tool_action: ObservedActionClass,
    overflow_call: Option<OverflowToolCall>,
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
            overflow_call: None,
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
                    command_classifier: StreamingCommandClassifier::new(),
                });
                self.calls.len().checked_sub(1)
            });
        let Some(call_index) = call_index else {
            let (mutates, ambiguous_eviction) = {
                let overflow = self.overflow_call.get_or_insert_with(|| OverflowToolCall {
                    id_digest,
                    command_classifier: StreamingCommandClassifier::new(),
                });
                let mut ambiguous_eviction = false;
                if overflow.id_digest != id_digest {
                    ambiguous_eviction = overflow.command_classifier.ambiguous_if_evicted();
                    *overflow = OverflowToolCall {
                        id_digest,
                        command_classifier: StreamingCommandClassifier::new(),
                    };
                }
                (
                    overflow.command_classifier.observe(name, arguments),
                    ambiguous_eviction,
                )
            };
            if mutates || ambiguous_eviction {
                self.dominant_tool_action = ObservedActionClass::Mutate;
            }
            return;
        };
        let call = &mut self.calls[call_index];
        let streaming_mutation = call.command_classifier.observe(name, arguments);
        if let Some(name) = name {
            append_bounded(&mut call.name, name, &mut self.buffered_bytes);
        }
        append_bounded(&mut call.arguments, arguments, &mut self.buffered_bytes);
        let action = classify_tool_call(&call.name, &call.arguments, &self.tool_definitions);
        self.dominant_tool_action = self.dominant_tool_action.dominant(action);
        if streaming_mutation {
            self.dominant_tool_action = ObservedActionClass::Mutate;
        }
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

#[derive(Debug)]
struct StreamingCommandClassifier {
    command_tool: StreamingPatternSetState<6>,
    arguments: CommandArgumentClassifier,
}

impl StreamingCommandClassifier {
    fn new() -> Self {
        Self {
            command_tool: StreamingPatternSetState::new(),
            arguments: CommandArgumentClassifier::new(),
        }
    }

    fn observe(&mut self, name: Option<&str>, arguments: &str) -> bool {
        if let Some(name) = name {
            self.command_tool
                .observe_fragment(name, &COMMAND_TOOL_MARKERS);
        }
        self.arguments.observe(arguments);
        self.command_tool.matched() && self.arguments.mutates()
    }

    fn ambiguous_if_evicted(&self) -> bool {
        self.command_tool.matched() && !self.arguments.is_complete()
    }
}

#[derive(Debug)]
struct CommandArgumentClassifier {
    state: CommandArgumentState,
    mutates: bool,
}

impl CommandArgumentClassifier {
    fn new() -> Self {
        Self {
            state: CommandArgumentState::SeekingRoot,
            mutates: false,
        }
    }

    fn observe(&mut self, fragment: &str) {
        for byte in fragment.bytes() {
            self.state = self.state.advance(byte, &mut self.mutates);
        }
    }

    fn mutates(&self) -> bool {
        self.mutates
    }

    fn is_complete(&self) -> bool {
        matches!(self.state, CommandArgumentState::Complete)
    }
}

#[derive(Debug, Clone, Copy)]
enum CommandArgumentState {
    SeekingRoot,
    SeekingRootKey,
    ReadingRootKey {
        matcher: CommandKeyMatcher,
        escaped: bool,
    },
    SeekingColon {
        command_key: bool,
    },
    SeekingValue {
        command_key: bool,
    },
    ReadingStringValue {
        command_value: bool,
        decoder: JsonStringDecoderState,
        mutation: StreamingPatternSetState<11>,
    },
    SkippingNestedValue {
        depth: u16,
        in_string: bool,
        escaped: bool,
    },
    SkippingPrimitiveValue,
    AfterValue,
    Complete,
}

impl CommandArgumentState {
    fn advance(self, byte: u8, mutates: &mut bool) -> Self {
        match self {
            Self::SeekingRoot => {
                if byte.is_ascii_whitespace() {
                    Self::SeekingRoot
                } else if byte == b'{' {
                    Self::SeekingRootKey
                } else {
                    Self::Complete
                }
            }
            Self::SeekingRootKey => match byte {
                b'"' => Self::ReadingRootKey {
                    matcher: CommandKeyMatcher::new(),
                    escaped: false,
                },
                b'}' => Self::Complete,
                _ => Self::SeekingRootKey,
            },
            Self::ReadingRootKey {
                mut matcher,
                escaped,
            } => {
                if escaped {
                    matcher.invalidate();
                    Self::ReadingRootKey {
                        matcher,
                        escaped: false,
                    }
                } else if byte == b'\\' {
                    Self::ReadingRootKey {
                        matcher,
                        escaped: true,
                    }
                } else if byte == b'"' {
                    Self::SeekingColon {
                        command_key: matcher.is_command_key(),
                    }
                } else {
                    matcher.observe(byte);
                    Self::ReadingRootKey {
                        matcher,
                        escaped: false,
                    }
                }
            }
            Self::SeekingColon { command_key } => {
                if byte == b':' {
                    Self::SeekingValue { command_key }
                } else if byte.is_ascii_whitespace() {
                    Self::SeekingColon { command_key }
                } else {
                    Self::SeekingRootKey
                }
            }
            Self::SeekingValue { command_key } => {
                if byte.is_ascii_whitespace() {
                    Self::SeekingValue { command_key }
                } else if byte == b'"' {
                    Self::ReadingStringValue {
                        command_value: command_key,
                        decoder: JsonStringDecoderState::Plain,
                        mutation: StreamingPatternSetState::new(),
                    }
                } else if matches!(byte, b'{' | b'[') {
                    Self::SkippingNestedValue {
                        depth: 1,
                        in_string: false,
                        escaped: false,
                    }
                } else if byte == b'}' {
                    Self::Complete
                } else {
                    Self::SkippingPrimitiveValue
                }
            }
            Self::ReadingStringValue {
                command_value,
                decoder,
                mut mutation,
            } => {
                let (decoder, output) = decoder.advance(byte);
                match output {
                    JsonStringDecoderOutput::Pending => Self::ReadingStringValue {
                        command_value,
                        decoder,
                        mutation,
                    },
                    JsonStringDecoderOutput::Byte(decoded) => {
                        if command_value {
                            mutation.observe(decoded, &MUTATING_COMMAND_MARKERS);
                        }
                        Self::ReadingStringValue {
                            command_value,
                            decoder,
                            mutation,
                        }
                    }
                    JsonStringDecoderOutput::Delimiter => {
                        if command_value {
                            mutation.reset_progress();
                        }
                        Self::ReadingStringValue {
                            command_value,
                            decoder,
                            mutation,
                        }
                    }
                    JsonStringDecoderOutput::End => {
                        *mutates |= command_value && mutation.matched();
                        Self::AfterValue
                    }
                }
            }
            Self::SkippingNestedValue {
                mut depth,
                in_string,
                escaped,
            } => {
                if in_string {
                    if escaped {
                        Self::SkippingNestedValue {
                            depth,
                            in_string: true,
                            escaped: false,
                        }
                    } else if byte == b'\\' {
                        Self::SkippingNestedValue {
                            depth,
                            in_string: true,
                            escaped: true,
                        }
                    } else {
                        Self::SkippingNestedValue {
                            depth,
                            in_string: byte != b'"',
                            escaped: false,
                        }
                    }
                } else {
                    match byte {
                        b'"' => Self::SkippingNestedValue {
                            depth,
                            in_string: true,
                            escaped: false,
                        },
                        b'{' | b'[' => {
                            depth = depth.saturating_add(1);
                            Self::SkippingNestedValue {
                                depth,
                                in_string: false,
                                escaped: false,
                            }
                        }
                        b'}' | b']' => {
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                Self::AfterValue
                            } else {
                                Self::SkippingNestedValue {
                                    depth,
                                    in_string: false,
                                    escaped: false,
                                }
                            }
                        }
                        _ => Self::SkippingNestedValue {
                            depth,
                            in_string: false,
                            escaped: false,
                        },
                    }
                }
            }
            Self::SkippingPrimitiveValue => match byte {
                b',' => Self::SeekingRootKey,
                b'}' => Self::Complete,
                _ => Self::SkippingPrimitiveValue,
            },
            Self::AfterValue => match byte {
                b',' => Self::SeekingRootKey,
                b'}' => Self::Complete,
                _ => Self::AfterValue,
            },
            Self::Complete => Self::Complete,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum JsonStringDecoderState {
    Plain,
    Escape,
    Unicode { value: u16, digits: u8, valid: bool },
}

impl JsonStringDecoderState {
    fn advance(self, byte: u8) -> (Self, JsonStringDecoderOutput) {
        match self {
            Self::Plain => match byte {
                b'\\' => (Self::Escape, JsonStringDecoderOutput::Pending),
                b'"' => (Self::Plain, JsonStringDecoderOutput::End),
                _ => (Self::Plain, JsonStringDecoderOutput::Byte(byte)),
            },
            Self::Escape => match byte {
                b'"' | b'\\' | b'/' => (Self::Plain, JsonStringDecoderOutput::Byte(byte)),
                b'b' => (Self::Plain, JsonStringDecoderOutput::Byte(0x08)),
                b'f' => (Self::Plain, JsonStringDecoderOutput::Byte(0x0c)),
                b'n' => (Self::Plain, JsonStringDecoderOutput::Byte(b'\n')),
                b'r' => (Self::Plain, JsonStringDecoderOutput::Byte(b'\r')),
                b't' => (Self::Plain, JsonStringDecoderOutput::Byte(b'\t')),
                b'u' => (
                    Self::Unicode {
                        value: 0,
                        digits: 0,
                        valid: true,
                    },
                    JsonStringDecoderOutput::Pending,
                ),
                _ => (Self::Plain, JsonStringDecoderOutput::Delimiter),
            },
            Self::Unicode {
                value,
                digits,
                valid,
            } => {
                let digit = json_hex_digit(byte);
                let valid = valid && digit.is_some();
                let value = digit.map_or(value, |digit| (value << 4) | digit);
                let digits = digits.saturating_add(1);
                if digits < 4 {
                    (
                        Self::Unicode {
                            value,
                            digits,
                            valid,
                        },
                        JsonStringDecoderOutput::Pending,
                    )
                } else if valid {
                    match u8::try_from(value) {
                        Ok(decoded) => (Self::Plain, JsonStringDecoderOutput::Byte(decoded)),
                        Err(_) => (Self::Plain, JsonStringDecoderOutput::Delimiter),
                    }
                } else {
                    (Self::Plain, JsonStringDecoderOutput::Delimiter)
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum JsonStringDecoderOutput {
    Pending,
    Byte(u8),
    Delimiter,
    End,
}

fn json_hex_digit(byte: u8) -> Option<u16> {
    match byte {
        b'0'..=b'9' => Some(u16::from(byte - b'0')),
        b'a'..=b'f' => Some(u16::from(byte - b'a') + 10),
        b'A'..=b'F' => Some(u16::from(byte - b'A') + 10),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy)]
struct CommandKeyMatcher {
    candidates: u8,
    index: u8,
}

impl CommandKeyMatcher {
    const KEYS: [&'static [u8]; 3] = [b"cmd", b"command", b"script"];

    fn new() -> Self {
        Self {
            candidates: 0b111,
            index: 0,
        }
    }

    fn observe(&mut self, byte: u8) {
        let index = usize::from(self.index);
        for (candidate_index, key) in Self::KEYS.iter().enumerate() {
            if index >= key.len() || byte != key[index] {
                self.candidates &= !(1 << candidate_index);
            }
        }
        self.index = self.index.saturating_add(1);
    }

    fn invalidate(&mut self) {
        self.candidates = 0;
    }

    fn is_command_key(self) -> bool {
        let index = usize::from(self.index);
        Self::KEYS.iter().enumerate().any(|(candidate_index, key)| {
            self.candidates & (1 << candidate_index) != 0 && index == key.len()
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct StreamingPatternSetState<const N: usize> {
    progress: [u8; N],
    matched: bool,
}

impl<const N: usize> StreamingPatternSetState<N> {
    fn new() -> Self {
        Self {
            progress: [0; N],
            matched: false,
        }
    }

    fn observe_fragment(&mut self, fragment: &str, patterns: &[&str; N]) {
        for byte in fragment.bytes() {
            self.observe(byte, patterns);
        }
    }

    fn observe(&mut self, byte: u8, patterns: &[&str; N]) {
        let byte = byte.to_ascii_lowercase();
        for (index, pattern) in patterns.iter().enumerate() {
            let pattern = pattern.as_bytes();
            let progress = usize::from(self.progress[index]);
            if byte == pattern[progress] {
                let next = progress.saturating_add(1);
                if next == pattern.len() {
                    self.matched = true;
                    self.progress[index] = 0;
                } else {
                    self.progress[index] = self.progress[index].saturating_add(1);
                }
            } else {
                self.progress[index] = u8::from(byte == pattern[0]);
            }
        }
    }

    fn matched(&self) -> bool {
        self.matched
    }

    fn reset_progress(&mut self) {
        self.progress.fill(0);
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
    if contains_any(&command, &MUTATING_COMMAND_MARKERS) {
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
    contains_any(&name, &COMMAND_TOOL_MARKERS)
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
    async fn response_observer_classifies_fragmented_command_mutation_after_tool_call_limit()
    -> anyhow::Result<()> {
        let (pending, invocation) = pending("stream-command-call-limit");
        let observer = PredictiveResponseObserver::new(pending.clone());
        let context =
            pipeline_context("stream-command-call-limit", prompt(Vec::new()), &invocation);
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
        for (name, arguments) in [
            (Some("exec_command".into()), r#"{"cmd":"r"#),
            (None, r#"m private-file"}"#),
        ] {
            observer
                .on_stream_part(
                    &stream,
                    &StreamPart::ToolCallDelta {
                        id: "command-mutation-after-limit".into(),
                        name,
                        arguments: arguments.into(),
                    },
                )
                .await;
        }
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
    async fn response_observer_conservatively_classifies_interleaved_overflow_command()
    -> anyhow::Result<()> {
        let (pending, invocation) = pending("stream-interleaved-overflow");
        let observer = PredictiveResponseObserver::new(pending.clone());
        let context = pipeline_context(
            "stream-interleaved-overflow",
            prompt(Vec::new()),
            &invocation,
        );
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
        for (id, name, arguments) in [
            ("overflow-a", Some("exec_command".into()), r#"{"cmd":"r"#),
            (
                "overflow-b",
                Some("opaque_capability".into()),
                r#"{"value":"harmless"}"#,
            ),
            ("overflow-a", None, r#"m private-file"}"#),
        ] {
            observer
                .on_stream_part(
                    &stream,
                    &StreamPart::ToolCallDelta {
                        id: id.into(),
                        name,
                        arguments: arguments.into(),
                    },
                )
                .await;
        }
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
    async fn response_observer_keeps_complete_harmless_overflow_command_unknown()
    -> anyhow::Result<()> {
        let (pending, invocation) = pending("stream-harmless-overflow");
        let observer = PredictiveResponseObserver::new(pending.clone());
        let context = pipeline_context("stream-harmless-overflow", prompt(Vec::new()), &invocation);
        observer.after_phase(Phase::Route, &context).await;
        let stream = context.stream_context();
        for index in 0..32 {
            observer
                .on_stream_part(
                    &stream,
                    &StreamPart::ToolCallDelta {
                        id: format!("unknown-{index}"),
                        name: Some("opaque_capability".into()),
                        arguments: r#"{}"#.into(),
                    },
                )
                .await;
        }
        for id in ["harmless-command-a", "harmless-command-b"] {
            observer
                .on_stream_part(
                    &stream,
                    &StreamPart::ToolCallDelta {
                        id: id.into(),
                        name: Some("exec_command".into()),
                        arguments: r#"{"cmd":"echo harmless"}"#.into(),
                    },
                )
                .await;
        }
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
        Ok(())
    }

    #[tokio::test]
    async fn response_observer_ignores_nested_command_keys_like_nonstream() -> anyhow::Result<()> {
        let arguments = r#"{"cmd":"echo harmless","metadata":{"command":"rm private-file"}}"#;
        let nonstream = observe_nonstream(
            "nested-command-nonstream",
            vec![tool_call("exec_command", arguments)],
            Vec::new(),
        )
        .await?;
        assert_eq!(nonstream, ObservedActionClass::Unknown);

        let (pending, invocation) = pending("nested-command-stream");
        let observer = PredictiveResponseObserver::new(pending.clone());
        let context = pipeline_context("nested-command-stream", prompt(Vec::new()), &invocation);
        observer.after_phase(Phase::Route, &context).await;
        let stream = context.stream_context();
        observer
            .on_stream_part(
                &stream,
                &StreamPart::ToolCallDelta {
                    id: "nested-command".into(),
                    name: Some("exec_command".into()),
                    arguments: arguments.into(),
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
            Some(nonstream)
        );
        Ok(())
    }

    #[tokio::test]
    async fn response_observer_treats_complete_root_array_as_non_command_json() -> anyhow::Result<()>
    {
        let arguments = r#"[{"command":"rm private-file"}]"#;
        let nonstream = observe_nonstream(
            "root-array-nonstream",
            vec![tool_call("exec_command", arguments)],
            Vec::new(),
        )
        .await?;
        assert_eq!(nonstream, ObservedActionClass::Unknown);

        let (pending, invocation) = pending("root-array-stream");
        let observer = PredictiveResponseObserver::new(pending.clone());
        let context = pipeline_context("root-array-stream", prompt(Vec::new()), &invocation);
        observer.after_phase(Phase::Route, &context).await;
        let stream = context.stream_context();
        for index in 0..32 {
            observer
                .on_stream_part(
                    &stream,
                    &StreamPart::ToolCallDelta {
                        id: format!("unknown-{index}"),
                        name: Some("opaque_capability".into()),
                        arguments: r#"{}"#.into(),
                    },
                )
                .await;
        }
        for (id, name, arguments) in [
            ("root-array", Some("exec_command".into()), arguments),
            ("next-overflow", Some("opaque_capability".into()), r#"{}"#),
        ] {
            observer
                .on_stream_part(
                    &stream,
                    &StreamPart::ToolCallDelta {
                        id: id.into(),
                        name,
                        arguments: arguments.into(),
                    },
                )
                .await;
        }
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
            Some(nonstream)
        );
        Ok(())
    }

    #[tokio::test]
    async fn response_observer_decodes_fragmented_simple_escapes_like_nonstream()
    -> anyhow::Result<()> {
        let cases: [(&str, &[&str]); 3] = [
            ("newline", &[r#"{"cmd":"r"#, r#"\n"#, r#"m private-file"}"#]),
            ("escaped-quote", &[r#"{"cmd":"r\"#, r#""m private-file"}"#]),
            (
                "escaped-backslash",
                &[r#"{"cmd":"r\"#, r#"\m private-file"}"#],
            ),
        ];

        for (name, fragments) in cases {
            let arguments = fragments.concat();
            let nonstream = observe_nonstream(
                &format!("simple-escape-{name}-nonstream"),
                vec![tool_call("exec_command", &arguments)],
                Vec::new(),
            )
            .await?;
            assert_eq!(nonstream, ObservedActionClass::Unknown, "{name} nonstream");

            let streaming = observe_streamed_command_after_raw_limit(
                &format!("simple-escape-{name}-stream"),
                fragments,
            )
            .await?;
            assert_eq!(streaming, nonstream, "{name} streaming");
        }
        Ok(())
    }

    #[tokio::test]
    async fn response_observer_decodes_fragmented_ascii_unicode_escape_like_nonstream()
    -> anyhow::Result<()> {
        let fragments = [r#"{"cmd":"r\"#, "u0", "06", r#"d private-file"}"#];
        let arguments = fragments.concat();
        let nonstream = observe_nonstream(
            "unicode-escape-nonstream",
            vec![tool_call("exec_command", &arguments)],
            Vec::new(),
        )
        .await?;
        assert_eq!(nonstream, ObservedActionClass::Mutate);

        let streaming =
            observe_streamed_command_after_raw_limit("unicode-escape-stream", &fragments).await?;
        assert_eq!(streaming, nonstream);
        Ok(())
    }

    #[tokio::test]
    async fn response_observer_treats_complete_scalar_roots_as_non_command_json()
    -> anyhow::Result<()> {
        for (name, arguments) in [
            ("string", r#""echo harmless""#),
            ("null", "null"),
            ("boolean", "true"),
            ("number", "42"),
        ] {
            let nonstream = observe_nonstream(
                &format!("root-scalar-{name}-nonstream"),
                vec![tool_call("exec_command", arguments)],
                Vec::new(),
            )
            .await?;
            assert_eq!(nonstream, ObservedActionClass::Unknown, "{name} nonstream");

            let streaming =
                observe_evicted_overflow_command(&format!("root-scalar-{name}-stream"), arguments)
                    .await?;
            assert_eq!(streaming, nonstream, "{name} streaming");
        }
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
    async fn response_observer_classifies_fragmented_command_mutation_after_raw_byte_limit()
    -> anyhow::Result<()> {
        let (pending, invocation) = pending("stream-command-byte-limit");
        let observer = PredictiveResponseObserver::new(pending.clone());
        let context =
            pipeline_context("stream-command-byte-limit", prompt(Vec::new()), &invocation);
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
        for (name, arguments) in [
            (Some("exec_command".into()), r#"{"com"#),
            (None, r#"mand":"echo \"quoted\"; r"#),
            (None, r#"m private-file"}"#),
        ] {
            observer
                .on_stream_part(
                    &stream,
                    &StreamPart::ToolCallDelta {
                        id: "command-mutation-after-bytes".into(),
                        name,
                        arguments: arguments.into(),
                    },
                )
                .await;
        }
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

    async fn observe_streamed_command_after_raw_limit(
        request_id: &str,
        fragments: &[&str],
    ) -> anyhow::Result<ObservedActionClass> {
        let (pending, invocation) = pending(request_id);
        let observer = PredictiveResponseObserver::new(pending.clone());
        let context = pipeline_context(request_id, prompt(Vec::new()), &invocation);
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
        for (index, fragment) in fragments.iter().enumerate() {
            observer
                .on_stream_part(
                    &stream,
                    &StreamPart::ToolCallDelta {
                        id: "streamed-command".into(),
                        name: (index == 0).then(|| "exec_command".into()),
                        arguments: (*fragment).into(),
                    },
                )
                .await;
        }
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

        pending
            .peek(&invocation, "local")
            .and_then(|decision| decision.observation)
            .map(|observation| observation.observed_action)
            .ok_or_else(|| anyhow::anyhow!("stream observation missing for {request_id}"))
    }

    async fn observe_evicted_overflow_command(
        request_id: &str,
        arguments: &str,
    ) -> anyhow::Result<ObservedActionClass> {
        let (pending, invocation) = pending(request_id);
        let observer = PredictiveResponseObserver::new(pending.clone());
        let context = pipeline_context(request_id, prompt(Vec::new()), &invocation);
        observer.after_phase(Phase::Route, &context).await;
        let stream = context.stream_context();
        for index in 0..32 {
            observer
                .on_stream_part(
                    &stream,
                    &StreamPart::ToolCallDelta {
                        id: format!("retained-{index}"),
                        name: Some("opaque_capability".into()),
                        arguments: r#"{}"#.into(),
                    },
                )
                .await;
        }
        for (id, name, arguments) in [
            ("scalar-root", "exec_command", arguments),
            ("replacement", "opaque_capability", r#"{}"#),
        ] {
            observer
                .on_stream_part(
                    &stream,
                    &StreamPart::ToolCallDelta {
                        id: id.into(),
                        name: Some(name.into()),
                        arguments: arguments.into(),
                    },
                )
                .await;
        }
        observer
            .on_stream_part(
                &stream,
                &StreamPart::Finish {
                    reason: FinishReason::Stop,
                },
            )
            .await;

        pending
            .peek(&invocation, "local")
            .and_then(|decision| decision.observation)
            .map(|observation| observation.observed_action)
            .ok_or_else(|| anyhow::anyhow!("stream observation missing for {request_id}"))
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
            continuation_proposed_tier: None,
            continuation_proposed_model: None,
            continuation_adjustment: None,
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
