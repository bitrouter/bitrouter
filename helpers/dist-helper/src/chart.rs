//! Registry chart SVG rendered from the committed `dist/registry` artifacts.
//!
//! The README embeds these files from an orphan `charts` branch, so nothing here
//! is committed to `main`; CI regenerates them whenever `registry/` changes. The
//! input is `dist/registry/{models,providers}.json` — the same published contract
//! the docs site reads — so the chart can never disagree with the catalog.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// One canonical model as published in `dist/registry/models.json`.
#[derive(Debug, Deserialize)]
struct ModelEntry {
    id: String,
    name: String,
    /// Only the count matters here; the entries themselves are not inspected.
    providers: Vec<serde::de::IgnoredAny>,
}

#[derive(Debug, Deserialize)]
struct ProviderEntry {
    billing: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    data: Vec<T>,
}

/// A lab's models, sorted by how many providers route to them.
struct LabGroup {
    label: String,
    models: Vec<ModelRow>,
}

struct ModelRow {
    name: String,
    routes: usize,
}

/// Totals rendered in the caption under the chart.
struct Totals {
    models: usize,
    providers: usize,
    routes: usize,
    subscription_providers: usize,
}

pub fn generate(root: &Path, out_dir: &Path) -> Result<()> {
    let dist = root.join("dist").join("registry");
    let models: Envelope<ModelEntry> = read_json(&dist.join("models.json"))?;
    let providers: Envelope<ProviderEntry> = read_json(&dist.join("providers.json"))?;

    let groups = group_by_lab(&models.data)?;
    let totals = Totals {
        models: models.data.len(),
        providers: providers.data.len(),
        routes: models.data.iter().map(|m| m.providers.len()).sum(),
        subscription_providers: providers
            .data
            .iter()
            .filter(|p| p.billing.as_deref() == Some("subscription"))
            .count(),
    };

    fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    for theme in [Theme::light(), Theme::dark()] {
        let path = out_dir.join(theme.file_name);
        let svg = render(&groups, &totals, &theme);
        fs::write(&path, svg).with_context(|| format!("writing {}", path.display()))?;
    }

    println!(
        "wrote {}/registry-by-lab{{,-dark}}.svg - {} models across {} labs, {} routes",
        out_dir.display(),
        totals.models,
        groups.len(),
        totals.routes
    );
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let raw = fs::read_to_string(path).with_context(|| {
        format!(
            "reading {} - run `cargo run -p dist-helper -- registry build` first",
            path.display()
        )
    })?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

/// Group models by their id namespace, taking each lab's display label from the
/// `"Lab: Model"` prefix already in `name` so no hardcoded lab table can drift.
fn group_by_lab(models: &[ModelEntry]) -> Result<Vec<LabGroup>> {
    let mut groups: Vec<LabGroup> = Vec::new();
    for model in models {
        let (_, short_id) = model
            .id
            .split_once('/')
            .with_context(|| format!("model id {} is not `namespace/model`", model.id))?;
        let Some((label, _)) = model.name.split_once(": ") else {
            bail!(
                "model {} has name {:?}, expected a `Lab: Model` prefix",
                model.id,
                model.name
            );
        };
        let row = ModelRow {
            name: short_id.to_string(),
            routes: model.providers.len(),
        };
        match groups.iter_mut().find(|g| g.label == label) {
            Some(group) => group.models.push(row),
            None => groups.push(LabGroup {
                label: label.to_string(),
                models: vec![row],
            }),
        }
    }
    for group in &mut groups {
        group
            .models
            .sort_by(|a, b| b.routes.cmp(&a.routes).then_with(|| a.name.cmp(&b.name)));
    }
    // Widest catalogs first so the grid fills top-down.
    groups.sort_by(|a, b| {
        b.models
            .len()
            .cmp(&a.models.len())
            .then_with(|| a.label.cmp(&b.label))
    });
    Ok(groups)
}

struct Theme {
    file_name: &'static str,
    canvas: &'static str,
    border: &'static str,
    header_fill: &'static str,
    header_text: &'static str,
    track: &'static str,
    bar: &'static str,
    text: &'static str,
    muted: &'static str,
}

impl Theme {
    fn light() -> Self {
        Self {
            file_name: "registry-by-lab.svg",
            canvas: "#ffffff",
            border: "#e4e4e7",
            header_fill: "#0a0a0a",
            header_text: "#fafafa",
            track: "#f4f4f5",
            bar: "#d4d4d8",
            text: "#0a0a0a",
            muted: "#71717a",
        }
    }

    fn dark() -> Self {
        Self {
            file_name: "registry-by-lab-dark.svg",
            canvas: "#0d1117",
            border: "#21262d",
            header_fill: "#e6edf3",
            header_text: "#0d1117",
            track: "#161b22",
            bar: "#30363d",
            text: "#e6edf3",
            muted: "#8b949e",
        }
    }
}

const WIDTH: f64 = 1200.0;
const MARGIN: f64 = 28.0;
const COLUMNS: usize = 4;
const GUTTER: f64 = 24.0;
const HEADER_H: f64 = 30.0;
const HEADER_GAP: f64 = 8.0;
const ROW_H: f64 = 22.0;
const ROW_GAP: f64 = 4.0;
const BAND_GAP: f64 = 26.0;
const TITLE_H: f64 = 62.0;
const CAPTION_H: f64 = 44.0;
const CELL_PAD: f64 = 10.0;
const COUNT_W: f64 = 34.0;
const LABEL_PAD: f64 = 8.0;
const FONT_SIZE: f64 = 11.0;
/// Monospace advance width at `FONT_SIZE`, used only to decide truncation.
const CHAR_W: f64 = 6.6;

fn column_width() -> f64 {
    (WIDTH - 2.0 * MARGIN - GUTTER * (COLUMNS as f64 - 1.0)) / COLUMNS as f64
}

fn group_height(model_count: usize) -> f64 {
    HEADER_H + HEADER_GAP + model_count as f64 * (ROW_H + ROW_GAP) - ROW_GAP
}

fn render(groups: &[LabGroup], totals: &Totals, theme: &Theme) -> String {
    let col_w = column_width();
    let bands: Vec<&[LabGroup]> = groups.chunks(COLUMNS).collect();
    let band_heights: Vec<f64> = bands
        .iter()
        .map(|band| {
            band.iter()
                .map(|g| group_height(g.models.len()))
                .fold(0.0_f64, f64::max)
        })
        .collect();
    let body_h: f64 =
        band_heights.iter().sum::<f64>() + BAND_GAP * band_heights.len().saturating_sub(1) as f64;
    let height = MARGIN + TITLE_H + body_h + CAPTION_H + MARGIN;

    let max_routes = groups
        .iter()
        .flat_map(|g| g.models.iter())
        .map(|m| m.routes)
        .max()
        .unwrap_or(1)
        .max(1) as f64;

    let mut svg = String::new();
    let _ = write!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {WIDTH:.0} {height:.0}" width="{WIDTH:.0}" height="{height:.0}" preserveAspectRatio="xMidYMid meet" role="img" aria-label="BitRouter model catalog grouped by lab, bar length is the number of providers routing each model">
<style>
  text {{ font-family: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace; fill: {text}; }}
  .title {{ font-size: 17px; font-weight: 600; }}
  .subtitle, .caption {{ font-size: 12px; fill: {muted}; }}
  .lab {{ font-size: 12px; font-weight: 600; letter-spacing: 0.06em; fill: {header_text}; }}
  .lab-count {{ font-size: 11px; fill: {header_text}; fill-opacity: 0.7; }}
  .model {{ font-size: {FONT_SIZE}px; }}
  .routes {{ font-size: {FONT_SIZE}px; fill: {muted}; }}
</style>
<rect x="0" y="0" width="{WIDTH:.0}" height="{height:.0}" rx="10" fill="{canvas}" stroke="{border}"/>
"#,
        text = theme.text,
        muted = theme.muted,
        header_text = theme.header_text,
        canvas = theme.canvas,
        border = theme.border,
    );

    let _ = write!(
        svg,
        r#"<text class="title" x="{x:.1}" y="{y:.1}">BitRouter model catalog</text>
<text class="subtitle" x="{x:.1}" y="{sy:.1}">Grouped by lab. Bar length = providers routing that model.</text>
"#,
        x = MARGIN,
        y = MARGIN + 20.0,
        sy = MARGIN + 40.0,
    );

    let mut band_y = MARGIN + TITLE_H;
    for (band, band_h) in bands.iter().zip(&band_heights) {
        for (index, group) in band.iter().enumerate() {
            let x = MARGIN + index as f64 * (col_w + GUTTER);
            render_group(&mut svg, group, x, band_y, col_w, max_routes, theme);
        }
        band_y += band_h + BAND_GAP;
    }

    let _ = write!(
        svg,
        r#"<text class="caption" x="{x:.1}" y="{y:.1}">{models} models · {providers} providers · {routes} model→provider routes · {subs} providers billed by subscription</text>
</svg>
"#,
        x = MARGIN,
        y = height - MARGIN - 10.0,
        models = totals.models,
        providers = totals.providers,
        routes = totals.routes,
        subs = totals.subscription_providers,
    );
    svg
}

fn render_group(
    svg: &mut String,
    group: &LabGroup,
    x: f64,
    y: f64,
    col_w: f64,
    max_routes: f64,
    theme: &Theme,
) {
    let _ = write!(
        svg,
        r#"<rect x="{x:.1}" y="{y:.1}" width="{col_w:.1}" height="{HEADER_H:.1}" rx="5" fill="{fill}"/>
<text class="lab" x="{tx:.1}" y="{ty:.1}">{label}</text>
<text class="lab-count" x="{cx:.1}" y="{ty:.1}" text-anchor="end">{count}</text>
"#,
        fill = theme.header_fill,
        tx = x + CELL_PAD,
        ty = y + HEADER_H / 2.0 + 4.0,
        cx = x + col_w - CELL_PAD,
        label = escape(&group.label.to_uppercase()),
        count = group.models.len(),
    );

    let track_w = col_w - 2.0 * CELL_PAD - COUNT_W;
    let max_chars = ((track_w - 2.0 * LABEL_PAD) / CHAR_W).floor().max(1.0) as usize;

    for (index, model) in group.models.iter().enumerate() {
        let row_y = y + HEADER_H + HEADER_GAP + index as f64 * (ROW_H + ROW_GAP);
        // Keep a one-route model visible rather than letting it round away.
        let bar_w = (track_w * model.routes as f64 / max_routes).max(3.0);
        let _ = write!(
            svg,
            r#"<rect x="{tx:.1}" y="{row_y:.1}" width="{track_w:.1}" height="{ROW_H:.1}" rx="4" fill="{track}"/>
<rect x="{tx:.1}" y="{row_y:.1}" width="{bar_w:.1}" height="{ROW_H:.1}" rx="4" fill="{bar}"/>
<text class="model" x="{lx:.1}" y="{ly:.1}">{name}</text>
<text class="routes" x="{rx:.1}" y="{ly:.1}" text-anchor="end">{routes}</text>
"#,
            tx = x + CELL_PAD,
            track = theme.track,
            bar = theme.bar,
            lx = x + CELL_PAD + LABEL_PAD,
            ly = row_y + ROW_H / 2.0 + 4.0,
            name = escape(&truncate(&model.name, max_chars)),
            rx = x + col_w - CELL_PAD,
            routes = model.routes,
        );
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let kept: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{kept}…")
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
