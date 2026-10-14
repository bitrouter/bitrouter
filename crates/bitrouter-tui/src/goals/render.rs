use super::{Screen, State};
use crate::wrap::wrap;
use ratatui::layout::Position;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

pub(super) fn safe(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if ch.is_control() && ch != '\n' && ch != '\r' {
                if ch.len_utf8() == 1 { ' ' } else { '·' }
            } else {
                ch
            }
        })
        .collect()
}

fn line(text: impl AsRef<str>) -> Line<'static> {
    Line::raw(safe(text.as_ref()).replace(['\n', '\r'], " "))
}

fn compact(text: &str, width: u16) -> String {
    let mut output = String::new();
    let mut cells = 0;
    for grapheme in text.graphemes(true) {
        if cells + grapheme.width() > usize::from(width.saturating_sub(1)) {
            output.push('…');
            break;
        }
        output.push_str(grapheme);
        cells += grapheme.width();
    }
    output
}

fn window(
    rows: &mut Vec<Line<'static>>,
    body: Vec<Line<'static>>,
    selected: usize,
    width: u16,
    height: u16,
    tail: usize,
) {
    let prefix = rows.iter().flat_map(|row| wrap(row, width)).count();
    let mut available = usize::from(height.saturating_sub(2)).saturating_sub(prefix + tail);
    let indicator = body.len() > available && available > 1;
    if indicator {
        available -= 1;
    }
    let first = selected
        .saturating_sub(available / 2)
        .min(body.len().saturating_sub(available));
    let total = body.len();
    rows.extend(body.into_iter().skip(first).take(available));
    if indicator {
        rows.push(line(format!(
            "Rows {}–{} of {total}",
            first + 1,
            (first + available).min(total)
        )));
    }
}

pub(super) fn document(state: &State) -> Vec<Line<'static>> {
    let mut lines = vec![line("BRO / Goals")];
    if state.screen == Screen::New {
        lines.push(line(
            "New Goal — Enter delegates this objective; viewing does not run work.",
        ));
    } else if let Some(goal) = state.current_goal() {
        lines.push(line(format!("{} / {}", goal.workspace, goal.title)));
        if let Some(book) = state.notebook() {
            for event in &book.events {
                lines.push(Line::default());
                lines.extend(event.lines().map(line));
            }
        }
    }
    lines
}

pub(super) fn footer(
    state: &State,
    width: u16,
    height: u16,
) -> (Vec<Line<'static>>, Option<Position>) {
    let width = width.max(1);
    let height = height.max(1);
    if width < 40 || height < 12 {
        return (
            vec![line(
                "Resize to at least 40 columns × 12 rows. Ctrl-Q detaches.",
            )],
            None,
        );
    }
    let mut rows = Vec::new();
    let heading = if state.screen == Screen::Goal {
        state
            .current_goal()
            .map(|goal| {
                format!(
                    "Goal {} · {}",
                    goal.id.get(..8).unwrap_or(&goal.id),
                    goal.title
                )
            })
            .unwrap_or_else(|| "Current Goal".into())
    } else {
        match state.screen {
            Screen::Goals => "Goals",
            Screen::New => "New Goal",
            Screen::Workflow => "Workflow · read only",
            _ => "Task / evidence · read only",
        }
        .into()
    };
    rows.push(line(compact(&heading, width)).style(Style::default().fg(Color::Cyan)));
    if let Some(notice) = &state.notice {
        rows.push(line(compact(notice, width)));
    }
    let mut cursor = None;
    if let Some(dialog) = &state.budget_dialog {
        rows.push(line(match &dialog.goal {
            Some((id, _)) => format!("Budget for Goal {id} · opened limits pinned"),
            None => "Initial limits for this new Goal".into(),
        }));
        rows.push(line(
            "Total elapsed seconds: includes waits and pauses since creation.",
        ));
        rows.push(line(
            "Known model-token USD estimates: configured prices, not invoices.",
        ));
        rows.push(line(
            "Harness/tool/request fees may be unknown; no all-cost hard cap.",
        ));
        for (index, label) in ["Elapsed seconds", "Model-token USD"].iter().enumerate() {
            rows.push(line(label));
            let editor = &dialog.fields[index];
            let composer = crate::composer::layout(
                &safe(editor.text()),
                editor.cursor_byte(),
                width.saturating_sub(2).max(1),
                if index == dialog.selected { "›" } else { " " },
                Style::default(),
            );
            let first = composer.cursor_row.saturating_sub(1);
            if index == dialog.selected {
                let prefix = rows.iter().flat_map(|row| wrap(row, width)).count();
                cursor = Some(Position::new(
                    composer.cursor_column,
                    u16::try_from(prefix + composer.cursor_row.saturating_sub(first))
                        .unwrap_or(u16::MAX),
                ));
            }
            rows.extend(composer.rows.into_iter().skip(first).take(2));
        }
        rows.push(line(
            "Tab field · Enter save · Esc back · none removes that limit",
        ));
        rows.push(line(
            "Limits preserve cumulative use. Updating does not resume execution.",
        ));
    } else if let Some(dialog) = &state.decision {
        if dialog.stale {
            rows.push(line("Permission no longer pending"));
            rows.push(line(
                "Its status or binding changed. No response is available.",
            ));
            rows.push(line("Esc back · Ctrl-A reopen current requests"));
        } else if let Some(decision) = dialog.current() {
            rows.push(line(compact(&decision.label, width)));
            rows.push(line(compact(
                &format!(
                    "Permission {}/{} · requirements {} · {} · pinned",
                    dialog.index + 1,
                    dialog.choices.len(),
                    decision.requirements_revision,
                    decision.id
                ),
                width,
            )));
            let request_rows = dialog.request_capacity(height, state.notice.is_some());
            let content = match &dialog.material {
                Some(Ok(text)) => text.clone(),
                Some(Err(reason)) => format!(
                    "Request evidence unavailable: {reason}\nAllow unavailable; explicit denial remains possible."
                ),
                None => "Reading and verifying the original request; no default consent.".into(),
            };
            rows.extend(
                content
                    .lines()
                    .map(line)
                    .flat_map(|row| wrap(&row, width))
                    .chain(
                        decision
                            .details
                            .iter()
                            .chain(std::iter::once(&decision.request.label))
                            .map(line)
                            .flat_map(|row| wrap(&row, width)),
                    )
                    .skip(dialog.offset)
                    .take(request_rows),
            );
            let body = decision
                .options
                .iter()
                .enumerate()
                .map(|(index, option)| {
                    line(compact(
                        &format!(
                            "{} {} · {}{}",
                            if dialog.selected == Some(index) {
                                ">"
                            } else {
                                " "
                            },
                            option.label,
                            if option.allows { "allow" } else { "deny" },
                            if option.persistent {
                                " · persistent (harness scope)"
                            } else {
                                " · once"
                            }
                        ),
                        width,
                    ))
                })
                .collect();
            window(
                &mut rows,
                body,
                dialog.selected.unwrap_or(0),
                width,
                height,
                2,
            );
            rows.push(line(compact(
                "↑↓ choose · Enter respond · PgUp/PgDn read · Tab next · r retry",
                width,
            )));
            rows.push(line(compact(
                "Esc back · no default consent · saved response is not tool success",
                width,
            )));
        }
    } else if let Some((_, acceptance)) = &state.acceptance_dialog {
        rows.push(line(format!(
            "Explicit human acceptance · requirements {}",
            acceptance.requirements_revision
        )));
        rows.push(line(format!(
            "Delivery SHA-256: {}",
            acceptance.delivery_sha256
        )));
        rows.push(line("Enter accepts this exact delivery · Esc back"));
    } else {
        match state.screen {
            Screen::Goals => {
                let ids = state.directory_ids();
                rows.push(line(compact(
                    &format!(
                        "Filter: {} · f next · search/filter apply to this page",
                        state.filter.label()
                    ),
                    width,
                )));
                if ids.is_empty() {
                    rows.push(line("No matching Goals on this page. n creates a Goal."));
                }
                let selected = ids
                    .iter()
                    .position(|id| state.selected_goal.as_ref() == Some(id))
                    .unwrap_or(0);
                let mut body = Vec::new();
                for id in ids {
                    if let Some(goal) = state.goals.get(&id) {
                        body.push(line(compact(
                            &format!(
                                "{} {} · {} · {}",
                                if state.selected_goal.as_ref() == Some(&id) {
                                    ">"
                                } else {
                                    " "
                                },
                                goal.title,
                                goal.satisfaction,
                                if goal.needs_you {
                                    "needs you"
                                } else {
                                    &goal.reason
                                }
                            ),
                            width,
                        )));
                    }
                }
                window(
                    &mut rows,
                    body,
                    selected,
                    width,
                    height,
                    if state.search.is_some() { 6 } else { 2 },
                );
                rows.push(line(compact(
                    "↑↓ select · Enter observe · n new · / search page · PgUp/PgDn pages",
                    width,
                )));
                rows.push(line(compact(
                    "r refresh · opening changes observation only · Ctrl-Q detach",
                    width,
                )));
            }
            Screen::New => {
                rows.push(line(format!("Workspace: {}", state.new_workspace)));
                rows.push(line(format!(
                    "Initial limits: {} elapsed seconds · {} known token USD · Ctrl-B edit",
                    super::budget::decimal(state.new_budget.elapsed_ms, 3),
                    super::budget::decimal(state.new_budget.managed_token_estimates_micro_usd, 6)
                )));
                rows.push(line(if state.new_pending {
                    "Saving original operation; draft retained."
                } else {
                    "Describe the objective, boundaries and required evidence."
                }));
            }
            Screen::Goal => {
                if let Some(goal) = state.current_goal() {
                    rows.push(line(format!(
                        "{} · scope: this Goal · revision {}",
                        goal.title, goal.revision
                    )));
                    rows.push(line(format!(
                        "Satisfaction: {} · execution: {}",
                        goal.satisfaction, goal.execution
                    )));
                    rows.push(line(format!(
                        "Reason: {} · delivery: {}",
                        goal.reason, goal.delivery
                    )));
                    rows.push(line(&goal.freshness));
                    rows.push(line(format!("Objective: {}", goal.objective)));
                    for condition in goal.conditions.iter().take(3) {
                        rows.push(line(condition));
                    }
                    if goal.conditions.len() > 3 {
                        rows.push(line("More conditions retained in the Goal record."));
                    }
                    for budget in goal.budget.iter().take(3) {
                        rows.push(line(budget));
                    }
                    if state.notebook().is_some_and(|book| book.pending) {
                        rows.push(line(
                            "Saving original operation; other views remain available.",
                        ));
                    }
                    rows.push(line("Ctrl-P pause · Ctrl-X stop · Ctrl-R resume · Ctrl-B budget · Ctrl-A decisions/accept · Ctrl-I evidence"));
                }
            }
            Screen::Workflow => {
                if let Some(goal) = state.current_goal() {
                    rows.push(line(compact(
                        &format!("{} · browsing does not retarget input", goal.title),
                        width,
                    )));
                }
                let ids = state.task_ids();
                let selected = state
                    .notebook()
                    .and_then(|book| book.selected_task.as_ref())
                    .or_else(|| ids.first())
                    .cloned();
                let mut body = Vec::new();
                let mut selected_row = 0;
                if ids.is_empty() {
                    rows.push(line("No matching tasks; planning may still be pending."));
                }
                for id in ids {
                    if let Some(task) = state
                        .current_goal()
                        .and_then(|goal| goal.tasks.iter().find(|task| task.id == id))
                    {
                        let expanded = state
                            .notebook()
                            .is_some_and(|book| book.expanded_tasks.contains(&id));
                        if selected.as_ref() == Some(&id) {
                            selected_row = body.len();
                        }
                        // The status remains visible when a node is collapsed.
                        body.push(line(compact(
                            &format!(
                                "{}{} {} · {} · {}",
                                if selected.as_ref() == Some(&id) {
                                    ">"
                                } else {
                                    " "
                                },
                                if expanded { "−" } else { "+" },
                                task.status,
                                task.id,
                                task.title
                            ),
                            width,
                        )));
                        if expanded {
                            body.push(line(compact(&format!("    {}", task.resource), width)));
                            body.push(line(compact(
                                &format!(
                                    "    {} evidence references · Enter opens pinned details",
                                    task.artifacts.len()
                                ),
                                width,
                            )));
                        }
                        if expanded && !task.dependencies.is_empty() {
                            body.push(line(compact(
                                &format!("    requires: {}", task.dependencies.join(", ")),
                                width,
                            )));
                        }
                    }
                }
                window(
                    &mut rows,
                    body,
                    selected_row,
                    width,
                    height,
                    if state.search.is_some() { 5 } else { 1 },
                );
                rows.push(line(compact("↑↓ select · Space collapse · Enter details · / search · Ctrl-F fullscreen · Esc back", width)));
            }
            Screen::Detail => {
                if state.detail_updated {
                    rows.push(line(
                        "Newer state available; this opened version remains pinned.",
                    ));
                }
                if let Some(task) = &state.detail {
                    rows.push(line(format!(
                        "{} · {} · {}",
                        task.id, task.title, task.status
                    )));
                    rows.push(line(&task.resource));
                    let offset = state.notebook().map_or(0, |book| book.detail_offset);
                    if let Some(preview) = &state.artifact_preview {
                        rows.extend(
                            preview
                                .lines()
                                .map(line)
                                .flat_map(|row| wrap(&row, width))
                                .skip(offset)
                                .take(usize::from(height.saturating_sub(8))),
                        );
                    } else {
                        rows.extend(
                            task.details
                                .iter()
                                .flat_map(|text| text.lines())
                                .map(line)
                                .flat_map(|row| wrap(&row, width))
                                .skip(offset)
                                .take(usize::from(height.saturating_sub(9))),
                        );
                        let selected = state.notebook().map_or(0, |book| book.selected_artifact);
                        if let Some(artifact) = task.artifacts.get(selected) {
                            rows.push(line(format!(
                                "Evidence {}/{}: {}",
                                selected + 1,
                                task.artifacts.len(),
                                artifact.label
                            )));
                        }
                    }
                }
                rows.push(line("↑↓ read · Tab evidence · Enter inspect · Ctrl-F fullscreen · Esc back · read only"));
            }
        }
        if let Some(editor) = state.editor() {
            if state.search.is_some() {
                rows.push(line("Search this page / workflow"));
            } else {
                rows.push(line(
                    "Enter submits · Ctrl-J newline · Ctrl-G external editor · Ctrl-←/→ navigate",
                ));
            }
            let text = safe(editor.text());
            let composer = crate::composer::layout(
                &text,
                editor.cursor_byte(),
                width.saturating_sub(2).max(1),
                "›",
                Style::default(),
            );
            let first = composer.cursor_row.saturating_sub(2);
            let prefix = rows.iter().flat_map(|row| wrap(row, width)).count();
            cursor = Some(Position::new(
                composer.cursor_column,
                u16::try_from(prefix + composer.cursor_row.saturating_sub(first))
                    .unwrap_or(u16::MAX),
            ));
            rows.extend(composer.rows.into_iter().skip(first).take(3));
        }
    }
    let mut physical: Vec<_> = rows.iter().flat_map(|row| wrap(row, width)).collect();
    let limit = usize::from(height.saturating_sub(2).max(1));
    if physical.len() > limit {
        // Keep the input/action footer visible without erasing terminal history.
        let removed = physical.len() - limit;
        let keep = limit.saturating_sub(4).clamp(1, 5);
        physical.drain(keep..keep + removed);
        if let Some(position) = &mut cursor {
            if usize::from(position.y) >= keep + removed {
                position.y = position
                    .y
                    .saturating_sub(u16::try_from(removed).unwrap_or(u16::MAX));
            } else if usize::from(position.y) >= keep {
                cursor = None;
            }
        }
    }
    (physical, cursor)
}
