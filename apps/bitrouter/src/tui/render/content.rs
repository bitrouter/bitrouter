//! What a tool produced: its output, and the blocks that are not text.
//!
//! # Why there is a cap
//!
//! A tool's output is unbounded — `cargo test` on this workspace is thousands
//! of lines — and this renderer paints into the terminal the user is sitting
//! in. Worse, rows that scroll above the fold can never be revised again, so
//! an uncapped `Content` block does not merely fill the screen: it pushes the
//! rest of the session into territory the writer has given up on.
//!
//! So output is capped and the remainder is *counted*, not dropped silently.
//! `… 1,240 more lines` is a fact the reader can act on; forty rows that stop
//! for no stated reason are not.

use agent_client_protocol_schema::v1::{ContentBlock, EmbeddedResourceResource};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// How many rows one `Content` block may occupy.
///
/// Forty is about a screenful: enough that a short command's whole output
/// arrives intact, short enough that a long one cannot bury the turn that
/// produced it.
pub const MAX_ROWS: usize = 40;

/// The indent every row of tool output carries, so output is visibly the
/// tool's rather than the agent's.
const INDENT: &str = "  ";

/// Render one content block's text, capped at [`MAX_ROWS`].
///
/// Blocks that are not text have no honest multi-line spelling, so they get a
/// single descriptive row instead — see [`describe`].
pub fn render(block: &ContentBlock) -> Vec<Line<'static>> {
    let ContentBlock::Text(text) = block else {
        return vec![Line::from(Span::styled(
            format!("{INDENT}{}", describe(block)),
            Style::default().fg(Color::DarkGray),
        ))];
    };

    let total = text.text.lines().count();
    let mut lines: Vec<Line<'static>> = text
        .text
        .lines()
        .take(MAX_ROWS)
        .map(|line| Line::from(format!("{INDENT}{line}")))
        .collect();
    if total > MAX_ROWS {
        lines.push(more(total.saturating_sub(MAX_ROWS)));
    }
    lines
}

/// The `… N more lines` tail. Public because the diff renderer's own cap ends
/// the same way, and two spellings of the same fact would read as two
/// different facts.
pub fn more(remaining: usize) -> Line<'static> {
    Line::from(Span::styled(
        format!("{INDENT}… {} more lines", thousands(remaining)),
        Style::default().fg(Color::DarkGray),
    ))
}

/// A one-line description of a block that is not text.
///
/// An image has no textual rendering, and a placeholder that said nothing
/// would be worse than the size and type, which at least tell the reader what
/// they are not seeing.
pub fn describe(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::Image(image) => {
            format!("[image {} {}]", image.mime_type, bytes(image.data.len()))
        }
        ContentBlock::Audio(audio) => {
            format!("[audio {} {}]", audio.mime_type, bytes(audio.data.len()))
        }
        ContentBlock::ResourceLink(link) => format!("[resource: {}]", link.uri),
        ContentBlock::Resource(resource) => match &resource.resource {
            EmbeddedResourceResource::TextResourceContents(text) => {
                format!("[resource: {}]", text.uri)
            }
            EmbeddedResourceResource::BlobResourceContents(blob) => {
                format!("[resource: {} {}]", blob.uri, bytes(blob.blob.len()))
            }
            // `EmbeddedResourceResource` is `#[non_exhaustive]`.
            _ => "[resource]".to_string(),
        },
        // `ContentBlock` is `#[non_exhaustive]`: a block kind this build has
        // never heard of is reported as unknown rather than shown as nothing.
        _ => "[unrecognised content]".to_string(),
    }
}

/// Base64 payload length as the size of what it decodes to.
///
/// Base64 is four characters per three bytes, so the encoded length overstates
/// the file by a third. Reporting the encoded length would tell the user the
/// size of our transport rather than the size of their image.
fn bytes(encoded: usize) -> String {
    /// One kibibyte, and the unit every step below is a multiple of.
    const KB: usize = 1024;
    let decoded = encoded.saturating_mul(3) / 4;
    // Integer arithmetic to one decimal place rather than a float: a size is
    // a count, and casting a count to `f64` to print it is how precision
    // warnings get silenced with an `allow`.
    let tenths = |value: usize, unit: usize| (value / unit, (value % unit) * 10 / unit);
    if decoded < KB {
        format!("{decoded} B")
    } else if decoded < KB * KB {
        let (whole, tenth) = tenths(decoded, KB);
        format!("{whole}.{tenth} KB")
    } else {
        let (whole, tenth) = tenths(decoded, KB * KB);
        format!("{whole}.{tenth} MB")
    }
}

/// `1240` → `1,240`. A five-figure line count is unreadable without it, and
/// the whole point of the number is that the reader takes it in at a glance.
pub fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::{
        EmbeddedResource, ImageContent, ResourceLink, TextContent, TextResourceContents,
    };

    use super::*;

    fn text_of(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn block(text: &str) -> ContentBlock {
        ContentBlock::Text(TextContent::new(text.to_string()))
    }

    /// Short output arrives whole. The cap is a bound, not a habit.
    #[test]
    fn output_under_the_cap_is_untouched() {
        let rendered = render(&block("running 3 tests\ntest result: ok."));
        assert_eq!(text_of(&rendered), "  running 3 tests\n  test result: ok.");
    }

    /// The named regression: an `Execute` call's output appears **and** is
    /// capped, with the remainder counted rather than silently dropped.
    #[test]
    fn long_output_is_capped_and_the_remainder_is_counted() {
        let long = (1..=1_280)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rendered = render(&block(&long));

        assert_eq!(
            rendered.len(),
            MAX_ROWS + 1,
            "forty rows and the tail that explains them"
        );
        let text = text_of(&rendered);
        assert!(text.starts_with("  line 1\n"), "the output does appear");
        assert!(text.contains("line 40"), "up to the cap: {text:?}");
        assert!(!text.contains("line 41"), "and no further: {text:?}");
        assert!(
            text.ends_with("… 1,240 more lines"),
            "the remainder is counted: {text:?}"
        );
    }

    /// Exactly at the cap there is no remainder, so there is no tail.
    #[test]
    fn output_exactly_at_the_cap_has_no_tail() {
        let exact = (1..=MAX_ROWS)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rendered = render(&block(&exact));
        assert_eq!(rendered.len(), MAX_ROWS);
        assert!(!text_of(&rendered).contains("more lines"));
    }

    /// Every non-text block gets one row that says what it is. `Transcript`
    /// renders four of `ContentBlock`'s five variants as nothing at all.
    #[test]
    fn non_text_blocks_are_described_in_one_row() {
        let image = ContentBlock::Image(ImageContent::new(
            // 1.4 MB of base64 → about 1.0 MB of image.
            "A".repeat(1_400_000),
            "image/png",
        ));
        let described = render(&image);
        assert_eq!(described.len(), 1, "one row, whatever the payload");
        let text = text_of(&described);
        assert!(text.contains("[image image/png"), "{text:?}");
        assert!(
            text.contains("MB]"),
            "the size, so the reader knows: {text:?}"
        );

        let link = ContentBlock::ResourceLink(ResourceLink::new("readme", "file:///tmp/README.md"));
        assert_eq!(
            text_of(&render(&link)),
            "  [resource: file:///tmp/README.md]"
        );

        let embedded = ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
                "fn main() {}",
                "file:///tmp/main.rs",
            )),
        ));
        assert_eq!(
            text_of(&render(&embedded)),
            "  [resource: file:///tmp/main.rs]"
        );
    }

    /// A size in bytes that a person can read at a glance.
    #[test]
    fn sizes_are_reported_as_what_they_decode_to() {
        // Base64 overstates by a third; 1,400,000 characters is ~1.0 MB.
        assert_eq!(bytes(1_400_000), "1.0 MB");
        assert_eq!(bytes(4_000), "2.9 KB");
        assert_eq!(bytes(40), "30 B");
    }

    #[test]
    fn line_counts_are_grouped_for_reading() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(1_240), "1,240");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
