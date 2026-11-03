//! Git write limits shared by native admission and browser forms.
/// Longest merge or squash message, title included, measured in UTF-8 bytes.
pub const MAX_MERGE_MESSAGE_BYTES: usize = 8192;
