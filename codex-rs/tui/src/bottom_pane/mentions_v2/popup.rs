use codex_file_search::FileMatch;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Widget;
use ratatui::widgets::WidgetRef;

use super::candidate::Candidate;
use super::candidate::SearchResult;
use super::candidate::Selection;
use super::filter::filtered_candidates;
use super::render::render_popup;
use super::search_mode::SearchMode;
use crate::bottom_pane::popup_consts::MAX_POPUP_ROWS;
use crate::bottom_pane::scroll_state::ScrollState;

pub(crate) struct Popup {
    query: String,
    file_search: FileSearch,
    candidates: Vec<Candidate>,
    rows: Vec<SearchResult>,
    search_mode: SearchMode,
    state: ScrollState,
}

impl Popup {
    pub(crate) fn new(candidates: Vec<Candidate>) -> Self {
        let mut popup = Self {
            query: String::new(),
            file_search: FileSearch::default(),
            candidates,
            rows: Vec::new(),
            search_mode: SearchMode::Results,
            state: ScrollState::new(),
        };
        popup.refresh_rows();
        popup
    }

    pub(crate) fn set_candidates(&mut self, candidates: Vec<Candidate>) {
        if self.candidates == candidates {
            return;
        }
        self.candidates = candidates;
        self.refresh_rows();
    }

    pub(crate) fn set_query(&mut self, query: &str) {
        if self.query == query {
            return;
        }
        self.query = query.to_string();
        self.file_search.set_query(query);
        self.refresh_rows();
    }

    pub(crate) fn set_file_matches(
        &mut self,
        query: &str,
        matches: Vec<FileMatch>,
        walk_complete: bool,
    ) {
        if self.file_search.set_matches(query, matches, walk_complete) {
            self.refresh_rows();
        }
    }

    pub(crate) fn selected(&self) -> Option<Selection> {
        let idx = self.state.selected_idx?;
        self.rows.get(idx).map(|row| row.selection.clone())
    }

    pub(crate) fn move_up(&mut self) {
        let len = self.rows.len();
        self.state.move_up_wrap(len);
        self.state.ensure_visible(len, MAX_POPUP_ROWS.min(len));
    }

    pub(crate) fn move_down(&mut self) {
        let len = self.rows.len();
        self.state.move_down_wrap(len);
        self.state.ensure_visible(len, MAX_POPUP_ROWS.min(len));
    }

    pub(crate) fn previous_search_mode(&mut self) {
        self.search_mode = self.search_mode.previous();
        self.refresh_rows();
    }

    pub(crate) fn next_search_mode(&mut self) {
        self.search_mode = self.search_mode.next();
        self.refresh_rows();
    }

    pub(crate) fn calculate_required_height(&self, _width: u16) -> u16 {
        (MAX_POPUP_ROWS as u16).saturating_add(2)
    }

    pub(crate) fn searches_files(&self) -> bool {
        self.search_mode != SearchMode::Tools
    }

    fn clamp_selection(&mut self) {
        let len = self.rows.len();
        self.state.clamp_selection(len);
        self.state.ensure_visible(len, MAX_POPUP_ROWS.min(len));
    }

    fn refresh_rows(&mut self) {
        self.rows = filtered_candidates(
            &self.candidates,
            &self.file_search.matches,
            &self.query,
            self.search_mode,
            self.file_search.should_show_matches(),
        );
        self.clamp_selection();
    }
}

impl WidgetRef for Popup {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        render_popup(
            area,
            buf,
            &self.rows,
            &self.state,
            if self.searches_files() {
                self.file_search.empty_message()
            } else {
                "no matches"
            },
            self.search_mode,
        );
        if self.searches_files()
            && self.file_search.is_incomplete()
            && !self.rows.is_empty()
            && area.height > 2
        {
            Line::from("  File search incomplete; narrow search root".dim()).render(
                Rect {
                    y: area.y + area.height - 2,
                    height: 1,
                    ..area
                },
                buf,
            );
        }
    }
}

#[derive(Default)]
struct FileSearch {
    pending_query: String,
    display_query: String,
    waiting: bool,
    walk_complete: bool,
    matches: Vec<FileMatch>,
}

impl FileSearch {
    fn set_query(&mut self, query: &str) {
        if query.is_empty() {
            self.pending_query.clear();
            self.display_query.clear();
            self.waiting = false;
            self.matches.clear();
        } else if query != self.pending_query {
            self.pending_query = query.to_string();
            self.waiting = true;
        }
    }

    fn set_matches(&mut self, query: &str, matches: Vec<FileMatch>, walk_complete: bool) -> bool {
        if query != self.pending_query {
            return false;
        }

        self.display_query = query.to_string();
        self.matches = matches.into_iter().take(MAX_POPUP_ROWS).collect();
        self.waiting = false;
        self.walk_complete = walk_complete;
        true
    }

    fn should_show_matches(&self) -> bool {
        self.display_query == self.pending_query && !self.matches.is_empty()
    }

    fn empty_message(&self) -> &'static str {
        if self.waiting {
            "loading..."
        } else if self.is_incomplete() {
            "File search incomplete; narrow search root"
        } else {
            "no matches"
        }
    }

    fn is_incomplete(&self) -> bool {
        !self.pending_query.is_empty() && !self.waiting && !self.walk_complete
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_file_search::MatchType;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    fn file_match(index: usize) -> FileMatch {
        FileMatch {
            score: index as u32,
            path: PathBuf::from(format!("src/file_{index:02}.rs")),
            match_type: MatchType::File,
            root: PathBuf::from("/tmp/repo"),
            indices: None,
        }
    }

    #[test]
    fn set_matches_keeps_only_the_first_page_of_results() {
        let mut popup = Popup::new(Vec::new());
        popup.set_query("file");
        popup.set_file_matches(
            "file",
            (0..(MAX_POPUP_ROWS + 2)).map(file_match).collect(),
            true,
        );

        assert_eq!(
            popup.file_search.matches,
            (0..MAX_POPUP_ROWS).map(file_match).collect::<Vec<_>>()
        );
    }

    #[test]
    fn query_changes_hide_stale_files_until_current_results_arrive() {
        let mut popup = Popup::new(Vec::new());
        popup.set_query("alpha");
        popup.set_file_matches("alpha", vec![file_match(0)], true);
        assert_eq!(popup.selected(), Some(Selection::File(file_match(0).path)));

        popup.set_query("beta");
        popup.move_down();
        assert_eq!(popup.selected(), None);
        let area = Rect::new(0, 0, 30, 1);
        let mut buf = Buffer::empty(area);
        popup.render_ref(area, &mut buf);
        assert_eq!(buf[(2, 0)].symbol(), "l");
        popup.set_file_matches("alpha", vec![file_match(1)], true);
        assert_eq!(popup.selected(), None);

        popup.set_file_matches("beta", vec![file_match(2)], true);
        assert_eq!(popup.selected(), Some(Selection::File(file_match(2).path)));
        popup.set_query("beta");
        assert_eq!(popup.selected(), Some(Selection::File(file_match(2).path)));
        popup.set_query("");
        assert_eq!(popup.selected(), None);
    }

    #[test]
    fn incomplete_file_search_is_visible_with_and_without_matches() {
        let mut popup = Popup::new(Vec::new());
        popup.set_query("file");
        let render = |popup: &Popup| {
            let area = Rect::new(0, 0, 60, 10);
            let mut buf = Buffer::empty(area);
            popup.render_ref(area, &mut buf);
            buf.content
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
        };
        for matches in [Vec::new(), vec![file_match(0)]] {
            popup.set_file_matches("file", matches, false);
            assert!(render(&popup).contains("File search incomplete; narrow search root"));
        }
        assert_eq!(popup.selected(), Some(Selection::File(file_match(0).path)));
        popup.set_file_matches("file", Vec::new(), true);
        let complete = render(&popup);
        assert!(complete.contains("no matches"));
        assert!(!complete.contains("incomplete"));
        popup.set_query("other");
        popup.set_file_matches("file", Vec::new(), false);
        assert!(render(&popup).contains("loading..."));
        assert!(!render(&popup).contains("incomplete"));
    }

    #[test]
    fn rows_follow_catalog_and_mode_changes_and_scroll_beyond_one_page() {
        use super::super::candidate::MentionType;

        let candidate = |index| Candidate {
            display_name: format!("tool_{index:02}"),
            description: None,
            search_terms: Vec::new(),
            mention_type: MentionType::Skill,
            selection: Selection::Tool {
                insert_text: format!("$tool_{index:02}"),
                path: None,
            },
        };
        let mut popup = Popup::new((0..MAX_POPUP_ROWS + 2).map(candidate).collect());
        for _ in 0..MAX_POPUP_ROWS + 1 {
            popup.move_down();
        }
        assert_eq!(
            popup.selected(),
            Some(candidate(MAX_POPUP_ROWS + 1).selection)
        );
        popup.move_down();
        assert_eq!(popup.selected(), Some(candidate(0).selection));
        popup.move_up();
        assert_eq!(
            popup.selected(),
            Some(candidate(MAX_POPUP_ROWS + 1).selection)
        );
        popup.next_search_mode();
        assert_eq!(popup.selected(), None);
        popup.next_search_mode();
        assert_eq!(popup.selected(), Some(candidate(0).selection));
        popup.set_candidates(vec![candidate(42)]);
        assert_eq!(popup.selected(), Some(candidate(42).selection));
        popup.previous_search_mode();
        assert_eq!(popup.selected(), None);
        popup.previous_search_mode();
        popup.set_query("missing");
        assert_eq!(popup.selected(), None);
        popup.set_query("42");
        assert_eq!(popup.selected(), Some(candidate(42).selection));
    }
}
