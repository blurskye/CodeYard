//! Issue and pull request pages: the list, the detail, and the new and edit forms for both kinds.
pub(in crate::webui::pages::repos) mod detail;
mod dom;
pub(in crate::webui::pages::repos) mod editor;
pub(in crate::webui::pages::repos) mod filter_menu;
pub(in crate::webui::pages::repos) mod list;
pub(in crate::webui::pages::repos) mod new;
mod page;
pub(in crate::webui::pages::repos) mod reactions;
mod search;
mod search_box;
#[cfg(test)]
mod tests;

pub(in crate::webui::pages::repos) use detail::{csrf, notice};
pub use detail::{detail, DetailDraft};
pub(crate) use detail::{item_icon, state_class};
pub use list::list;
pub(in crate::webui::pages::repos) use list::rows;
pub(in crate::webui::pages::repos) use new::branch_select;
pub use new::{new, BranchChoices, IssueChoices, NewDraft};
pub use page::{
    label_hex, label_style, merge_method_available, preselected_merge_method, render, repo_url,
    Comment, CommentPage, Comparison, Event, Item, Kind, Label, ListCounts, ListFilters, Milestone,
    MilestoneDraft, Page, Reaction, Repository, StackLayer, StackMap, MERGE_COMMIT_REFUSED,
};
pub use search::{parse_search, MilestoneFilter};
