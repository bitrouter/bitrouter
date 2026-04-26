use super::App;
use super::helpers::entry_contains_text;

impl App {
    pub(super) fn recompute_search(&mut self) {
        let query = match &self.state.search {
            Some(s) if !s.query.is_empty() => s.query.to_lowercase(),
            _ => {
                if let Some(search) = &mut self.state.search {
                    search.matches.clear();
                    search.current_match = 0;
                }
                return;
            }
        };

        let matches: Vec<u64> = if let Some(sb) = self.state.active_scrollback() {
            sb.entries
                .iter()
                .filter(|e| entry_contains_text(&e.kind, &query))
                .map(|e| e.id)
                .collect()
        } else {
            Vec::new()
        };

        if let Some(search) = &mut self.state.search {
            search.matches = matches;
            search.current_match = 0;
        }
    }

    pub(super) fn scroll_to_search_match(&mut self) {
        let target_id = match &self.state.search {
            Some(s) => s.matches.get(s.current_match).copied(),
            None => None,
        };
        let Some(target_id) = target_id else { return };

        if let Some(sb) = self.state.active_scrollback_mut() {
            let Some(idx) = sb.index_of(target_id) else {
                return;
            };
            // Use exact line offsets if available (populated by render loop).
            if sb.line_offsets.len() > idx {
                let line_pos = sb.line_offsets[idx];
                sb.scroll_offset = line_pos.saturating_sub(3);
                sb.follow = false;
            }
        }
    }
}
