//! A gpui [`Element`] that paints a [`TerminalSnapshot`] as a monospace grid.
//!
//! Minimal adaptation of Zed's `TerminalElement`: measure a single cell from a
//! monospace font, then for each row build style runs and shape one line per run
//! of identically-styled cells.

use gpui::{
    fill, point, size, App, Bounds, ContentMask, Element, ElementId, Font, FontFallbacks,
    FontFeatures, FontStyle, FontWeight, GlobalElementId, Hsla, InspectorElementId, IntoElement,
    LayoutId, Pixels, Point, Style, TextAlign, TextRun, Window,
};

use crate::terminal::terminal::{Cell, Color, TerminalSnapshot};

/// Measured monospace cell geometry plus the base font for shaping.
#[derive(Clone)]
pub struct GridMetrics {
    pub cell_width: Pixels,
    pub line_height: Pixels,
    pub font: Font,
    pub font_size: Pixels,
}

impl GridMetrics {
    /// Measure cell geometry for a monospace font of the given pixel size.
    pub fn measure(cx: &App, font_size: Pixels) -> Self {
        let font = Font {
            family: "monospace".into(),
            features: FontFeatures::default(),
            fallbacks: Some(FontFallbacks::from_fonts(vec![
                "Menlo".into(),
                "Monaco".into(),
                "Courier New".into(),
            ])),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
        };

        let text_system = cx.text_system();
        let font_id = text_system.resolve_font(&font);
        let cell_width = text_system
            .advance(font_id, font_size, 'm')
            .map(|adv| adv.width)
            .unwrap_or(font_size * 0.6);
        // A 1.3x line-height multiplier reads comfortably for terminal text.
        let line_height = (font_size * 1.3).round();

        Self {
            cell_width,
            line_height,
            font,
            font_size,
        }
    }

    /// Grid dimensions (cols, rows) that fit within `bounds`.
    pub fn grid_size(&self, bounds_size: gpui::Size<Pixels>) -> (u16, u16) {
        let cols = (f32::from(bounds_size.width) / f32::from(self.cell_width)).floor();
        let rows = (f32::from(bounds_size.height) / f32::from(self.line_height)).floor();
        (cols.max(1.0) as u16, rows.max(1.0) as u16)
    }
}

fn to_hsla(c: Color) -> Hsla {
    gpui::Rgba {
        r: c.r as f32 / 255.0,
        g: c.g as f32 / 255.0,
        b: c.b as f32 / 255.0,
        a: 1.0,
    }
    .into()
}

/// One painted span of same-styled cells within a row.
struct StyleRun {
    start_col: usize,
    text: String,
    fg: Color,
    bg: Color,
    bold: bool,
    cell_count: usize,
}

fn style_runs(row: &[Cell]) -> Vec<StyleRun> {
    let mut runs: Vec<StyleRun> = Vec::new();
    for (col, cell) in row.iter().enumerate() {
        let same = runs
            .last()
            .is_some_and(|run| run.fg == cell.fg && run.bg == cell.bg && run.bold == cell.bold);
        if same {
            if let Some(run) = runs.last_mut() {
                run.text.push(cell.ch);
                run.cell_count += 1;
            }
        } else {
            runs.push(StyleRun {
                start_col: col,
                text: cell.ch.to_string(),
                fg: cell.fg,
                bg: cell.bg,
                bold: cell.bold,
                cell_count: 1,
            });
        }
    }
    runs
}

/// Element that paints a single terminal snapshot.
pub struct TerminalElement {
    id: ElementId,
    snapshot: TerminalSnapshot,
    metrics: GridMetrics,
}

impl TerminalElement {
    pub fn new(id: ElementId, snapshot: TerminalSnapshot, metrics: GridMetrics) -> Self {
        Self {
            id,
            snapshot,
            metrics,
        }
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
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
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let origin = bounds.origin;
        let cell_width = self.metrics.cell_width;
        let line_height = self.metrics.line_height;
        let font = self.metrics.font.clone();
        let font_size = self.metrics.font_size;

        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for (line, row) in self.snapshot.rows.iter().enumerate() {
                let y = origin.y + line_height * line as f32;
                let runs = style_runs(row);

                // Paint backgrounds first.
                for run in &runs {
                    let bg = to_hsla(run.bg);
                    let x = origin.x + cell_width * run.start_col as f32;
                    let rect = Bounds::new(
                        point(x, y),
                        size(cell_width * run.cell_count as f32, line_height),
                    );
                    window.paint_quad(fill(rect, bg));
                }

                // Then glyphs, one shaped line per style run.
                for run in &runs {
                    if run.text.trim().is_empty() {
                        continue;
                    }
                    let weight = if run.bold {
                        FontWeight::BOLD
                    } else {
                        FontWeight::NORMAL
                    };
                    let text_run = TextRun {
                        len: run.text.len(),
                        font: Font {
                            weight,
                            ..font.clone()
                        },
                        color: to_hsla(run.fg),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    };
                    let shaped = window.text_system().shape_line(
                        run.text.clone().into(),
                        font_size,
                        std::slice::from_ref(&text_run),
                        Some(cell_width),
                    );
                    let x = origin.x + cell_width * run.start_col as f32;
                    let pos: Point<Pixels> = point(x, y);
                    let _ = shaped.paint(pos, line_height, TextAlign::Left, None, window, cx);
                }
            }
        });
    }
}
