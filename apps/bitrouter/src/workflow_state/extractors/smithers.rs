use crate::workflow_state::extractors::generic::GenericPromptExtractor;
use crate::workflow_state::extractors::{ExtractorInput, WorkflowStateExtractor};
use crate::workflow_state::ir::{HarnessId, WorkflowStateIR};

pub struct SmithersExtractor;

impl WorkflowStateExtractor for SmithersExtractor {
    fn extract(&self, input: &ExtractorInput<'_>) -> WorkflowStateIR {
        let mut ir = GenericPromptExtractor.extract(input);
        ir.harness_id = HarnessId::Smithers;
        ir.active_workflow = non_empty_header(input, "x-smithers-workflow-id");
        ir.subagent_role = non_empty_header(input, "x-smithers-node-id");
        ir
    }
}

fn non_empty_header(input: &ExtractorInput<'_>, name: &str) -> Option<String> {
    input
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}
