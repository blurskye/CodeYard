//! Repository property definitions and owner-only bulk value forms; all forms work without scripts.
use super::Page;
use crate::webui::{
    components::{DraftCheckbox, DraftInput, DraftSelect, DraftTextarea, SelectOption},
    navigation::NavLink,
    pages::fields::csrf,
};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PropertyPanel {
    pub definitions: Vec<PropertySummary>,
    pub selected: Option<PropertyDefinition>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PropertySummary {
    pub name: String,
    pub value_type: String,
    pub description: String,
    pub required: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PropertyDefinition {
    pub name: String,
    pub value_type: String,
    pub description: String,
    pub required: bool,
    pub require_explicit_values: bool,
    pub has_default: bool,
    pub default_value: String,
    pub allowed_values: String,
    pub values_editable_by: String,
}
fn value(page: &Page, action: &str, key: &str, fallback: &str) -> String {
    if page
        .draft
        .get("action")
        .is_some_and(|value| value == action)
    {
        page.draft.get(key).cloned().unwrap_or_default()
    } else {
        fallback.to_owned()
    }
}
fn checked_action(page: &Page, action: &str, key: &str, fallback: bool) -> bool {
    if page
        .draft
        .get("action")
        .is_some_and(|value| value == action)
    {
        page.draft.get(key).is_some_and(|value| value == "true")
    } else {
        fallback
    }
}
fn checked(page: &Page, key: &str, fallback: bool) -> bool {
    checked_action(page, "save-property", key, fallback)
}
fn kind_label(kind: &str) -> &str {
    match kind {
        "string" => "Text",
        "single_select" => "Single choice",
        "multi_select" => "Multiple choices",
        "true_false" => "True or false",
        "url" => "Web URL",
        _ => kind,
    }
}
fn hidden(name: &str, value: &str) -> Element {
    rsx! {input {r#type:"hidden",name:name.to_owned(),value:value.to_owned()}}
}
pub(super) fn panel(page: &Page) -> Element {
    let (Some(org), Some(panel)) = (&page.selected, &page.properties) else {
        return rsx! {};
    };
    let base = format!("/settings/organizations/{}", org.id);
    let selected = panel.selected.as_ref();
    let name = selected.map_or("", |property| property.name.as_str());
    let target = format!(
        "{base}/properties?tab=properties&property={}",
        crate::webui::encode_component(name)
    );
    let mutable = page.is_owner && !org.suspended;
    let kind = value(
        page,
        "save-property",
        "value_type",
        selected.map_or("string", |property| property.value_type.as_str()),
    );
    let editors = value(
        page,
        "save-property",
        "values_editable_by",
        selected.map_or("org_actors", |property| {
            property.values_editable_by.as_str()
        }),
    );
    rsx! {
        section {class:"org-section org-properties",aria_labelledby:"organization-properties-title",
            div {class:"org-section-header",div {h2 {id:"organization-properties-title","Repository properties"} p {"Organization-defined metadata for repositories. Members can read definitions; only owners manage them."}}}
            section {class:"org-panel",header {class:"org-panel-header",h3 {"Property catalog"}}
                div {class:"org-panel-body",
                    if panel.definitions.is_empty() {p {"No repository properties are defined."}}
                    else {
                        form {method:"get",action:base.clone(),class:"org-form",
                            {hidden("tab","properties")}
                            label {r#for:"property-selection","Choose a property"}
                            DraftSelect {id:Some("property-selection".into()),name:"property",initial_value:name.to_owned(),
                                options:std::iter::once(SelectOption::new("",if mutable {"New property"} else {"Choose a property"})).chain(panel.definitions.iter().map(|property|SelectOption::new(property.name.clone(),format!("{} · {}",property.name,kind_label(&property.value_type))))).collect::<Vec<_>>()}
                            button {r#type:"submit",class:"btn btn-secondary","View property"}
                        }
                    }
                    if let Some(property)=selected {
                        dl {class:"org-facts",dt {"Name"} dd {"{property.name}"} dt {"Type"} dd {"{kind_label(&property.value_type)}"} dt {"Description"} dd {"{property.description}"}
                            dt {"Required"} dd {if property.required {"Yes"} else {"No"}}
                            dt {"Explicit value required"} dd {if property.require_explicit_values {"Yes"} else {"No"}}
                            dt {"Value editors"} dd {if property.values_editable_by=="org_and_repo_actors" {"Organization owners and repository administrators"} else {"Organization owners"}}
                        }
                        if !property.allowed_values.is_empty() {p {"Allowed choices"} pre {class:"whitespace-pre-wrap break-words","{property.allowed_values}"}}
                        if property.has_default {p {"Default value"} pre {class:"whitespace-pre-wrap break-words","{property.default_value}"}}
                    }
                }
            }
            if mutable {
                section {class:"org-panel",header {class:"org-panel-header",h3 {if selected.is_some() {"Edit definition"} else {"New definition"}}}
                    div {class:"org-panel-body",
                        form {key:"definition-{name}",method:"post",action:target.clone(),class:"org-form",
                            {csrf(&page.csrf_token)} {hidden("action","save-property")}
                            label {r#for:"property-name","Property name"}
                            DraftInput {id:Some("property-name".into()),name:"property_name",initial_value:value(page,"save-property","property_name",name),readonly:selected.is_some(),required:true,maxlength:Some(75),pattern:Some("[A-Za-z0-9_-]{1,75}".into())}
                            p {class:"org-field-help","Names use ASCII letters, numbers, hyphens and underscores."}
                            label {r#for:"property-type","Value type"}
                            DraftSelect {id:Some("property-type".into()),name:"value_type",initial_value:kind.clone(),
                                options:["string","single_select","multi_select","true_false","url"].into_iter().map(|kind|SelectOption::new(kind,kind_label(kind))).collect::<Vec<_>>()}
                            label {r#for:"property-description","Description"}
                            DraftTextarea {id:Some("property-description".into()),name:"description",rows:3,initial_value:value(page,"save-property","description",selected.map_or("",|property|property.description.as_str()))}
                            label {r#for:"property-choices","Allowed choices"}
                            DraftTextarea {id:Some("property-choices".into()),name:"allowed_values",rows:5,initial_value:value(page,"save-property","allowed_values",selected.map_or("",|property|property.allowed_values.as_str()))}
                            p {class:"org-field-help","Select properties only: up to 200 exact choices, one per line."}
                            label {class:"org-confirm",DraftCheckbox {name:"required",submission_value:"true",initial_checked:checked(page,"required",selected.is_some_and(|property|property.required))} span {"Require a value"}}
                            label {class:"org-confirm",DraftCheckbox {name:"require_explicit_values",submission_value:"true",initial_checked:checked(page,"require_explicit_values",selected.is_some_and(|property|property.require_explicit_values))} span {"Require an explicitly assigned value, not a default"}}
                            p {class:"org-field-help","Assign values to existing repositories before requiring explicit values. New repositories must supply them during creation."}
                            label {class:"org-confirm",DraftCheckbox {name:"use_default",submission_value:"true",initial_checked:checked(page,"use_default",selected.is_some_and(|property|property.has_default))} span {"Use a default for repositories without an explicit value"}}
                            label {r#for:"property-default","Default value"}
                            DraftTextarea {id:Some("property-default".into()),name:"default_value",rows:3,initial_value:value(page,"save-property","default_value",selected.map_or("",|property|property.default_value.as_str()))}
                            p {class:"org-field-help","Text, URL or true/false as text; multiple choices as one exact choice per line."}
                            label {r#for:"property-editors","Who can edit repository values?"}
                            DraftSelect {id:Some("property-editors".into()),name:"values_editable_by",initial_value:editors.clone(),
                                options:vec![SelectOption::new("org_actors","Organization owners only"),SelectOption::new("org_and_repo_actors","Organization owners and repository administrators")]}
                            button {r#type:"submit",class:"btn btn-primary","Save definition"}
                        }
                    }
                }
                if let Some(property)=selected {
                    section {class:"org-panel",header {class:"org-panel-header",h3 {"Apply a value to repositories"}}
                        div {class:"org-panel-body",
                            form {key:"value-{name}",method:"post",action:target.clone(),class:"org-form",
                                {csrf(&page.csrf_token)} {hidden("action","set-property-value")} {hidden("property_name",&property.name)}
                                label {r#for:"property-repositories","Repository names"}
                                DraftTextarea {id:Some("property-repositories".into()),name:"repository_names",rows:4,required:true,initial_value:value(page,"set-property-value","repository_names","")}
                                p {class:"org-field-help","Up to 30 repository names in this organization, one per line. The whole batch succeeds or fails together."}
                                label {r#for:"property-value","Value"}
                                DraftTextarea {id:Some("property-value".into()),name:"value",rows:3,initial_value:value(page,"set-property-value","value","")}
                                p {class:"org-field-help","Multiple choices: one exact allowed choice per line. True/false: enter true or false."}
                                if !property.require_explicit_values&&(!property.required||property.has_default) {
                                    label {class:"org-confirm",DraftCheckbox {name:"unset",submission_value:"true",initial_checked:checked_action(page,"set-property-value","unset",false)} span {"Unset the explicit value and use the default, if any"}}
                                }
                                button {r#type:"submit",class:"btn btn-primary","Apply value"}
                            }
                        }
                    }
                    section {class:"org-panel org-danger",header {class:"org-panel-header",h3 {"Delete property"}}
                        div {class:"org-panel-body",p {"Deleting a definition also removes its explicit values from every repository."}
                            form {key:"delete-{name}",method:"post",action:target.clone(),class:"org-form",
                                {csrf(&page.csrf_token)} {hidden("action","delete-property")} {hidden("property_name",&property.name)}
                                label {r#for:"delete-property-confirm","Type the property name to confirm"}
                                DraftInput {id:Some("delete-property-confirm".into()),name:"confirm",initial_value:value(page,"delete-property","confirm",""),required:true}
                                button {r#type:"submit",class:"btn btn-danger","Delete property"}
                            }
                        }
                    }
                }
            } else if page.is_owner {p {"Organization changes are suspended until an active owner is available."}}
            NavLink {href:format!("{base}?tab=properties"),"View all definitions"}
        }
    }
}
