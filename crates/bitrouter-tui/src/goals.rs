//! Goal presentation and key semantics. No runtime, protocol or worker owner.

use crate::editor::{Edit, Editor, press};
use crossterm::event::{Event, KeyCode, KeyModifiers};
use std::collections::{BTreeMap, BTreeSet};

pub mod budget;
mod decision;
mod history;
mod render;
#[cfg(test)]
mod tests;
pub mod view;

#[derive(Debug, Clone, Default)]
pub struct Goal {
    pub id: String,
    pub revision: u64,
    pub title: String,
    pub workspace: String,
    pub objective: String,
    pub satisfaction: String,
    pub execution: String,
    pub reason: String,
    pub needs_you: bool,
    pub delivery: String,
    pub freshness: String,
    pub conditions: Vec<String>,
    pub budget: Vec<String>,
    pub budget_limits: budget::Limits,
    pub tasks: Vec<Task>,
    pub decisions: Vec<Decision>,
    pub acceptance: Option<Acceptance>,
    pub artifacts: Vec<Artifact>,
    pub facts: Facts,
}

/// Independent snapshot facts; filters never infer state from rendered prose.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub running: bool,
    pub completed: bool,
    pub paused: bool,
    pub budget_limited: bool,
    pub blocked: bool,
    pub has_results: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Filter {
    #[default]
    All,
    Running,
    NeedsYou,
    Results,
    Completed,
    Paused,
    BudgetLimited,
    Blocked,
}

impl Filter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Running,
            Self::Running => Self::NeedsYou,
            Self::NeedsYou => Self::Results,
            Self::Results => Self::Completed,
            Self::Completed => Self::Paused,
            Self::Paused => Self::BudgetLimited,
            Self::BudgetLimited => Self::Blocked,
            Self::Blocked => Self::All,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Running => "running",
            Self::NeedsYou => "needs you",
            Self::Results => "has results",
            Self::Completed => "completed",
            Self::Paused => "paused",
            Self::BudgetLimited => "budget limited",
            Self::Blocked => "blocked",
        }
    }
    fn includes(self, goal: &Goal) -> bool {
        match self {
            Self::All => true,
            Self::Running => goal.facts.running,
            Self::NeedsYou => goal.needs_you,
            Self::Results => goal.facts.has_results,
            Self::Completed => goal.facts.completed,
            Self::Paused => goal.facts.paused,
            Self::BudgetLimited => goal.facts.budget_limited,
            Self::Blocked => goal.facts.blocked,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub status: String,
    pub dependencies: Vec<String>,
    pub resource: String,
    pub details: Vec<String>,
    pub artifacts: Vec<Artifact>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub id: String,
    pub requirements_revision: u64,
    pub label: String,
    pub details: Vec<String>,
    pub request: Artifact,
    pub options: Vec<DecisionOption>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionOption {
    pub id: String,
    pub label: String,
    pub allows: bool,
    pub persistent: bool,
}

#[derive(Debug, Clone)]
pub struct Acceptance {
    pub attempt_id: String,
    pub requirements_revision: u64,
    pub delivery_sha256: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Screen {
    #[default]
    Goals,
    Goal,
    New,
    Workflow,
    Detail,
    History,
}

/// These are client requests; receipt, validation and effects stay in the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Observe(String),
    Submit {
        goal_id: String,
        text: String,
    },
    Create {
        text: String,
        limits: budget::Limits,
    },
    SetBudget {
        goal_id: String,
        original: budget::Limits,
        limits: budget::Limits,
    },
    Control {
        goal_id: String,
        intent: Control,
    },
    Respond {
        goal_id: String,
        decision_id: String,
        requirements_revision: u64,
        option_id: String,
    },
    Accept {
        goal_id: String,
        attempt_id: String,
        requirements_revision: u64,
        delivery_sha256: String,
    },
    Inspect {
        goal_id: String,
        artifact_id: String,
    },
    ExternalEditor {
        goal_id: Option<String>,
        text: String,
    },
    Refresh,
    NextPage,
    PreviousPage,
    ReadHistory {
        goal_id: String,
        after: u64,
        cutoff: Option<u64>,
        epoch: u64,
    },
    Detach,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Pause,
    Stop,
    Run,
}

#[derive(Default)]
pub struct Notebook {
    pub editor: Editor,
    pub events: Vec<String>,
    pub omitted_events: usize,
    pub selected_task: Option<String>,
    pub detail_offset: usize,
    pub selected_artifact: usize,
    pub workflow_search: String,
    pub expanded_tasks: BTreeSet<String>,
    pub pending: bool,
}

pub struct State {
    pub screen: Screen,
    pub current: Option<String>,
    pub goals: BTreeMap<String, Goal>,
    pub directory: Vec<String>,
    pub selected_goal: Option<String>,
    pub filter: Filter,
    pub notebooks: BTreeMap<String, Notebook>,
    pub new_editor: Editor,
    pub new_pending: bool,
    pub new_budget: budget::Limits,
    pub new_workspace: String,
    pub notice: Option<String>,
    pub next_page: bool,
    pub previous_page: bool,
    pub detail: Option<Task>,
    pub detail_revision: u64,
    pub detail_updated: bool,
    pub artifact_preview: Option<String>,
    pub fullscreen: bool,
    pub viewport: Option<(u16, u16)>,
    archived_detail: Option<(Task, usize, usize)>,
    search: Option<Editor>,
    directory_search: String,
    decision: Option<decision::Dialog>,
    acceptance_dialog: Option<(String, Acceptance)>,
    budget_dialog: Option<budget::Dialog>,
    history: Option<history::History>,
}

impl State {
    pub fn new(workspace: String) -> Self {
        Self {
            screen: Screen::Goals,
            current: None,
            goals: BTreeMap::new(),
            directory: Vec::new(),
            selected_goal: None,
            filter: Filter::default(),
            notebooks: BTreeMap::new(),
            new_editor: Editor::default(),
            new_pending: false,
            new_budget: budget::Limits::default(),
            new_workspace: workspace,
            notice: None,
            next_page: false,
            previous_page: false,
            detail: None,
            detail_revision: 0,
            detail_updated: false,
            artifact_preview: None,
            fullscreen: false,
            search: None,
            directory_search: String::new(),
            decision: None,
            acceptance_dialog: None,
            budget_dialog: None,
            viewport: None,
            archived_detail: None,
            history: None,
        }
    }

    pub fn receive(&mut self, goal: Goal) {
        if self
            .goals
            .get(&goal.id)
            .is_some_and(|previous| previous.revision > goal.revision)
        {
            return;
        }
        if let Some(dialog) = &mut self.decision
            && dialog.goal_id == goal.id
            && dialog
                .current()
                .is_some_and(|pinned| !goal.decisions.iter().any(|fresh| fresh == pinned))
        {
            dialog.stale = true;
            dialog.selected = None;
            dialog.material = None;
        }
        if self.current.as_deref() == Some(&goal.id)
            && self.screen == Screen::Detail
            && goal.revision > self.detail_revision
        {
            self.detail_updated = true;
        }
        self.notebooks.entry(goal.id.clone()).or_default();
        self.goals.insert(goal.id.clone(), goal);
    }

    pub fn receive_directory(&mut self, goals: Vec<Goal>, next: bool, previous: bool) {
        let incoming: Vec<_> = goals.iter().map(|goal| goal.id.clone()).collect();
        self.directory.retain(|id| incoming.contains(id));
        for goal in goals {
            if !self.directory.contains(&goal.id) {
                self.directory.push(goal.id.clone());
            }
            self.receive(goal);
        }
        if !self
            .selected_goal
            .as_ref()
            .is_some_and(|id| self.directory.contains(id))
        {
            self.selected_goal = self.directory.first().cloned();
        }
        self.next_page = next;
        self.previous_page = previous;
        self.notebooks.retain(|id, book| {
            self.current.as_ref() == Some(id)
                || self.directory.contains(id)
                || book.pending
                || !book.editor.text().is_empty()
        });
        self.goals.retain(|id, _| {
            self.current.as_ref() == Some(id)
                || self.directory.contains(id)
                || self.notebooks.contains_key(id)
        });
    }

    pub fn observe(&mut self, id: String) {
        self.notebooks.entry(id.clone()).or_default();
        self.current = Some(id);
        self.screen = Screen::Goal;
        self.decision = None;
        self.acceptance_dialog = None;
        self.budget_dialog = None;
        self.search = None;
        self.fullscreen = false;
        self.archived_detail = None;
    }

    pub fn current_goal(&self) -> Option<&Goal> {
        self.current.as_ref().and_then(|id| self.goals.get(id))
    }
    pub fn notebook(&self) -> Option<&Notebook> {
        self.current.as_ref().and_then(|id| self.notebooks.get(id))
    }
    pub fn editor(&self) -> Option<&Editor> {
        if self.decision.is_some()
            || self.acceptance_dialog.is_some()
            || self.budget_dialog.is_some()
        {
            return None;
        }
        if let Some(search) = &self.search {
            return Some(search);
        }
        match self.screen {
            Screen::New => Some(&self.new_editor),
            Screen::Goal => self.notebook().map(|book| &book.editor),
            _ => None,
        }
    }

    pub fn pending(&mut self, goal_id: Option<&str>, value: bool) {
        if let Some(id) = goal_id {
            self.notebooks.entry(id.into()).or_default().pending = value;
        } else {
            self.new_pending = value;
        }
    }

    pub fn accepted(&mut self, goal_id: Option<&str>, text: &str) {
        let editor = if let Some(id) = goal_id {
            &mut self.notebooks.entry(id.into()).or_default().editor
        } else {
            &mut self.new_editor
        };
        if editor.text() == text {
            editor.clear();
        }
        editor.push_history(text.to_owned());
        editor.retain_recent_history();
        self.pending(goal_id, false);
    }

    pub fn event_line(&mut self, goal_id: &str, mut line: String) {
        if line.len() > 32 * 1024 {
            let end = line
                .char_indices()
                .map(|(index, _)| index)
                .take_while(|index| *index <= 32 * 1024)
                .last()
                .unwrap_or(0);
            line.truncate(end);
            line.push_str("\nDisplay truncated · Ctrl-O persistent history");
        }
        let book = self.notebooks.entry(goal_id.into()).or_default();
        if book.events.last() == Some(&line) {
            return;
        }
        book.events.push(line);
        let mut bytes: usize = book.events.iter().map(String::len).sum();
        while book.events.len() > 128 || bytes > 256 * 1024 {
            bytes = bytes.saturating_sub(book.events.remove(0).len());
            book.omitted_events = book.omitted_events.saturating_add(1);
        }
    }

    fn directory_ids(&self) -> Vec<String> {
        let search = self.directory_search.to_lowercase();
        self.directory
            .iter()
            .filter(|id| {
                self.goals.get(*id).is_some_and(|goal| {
                    format!(
                        "{} {} {} {}",
                        goal.title, goal.workspace, goal.reason, goal.id
                    )
                    .to_lowercase()
                    .contains(&search)
                        && self.filter.includes(goal)
                })
            })
            .cloned()
            .collect()
    }

    fn task_ids(&self) -> Vec<String> {
        let search = self
            .notebook()
            .map(|book| book.workflow_search.to_lowercase())
            .unwrap_or_default();
        self.current_goal()
            .map(|goal| {
                goal.tasks
                    .iter()
                    .filter(|task| {
                        format!("{} {} {}", task.id, task.title, task.status)
                            .to_lowercase()
                            .contains(&search)
                    })
                    .map(|task| task.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Right-side keys only navigate/read; they never reach the middle composer.
    pub fn event(&mut self, event: &Event) -> Vec<Effect> {
        let Some(key) = press(event) else {
            if let Event::Paste(text) = event {
                if let Some(dialog) = &mut self.budget_dialog {
                    dialog.fields[dialog.selected].paste(text);
                } else if self.acceptance_dialog.is_some() || self.decision.is_some() {
                    // A paste in a pinned decision cannot edit the hidden draft.
                } else if let Some(search) = &mut self.search {
                    search.paste(text);
                } else if self.screen == Screen::New && !self.new_pending {
                    self.new_editor.paste(text);
                } else if self.screen == Screen::Goal
                    && self.decision.is_none()
                    && let Some(id) = &self.current
                    && let Some(book) = self.notebooks.get_mut(id)
                    && !book.pending
                {
                    book.editor.paste(text);
                }
            }
            return Vec::new();
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('q') {
            return vec![Effect::Detach];
        }
        if ctrl && key.code == KeyCode::Left {
            self.fullscreen = false;
            self.screen = Screen::Goals;
            self.search = None;
            self.decision = None;
            self.acceptance_dialog = None;
            self.budget_dialog = None;
            return vec![Effect::Refresh];
        }
        if ctrl && key.code == KeyCode::Right && self.current.is_some() {
            self.fullscreen = false;
            self.screen = Screen::Workflow;
            self.search = None;
            self.decision = None;
            self.acceptance_dialog = None;
            self.budget_dialog = None;
            return Vec::new();
        }
        if let Some(search) = &mut self.search {
            if key.code == KeyCode::Esc || key.code == KeyCode::Enter {
                self.search = None;
            } else {
                search.apply(*key);
                if self.screen == Screen::Goals {
                    self.directory_search = search.text().into();
                } else if let Some(id) = &self.current
                    && let Some(book) = self.notebooks.get_mut(id)
                {
                    book.workflow_search = search.text().into();
                }
            }
            return Vec::new();
        }
        if self.screen == Screen::History {
            return self.history_event(key.code);
        }
        if self.screen == Screen::Workflow || self.screen == Screen::Detail {
            return self.browse(key.code, ctrl);
        }
        if self.decision.is_some()
            && self
                .viewport
                .is_some_and(|(width, height)| width < 40 || height < 12)
            && key.code != KeyCode::Esc
        {
            return Vec::new();
        }
        if self
            .viewport
            .is_some_and(|(width, height)| width < 40 || height < 12)
            && (key.code == KeyCode::Enter
                || ctrl && matches!(key.code, KeyCode::Char('p' | 'x' | 'r' | 'a')))
        {
            self.notice = Some("Resize before submitting or controlling the current Goal".into());
            return Vec::new();
        }
        if self.screen == Screen::Goals {
            let ids = self.directory_ids();
            match key.code {
                KeyCode::Up => move_selection(&mut self.selected_goal, &ids, false),
                KeyCode::Down => move_selection(&mut self.selected_goal, &ids, true),
                KeyCode::Enter => {
                    if let Some(id) = self.selected_goal.clone().filter(|id| ids.contains(id)) {
                        self.observe(id.clone());
                        return vec![Effect::Observe(id)];
                    }
                }
                KeyCode::Char('n') if !ctrl => {
                    self.screen = Screen::New;
                }
                KeyCode::Char('/') if !ctrl => {
                    self.search = Some(Editor::default());
                }
                KeyCode::Char('r') if !ctrl => return vec![Effect::Refresh],
                KeyCode::Char('f') if !ctrl => {
                    self.filter = self.filter.next();
                    let ids = self.directory_ids();
                    if !self
                        .selected_goal
                        .as_ref()
                        .is_some_and(|id| ids.contains(id))
                    {
                        self.selected_goal = ids.first().cloned();
                    }
                }
                KeyCode::PageDown if self.next_page => return vec![Effect::NextPage],
                KeyCode::PageUp if self.previous_page => return vec![Effect::PreviousPage],
                KeyCode::Esc | KeyCode::Left if self.current.is_some() => {
                    self.screen = Screen::Goal;
                }
                _ => {}
            }
            return Vec::new();
        }
        if self.decision.is_some() {
            return self.decision_event(key.code);
        }
        if self.budget_dialog.is_some() {
            return self.budget_event(*key);
        }
        if let Some((id, acceptance)) = &self.acceptance_dialog {
            if key.code == KeyCode::Esc {
                self.acceptance_dialog = None;
                return Vec::new();
            }
            if key.code == KeyCode::Enter {
                let effect = Effect::Accept {
                    goal_id: id.clone(),
                    attempt_id: acceptance.attempt_id.clone(),
                    requirements_revision: acceptance.requirements_revision,
                    delivery_sha256: acceptance.delivery_sha256.clone(),
                };
                self.acceptance_dialog = None;
                return vec![effect];
            }
            return Vec::new();
        }
        if ctrl && key.code == KeyCode::Char('n') {
            self.screen = Screen::New;
            return Vec::new();
        }
        if self.screen == Screen::New {
            if key.code == KeyCode::Esc {
                self.screen = Screen::Goals;
                return Vec::new();
            }
            if self.new_pending {
                return Vec::new();
            }
            if ctrl && key.code == KeyCode::Char('b') {
                self.budget_dialog = Some(budget::Dialog::new(None, self.new_budget.clone()));
                return Vec::new();
            }
            return match self.new_editor.apply(*key) {
                Edit::Submitted if !self.new_editor.text().trim().is_empty() => {
                    vec![Effect::Create {
                        text: self.new_editor.text().into(),
                        limits: self.new_budget.clone(),
                    }]
                }
                Edit::Ended => vec![Effect::Detach],
                Edit::OpenExternalEditor => vec![Effect::ExternalEditor {
                    goal_id: None,
                    text: self.new_editor.text().into(),
                }],
                _ => Vec::new(),
            };
        }
        let Some(id) = self.current.clone() else {
            return Vec::new();
        };
        if ctrl {
            if key.code == KeyCode::Char('o') {
                return vec![self.open_history(id)];
            }
            if key.code == KeyCode::Char('b') {
                if let Some(goal) = self.current_goal() {
                    self.budget_dialog =
                        Some(budget::Dialog::new(Some(id), goal.budget_limits.clone()));
                }
                return Vec::new();
            }
            if key.code == KeyCode::Char('i') {
                if let Some(goal) = self.current_goal() {
                    let detail = Task {
                        id: goal.id.clone(),
                        title: "Goal requirements and delivery".into(),
                        status: goal.satisfaction.clone(),
                        resource: format!(
                            "Requirements / Goal snapshot revision {}",
                            goal.revision
                        ),
                        dependencies: Vec::new(),
                        details: std::iter::once(goal.objective.clone())
                            .chain(goal.conditions.clone())
                            .chain(goal.budget.clone())
                            .collect(),
                        artifacts: goal.artifacts.clone(),
                    };
                    let revision = goal.revision;
                    self.detail = Some(detail);
                    self.archived_detail = None;
                    self.detail_revision = revision;
                    self.detail_updated = false;
                    self.artifact_preview = None;
                    self.screen = Screen::Detail;
                }
                return Vec::new();
            }
            let intent = match key.code {
                KeyCode::Char('p') => Some(Control::Pause),
                KeyCode::Char('x') => Some(Control::Stop),
                KeyCode::Char('r') => Some(Control::Run),
                _ => None,
            };
            if let Some(intent) = intent {
                return vec![Effect::Control {
                    goal_id: id,
                    intent,
                }];
            }
            if key.code == KeyCode::Char('a') {
                if let Some(goal) = self.current_goal() {
                    if !goal.decisions.is_empty() {
                        let dialog = decision::Dialog::new(id, goal.decisions.clone());
                        let effect = dialog.inspect();
                        self.decision = Some(dialog);
                        return effect.into_iter().collect();
                    } else if let Some(acceptance) = &goal.acceptance {
                        self.acceptance_dialog = Some((id, acceptance.clone()));
                    }
                }
                return Vec::new();
            }
        }
        let Some(book) = self.notebooks.get_mut(&id) else {
            return Vec::new();
        };
        if key.modifiers.is_empty() && book.editor.is_empty() {
            match key.code {
                KeyCode::Left => {
                    self.screen = Screen::Goals;
                    return vec![Effect::Refresh];
                }
                KeyCode::Right => {
                    self.screen = Screen::Workflow;
                    return Vec::new();
                }
                KeyCode::Esc => {
                    self.screen = Screen::Goals;
                    return Vec::new();
                }
                _ => {}
            }
        }
        if book.pending {
            return Vec::new();
        }
        match book.editor.apply(*key) {
            Edit::Submitted if !book.editor.text().trim().is_empty() => vec![Effect::Submit {
                goal_id: id,
                text: book.editor.text().into(),
            }],
            Edit::Ended => vec![Effect::Detach],
            Edit::OpenExternalEditor => vec![Effect::ExternalEditor {
                goal_id: Some(id),
                text: book.editor.text().into(),
            }],
            _ => Vec::new(),
        }
    }

    fn browse(&mut self, code: KeyCode, ctrl: bool) -> Vec<Effect> {
        if ctrl && code == KeyCode::Char('f') {
            self.fullscreen = !self.fullscreen;
            return Vec::new();
        }
        if code == KeyCode::Esc || code == KeyCode::Left {
            if self.fullscreen {
                self.fullscreen = false;
                return Vec::new();
            }
            if self.artifact_preview.take().is_some() {
                return Vec::new();
            }
            if let Some((detail, offset, selected)) = self.archived_detail.take() {
                self.detail = Some(detail);
                if let Some(id) = &self.current
                    && let Some(book) = self.notebooks.get_mut(id)
                {
                    book.detail_offset = offset;
                    book.selected_artifact = selected;
                }
                return Vec::new();
            }
            self.screen = if self.screen == Screen::Detail {
                Screen::Workflow
            } else {
                Screen::Goal
            };
            return Vec::new();
        }
        if ctrl {
            return Vec::new();
        }
        if self.screen == Screen::Detail {
            if let Some(id) = &self.current
                && let Some(book) = self.notebooks.get_mut(id)
            {
                match code {
                    KeyCode::Up => book.detail_offset = book.detail_offset.saturating_sub(1),
                    KeyCode::Down => book.detail_offset = book.detail_offset.saturating_add(1),
                    KeyCode::Tab => {
                        book.selected_artifact = book.selected_artifact.saturating_add(1)
                            % self
                                .detail
                                .as_ref()
                                .map_or(1, |task| task.artifacts.len().max(1))
                    }
                    _ => {}
                }
            }
            if code == KeyCode::Enter
                && self.artifact_preview.is_none()
                && let (Some(goal_id), Some(artifact)) = (
                    &self.current,
                    self.detail.as_ref().and_then(|task| {
                        task.artifacts
                            .get(self.notebook().map_or(0, |book| book.selected_artifact))
                    }),
                )
            {
                return vec![Effect::Inspect {
                    goal_id: goal_id.clone(),
                    artifact_id: artifact.id.clone(),
                }];
            }
            return Vec::new();
        }
        let ids = self.task_ids();
        let Some(id) = &self.current else {
            return Vec::new();
        };
        let Some(book) = self.notebooks.get_mut(id) else {
            return Vec::new();
        };
        match code {
            KeyCode::Up => move_selection(&mut book.selected_task, &ids, false),
            KeyCode::Down => move_selection(&mut book.selected_task, &ids, true),
            KeyCode::Char('/') => self.search = Some(Editor::default()),
            KeyCode::Char(' ') => {
                if let Some(selected) = book.selected_task.clone().or_else(|| ids.first().cloned())
                {
                    if !book.expanded_tasks.remove(&selected) {
                        book.expanded_tasks.insert(selected.clone());
                    }
                    book.selected_task = Some(selected);
                }
            }
            KeyCode::Enter => {
                let selected = book.selected_task.clone().or_else(|| ids.first().cloned());
                if let Some(goal) = self.current_goal()
                    && let Some(task) = goal
                        .tasks
                        .iter()
                        .find(|task| Some(&task.id) == selected.as_ref())
                {
                    let detail = task.clone();
                    let revision = goal.revision;
                    self.detail = Some(detail);
                    self.archived_detail = None;
                    self.detail_revision = revision;
                    self.detail_updated = false;
                    self.artifact_preview = None;
                    self.screen = Screen::Detail;
                }
            }
            _ => {}
        }
        Vec::new()
    }
}

fn move_selection(selected: &mut Option<String>, ids: &[String], down: bool) {
    if ids.is_empty() {
        *selected = None;
        return;
    }
    let index = selected
        .as_ref()
        .and_then(|id| ids.iter().position(|candidate| candidate == id))
        .unwrap_or(0);
    let next = if down {
        index.saturating_add(1).min(ids.len().saturating_sub(1))
    } else {
        index.saturating_sub(1)
    };
    *selected = ids.get(next).cloned();
}
