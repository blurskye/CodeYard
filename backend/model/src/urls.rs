use crate::http_config::link_origin;
pub fn api_url(path: &str) -> String {
    format!("{}/api/v3{path}", crate::http_config::link_origin())
}

pub fn repo_url(owner: &str, repo: &str) -> String {
    api_url(&format!(
        "/repos/{}/{}",
        codeyard_webui::encode_component(owner),
        codeyard_webui::encode_component(repo),
    ))
}
pub fn namespace_url(owner: &str, name: &str) -> String {
    format!(
        "{}/{}/{}",
        link_origin(),
        codeyard_webui::encode_component(owner),
        codeyard_webui::encode_component(name)
    )
}
