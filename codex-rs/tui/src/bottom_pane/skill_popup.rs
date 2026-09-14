use crossterm::event::KeyCode;
use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Widget;
use ratatui::widgets::WidgetRef;

use super::popup_consts::MAX_POPUP_ROWS;
use super::scroll_state::ScrollState;
use super::selection_popup_common::GenericDisplayRow;
use super::selection_popup_common::render_rows_single_line;
use crate::fuzzy_match::fuzzy_match;
use crate::key_hint;
use crate::render::Insets;
use crate::render::RectExt;
use crate::text_formatting::truncate_text;

#[derive(Clone, Debug)]
pub(crate) struct MentionItem {
    pub(crate) display_name: String,
    pub(crate) description: Option<String>,
    pub(crate) insert_text: String,
    pub(crate) search_terms: Vec<String>,
    pub(crate) path: Option<String>,
    pub(crate) category_tag: Option<String>,
    pub(crate) sort_rank: u8,
}

const MENTION_NAME_TRUNCATE_LEN: usize = 28;

pub(crate) struct SkillPopup {
    query: String,
    mentions: Vec<MentionItem>,
    state: ScrollState,
    matches: Vec<(usize, Option<Vec<usize>>, i32)>,
}

impl SkillPopup {
    pub(crate) fn new(mentions: Vec<MentionItem>) -> Self {
        let mut popup = Self {
            query: String::new(),
            mentions,
            state: ScrollState::new(),
            matches: Vec::new(),
        };
        popup.matches = popup.filtered();
        popup
    }

    pub(crate) fn set_mentions(&mut self, mentions: Vec<MentionItem>) {
        self.mentions = mentions;
        self.matches = self.filtered();
        self.clamp_selection();
    }

    pub(crate) fn set_query(&mut self, query: &str) {
        if self.query == query {
            self.clamp_selection();
            return;
        }
        self.query = query.to_string();
        self.matches = self.filtered();
        self.clamp_selection();
    }

    pub(crate) fn calculate_required_height(&self, _width: u16) -> u16 {
        let visible = self.matches.len().clamp(1, MAX_POPUP_ROWS);
        (visible as u16).saturating_add(2)
    }

    pub(crate) fn move_up(&mut self) {
        let len = self.matches.len();
        self.state.move_up_wrap(len);
        self.state.ensure_visible(len, MAX_POPUP_ROWS.min(len));
    }

    pub(crate) fn move_down(&mut self) {
        let len = self.matches.len();
        self.state.move_down_wrap(len);
        self.state.ensure_visible(len, MAX_POPUP_ROWS.min(len));
    }

    pub(crate) fn selected_mention(&self) -> Option<&MentionItem> {
        let idx = self.state.selected_idx?;
        self.mentions.get(self.matches.get(idx)?.0)
    }

    fn clamp_selection(&mut self) {
        let len = self.matches.len();
        self.state.clamp_selection(len);
        self.state.ensure_visible(len, MAX_POPUP_ROWS.min(len));
    }

    #[cfg(test)]
    fn filtered_items(&self) -> Vec<usize> {
        self.matches.iter().map(|(idx, _, _)| *idx).collect()
    }

    fn rows_from_matches(
        &self,
        matches: Vec<(usize, Option<Vec<usize>>, i32)>,
    ) -> Vec<GenericDisplayRow> {
        matches
            .into_iter()
            .map(|(idx, indices, _score)| {
                let mention = &self.mentions[idx];
                let name = truncate_text(&mention.display_name, MENTION_NAME_TRUNCATE_LEN);
                let description = match (
                    mention.category_tag.as_deref(),
                    mention.description.as_deref(),
                ) {
                    (Some(tag), Some(description)) if !description.is_empty() => {
                        Some(format!("{tag} {description}"))
                    }
                    (Some(tag), _) => Some(tag.to_string()),
                    (None, Some(description)) if !description.is_empty() => {
                        Some(description.to_string())
                    }
                    _ => None,
                };
                GenericDisplayRow {
                    name,
                    name_prefix_spans: Vec::new(),
                    match_indices: indices,
                    display_shortcut: None,
                    description,
                    category_tag: None,
                    is_disabled: false,
                    disabled_reason: None,
                    wrap_indent: None,
                }
            })
            .collect()
    }

    fn filtered(&self) -> Vec<(usize, Option<Vec<usize>>, i32)> {
        let filter = self.query.trim();
        let mut out: Vec<(usize, Option<Vec<usize>>, i32)> = Vec::new();

        for (idx, mention) in self.mentions.iter().enumerate() {
            if filter.is_empty() {
                out.push((idx, None, 0));
                continue;
            }

            let best_match =
                if let Some((indices, score)) = fuzzy_match(&mention.display_name, filter) {
                    Some((Some(indices), score))
                } else {
                    mention
                        .search_terms
                        .iter()
                        .filter(|term| *term != &mention.display_name)
                        .filter_map(|term| fuzzy_match(term, filter).map(|(_indices, score)| score))
                        .min()
                        .map(|score| (None, score))
                };

            if let Some((indices, score)) = best_match {
                out.push((idx, indices, score));
            }
        }

        out.sort_by(|a, b| {
            if filter.is_empty() {
                self.mentions[a.0]
                    .sort_rank
                    .cmp(&self.mentions[b.0].sort_rank)
            } else {
                a.1.is_none()
                    .cmp(&b.1.is_none())
                    .then_with(|| a.2.cmp(&b.2))
                    .then_with(|| {
                        self.mentions[a.0]
                            .sort_rank
                            .cmp(&self.mentions[b.0].sort_rank)
                    })
            }
            .then_with(|| {
                let an = self.mentions[a.0].display_name.as_str();
                let bn = self.mentions[b.0].display_name.as_str();
                an.cmp(bn)
            })
        });

        out
    }
}

impl WidgetRef for SkillPopup {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        let (list_area, hint_area) = if area.height > 2 {
            let [list_area, _spacer_area, hint_area] = Layout::vertical([
                Constraint::Length(area.height - 2),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .areas(area);
            (list_area, Some(hint_area))
        } else {
            (area, None)
        };
        let visible = MAX_POPUP_ROWS.min(list_area.height as usize);
        let mut state = self.state;
        state.ensure_visible(self.matches.len(), visible);
        let start = state.scroll_top;
        let rows = self.rows_from_matches(
            self.matches
                .iter()
                .skip(start)
                .take(visible)
                .cloned()
                .collect(),
        );
        state.selected_idx = state.selected_idx.map(|idx| idx.saturating_sub(start));
        state.scroll_top = 0;
        render_rows_single_line(
            list_area.inset(Insets::tlbr(
                /*top*/ 0, /*left*/ 2, /*bottom*/ 0, /*right*/ 0,
            )),
            buf,
            &rows,
            &state,
            MAX_POPUP_ROWS,
            "no matches",
        );
        if let Some(hint_area) = hint_area {
            let hint_area = Rect {
                x: hint_area.x + 2,
                y: hint_area.y,
                width: hint_area.width.saturating_sub(2),
                height: hint_area.height,
            };
            skill_popup_hint_line().render(hint_area, buf);
        }
    }
}

fn skill_popup_hint_line() -> Line<'static> {
    Line::from(vec![
        "Press ".into(),
        key_hint::plain(KeyCode::Enter).into(),
        " to insert or ".into(),
        key_hint::plain(KeyCode::Esc).into(),
        " to close".into(),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    fn mention_item(index: usize) -> MentionItem {
        MentionItem {
            display_name: format!("Mention {index:02}"),
            description: Some(format!("Description {index:02}")),
            insert_text: format!("$mention-{index:02}"),
            search_terms: vec![format!("mention-{index:02}")],
            path: Some(format!("skill://mention-{index:02}")),
            category_tag: Some("[Skill]".to_string()),
            sort_rank: 1,
        }
    }

    fn ranked_mention_item(
        display_name: &str,
        search_terms: &[&str],
        category_tag: &str,
        sort_rank: u8,
    ) -> MentionItem {
        MentionItem {
            display_name: display_name.to_string(),
            description: None,
            insert_text: format!("${display_name}"),
            search_terms: search_terms
                .iter()
                .map(|term| (*term).to_string())
                .collect(),
            path: None,
            category_tag: Some(category_tag.to_string()),
            sort_rank,
        }
    }

    fn named_mention_item(display_name: &str, search_terms: &[&str]) -> MentionItem {
        ranked_mention_item(display_name, search_terms, "[Skill]", /*sort_rank*/ 1)
    }

    fn plugin_mention_item(display_name: &str, search_terms: &[&str]) -> MentionItem {
        ranked_mention_item(display_name, search_terms, "[Plugin]", /*sort_rank*/ 0)
    }

    #[test]
    fn cached_matches_refresh_when_query_or_catalog_changes() {
        let mut popup = SkillPopup::new(vec![
            named_mention_item("alpha", &["alpha"]),
            named_mention_item("beta", &["beta"]),
        ]);
        popup.set_query("");
        assert_eq!(popup.selected_mention().unwrap().display_name, "alpha");
        popup.set_query("beta");
        assert_eq!(popup.selected_mention().unwrap().display_name, "beta");
        assert_eq!(popup.calculate_required_height(72), 3);
        popup.set_mentions(vec![named_mention_item("beta-two", &["beta"])]);
        assert_eq!(popup.selected_mention().unwrap().display_name, "beta-two");
        popup.set_mentions(vec![named_mention_item("other", &["other"])]);
        assert!(popup.selected_mention().is_none());
        popup.set_query("");
        assert_eq!(popup.selected_mention().unwrap().display_name, "other");
    }

    #[test]
    fn filtered_mentions_preserve_results_beyond_popup_height() {
        let popup = SkillPopup::new((0..(MAX_POPUP_ROWS + 2)).map(mention_item).collect());

        let filtered_names: Vec<String> = popup
            .filtered_items()
            .into_iter()
            .map(|idx| popup.mentions[idx].display_name.clone())
            .collect();

        assert_eq!(
            filtered_names,
            (0..(MAX_POPUP_ROWS + 2))
                .map(|idx| format!("Mention {idx:02}"))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            popup.calculate_required_height(72),
            (MAX_POPUP_ROWS as u16) + 2
        );
    }

    fn render_popup(popup: &SkillPopup, width: u16) -> String {
        let area = Rect::new(0, 0, width, popup.calculate_required_height(width));
        let mut buf = Buffer::empty(area);
        popup.render_ref(area, &mut buf);
        format!("{buf:?}")
    }

    #[test]
    fn scrolling_mentions_shifts_rendered_window_snapshot() {
        let mut popup = SkillPopup::new((0..(MAX_POPUP_ROWS + 2)).map(mention_item).collect());

        for _ in 0..=MAX_POPUP_ROWS {
            popup.move_down();
        }

        insta::assert_snapshot!("skill_popup_scrolled", render_popup(&popup, /*width*/ 72));
    }

    #[test]
    fn display_name_match_sorting_beats_worse_secondary_search_term_matches() {
        let mut popup = SkillPopup::new(vec![
            named_mention_item("pr-review-triage", &["pr-review-triage"]),
            named_mention_item("prd", &["prd"]),
            named_mention_item("PR Babysitter", &["babysit-pr", "PR Babysitter"]),
            named_mention_item("Plugin Creator", &["plugin-creator", "Plugin Creator"]),
            named_mention_item(
                "Logging Best Practices",
                &["logging-best-practices", "Logging Best Practices"],
            ),
        ]);
        popup.set_query("pr");

        let filtered_names: Vec<String> = popup
            .filtered_items()
            .into_iter()
            .map(|idx| popup.mentions[idx].display_name.clone())
            .collect();

        assert_eq!(
            filtered_names,
            vec![
                "PR Babysitter".to_string(),
                "pr-review-triage".to_string(),
                "prd".to_string(),
                "Plugin Creator".to_string(),
                "Logging Best Practices".to_string(),
            ]
        );
    }

    #[test]
    fn query_match_score_sorts_before_plugin_rank_bias() {
        let mut popup = SkillPopup::new(vec![
            plugin_mention_item("GitHub", &["github", "pull requests", "pr"]),
            named_mention_item("pr-review-triage", &["pr-review-triage"]),
            named_mention_item("prd", &["prd"]),
            named_mention_item("Plugin Creator", &["plugin-creator", "Plugin Creator"]),
            named_mention_item(
                "Logging Best Practices",
                &["logging-best-practices", "Logging Best Practices"],
            ),
            named_mention_item("PR Babysitter", &["babysit-pr", "PR Babysitter"]),
        ]);
        popup.set_query("pr");

        let filtered_items: Vec<(String, Option<String>)> = popup
            .filtered_items()
            .into_iter()
            .map(|idx| {
                (
                    popup.mentions[idx].display_name.clone(),
                    popup.mentions[idx].category_tag.clone(),
                )
            })
            .collect();

        assert_eq!(
            filtered_items,
            vec![
                ("PR Babysitter".to_string(), Some("[Skill]".to_string())),
                ("pr-review-triage".to_string(), Some("[Skill]".to_string())),
                ("prd".to_string(), Some("[Skill]".to_string())),
                ("Plugin Creator".to_string(), Some("[Skill]".to_string())),
                (
                    "Logging Best Practices".to_string(),
                    Some("[Skill]".to_string())
                ),
                ("GitHub".to_string(), Some("[Plugin]".to_string())),
            ]
        );
    }
}
