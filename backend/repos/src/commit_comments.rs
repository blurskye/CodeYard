use codeyard_database::models::CommitComment;
pub fn comment_link(owner: &str, name: &str, comment: &CommitComment) -> String {
    format!(
        "/{}/{}/commit/{}?comment={}#commitcomment-{}",
        codeyard_types::paths::encode_component(owner),
        codeyard_types::paths::encode_component(name),
        comment.sha,
        comment.id,
        comment.id
    )
}
