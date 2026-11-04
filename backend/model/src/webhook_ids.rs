/// The native comment tables have separate sequences; REST IDs keep them distinct.
pub fn conversation_comment_id(id: i64, pull_request: bool) -> i64 {
    id * 2 + i64::from(pull_request)
}
