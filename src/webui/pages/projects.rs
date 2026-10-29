//! Native project forms render the same models on the server and after hydration.
use crate::webui::components::{
    DraftCheckbox, DraftInput, DraftSelect, DraftTextarea, SelectOption,
};
use crate::webui::pages::fields::{csrf, text_field, FIELD, SUBMIT};
use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
#[cfg(test)]
mod tests;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub number: i64,
    pub title: String,
    pub description: String,
    pub public: bool,
    pub closed: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Field {
    pub id: i64,
    pub name: String,
    pub data_type: String,
    pub options: Vec<(String, String)>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: i64,
    pub title: String,
    pub url: Option<String>,
    pub body: String,
    pub values: Vec<(i64, String, String)>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct View {
    pub number: i64,
    pub name: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub items: Vec<Item>,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ViewState {
    pub location: String,
    pub sort: String,
    pub direction: String,
    pub visible_fields: Vec<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Page {
    #[serde(default)]
    pub view_state: ViewState,
    pub owner: String,
    pub base: String,
    pub csrf_token: String,
    pub can_edit: bool,
    pub projects: Vec<Project>,
    pub project: Option<Project>,
    pub fields: Vec<Field>,
    pub settings: bool,
    pub items: Vec<Item>,
    pub columns: Vec<Column>,
    pub views: Vec<View>,
    pub view_number: i64,
    pub filter: String,
    pub message: Option<String>,
    #[serde(default)]
    pub previous: Option<String>,
    #[serde(default)]
    pub next: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Assignment {
    pub path: String,
    pub title: String,
    pub item_id: Option<i64>,
    pub can_edit: bool,
}

pub fn sidebar(
    repo: &crate::webui::pages::repos::issues::Repository,
    item: &crate::webui::pages::repos::issues::Item,
    kind: crate::webui::pages::repos::issues::Kind,
) -> Element {
    rsx! { section { class: "collaboration-sidebar-section", aria_label: "Projects",
        h3 { "Projects" }
        if item.projects.is_empty() { p { "None yet" } }
        for project in &item.projects {
            div { class: "my-2",
                a { href: "{project.path}", "{project.title}" }
                if project.can_edit {
                    form { method: "post", action: "{project.path}",
                        {csrf(&repo.csrf)}
                        input { r#type: "hidden", name: "return_to", value: "{repo.family_url(kind)}/{item.number}" }
                        if let Some(id) = project.item_id {
                            input { r#type: "hidden", name: "action", value: "delete-item" }
                            input { r#type: "hidden", name: "item_id", value: "{id}" }
                            button { "Remove" }
                        } else {
                            input { r#type: "hidden", name: "action", value: "add-item" }
                            input { r#type: "hidden", name: "kind", value: if kind == crate::webui::pages::repos::issues::Kind::Issue {"Issue"} else {"PullRequest"} }
                            input { r#type: "hidden", name: "owner", value: "{repo.owner}" }
                            input { r#type: "hidden", name: "repo", value: "{repo.name}" }
                            input { r#type: "hidden", name: "number", value: "{item.number}" }
                            button { "Add" }
                        }
                    }
                }
            }
        }
    } }
}

fn item_title(item: &Item) -> Element {
    rsx! {
        if let Some(url) = &item.url { a { href: "{url}", "{item.title}" } } else { span { "{item.title}" } }
        if !item.body.is_empty() { p { class: "text-sm", "{item.body}" } }
    }
}

fn item_editor(page: &Page, item: &Item) -> Element {
    if !page.can_edit {
        return rsx! {};
    }
    let Some(project) = &page.project else {
        return rsx! {};
    };
    rsx! { details { class: "mt-2", summary { "Edit fields" }
        form { method: "post", action: "{page.base}/{project.number}", class: "identity-form",
            {csrf(&page.csrf_token)}
            input { r#type: "hidden", name: "return_to", value: page.view_state.location.clone() }
            input { r#type: "hidden", name: "action", value: "update-item" }
            input { r#type: "hidden", name: "item_id", value: "{item.id}" }
            for field in page.fields.iter().filter(|f| f.data_type != "title") {
                {field_editor(field, item.values.iter().find(|(id,_,_)|*id == field.id).map(|(_,raw,_)|raw.as_str()).unwrap_or(""), item.id)}
            }
            button { class: SUBMIT, "Save fields" }
        }
        form { method: "post", action: "{page.base}/{project.number}",
            {csrf(&page.csrf_token)}
            input { r#type: "hidden", name: "return_to", value: page.view_state.location.clone() }
            input { r#type: "hidden", name: "action", value: "delete-item" }
            input { r#type: "hidden", name: "item_id", value: "{item.id}" }
            button { "Remove from project" }
        }
    } }
}

fn field_editor(field: &Field, raw: &str, item: i64) -> Element {
    let name = format!("value_{}", field.id);
    let id = format!("project-item-{item}-{}", field.id);
    rsx! {
        // The shown value goes along, so saving changes only the fields this editor changed.
        input { r#type: "hidden", name: "was_{field.id}", value: serde_json::to_string(raw).unwrap_or_default() }
        label { r#for: "{id}", "{field.name}" }
        if matches!(field.data_type.as_str(), "single_select" | "iteration") {
            DraftSelect { class: FIELD, id: Some(id.clone()), name, initial_value: raw.to_owned(), options: std::iter::once(SelectOption::new("", "No value")).chain(field.options.iter().map(|(id,label)|SelectOption::new(id,label))).collect::<Vec<_>>() }
        } else if field.data_type == "text" {
            crate::webui::components::DraftTextarea { class: FIELD, id: Some(id.clone()), name, initial_value: raw.to_owned(), rows: 3 }
        } else {
            DraftInput { class: FIELD, id: Some(id.clone()), name, initial_value: raw.to_owned(),
                input_type: if field.data_type == "date" { "date" } else { "text" }, inputmode: (field.data_type == "number").then(||"decimal".to_owned()) }
        }
    }
}

pub fn render(page: Page) -> Element {
    let base = &page.base;
    let visible: Vec<_> = page.fields.iter().filter(|field| field.data_type != "title" && (page.view_state.visible_fields.is_empty() || page.view_state.visible_fields.contains(&field.id))).collect();
    rsx! {
        main { class: "container mx-auto px-4 py-6", style: "max-width:1280px;min-width:0;overflow-wrap:anywhere",
            nav { aria_label: "Projects", a { href: "{base}", "{page.owner} / Projects" } }
            if let Some(message) = &page.message { p { role: "alert", "{message}" } }
            if let Some(project) = &page.project {
                h1 { class: "text-2xl font-semibold my-4", "{project.title}" }
                p { "{project.description}" }
                nav { class: "flex gap-4 my-4",
                    a { href: "{base}/{project.number}", "Project" }
                    if page.can_edit { a { href: "{base}/{project.number}/settings", "Settings" } }
                }
                if page.settings && page.can_edit {
                    h2 { class: "text-xl font-semibold", "Project settings" }
                    form { method: "post", action: "{base}/{project.number}", class: "identity-form",
                        {csrf(&page.csrf_token)}
                        input { r#type: "hidden", name: "action", value: "save" }
                        {text_field("title", "Project name", project.title.clone(), "text", true, 255)}
                        label { r#for: "description", "Description" }
                        DraftTextarea { class: FIELD, id: Some("description".into()), name: "description", maxlength: Some(4096), initial_value: project.description.clone() }
                        label { DraftCheckbox { name: "public", initial_checked: project.public } " Public project" }
                        label { DraftCheckbox { name: "closed", initial_checked: project.closed } " Closed" }
                        button { class: SUBMIT, "Save" }
                    }
                    h2 { class: "text-xl font-semibold mt-6", "Fields" }
                    ul { for field in &page.fields {
                        li { class: "flex flex-wrap gap-2 my-2", "{field.name} ({field.data_type})"
                            if field.data_type != "title" {
                                form { method: "post", action: "{base}/{project.number}",
                                    {csrf(&page.csrf_token)}
                                    input { r#type: "hidden", name: "action", value: "delete-field" }
                                    input { r#type: "hidden", name: "field_id", value: "{field.id}" }
                                    button { r#type: "submit", "Delete field" }
                                }
                            }
                        }
                    } }
                    form { method: "post", action: "{base}/{project.number}", class: "identity-form",
                        {csrf(&page.csrf_token)}
                        input { r#type: "hidden", name: "action", value: "add-field" }
                        {text_field("name", "Field name", String::new(), "text", true, 255)}
                        label { r#for: "data_type", "Field type" }
                        select { class: FIELD, id: "data_type", name: "data_type",
                            for kind in ["text", "number", "date", "single_select", "iteration"] { option { value: kind, "{kind}" } }
                        }
                        label { r#for: "options", "Single-select options (one name per line)" }
                        DraftTextarea { class: FIELD, id: Some("options".into()), name: "options", initial_value: String::new() }
                        {text_field("start_date", "Iteration start date", String::new(), "date", false, 10)}
                        {text_field("duration", "Iteration duration in days", "14".into(), "number", false, 3)}
                        button { class: SUBMIT, "Add field" }
                    }
                    h2 { class: "text-xl font-semibold mt-6", "Link a repository" }
                    form { method: "post", action: "{base}/{project.number}", class: "identity-form",
                        {csrf(&page.csrf_token)}
                        input { r#type: "hidden", name: "action", value: "link-repository" }
                        {text_field("owner", "Repository owner", String::new(), "text", true, 255)}
                        {text_field("repo", "Repository name", String::new(), "text", true, 255)}
                        label { input { r#type: "checkbox", name: "unlink" } " Unlink repository" }
                        button { class: SUBMIT, "Save repository link" }
                    }
                    form { method: "post", action: "{base}/{project.number}", class: "mt-6",
                        {csrf(&page.csrf_token)}
                        input { r#type: "hidden", name: "action", value: "delete" }
                        button { class: "btn-danger", "Delete project" }
                    }
                } else {
                    p { if project.closed { "This project is closed." } else { "Plan and track work across repositories." } }
                    nav { class: "flex flex-wrap gap-4 my-4", for view in &page.views {
                        a { href: "{base}/{project.number}/views/{view.number}", "{view.name}" }
                    } }
                    form { method: "get", class: "flex flex-wrap gap-2 my-4",
                        DraftInput { class: FIELD, name: "q", aria_label: Some("Filter project items".into()), initial_value: page.filter.clone(), placeholder: "Filter items, e.g. status:Todo" }
                        DraftSelect { class: FIELD, name: "sort", aria_label: Some("Sort field".into()), initial_value: page.view_state.sort.clone(), options: std::iter::once(SelectOption::new("", "Sort by…")).chain(page.fields.iter().map(|field| SelectOption::new(field.id.to_string(), &field.name))).collect::<Vec<_>>() }
                        DraftSelect { class: FIELD, name: "direction", aria_label: Some("Sort direction".into()), initial_value: page.view_state.direction.clone(), options: vec![SelectOption::new("asc", "Ascending"), SelectOption::new("desc", "Descending")] }
                        button { class: SUBMIT, "Apply" }
                    }
                    if page.columns.is_empty() {
                        div { style: "max-width:100%;overflow-x:auto",
                            table { class: "w-full border-collapse", aria_label: "Project items",
                                thead { tr { th { class: "p-2 text-left", "Title" } for field in &visible { th { class: "p-2 text-left", "{field.name}" } } } }
                                tbody { for item in &page.items {
                                    tr { class: "border-t",
                                        td { class: "p-2", {item_title(item)} {item_editor(&page, item)} }
                                        for field in &visible { td { class: "p-2", "{item.values.iter().find(|(id,_,_)| *id == field.id).map_or(\"\", |(_,_,display)| display.as_str())}" } }
                                    }
                                } }
                            }
                        }
                    } else {
                        div { style: "display:grid;grid-template-columns:repeat(auto-fit,minmax(min(100%,260px),1fr));gap:16px",
                            for column in &page.columns {
                                section { class: "border rounded p-3", style: "min-width:0", aria_label: "{column.name}",
                                    h2 { class: "font-semibold mb-3", "{column.name} ({column.items.len()})" }
                                    for item in &column.items { article { class: "border rounded p-3 my-2", {item_title(item)} {item_editor(&page,item)} } }
                                }
                            }
                        }
                    }
                    if page.items.is_empty() { p { "No matching items." } }
                    {crate::webui::pages::discovery::page_links(page.previous.clone(), page.next.clone(), "Project items")}
                    if page.can_edit {
                        h2 { class: "text-xl font-semibold mt-6", "Add item" }
                        form { method: "post", action: "{base}/{project.number}", class: "identity-form",
                            {csrf(&page.csrf_token)}
                            input { r#type: "hidden", name: "action", value: "draft" }
                            {text_field("title", "Draft title", String::new(), "text", true, 255)}
                            label { r#for: "body", "Draft description" }
                            DraftTextarea { class: FIELD, id: Some("body".into()), name: "body", maxlength: Some(65536), initial_value: String::new() }
                            button { class: SUBMIT, "Add draft" }
                        }
                        form { method: "post", action: "{base}/{project.number}", class: "identity-form mt-4",
                            {csrf(&page.csrf_token)}
                            input { r#type: "hidden", name: "action", value: "add-item" }
                            select { class: FIELD, name: "kind", aria_label: "Item type", option { value: "Issue", "Issue" } option { value: "PullRequest", "Pull request" } }
                            {text_field("owner", "Repository owner", String::new(), "text", true, 255)}
                            {text_field("repo", "Repository name", String::new(), "text", true, 255)}
                            {text_field("number", "Issue or pull request number", String::new(), "number", true, 10)}
                            button { class: SUBMIT, "Add existing item" }
                        }
                        details { class: "my-4", summary { "New view" }
                            form { method: "post", action: "{base}/{project.number}", class: "identity-form",
                                {csrf(&page.csrf_token)}
                                input { r#type: "hidden", name: "action", value: "view" }
                                {text_field("name", "View name", String::new(), "text", true, 255)}
                                select { class: FIELD, name: "layout", aria_label: "View layout", option { value: "table", "Table" } option { value: "board", "Board" } }
                                {text_field("filter", "Filter", page.filter.clone(), "text", false, 4096)}
                                select { class: FIELD, name: "group", aria_label: "Board field",
                                    option { value: "", "Status" }
                                    for field in page.fields.iter().filter(|f|f.data_type == "single_select") { option { value: "{field.id}", "{field.name}" } }
                                }
                                button { class: SUBMIT, "Create view" }
                            }
                        }
                    }
                }
            } else {
                h1 { class: "text-2xl font-semibold my-4", "Projects" }
                ul { for project in &page.projects {
                    li { class: "border rounded p-4 my-2",
                        a { href: "{base}/{project.number}", class: "font-semibold", "{project.title}" }
                        p { "{project.description}" }
                        span { if project.closed { "Closed" } else { "Open" } }
                    }
                } }
                if page.projects.is_empty() { p { "No projects yet." } }
                if page.can_edit {
                    h2 { class: "text-xl font-semibold mt-6", "New project" }
                    form { method: "post", action: "{base}", class: "identity-form",
                        {csrf(&page.csrf_token)}
                        {text_field("title", "Project name", String::new(), "text", true, 255)}
                        label { input { r#type: "checkbox", name: "public" } " Public project" }
                        button { class: SUBMIT, "Create project" }
                    }
                }
            }
        }
    }
}
