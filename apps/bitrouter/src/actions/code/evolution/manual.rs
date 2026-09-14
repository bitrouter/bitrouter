//! An explicit manual review draft; no model, storage or routing side effects.

use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use bitrouter_tui::code::{Inspector, Selector, SelectorRow};

use crate::acp_trajectory::checkpoint::types::{
    AssessmentRevision, AssessmentSource, CriterionScore, EvidenceCitation,
};
use crate::evolution::operator::checkpoint::ReviewInput;
use crate::evolution::rubric::{
    Applicability, RUBRIC_VERSION, RubricEvaluation, RubricItem, library,
};
use crate::evolution::scoring::RubricSubmission;

use super::Panel;

#[derive(Clone)]
pub(crate) struct ReviewDraft {
    pub input: Arc<ReviewInput>,
    pub evaluation: RubricEvaluation,
    submission_id: String,
}

pub(super) fn label(id: &str) -> &str {
    match id {
        "delivery" => "Delivery",
        "constraints" => "User constraints",
        "verification" => "Tests and verification",
        "review_resolution" => "Review findings",
        "pr_delivery" => "PR delivery",
        "user_acceptance" => "User acceptance",
        _ => id,
    }
}

fn score_text(item: &RubricItem) -> String {
    match item.score {
        CriterionScore::Scored { value_ppm } => {
            format!("{:.2}", f64::from(value_ppm) / 1_000_000.0)
        }
        CriterionScore::NotApplicable => "Not applicable".into(),
        CriterionScore::Unknown if item.applicability == Applicability::Applicable => {
            "Applicable · insufficient evidence".into()
        }
        CriterionScore::Unknown => "Applicability unknown".into(),
    }
}

pub(super) fn evidence_text(input: &ReviewInput) -> Result<String> {
    let mut text = format!(
        "Recorded evidence · {}\nCheckpoint: {}\nPrefix: {}\n",
        input.identity.native_session_id,
        input.checkpoint.checkpoint_id,
        input.checkpoint.watermark
    );
    if !input.evidence.gaps.is_empty() {
        text.push_str(&format!(
            "Recording gaps: {}\n",
            input.evidence.gaps.join(", ")
        ));
    }
    for (i, item) in input.evidence.items.iter().enumerate() {
        text.push_str(&format!(
            "\n[{}] {:?} · {}\n{}\n{}\n",
            i + 1,
            item.kind,
            item.method,
            item.citation.node_id,
            serde_json::to_string_pretty(&item.content)?
        ));
    }
    Ok(text)
}

impl ReviewDraft {
    pub fn new(input: ReviewInput) -> Self {
        let evaluation = input
            .previous
            .clone()
            .filter(|prior| prior.rubric_version == RUBRIC_VERSION)
            .unwrap_or_else(|| RubricEvaluation {
                rubric_version: RUBRIC_VERSION.into(),
                items: library()
                    .into_iter()
                    .map(|criterion| RubricItem {
                        criterion_id: criterion.id.into(),
                        applicability: if criterion.mandatory {
                            Applicability::Applicable
                        } else {
                            Applicability::Unknown
                        },
                        selection_reason: String::new(),
                        score: CriterionScore::Unknown,
                        evidence: Vec::new(),
                        explanation: String::new(),
                    })
                    .collect(),
                diagnostics: Vec::new(),
                severe_violation: false,
                violation_evidence: Vec::new(),
                summary: String::new(),
            });
        Self {
            input: Arc::new(input),
            evaluation,
            submission_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    pub fn menu(&self) -> Selector {
        let mut rows = vec![SelectorRow::new(
            "evidence",
            "Read recorded evidence",
            "Search and copy the frozen transcript",
        )];
        if !self.input.history.is_empty() {
            rows.push(SelectorRow::new(
                "history",
                "Evaluation history",
                format!(
                    "{} revisions · {}",
                    self.input.history.len(),
                    self.prefill_notice()
                ),
            ));
        }
        for (index, item) in self.evaluation.items.iter().enumerate() {
            rows.push(SelectorRow::new(
                format!("item:{index}"),
                label(&item.criterion_id),
                score_text(item),
            ));
        }
        rows.extend([
            SelectorRow::new(
                "summary",
                "Overall feedback",
                if self.evaluation.summary.is_empty() {
                    "Required before submission"
                } else {
                    &self.evaluation.summary
                },
            ),
            SelectorRow::new(
                "violation",
                "Material violation",
                if self.evaluation.severe_violation {
                    "Marked · evidence required"
                } else {
                    "Not marked"
                },
            ),
            SelectorRow::new(
                "preview",
                "Review and submit",
                "Validate scores and inspect the reward before saving",
            ),
            SelectorRow::new(
                "discard",
                "Discard draft",
                "Leave the stored assessment unchanged",
            ),
        ]);
        Selector::new(
            "evolution:review",
            "Manual checkpoint evaluation",
            format!(
                "{} · prefix {} · {}",
                self.input.identity.native_session_id,
                self.input.checkpoint.watermark,
                if self.input.current_watermark == self.input.checkpoint.watermark {
                    "Draft is not saved"
                } else {
                    "Historical prefix · newer content exists"
                }
            ),
            rows,
        )
    }

    fn history_menu(&self) -> Selector {
        Selector::new(
            "evolution:assessment_history",
            "Checkpoint evaluation history",
            "Stored revisions as of opening this review. Reading history does not change the draft or the selected reward.",
            self.input
                .history
                .iter()
                .rev()
                .map(|revision| {
                    let origin = if revision.input.source == AssessmentSource::Human {
                        "Manual"
                    } else {
                        "Automatic"
                    };
                    let state = if revision.input.assessment.is_none() {
                        "Retracted"
                    } else if Some(&revision.revision_id) == self.input.expected_revision.as_ref() {
                        "Selected when review opened"
                    } else {
                        "Historical revision"
                    };
                    SelectorRow::new(
                        &revision.revision_id,
                        format!("{} · {origin}", revision.created_at),
                        format!("{state} · {}", revision.input.reason),
                    )
                })
                .collect(),
        )
    }

    fn prefill_notice(&self) -> String {
        let Some(revision) =
            self.input.history.iter().find(|revision| {
                Some(&revision.revision_id) == self.input.prefill_revision.as_ref()
            })
        else {
            return "No stored rubric selected as a starting point".into();
        };
        if revision.input.assessment.is_none() {
            return "Stored assessment was retracted; draft starts unscored".into();
        }
        if self
            .input
            .previous
            .as_ref()
            .is_none_or(|evaluation| evaluation.rubric_version != RUBRIC_VERSION)
        {
            return "Stored rubric format is unavailable; draft starts unscored".into();
        }
        let source = if revision.input.source == AssessmentSource::Human {
            "manual"
        } else {
            "automatic"
        };
        format!(
            "Prefilled from {source} evaluation saved {}",
            revision.created_at
        )
    }

    fn revision_text(&self, revision: &AssessmentRevision) -> String {
        let mut text = format!(
            "Checkpoint prefix: {}\nSaved: {}\nSource: {:?}\nEvaluator: {} · {}\nRevision: {}\nReplaces: {}\nSelected at submission: {}\nSubmission outcome: {}\nSelected when this review opened: {}\nReason: {}\n",
            self.input.checkpoint.watermark,
            revision.created_at,
            revision.input.source,
            revision.input.evaluator_id,
            revision.input.evaluator_version,
            revision.revision_id,
            revision.supersedes.as_deref().unwrap_or("none"),
            revision.selected_on_submission,
            revision.selection_reason,
            Some(&revision.revision_id) == self.input.expected_revision.as_ref(),
            revision.input.reason
        );
        if self.input.current_watermark != self.input.checkpoint.watermark {
            text.push_str("\nNewer session content exists. This checkpoint is historical and does not replace a newer prefix's selected evaluation.\n");
        }
        if self.input.prefill_revision.as_deref() == Some(&revision.revision_id) {
            text.push_str("\nThis is the stored starting point for the current draft. Unsaved edits are separate.\n");
        }
        if let Some(content) = &revision.input.assessment {
            text.push_str("\nStored scores:\n");
            for (id, score) in &content.scores {
                let value = match score {
                    CriterionScore::Scored { value_ppm } => {
                        format!("{:.2}", f64::from(*value_ppm) / 1_000_000.0)
                    }
                    CriterionScore::Unknown => "Unknown".into(),
                    CriterionScore::NotApplicable => "Not applicable".into(),
                };
                text.push_str(&format!("{}: {value}\n", label(id)));
            }
            text.push_str("\nOriginal stored explanation and rubric details:\n");
            text.push_str(
                &serde_json::from_str::<serde_json::Value>(&content.explanation)
                    .ok()
                    .and_then(|value| serde_json::to_string_pretty(&value).ok())
                    .unwrap_or_else(|| content.explanation.clone()),
            );
            text.push_str("\n\nOriginal evidence citations:\n");
            for citation in &content.evidence {
                text.push_str(&format!("{} · {}\n", citation.node_id, citation.digest));
            }
        } else {
            text.push_str("\nThis revision retracts the assessment. Earlier scores are not restored automatically.\n");
        }
        text
    }

    fn item_menu(&self, index: usize) -> Result<Selector> {
        let item = self.evaluation.items.get(index).context("Unknown rubric")?;
        let template = library()
            .into_iter()
            .find(|t| t.id == item.criterion_id)
            .context("Unsupported rubric")?;
        Ok(Selector::new(
            format!("evolution:item:{index}"),
            label(&item.criterion_id),
            format!(
                "Weight {}. {}\n{}",
                template.weight, template.applicability, template.anchors
            ),
            vec![
                SelectorRow::new("score", "Score and applicability", score_text(item)),
                SelectorRow::new(
                    "citations",
                    "Select supporting evidence",
                    format!("{} selected", item.evidence.len()),
                ),
                SelectorRow::new(
                    "reason",
                    "Explain applicability and score",
                    &item.explanation,
                ),
                SelectorRow::new("back", "Back to evaluation", "Keep draft changes"),
            ],
        ))
    }

    fn citations(&self, index: Option<usize>) -> Result<Selector> {
        let selected = if let Some(index) = index {
            &self
                .evaluation
                .items
                .get(index)
                .context("Unknown rubric")?
                .evidence
        } else {
            &self.evaluation.violation_evidence
        };
        let mut rows = vec![
            SelectorRow::new(
                "done",
                "Done selecting evidence",
                format!("{} selected", selected.len()),
            ),
            SelectorRow::new(
                "read",
                "Read full evidence",
                "Inspect complete content before citing it",
            ),
        ];
        for (i, evidence) in self.input.evidence.items.iter().enumerate() {
            let marked = selected
                .iter()
                .any(|c| c.node_id == evidence.citation.node_id);
            let snippet: String = serde_json::to_string(&evidence.content)?
                .chars()
                .take(160)
                .collect();
            rows.push(SelectorRow::new(
                i.to_string(),
                format!(
                    "{} [{}] {:?} · {}",
                    if marked { "✓" } else { "○" },
                    i + 1,
                    evidence.kind,
                    evidence.method
                ),
                snippet,
            ));
        }
        Ok(Selector::new(
            format!(
                "evolution:citations:{}",
                index
                    .map(|i| i.to_string())
                    .unwrap_or_else(|| "violation".into())
            ),
            "Supporting evidence",
            "Enter toggles a citation; read full evidence to inspect the result and its timing.",
            rows,
        ))
    }

    fn preview(&self) -> Result<String> {
        let bounds = self.evaluation.aggregate(&self.input.evidence)?;
        let mut text = format!(
            "Session: {}\nCheckpoint: {}\nQuality: {:.2}–{:.2}\nEvidence coverage: {:.0}%\n",
            self.input.identity.native_session_id,
            self.input.checkpoint.checkpoint_id,
            f64::from(bounds.lower_ppm) / 1_000_000.0,
            f64::from(bounds.upper_ppm) / 1_000_000.0,
            f64::from(bounds.coverage_ppm) / 10_000.0
        );
        text.push_str(&format!("{}\n", self.prefill_notice()));
        text.push_str("The range reflects missing evidence, not statistical confidence.\n");
        if bounds.required_unknown || !bounds.capture_gaps.is_empty() {
            text.push_str("Required evidence is unknown or recording is incomplete; this cannot establish a complete reward.\n");
        }
        if bounds.severe_violation {
            text.push_str("Material violation marked.\n");
        }
        for item in &self.evaluation.items {
            text.push_str(&format!(
                "\n{}: {}\n{}\nCitations: {}\n",
                label(&item.criterion_id),
                score_text(item),
                item.explanation,
                item.evidence
                    .iter()
                    .map(|c| c.node_id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        for diagnostic in &self.evaluation.diagnostics {
            text.push_str(&format!(
                "\nDiagnostic · {} · {:?}\n{}\n",
                label(&diagnostic.criterion_id),
                diagnostic.role,
                diagnostic.explanation
            ));
        }
        text.push_str(&format!("\nOverall feedback: {}\n\nSaving creates a manual revision for this prefix. Newer content or a changed assessment can prevent selection. Enabled evolution can use comparable feedback to update future routing.\n", self.evaluation.summary));
        Ok(text)
    }

    pub fn submission(&self) -> Result<RubricSubmission> {
        self.evaluation.aggregate(&self.input.evidence)?;
        Ok(RubricSubmission {
            submission_id: self.submission_id.clone(),
            checkpoint_id: self.input.checkpoint.checkpoint_id.clone(),
            expected_revision: self.input.expected_revision.clone(),
            source: AssessmentSource::Human,
            evaluator_id: crate::evolution::scoring::TUI_EVALUATOR_ID.into(),
            evaluator_version: crate::evolution::scoring::TUI_EVALUATOR_VERSION.into(),
            evaluation: self.evaluation.clone(),
        })
    }

    pub fn step(mut self, selector: &str, choice: &str) -> Result<Panel> {
        let mut panel = match selector {
            "evolution:review" => match choice {
                "history" => Panel::selector(self.history_menu()),
                "evidence" => {
                    Panel::inspector("Recorded checkpoint evidence", evidence_text(&self.input)?)
                }
                "summary" => Panel::selector(
                    Selector::new(
                        "evolution:summary",
                        "Overall feedback",
                        &self.evaluation.summary,
                        Vec::new(),
                    )
                    .allow_custom("Describe the recorded outcome"),
                ),
                "violation" => Panel::selector(Selector::new(
                    "evolution:violation",
                    "Material violation",
                    "Mark only an evidence-supported violation. Describe it in the overall feedback.",
                    vec![
                        SelectorRow::new("no", "No material violation marked", ""),
                        SelectorRow::new(
                            "yes",
                            "Mark a material violation",
                            "Select original evidence",
                        ),
                    ],
                )),
                "preview" => {
                    let content = self.preview()?;
                    let mut panel = Panel::selector(Selector::new(
                        "evolution:submit",
                        "Save manual evaluation",
                        "Review the assessment before saving.",
                        vec![
                            SelectorRow::new(
                                "save",
                                "Save this evaluation",
                                "Submit this checkpoint and expected revision",
                            ),
                            SelectorRow::new("back", "Continue editing", "Keep the unsaved draft"),
                        ],
                    ));
                    panel.inspector = Some(Inspector::new("Review before saving", content));
                    panel
                }
                "discard" => {
                    let mut panel = Panel::inspector(
                        "Draft discarded",
                        "The stored assessment was not changed.",
                    );
                    panel.clear_drafts = true;
                    return Ok(panel);
                }
                _ => Panel::selector(
                    self.item_menu(
                        choice
                            .strip_prefix("item:")
                            .context("Unknown evaluation choice")?
                            .parse()?,
                    )?,
                ),
            },
            "evolution:assessment_history" => {
                let revision = self
                    .input
                    .history
                    .iter()
                    .find(|revision| revision.revision_id == choice)
                    .context("Unknown checkpoint revision")?;
                Panel::inspector("Stored checkpoint evaluation", self.revision_text(revision))
            }
            "evolution:summary" => {
                ensure!(!choice.trim().is_empty(), "Overall feedback is required");
                self.evaluation.summary = choice.to_owned();
                self.changed();
                Panel::selector(self.menu())
            }
            "evolution:violation" => {
                ensure!(matches!(choice, "yes" | "no"), "Invalid violation choice");
                self.evaluation.severe_violation = choice == "yes";
                if choice == "no" {
                    self.evaluation.violation_evidence.clear();
                }
                self.changed();
                Panel::selector(if choice == "yes" {
                    self.citations(None)?
                } else {
                    self.menu()
                })
            }
            "evolution:submit" if choice == "back" => Panel::selector(self.menu()),
            _ if selector.starts_with("evolution:item:") => {
                let index: usize = selector.trim_start_matches("evolution:item:").parse()?;
                let item = self.evaluation.items.get(index).context("Unknown rubric")?;
                match choice {
                    "back" => Panel::selector(self.menu()),
                    "citations" => Panel::selector(self.citations(Some(index))?),
                    "reason" => Panel::selector(Selector::new(format!("evolution:reason:{index}"), format!("Explain {}", label(&item.criterion_id)), "State why this criterion applies and what the cited evidence supports.", Vec::new()).allow_custom("Enter your explanation")),
                    "score" => {
                        let mandatory = library().iter().any(|t| t.id == item.criterion_id && t.mandatory);
                        let mut na = SelectorRow::new("na", "Not applicable", "No such obligation in this recorded task");
                        if mandatory { na = na.unavailable("This criterion always applies"); }
                        Panel::selector(Selector::new(format!("evolution:score:{index}"), format!("Score {}", label(&item.criterion_id)), "Use the rubric's anchors. Missing evidence is unknown, not success.", vec![
                            SelectorRow::new("0", "0 · Not met", "Recorded failure or unfulfilled obligation"),
                            SelectorRow::new("0.5", "0.5 · Partly met", "Recorded partial delivery or limited validation"),
                            SelectorRow::new("1", "1 · Met", "Supported by original evidence"),
                            SelectorRow::new("unknown", "Applicable · insufficient evidence", "Retain the obligation without inventing a score"),
                            SelectorRow::new("unsure", "Applicability unknown", "Keep uncertainty in the reward bounds"), na,
                        ]).allow_custom("Score between 0 and 1"))
                    }
                    _ => bail!("Unknown rubric action"),
                }
            }
            _ if selector.starts_with("evolution:score:") => {
                let index: usize = selector.trim_start_matches("evolution:score:").parse()?;
                let item = self
                    .evaluation
                    .items
                    .get_mut(index)
                    .context("Unknown rubric")?;
                let (applicability, score) = match choice {
                    "na" => {
                        ensure!(
                            !library()
                                .iter()
                                .any(|t| t.id == item.criterion_id && t.mandatory),
                            "This criterion always applies"
                        );
                        (Applicability::NotApplicable, CriterionScore::NotApplicable)
                    }
                    "unknown" => (Applicability::Applicable, CriterionScore::Unknown),
                    "unsure" => (Applicability::Unknown, CriterionScore::Unknown),
                    value => {
                        let value: f64 = value.parse()?;
                        ensure!(
                            value.is_finite() && (0.0..=1.0).contains(&value),
                            "Score must be between 0 and 1"
                        );
                        (
                            Applicability::Applicable,
                            CriterionScore::Scored {
                                value_ppm: (value * 1_000_000.0).round() as u32,
                            },
                        )
                    }
                };
                item.applicability = applicability;
                item.score = score;
                self.changed();
                Panel::selector(self.item_menu(index)?)
            }
            _ if selector.starts_with("evolution:reason:") => {
                ensure!(!choice.trim().is_empty(), "Explain applicability and score");
                let index: usize = selector.trim_start_matches("evolution:reason:").parse()?;
                let item = self
                    .evaluation
                    .items
                    .get_mut(index)
                    .context("Unknown rubric")?;
                item.explanation = choice.to_owned();
                item.selection_reason = choice.to_owned();
                self.changed();
                Panel::selector(self.item_menu(index)?)
            }
            _ if selector.starts_with("evolution:citations:") => {
                let target = selector.trim_start_matches("evolution:citations:");
                let index = if target == "violation" {
                    None
                } else {
                    Some(target.parse::<usize>()?)
                };
                match choice {
                    "done" => Panel::selector(if let Some(index) = index {
                        self.item_menu(index)?
                    } else {
                        self.menu()
                    }),
                    "read" => Panel::inspector(
                        "Recorded checkpoint evidence",
                        evidence_text(&self.input)?,
                    ),
                    value => {
                        let citation = self
                            .input
                            .evidence
                            .items
                            .get(value.parse::<usize>()?)
                            .context("Unknown evidence item")?
                            .citation
                            .clone();
                        let citations = if let Some(index) = index {
                            &mut self
                                .evaluation
                                .items
                                .get_mut(index)
                                .context("Unknown rubric")?
                                .evidence
                        } else {
                            &mut self.evaluation.violation_evidence
                        };
                        toggle(citations, citation);
                        self.changed();
                        Panel::selector(self.citations(index)?)
                    }
                }
            }
            _ => bail!("Unknown review action"),
        };
        if panel.inspector.is_some() && panel.selector.is_none() {
            panel.selector = Some(
                if let Some(target) = selector.strip_prefix("evolution:citations:") {
                    self.citations(if target == "violation" {
                        None
                    } else {
                        Some(target.parse()?)
                    })?
                } else {
                    self.menu()
                },
            );
        }
        panel.review = Some(self);
        Ok(panel)
    }

    fn changed(&mut self) {
        self.submission_id = uuid::Uuid::new_v4().to_string();
    }
}

fn toggle(citations: &mut Vec<EvidenceCitation>, citation: EvidenceCitation) {
    if citations.iter().any(|c| c.node_id == citation.node_id) {
        citations.retain(|c| c.node_id != citation.node_id);
    } else {
        citations.push(citation);
    }
}
