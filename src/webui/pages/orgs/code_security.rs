use crate::webui::{navigation::NavLink, pages::fields::csrf};
use dioxus::prelude::*;
use std::collections::BTreeMap;

pub const TOGGLES: [(&str, &str); 5] = [
    ("secret_scanning", "Secret scanning"),
    ("secret_scanning_push_protection", "Push protection"),
    ("dependabot_alerts", "Dependabot alerts"),
    ("dependency_graph", "Dependency graph"),
    ("private_vulnerability_reporting", "Private vulnerability reporting"),
];
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Configuration {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub settings: BTreeMap<String, String>,
    pub default_for_new_repos: String,
}
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Page {
    pub org_id: i64,
    pub org_name: String,
    pub csrf_token: String,
    pub configurations: Vec<Configuration>,
}

fn editor(page: &Page, row: Option<&Configuration>) -> Element {
    let base = format!("/settings/organizations/{}/security", page.org_id);
    rsx! {
        form { method:"post",action:format!("{base}/save"),class:"repo-security-dismiss",
            {csrf(&page.csrf_token)}
            if let Some(row)=row {input {r#type:"hidden",name:"configuration_id",value:row.id.to_string()}}
            label {"Name" input {name:"name",required:true,maxlength:255,value:row.map_or("",|r|r.name.as_str()),class:"form-control"}}
            label {"Description" input {name:"description",maxlength:255,value:row.map_or("",|r|r.description.as_str()),class:"form-control"}}
            for (name,label) in TOGGLES {
                label {"{label}"
                    select {name,value:row.and_then(|r|r.settings.get(name)).map_or("disabled",String::as_str),class:"form-select",
                        option {value:"disabled",selected:row.and_then(|r|r.settings.get(name)).is_none_or(|s|s=="disabled"),"Disabled"}
                        option {value:"enabled",selected:row.and_then(|r|r.settings.get(name)).is_some_and(|s|s=="enabled"),"Enabled"}
                        option {value:"not_set",selected:row.and_then(|r|r.settings.get(name)).is_some_and(|s|s=="not_set"),"Leave unchanged"}
                    }
                }
            }
            button {r#type:"submit",class:"btn btn-primary",if row.is_some() {"Save configuration"} else {"Create configuration"}}
        }
    }
}
pub fn render(page: Page) -> Element {
    let base = format!("/settings/organizations/{}/security", page.org_id);
    rsx! {
        main {class:"page-shell repo-security",
            NavLink {href:format!("/settings/organizations/{}",page.org_id),"← Organization settings"}
            h1 {"{page.org_name}: Code security configurations"}
            p {"Configurations store repository security settings. CodeQL default setup is off; upload SARIF from an Actions workflow for code scanning."}
            h2 {"New configuration"}
            {editor(&page,None)}
            for row in &page.configurations {
                section {id:format!("code-security-{}",row.id),class:"repo-security-alert",
                    h2 {"{row.name}"}
                    {editor(&page,Some(row))}
                    form {method:"post",action:format!("{base}/defaults"),class:"repo-security-dismiss",
                        {csrf(&page.csrf_token)} input {r#type:"hidden",name:"configuration_id",value:row.id.to_string()}
                        label {"Apply to new repositories"
                            select {name:"default_for_new_repos",value:row.default_for_new_repos.clone(),class:"form-select",
                                option {value:"none",selected:row.default_for_new_repos=="none","None"}
                                option {value:"all",selected:row.default_for_new_repos=="all","All"}
                                option {value:"public",selected:row.default_for_new_repos=="public","Public"}
                                option {value:"private_and_internal",selected:row.default_for_new_repos=="private_and_internal","Private"}
                            }
                        }
                        button {r#type:"submit",class:"btn btn-secondary","Set default"}
                    }
                    form {method:"post",action:format!("{base}/attach"),class:"repo-security-dismiss",
                        {csrf(&page.csrf_token)} input {r#type:"hidden",name:"configuration_id",value:row.id.to_string()}
                        label {"Existing repositories"
                            select {name:"scope",class:"form-select",
                                option {value:"all","All"} option {value:"all_without_configurations","Without configurations"} option {value:"public","Public"} option {value:"private_or_internal","Private"} option {value:"selected","Selected IDs"}
                            }
                        }
                        label {"Repository IDs (comma-separated, for Selected IDs)" input {name:"repository_ids",class:"form-control"}}
                        button {r#type:"submit",class:"btn btn-secondary","Attach configuration"}
                    }
                    form {method:"post",action:format!("{base}/delete"),class:"repo-security-dismiss",
                        {csrf(&page.csrf_token)} input {r#type:"hidden",name:"configuration_id",value:row.id.to_string()}
                        button {r#type:"submit",class:"btn btn-danger","Delete configuration"}
                    }
                }
            }
        }
    }
}
