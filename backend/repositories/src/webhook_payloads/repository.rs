use codeyard_types::http_config::link_origin;
use codeyard_types::node_id;
use serde_json::{json, Value};
pub fn community_file(file: Option<&crate::repositories::community::File>) -> Value {
    let Some(file) = file else {
        return Value::Null;
    };
    let encode = codeyard_types::paths::encode_component;
    let path = codeyard_types::ref_path::encode_path(&file.path);
    let base = format!(
        "/repos/{}/{}",
        encode(&file.owner),
        encode(&file.repository)
    );
    json!({"url":codeyard_types::urls::api_url(&format!("{base}/contents/{path}?ref={}",file.reference)),"html_url":format!("{}{}",link_origin(),codeyard_types::ref_path::file_url(&format!("/{}/{}",encode(&file.owner),encode(&file.repository)),"blob",&file.reference,&file.path))})
}

pub fn community_profile(profile: &crate::repositories::community::Profile) -> Value {
    use codeyard_types::urls::api_url;

    let conduct = community_file(profile.conduct.as_ref());
    let license = match (profile.license_info, profile.license.as_ref()) {
        (Some(info), Some(file)) => {
            json!({"key":info.key,"name":info.name,"spdx_id":info.spdx,"node_id":node_id::git("LIC",info.key),"url":if info.key=="other" {Value::Null}else {json!(api_url(&format!("/licenses/{}",info.key)))},"html_url":community_file(Some(file))["html_url"]})
        }
        _ => Value::Null,
    };
    json!({"health_percentage":profile.health_percentage(),"description":profile.description,"documentation":profile.documentation,"updated_at":profile.updated_at,
        "files":{"code_of_conduct":if profile.contributor_covenant {json!({"key":"contributor_covenant","name":"Contributor Covenant","html_url":conduct["html_url"],"url":api_url("/codes_of_conduct/contributor_covenant")})}else {Value::Null},"code_of_conduct_file":conduct,"license":license,"readme":community_file(profile.readme.as_ref()),"contributing":community_file(profile.contributing.as_ref()),"issue_template":community_file(profile.issue_template.as_ref()),"pull_request_template":community_file(profile.pull_request_template.as_ref())}})
}
