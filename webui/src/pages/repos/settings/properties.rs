//! Native repository metadata values; schema administration stays with organization owners.
use crate::{
    components::{
        repository_properties::RepositoryPropertyField, DraftSelect, DraftTextarea, SelectOption,
    },
    pages::{fields::csrf, repos::chrome::RepoChrome},
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Properties {
    pub names: Vec<String>,
    pub selected: Option<Property>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Property {
    pub id: i64,
    pub field: RepositoryPropertyField,
    pub value: Option<String>,
    pub draft: String,
    pub can_unset: bool,
}
fn bindings(repo: &RepoChrome) -> Element {
    rsx! {{csrf(&repo.csrf_token)} input {r#type:"hidden",name:"repository_id",value:repo.id.to_string()}}
}
pub fn section(repo: &RepoChrome, panel: &Properties) -> Element {
    let selected = panel.selected.as_ref();
    rsx! {
        section {class:"repo-panel",
            h3 {"Repository properties"}
            p {class:"text-sm text-muted-foreground","Organization-defined metadata. Repository administrators may edit only properties delegated by organization policy."}
            if panel.names.is_empty() {p {"No organization properties are defined for this repository."}}
            else {
                form {method:"get",action:format!("{}/settings",repo.base()),class:"space-y-3",
                    input {r#type:"hidden",name:"tab",value:"properties"}
                    label {r#for:"repository-property-selection","Choose a property"}
                    DraftSelect {id:Some("repository-property-selection".into()),name:"property",initial_value:selected.map(|row|row.field.name.clone()).unwrap_or_default(),
                        options:std::iter::once(SelectOption::new("","Choose a property")).chain(panel.names.iter().map(|name|SelectOption::new(name.clone(),name.clone()))).collect::<Vec<_>>()}
                    button {r#type:"submit",class:"btn btn-secondary","View value"}
                }
            }
            if let Some(property)=selected {
                div {key:"property-{property.id}",class:"mt-4 space-y-4 min-w-0",
                    h4 {class:"font-medium break-words","{property.field.name}"}
                    p {class:"break-words","{property.field.description}"}
                    p {"Type: {property.field.value_type}"}
                    if let Some(value)=&property.value {p {"Current effective value"} pre {class:"whitespace-pre-wrap break-words max-w-full","{value}"}}
                    else {p {"No value is set."}}
                    if property.field.editable {
                        form {method:"post",action:format!("{}/_forms/repository-property-set",repo.base()),class:"space-y-3",
                            {bindings(repo)}
                            input {r#type:"hidden",name:"definition_id",value:property.id.to_string()}
                            input {r#type:"hidden",name:"property_name",value:property.field.name.clone()}
                            label {r#for:"repository-property-value","Assign an explicit value"}
                            DraftTextarea {id:Some("repository-property-value".into()),name:"value",initial_value:property.draft.clone(),rows:4,
                                class:"w-full min-w-0 rounded-md border border-border bg-background px-3 py-2"}
                            p {class:"text-xs text-muted-foreground","For multiple choices, enter one exact choice per line. For true/false, enter true or false. URLs must use HTTP or HTTPS."}
                            if !property.field.allowed_values.is_empty() {
                                details {summary {"Allowed choices"} ul {for choice in &property.field.allowed_values {li {class:"break-words","{choice}"}}}}
                            }
                            button {r#type:"submit",class:"btn btn-primary","Save explicit value"}
                        }
                        if property.can_unset {
                            form {method:"post",action:format!("{}/_forms/repository-property-unset",repo.base()),
                                {bindings(repo)}
                                input {r#type:"hidden",name:"definition_id",value:property.id.to_string()}
                                input {r#type:"hidden",name:"property_name",value:property.field.name.clone()}
                                button {r#type:"submit",class:"btn btn-secondary","Unset explicit value and use default"}
                            }
                        }
                    } else {p {"Organization policy allows only organization owners to edit this property."}}
                }
            }
        }
    }
}
