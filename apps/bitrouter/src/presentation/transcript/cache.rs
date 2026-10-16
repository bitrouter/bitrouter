//! Logical transcript assembly and revision/extent caching.

use super::text::{Extent, Row};
use super::{Registry, ToolContext};
use crate::presentation::journal::{Entry, EntryId, Journal};

/// Rendered rows per entity, so a frame re-renders only what changed.
///
/// Without this, §4.1's first step is a full re-derivation of the document on
/// every frame — every diff hunked again, every tool's output re-capped — up
/// to thirty times a second.
///
/// The key is `(size, revision)`: the revision is how the journal says an
/// entity moved on, and **both** terminal dimensions are part of what a
/// renderer saw. Width is the obvious one — it is what rows are wrapped to.
/// Height is the one that is easy to miss: [`crate::presentation::transcript::diff::render`] is
/// capped at the terminal's height, so a window made shorter leaves cached
/// diff blocks that are taller than the screen they are now drawn on, and a
/// window made taller leaves them clipped shorter than they need to be.
#[derive(Debug, Default)]
pub struct Cache {
    rows: std::collections::HashMap<EntryId, Cached>,
}

#[derive(Debug)]
struct Cached {
    size: Extent,
    revision: u64,
    rows: Vec<Row>,
}

impl Cache {
    /// The whole document: every entry, then the caller's footer.
    ///
    /// The footer is never cached. It is composed by the caller out of things
    /// this crate cannot see — cost, route, the open permission — and it
    /// changes on almost every frame that changes at all.
    pub fn document(
        &mut self,
        journal: &Journal,
        registry: &Registry,
        size: Extent,
        footer: &[Row],
    ) -> Vec<Row> {
        let mut document: Vec<Row> = Vec::new();
        let mut live: std::collections::HashSet<EntryId> = std::collections::HashSet::new();

        for item in journal.entries() {
            let fresh = self
                .rows
                .get(&item.id)
                .is_some_and(|cached| cached.size == size && cached.revision == item.revision);
            if !fresh {
                let rows = match item.entry {
                    Entry::Message(message) => super::message(message, size.width),
                    Entry::Tool(call) => registry.render(&ToolContext::new(call, size)),
                    Entry::Plan(plan) => super::session::plan(plan),
                };
                self.rows.insert(
                    item.id.clone(),
                    Cached {
                        size,
                        revision: item.revision,
                        rows,
                    },
                );
            }
            if let Some(cached) = self.rows.get(&item.id) {
                document.extend(cached.rows.iter().cloned());
            }
            live.insert(item.id);
        }

        // An entry the journal no longer has cannot come back — ids are
        // first-seen and never reissued — so its rows are dead weight.
        self.rows.retain(|id, _| live.contains(id));
        document.extend(footer.iter().cloned());
        document
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // ── scheduling and the cache ───────────────────────────────────────────

    use agent_client_protocol_schema::v1::{
        ContentBlock, ContentChunk, SessionUpdate, TextContent, ToolCall, ToolCallId, ToolKind,
    };

    use crate::presentation::journal::Journal;

    fn chunk(text: &str) -> SessionUpdate {
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            text.to_string(),
        ))))
    }

    /// A registry whose `Read` renderer counts how often it is asked to draw,
    /// so "cached" is observable rather than assumed.
    fn counting_registry() -> (Registry, std::rc::Rc<std::cell::Cell<usize>>) {
        let count = std::rc::Rc::new(std::cell::Cell::new(0));
        struct Shared(std::rc::Rc<std::cell::Cell<usize>>);
        impl crate::presentation::transcript::ToolRenderer for Shared {
            fn render(&self, ctx: &ToolContext<'_>) -> Vec<Row> {
                self.0.set(self.0.get().saturating_add(1));
                vec![Row::from(ctx.title.to_string())]
            }
        }
        let mut registry = Registry::default();
        registry.register(
            crate::presentation::transcript::ToolKey::Read,
            Box::new(Shared(std::rc::Rc::clone(&count))),
        );
        (registry, count)
    }

    /// An entity that has not changed is not re-rendered. Without this, every
    /// frame re-hunks every diff in the session.
    #[test]
    fn an_unchanged_entity_is_rendered_once() {
        let (registry, count) = counting_registry();
        let mut journal = Journal::default();
        journal.apply(SessionUpdate::ToolCall(
            ToolCall::new(ToolCallId::new("t1"), "Read src/lib.rs").kind(ToolKind::Read),
        ));
        let mut cache = Cache::default();
        let size = Extent {
            width: 80,
            height: 24,
        };

        let first = cache.document(&journal, &registry, size, &[]);
        let second = cache.document(&journal, &registry, size, &[]);
        assert_eq!(first, second, "the same document either way");
        assert_eq!(count.get(), 1, "rendered once, then cached");

        // A patch bumps the revision, so the next frame re-renders it.
        journal.apply(SessionUpdate::ToolCall(
            ToolCall::new(ToolCallId::new("t1"), "Read src/main.rs").kind(ToolKind::Read),
        ));
        cache.document(&journal, &registry, size, &[]);
        assert_eq!(count.get(), 2, "a changed entity is re-rendered");
    }

    /// The terminal's **size** is half the cache key, not its width.
    ///
    /// Width is the obvious half: wrapping and every cap depend on it. Height
    /// is the one that was missing — `render::diff::render` caps a diff at the
    /// terminal's height, so a window resized vertically with the width
    /// unchanged would otherwise keep serving diff blocks measured against a
    /// screen that is no longer there.
    #[test]
    fn a_resize_in_either_dimension_invalidates_the_cache() {
        for (before, after) in [
            (
                Extent {
                    width: 80,
                    height: 24,
                },
                Extent {
                    width: 40,
                    height: 24,
                },
            ),
            (
                Extent {
                    width: 80,
                    height: 24,
                },
                Extent {
                    width: 80,
                    height: 12,
                },
            ),
        ] {
            let (registry, count) = counting_registry();
            let mut journal = Journal::default();
            journal.apply(SessionUpdate::ToolCall(
                ToolCall::new(ToolCallId::new("t1"), "Read src/lib.rs").kind(ToolKind::Read),
            ));
            let mut cache = Cache::default();

            cache.document(&journal, &registry, before, &[]);
            cache.document(&journal, &registry, after, &[]);
            assert_eq!(
                count.get(),
                2,
                "{before:?} -> {after:?} kept a stale render"
            );

            // And an unchanged size still hits.
            cache.document(&journal, &registry, after, &[]);
            assert_eq!(count.get(), 2, "an unchanged size must still hit the cache");
        }
    }

    /// The footer is the document's tail, and is never cached — it is composed
    /// by the caller out of things this crate cannot see.
    #[test]
    fn the_footer_is_appended_to_every_document() {
        let registry = Registry::default();
        let mut journal = Journal::default();
        journal.apply(chunk("hello"));
        let mut cache = Cache::default();
        let footer = vec![Row::from("USD 0.4200 · claude-opus")];

        let document = cache.document(
            &journal,
            &registry,
            Extent {
                width: 80,
                height: 24,
            },
            &footer,
        );
        assert_eq!(
            document.last().map(ToString::to_string),
            Some("USD 0.4200 · claude-opus".to_string())
        );
    }
}
