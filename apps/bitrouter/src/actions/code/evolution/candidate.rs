//! Explicit candidate draft; persistence and live route validation stay in the service.

use anyhow::{Context, Result, bail, ensure};
use bitrouter_tui::code::{Inspector, Selector, SelectorRow};

use super::{Panel, SessionRef, mode_name};
use crate::evolution::control::BlockRule;
use crate::evolution::runtime::candidates::{
    CandidateCatalog, CandidatePreview, CandidateSpec, FeedbackChoice,
};

#[derive(Clone)]
pub(crate) struct CandidateDraft {
    pub session: SessionRef,
    pub spec: CandidateSpec,
    pub catalog: CandidateCatalog,
    pub preview: Option<CandidatePreview>,
    pending_selector: Option<String>,
    pending_rule: Option<usize>,
}

impl CandidateDraft {
    pub fn new(session: SessionRef, catalog: CandidateCatalog) -> Self {
        let feedback = if catalog.mode == crate::evolution::control::EvolutionMode::Automatic {
            catalog
                .judge_model
                .clone()
                .map_or(FeedbackChoice::Manual, |model| FeedbackChoice::Judge {
                    model,
                })
        } else {
            FeedbackChoice::Manual
        };
        Self {
            spec: CandidateSpec {
                block_id: format!("coding-{}", uuid::Uuid::new_v4().simple()),
                predecessor: None,
                source: session.source.clone(),
                rules: vec![],
                rationale: String::new(),
                independence_rationale: String::new(),
                feedback,
            },
            session,
            catalog,
            preview: None,
            pending_selector: None,
            pending_rule: None,
        }
    }

    pub fn revise(
        session: SessionRef,
        catalog: CandidateCatalog,
        spec: CandidateSpec,
    ) -> Result<Self> {
        ensure!(
            spec.source == session.source && spec.predecessor.is_some(),
            "revision belongs to a different agent scope"
        );
        Ok(Self {
            session,
            catalog,
            spec,
            preview: None,
            pending_selector: None,
            pending_rule: None,
        })
    }

    pub fn menu(&self) -> Selector {
        let mut rows = vec![
            SelectorRow::new("name", "Experiment name", &self.spec.block_id),
            SelectorRow::new(
                "add",
                "Add a routing change",
                "Keep related rules in the same candidate block",
            ),
            SelectorRow::new("feedback", "Evaluation source", self.spec.feedback.label()),
            SelectorRow::new(
                "rationale",
                "Why try this candidate?",
                if self.spec.rationale.is_empty() {
                    "Required"
                } else {
                    &self.spec.rationale
                },
            ),
            SelectorRow::new(
                "independence",
                "Relationship to other experiments",
                if self.spec.independence_rationale.is_empty() {
                    "Required"
                } else {
                    &self.spec.independence_rationale
                },
            ),
        ];
        if self.spec.predecessor.is_some() {
            rows.retain(|row| row.id != "name" && row.id != "add");
        }
        for (i, rule) in self.spec.rules.iter().enumerate() {
            if self.spec.predecessor.is_some() {
                rows.push(SelectorRow::new(
                    format!("change:{i}"),
                    format!("Change candidate: {}", rule.selector),
                    format!(
                        "Baseline: {}; candidate: {}{}",
                        rule.baseline_route,
                        rule.challenger_route,
                        rule.fingerprint
                            .as_ref()
                            .map(|f| format!("; request pattern: {f}"))
                            .unwrap_or_default()
                    ),
                ));
                continue;
            }
            rows.push(SelectorRow::new(
                format!("remove:{i}"),
                format!("Remove: {} → {}", rule.selector, rule.challenger_route),
                "Remove this change from the draft",
            ));
        }
        rows.push(SelectorRow::new(
            "preview",
            "Review experiment",
            "Validate routes and inspect the complete trial before registering",
        ));
        rows.push(SelectorRow::new(
            "discard",
            "Discard candidate draft",
            "Keep current routing and experiments",
        ));
        Selector::new(
            "evolution:candidate:form",
            "Candidate policy experiment",
            format!(
                "Agent: {}. Applies to future recorded sessions. Mode and routes are rechecked in the preview.",
                self.spec.source
            ),
            rows,
        )
    }

    fn panel(self, selector: Selector) -> Panel {
        let mut panel = Panel::selector(selector);
        panel.candidate = Some(self);
        panel
    }

    pub fn reviewed(mut self, preview: CandidatePreview) -> Panel {
        let mut text = format!(
            "Experiment: {}\nAgent: {}\nEvaluation: {}\nCurrent evolution mode: {}\n\n",
            preview.spec.block_id,
            preview.spec.source,
            preview.spec.feedback.label(),
            mode_name(preview.mode)
        );
        if let Some(previous) = &preview.spec.predecessor {
            text.push_str(&format!("Previous experiment: {previous}\n"));
            text.push_str(if preview.reset_to_configured {
                "The previous route contract changed. This trial starts from the currently configured baseline.\n\n"
            } else {
                "This trial inherits the supported baseline. Old sessions keep their experiment, and a later withdrawal of inherited evidence can withdraw this version.\n\n"
            });
        }
        for rule in &preview.definition.rules {
            text.push_str(&format!(
                "{}: {} → {}\n",
                rule.selector, rule.baseline_route, rule.challenger_route
            ));
        }
        let config = &preview.definition.bandit;
        text.push_str(&format!(
            "\nReason: {}\nRelationship to other experiments: {}\n\nInitial candidate exposure: {:.1}%\nMaximum trial exposure: {:.1}%\nOutstanding candidate limit: {}\nTotal candidate trial limit: {}\nQuality floor: {:.2}\nPermitted mean quality gap from baseline: {:.2}\n\n",
            preview.spec.rationale, preview.spec.independence_rationale,
            f64::from(config.initial_exposure_ppm) / 10000.0, f64::from(config.maximum_exposure_ppm) / 10000.0,
            config.maximum_pending_challenger, config.maximum_challenger_sessions,
            f64::from(config.minimum_quality_ppm) / 1000000.0, f64::from(config.noninferiority_margin_ppm) / 1000000.0));
        text.push_str("Promotion also requires comparable feedback and complete resource evidence. Unknown feedback does not count as success. A session keeps its trial assignment, including fallback. Runtime compatibility checks can retain the baseline.\n\n");
        for description in preview.route_descriptions.values() {
            text.push_str(description);
            text.push_str("\n\n");
        }
        text.push_str("Registering preserves the current evolution mode. Enabled evolution may enroll future recorded sessions; the current session is not reassigned. The selected evaluator defines comparable numeric feedback; changing evaluators can leave an experiment waiting for matching feedback.\n");
        self.preview = Some(preview);
        let mut panel = self.panel(Selector::new(
            "evolution:candidate:submit", "Register candidate experiment",
            "Escape from the preview returns here. Registration rechecks the reviewed routes and settings.",
            vec![
                SelectorRow::new("register", "Register this experiment", "Save the reviewed policy block with the current evolution mode"),
                SelectorRow::new("preview", "Refresh preview", "Revalidate against the latest settings"),
                SelectorRow::new("edit", "Edit candidate", "Return to the draft"),
            ],
        ));
        panel.inspector = Some(Inspector::new("Candidate experiment preview", text));
        panel
    }

    pub fn step(mut self, selector: &str, choice: &str) -> Result<Panel> {
        if self.spec.predecessor.is_some() {
            ensure!(
                !(selector == "evolution:candidate:form"
                    && (matches!(choice, "name" | "add") || choice.starts_with("remove:")))
                    && !matches!(
                        selector,
                        "evolution:candidate:name" | "evolution:candidate:baseline"
                    ),
                "an experiment revision preserves its block name and routing matchers"
            );
        }
        let next = match selector {
            "evolution:candidate:form" => match choice {
                "open" => self.menu(),
                "name" => Selector::new("evolution:candidate:name", "Experiment name", "Use a distinct name for this policy experiment.",
                    vec![SelectorRow::new(&self.spec.block_id, &self.spec.block_id, "Keep this name")]).allow_custom("Experiment name"),
                "add" => Selector::new("evolution:candidate:baseline", "Current route to improve",
                    "Choose a configured preset or virtual route used by this agent.",
                    self.catalog.routes.iter().filter(|r| r.can_match).map(|route| {
                        let mut row = SelectorRow::new(&route.selector, &route.selector, "Inherit this route as the baseline");
                        if self.spec.rules.iter().any(|r| r.selector == route.selector) {
                            row = row.unavailable("Already included in this draft");
                        } else if let Some(existing) = self.catalog.reserved_matchers.iter().find(|r| r.source == self.spec.source && r.selector == route.selector) {
                            row = row.unavailable(format!("Already covered by experiment {}", existing.block_id));
                        }
                        row
                    }).collect()).allow_custom("Configured preset or virtual route"),
                "feedback" => {
                    let mut rows = vec![SelectorRow::new("manual", "Manual rubric review", "Use scores submitted through the local review flow")];
                    if let Some(model) = &self.catalog.judge_model {
                        rows.push(SelectorRow::new("judge", format!("Automatic judge: {model}"), "Use this configured evaluator's feedback"));
                    }
                    Selector::new("evolution:candidate:feedback", "Evaluation source", "This selects the experiment's comparable feedback; it does not change evolution mode.", rows)
                }
                "rationale" => Selector::new("evolution:candidate:rationale", "Why try this candidate?", "Choose the reason or describe the recorded evidence motivating this trial.",
                    vec![SelectorRow::new("Reduce total session cost", "Reduce total session cost", "Compare complete costs while retaining quality"),
                         SelectorRow::new("Improve delivery quality", "Improve delivery quality", "Investigate a candidate with a quality advantage")]).allow_custom("Reason for this trial"),
                "independence" => Selector::new("evolution:candidate:independence", "Relationship to other experiments",
                    "Related changes should share one block. Separately running blocks require an independence assumption.",
                    vec![SelectorRow::new("Related changes are grouped in this block; other blocks are assumed independent",
                        "Related changes are in this block", "Record this explicit assumption; it is not statistical proof")]).allow_custom("Explain independence or included interactions"),
                "discard" => {
                    let mut panel = Panel::inspector("Candidate discarded", "No experiment was registered.");
                    panel.clear_drafts = true;
                    return Ok(panel);
                }
                _ if choice.starts_with("change:") => {
                    ensure!(self.spec.predecessor.is_some(), "choose a revision first");
                    let index: usize = choice.trim_start_matches("change:").parse()?;
                    let rule = self.spec.rules.get(index).context("routing change is unavailable")?;
                    self.pending_rule = Some(index);
                    self.pending_selector = Some(rule.baseline_route.clone());
                    Selector::new("evolution:candidate:target", "Candidate route", format!("Inherited baseline: {}. Applies to {}.", rule.baseline_route, rule.selector),
                        self.catalog.routes.iter().map(|route| SelectorRow::new(&route.selector, &route.selector, "Use this route in the next trial")).collect())
                        .allow_custom("Configured candidate route or model")
                }
                _ if choice.starts_with("remove:") => {
                    let index: usize = choice.trim_start_matches("remove:").parse()?;
                    ensure!(index < self.spec.rules.len(), "routing change is unavailable");
                    self.spec.rules.remove(index);
                    self.preview = None;
                    self.menu()
                }
                _ => bail!("Unknown candidate action"),
            },
            "evolution:candidate:baseline" => {
                ensure!(!choice.trim().is_empty(), "choose a current route");
                ensure!(!self.spec.rules.iter().any(|r| r.selector == choice), "route is already in the draft");
                ensure!(!self.catalog.reserved_matchers.iter().any(|r| r.source == self.spec.source && r.selector == choice),
                    "route is already covered by an experiment");
                self.pending_selector = Some(choice.into());
                Selector::new("evolution:candidate:target", "Candidate route",
                    format!("Baseline: {choice}. Select a complete route including its configured fallback."),
                    self.catalog.routes.iter().filter(|r| r.selector != choice).map(|route|
                        SelectorRow::new(&route.selector, &route.selector, "Try this configured route")).collect())
                    .allow_custom("Configured candidate route or model")
            }
            "evolution:candidate:target" => {
                let baseline = self.pending_selector.take().context("Choose a baseline route first")?;
                ensure!(!choice.trim().is_empty(), "candidate route is required");
                if let Some(index) = self.pending_rule.take() {
                    self.spec.rules.get_mut(index).context("routing change is unavailable")?.challenger_route = choice.into();
                } else {
                    ensure!(choice != baseline, "candidate must change the route");
                    self.spec.rules.push(BlockRule {selector: baseline.clone(), fingerprint: None, baseline_route: baseline, challenger_route: choice.into()});
                }
                self.preview = None;
                self.menu()
            }
            "evolution:candidate:name" => {
                ensure!(!choice.trim().is_empty(), "experiment name is required");
                self.spec.block_id = choice.trim().into(); self.preview = None; self.menu()
            }
            "evolution:candidate:rationale" => {
                ensure!(!choice.trim().is_empty(), "trial rationale is required");
                self.spec.rationale = choice.trim().into(); self.preview = None; self.menu()
            }
            "evolution:candidate:independence" => {
                ensure!(!choice.trim().is_empty(), "explain the relationship to other experiments");
                self.spec.independence_rationale = choice.trim().into(); self.preview = None; self.menu()
            }
            "evolution:candidate:feedback" => {
                self.spec.feedback = match choice {
                    "manual" => FeedbackChoice::Manual,
                    "judge" => FeedbackChoice::Judge {model: self.catalog.judge_model.clone().context("Choose a judge model first")?},
                    _ => bail!("Unknown evaluator"),
                };
                self.preview = None; self.menu()
            }
            "evolution:candidate:submit" if choice == "edit" => {
                self.preview = None; self.menu()
            }
            _ => bail!("Unknown candidate draft action"),
        };
        Ok(self.panel(next))
    }
}
