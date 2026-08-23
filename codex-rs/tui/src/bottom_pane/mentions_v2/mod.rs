//! The unified mention popup used for `@` mentions in the TUI.

mod candidate;
mod filter;
mod footer;
mod popup;
mod render;
mod search_catalog;
mod search_mode;

pub(crate) use candidate::Selection as MentionV2Selection;
pub(crate) use popup::Popup as MentionV2Popup;
pub(crate) use search_catalog::build_search_catalog;
