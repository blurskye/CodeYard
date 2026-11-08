use codeyard_db_security::code_scanning::Alert;
use codeyard_db_security::code_scanning::Instance;
use codeyard_model::http_config::link_origin;
use codeyard_model::urls::repo_url;
use serde_json::json;
use serde_json::Value;

fn html_url(owner: &str, repo: &str, number: i64, instance: Option<&Instance>) -> String {
    let mut url = format!(
        "{}{}/security/code-scanning/{number}",
        link_origin(),
        codeyard_types::paths::repository_path(owner, repo)
    );
    if let Some(instance) = instance {
        url.push_str(&format!(
            "?branch={}&category={}",
            codeyard_types::paths::encode_component(&instance.ref_name),
            codeyard_types::paths::encode_component(&instance.category)
        ));
    }
    url
}

pub fn code_scanning_instance(
    row: &Instance,
    owner: &str,
    repo: &str,
    number: i64,
) -> serde_json::Result<Value> {
    Ok(
        json!({"ref":row.ref_name,"analysis_key":row.category,"environment":"{}","category":row.category,
        "state":row.state,"commit_sha":row.commit_sha,"message":{"text":row.message},
        "location":serde_json::from_str::<Value>(&row.location)?,"classifications":[],
        "html_url":html_url(owner,repo,number,Some(row))}),
    )
}

pub fn code_scanning_alert(
    row: &Alert,
    instances: &[Instance],
    owner: &str,
    repo: &str,
    dismissed_by: Option<Value>,
) -> serde_json::Result<Value> {
    let url = format!(
        "{}/code-scanning/alerts/{}",
        repo_url(owner, repo),
        row.number
    );
    let instance = instances
        .iter()
        .find(|i| i.state == "open")
        .or_else(|| instances.first());
    let mut most_recent = instance
        .map(|i| code_scanning_instance(i, owner, repo, row.number))
        .transpose()?;
    if row.state == "dismissed" {
        if let Some(instance) = &mut most_recent {
            instance["state"] = json!("dismissed");
        }
    }
    let page_url = html_url(owner, repo, row.number, instance);
    Ok(
        json!({"number":row.number,"created_at":row.created_at,"updated_at":row.updated_at,
        "url":url,"html_url":page_url,
        "instances_url":format!("{url}/instances"),"state":row.state,"fixed_at":row.fixed_at,
        "dismissed_by":dismissed_by,"dismissed_at":row.dismissed_at,"dismissed_reason":row.dismissed_reason,"dismissed_comment":row.dismissed_comment,
        "rule":serde_json::from_str::<Value>(&row.rule)?,"tool":serde_json::from_str::<Value>(&row.tool)?,
        "most_recent_instance":most_recent,"assignees":[]}),
    )
}
