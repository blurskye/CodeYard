//! Git write limits shared by native admission and browser forms.
/// Longest merge or squash message, title included, measured in UTF-8 bytes.
pub const MAX_MERGE_MESSAGE_BYTES: usize = 8192;

/// Larger template files are ignored.
pub const MAX_TEMPLATE_BYTES: u64 = 64 * 1024;
/// The chooser lists at most this many templates, in path order.
pub const MAX_TEMPLATES: usize = 20;

/// The refusal for a merge commit into a branch that requires a linear history. The merge form
/// disables the merge commit method with this text, and the merge gives it as a blocker.
pub const MERGE_COMMIT_REFUSED: &str =
    "Merge commits are not allowed on this branch. Use squash or rebase.";
