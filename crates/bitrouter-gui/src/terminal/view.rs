//! [`TerminalView`]: a focusable gpui view that renders a [`Terminal`] entity,
//! forwards key events to it as PTY input, and resizes it to fit the painted
//! bounds.

use gpui::{
    div, px, App, Bounds, Context, Element, ElementId, Entity, FocusHandle, Focusable,
    GlobalElementId, InspectorElementId, InteractiveElement, IntoElement, KeyDownEvent, LayoutId,
    ParentElement, Pixels, Render, Style, Styled, WeakEntity, Window,
};

use crate::terminal::element::{GridMetrics, TerminalElement};
use crate::terminal::entity::Terminal;

/// Monospace font size used to render the grid.
fn font_size() -> Pixels {
    px(14.0)
}

pub struct TerminalView {
    terminal: Entity<Terminal>,
    focus_handle: FocusHandle,
}

impl TerminalView {
    pub fn new(terminal: Entity<Terminal>, cx: &mut Context<Self>) -> Self {
        // Observe the terminal entity so PTY output triggers a repaint.
        cx.observe(&terminal, |_, _, cx| cx.notify()).detach();
        Self {
            terminal,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Encode a key event into the byte sequence a terminal expects, or `None`
    /// for keys we don't handle.
    fn encode_key(event: &KeyDownEvent) -> Option<Vec<u8>> {
        let keystroke = &event.keystroke;
        let mods = &keystroke.modifiers;
        let key = keystroke.key.as_str();

        // Control combinations for letters: Ctrl-A..Ctrl-Z -> 0x01..0x1A.
        if mods.control && !mods.alt && !mods.platform {
            if let Some(c) = key.chars().next() {
                if key.len() == 1 && c.is_ascii_alphabetic() {
                    let ctrl = (c.to_ascii_uppercase() as u8) - b'A' + 1;
                    return Some(vec![ctrl]);
                }
            }
        }

        match key {
            "enter" => return Some(b"\r".to_vec()),
            "tab" => return Some(b"\t".to_vec()),
            "backspace" => return Some(vec![0x7f]),
            "escape" => return Some(vec![0x1b]),
            "left" => return Some(b"\x1b[D".to_vec()),
            "right" => return Some(b"\x1b[C".to_vec()),
            "up" => return Some(b"\x1b[A".to_vec()),
            "down" => return Some(b"\x1b[B".to_vec()),
            "home" => return Some(b"\x1b[H".to_vec()),
            "end" => return Some(b"\x1b[F".to_vec()),
            "space" => return Some(b" ".to_vec()),
            _ => {}
        }

        // Printable text: prefer the typed character (handles shift/option).
        if let Some(text) = keystroke.key_char.as_ref() {
            if !text.is_empty() {
                return Some(text.as_bytes().to_vec());
            }
        }
        if key.chars().count() == 1 {
            return Some(key.as_bytes().to_vec());
        }
        None
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(bytes) = Self::encode_key(event) {
            self.terminal
                .update(cx, |terminal, _| terminal.input(&bytes));
        }
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.terminal.read(cx).snapshot();
        let metrics = GridMetrics::measure(cx, font_size());
        let grid = TerminalElement::new(
            ElementId::Name("terminal-grid".into()),
            snapshot,
            metrics.clone(),
        );

        div()
            .track_focus(&self.focus_handle)
            .key_context("Terminal")
            .on_key_down(cx.listener(Self::on_key_down))
            .size_full()
            .child(ResizingGrid {
                terminal: self.terminal.downgrade(),
                metrics,
                grid,
            })
    }
}

/// Wraps the [`TerminalElement`] so that, before painting, the terminal entity
/// is resized to match the cell grid that fits the current bounds.
struct ResizingGrid {
    terminal: WeakEntity<Terminal>,
    metrics: GridMetrics,
    grid: TerminalElement,
}

impl IntoElement for ResizingGrid {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ResizingGrid {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(ElementId::Name("terminal-resizer".into()))
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = gpui::relative(1.0).into();
        style.size.height = gpui::relative(1.0).into();
        let layout_id = window.request_layout(style, [], cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let (cols, rows) = self.metrics.grid_size(bounds.size);
        let _ = self
            .terminal
            .update(cx, |terminal, _| terminal.resize(rows, cols));
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Delegate the actual grid painting to the inner element. Both the
        // request-layout and prepaint states of `TerminalElement` are `()`.
        self.grid
            .prepaint(global_id, inspector_id, bounds, &mut (), window, cx);
        self.grid.paint(
            global_id,
            inspector_id,
            bounds,
            &mut (),
            &mut (),
            window,
            cx,
        );
    }
}
