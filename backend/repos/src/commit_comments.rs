use codeyard_db_core::models::CommitComment;
pub fn comment_link(owner: &str, name: &str, comment: &CommitComment) -> String {
    format!(
        "/{}/{}/commit/{}?comment={}#commitcomment-{}",
        codeyard_webui::encode_component(owner),
        codeyard_webui::encode_component(name),
        comment.sha,
        comment.id,
        comment.id
    )
}
