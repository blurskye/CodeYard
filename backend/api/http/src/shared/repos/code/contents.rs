use codeyard_app::repositories::store::editor::MAX_EDITOR_BYTES;
/// LF textarea contents become CRLF and then `%0D%0A` in a native form.
pub const EDITOR_FORM_BODY_LIMIT: usize = 6 * MAX_EDITOR_BYTES + 64 * 1024;
