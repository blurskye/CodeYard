//! Issue and pull request pages: the list, the detail, and the new and edit forms for both kinds.
mod attributes;
pub(in crate::pages::repos) mod detail;
mod dom;
pub(in crate::pages::repos) mod editor;
pub(in crate::pages::repos) mod filter_menu;
pub(in crate::pages::repos) mod list;
mod management;
pub(in crate::pages::repos) mod new;
mod page;
pub(in crate::pages::repos) mod reactions;
mod relationships;
mod search;
mod search_box;
mod suggestions;
#[cfg(test)]
mod tests;

pub(in crate::pages::repos) use detail::notice;
pub use detail::{detail, DetailDraft};
pub(crate) use detail::{item_icon, state_class};
pub use list::list;
pub(in crate::pages::repos) use list::rows;
pub(in crate::pages::repos) use new::branch_select;
pub use new::{new, BranchChoices, FieldKind, FormField, IssueChoices, NewDraft, TemplateCard};
pub use page::{
    label_hex, label_style, merge_method_available, preselected_merge_method, render, repo_url,
    Comment, CommentEdit, CommentPage, Comparison, Event, IssueAttribute, IssueAttributes,
    IssueManagement, IssueSelectOption, IssueSuggestion, IssueSuggestions, Item, Kind, Label,
    ListCounts, ListFilters, Milestone, MilestoneDraft, Page, PinnedComment, Reaction,
    RelatedIssue, Repository, StackLayer, StackMap, SuggestionDraft, MERGE_COMMIT_REFUSED,
};
pub use search::{parse_search, MilestoneFilter};
