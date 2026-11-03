//! Typed repository metadata inputs shared by creation and repository settings.
use super::{DraftCheckbox, DraftInput, DraftSelect, DraftTextarea, SelectOption};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RepositoryPropertyField {
    pub name: String,
    pub value_type: String,
    pub description: String,
    pub allowed_values: Vec<String>,
    pub required: bool,
    pub editable: bool,
    pub default_preview: Option<String>,
}
pub fn repository_property_field(
    field: &RepositoryPropertyField,
    value: &str,
    set: bool,
) -> Element {
    let id = format!("repository-property-{}", field.name);
    let input = format!("property_value_{}", field.name);
    let enabled = format!("property_set_{}", field.name);
    let choices = match field.value_type.as_str() {
        "single_select" => field.allowed_values.clone(),
        "true_false" => vec!["true".into(), "false".into()],
        _ => Vec::new(),
    };
    rsx! {
        div {class:"space-y-2 min-w-0 repository-property-field",
            h3 {class:"font-medium break-words","{field.name}"}
            if !field.description.is_empty() {p {class:"text-sm text-muted-foreground break-words","{field.description}"}}
            if let Some(default)=&field.default_preview {p {class:"text-xs text-muted-foreground break-words","Default preview: {default}"}}
            if field.editable {
                if field.required {
                    input {r#type:"hidden",name:enabled,value:"true"}
                    p {class:"text-sm","An explicit value is required."}
                } else {
                    label {class:"repo-check-label",
                        DraftCheckbox {name:enabled,submission_value:"true",initial_checked:set}
                        span {"Set an explicit value instead of inheriting the default"}
                    }
                }
                label {r#for:id.clone(),class:"block text-sm","Value for {field.name}"}
                if matches!(field.value_type.as_str(),"single_select"|"true_false") {
                    DraftSelect {id:Some(id.clone()),name:input,initial_value:value.to_owned(),required:field.required,
                        class:"w-full min-w-0 rounded-md border border-border bg-background px-3 py-2",
                        options:std::iter::once(SelectOption::new("","Choose a value")).chain(choices.into_iter().map(|value|SelectOption::new(value.clone(),value))).collect::<Vec<_>>()}
                } else if field.value_type=="multi_select" {
                    DraftTextarea {id:Some(id.clone()),name:input,initial_value:value.to_owned(),rows:4,required:field.required,
                        class:"w-full min-w-0 rounded-md border border-border bg-background px-3 py-2"}
                    p {class:"text-xs text-muted-foreground","Enter exact allowed choices, one per line."}
                    details {summary {"Allowed choices"} ul {for choice in &field.allowed_values {li {class:"break-words","{choice}"}}}}
                } else {
                    DraftInput {id:Some(id.clone()),name:input,initial_value:value.to_owned(),required:field.required,maxlength:Some(1024),
                        input_type:if field.value_type=="url" {"url".to_owned()} else {"text".to_owned()},
                        class:"w-full min-w-0 rounded-md border border-border bg-background px-3 py-2"}
                }
            } else {
                p {class:"text-sm text-muted-foreground","Only an organization owner can assign this property."}
                if field.required {p {class:"text-sm text-destructive","An owner must provide this required value to create the repository."}}
            }
        }
    }
}
