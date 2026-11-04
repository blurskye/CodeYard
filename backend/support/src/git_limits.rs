//! Git write limits shared by native admission and browser forms.
/// Longest merge or squash message, title included, measured in UTF-8 bytes.
pub const MAX_MERGE_MESSAGE_BYTES: usize = 8192;

/// Larger template files are ignored.
pub const MAX_TEMPLATE_BYTES: u64 = 64 * 1024;
/// The chooser lists at most this many templates, in path order.
pub const MAX_TEMPLATES: usize = 20;
