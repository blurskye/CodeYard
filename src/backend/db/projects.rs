//! Owner-scoped projects. Native transactions keep authority and project changes together.
use crate::backend::db::users::accounts::AccountCredential;
use anyhow::Result;
use chrono::{DateTime, Utc};
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub(crate) fn readable_condition(viewer: &str) -> String {
    format!(
        "p.deleted_at IS NULL AND {}",
        readable_owner_condition(viewer)
    )
}

pub(crate) fn readable_owner_condition(viewer: &str) -> String {
    format!("(p.owner_kind<>'org' OR COALESCE((SELECT s.has_organization_projects FROM organization_settings s WHERE s.org_id=p.owner_org_id),TRUE)) AND (p.public OR (EXISTS(SELECT 1 FROM users active WHERE active.id={viewer} AND NOT active.is_banned AND NOT active.must_change_password) AND (p.owner_user_id={viewer} OR EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=p.owner_org_id AND m.user_id={viewer} AND m.role IN ('owner','admin','member')))))")
}

#[derive(Clone, Copy, Debug)]
pub enum Owner {
    User(i64),
    Organization(i64),
}
impl Owner {
    pub fn parts(self) -> (&'static str, i64) {
        match self {
            Self::User(id) => ("user", id),
            Self::Organization(id) => ("org", id),
        }
    }
}

/// Fields one project may have, the title included. One item update may set every field.
pub const MAX_FIELDS: usize = 100;
pub type ItemLink = (i64, String, String, String, i32);

native_operation!(crate::backend::db::postgres::projects::item_links, crate::backend::db::sqlite::projects::item_links,
    fn item_links(pool: &DbPool, viewer: Option<i64>, project: i64, items: &[i64]) -> Result<Vec<ItemLink>>);

pub struct ItemWindow {
    pub project: i64,
    pub include_deleted: bool,
    pub archived: Option<bool>,
    pub descending: bool,
    pub offset: i64,
    pub limit: i64,
    pub count: bool,
}
native_operation!(crate::backend::db::postgres::projects::items_window, crate::backend::db::sqlite::projects::items_window,
    fn items_window(pool: &DbPool, viewer: Option<i64>, window: &ItemWindow) -> Result<(i64, Vec<Item>)>);

// Deleted payloads retain read-only access in their originating request; ordinary readers use false.
native_operation!(crate::backend::db::postgres::projects::by_id_at, crate::backend::db::sqlite::projects::by_id_at,
    fn by_id_at(pool: &DbPool, viewer: Option<i64>, id: i64, include_deleted: bool) -> Result<Project>);
native_operation!(crate::backend::db::postgres::projects::fields_at, crate::backend::db::sqlite::projects::fields_at,
    fn fields_at(pool: &DbPool, viewer: Option<i64>, project: i64, include_deleted: bool) -> Result<Vec<Field>>);
native_operation!(crate::backend::db::postgres::projects::items_at, crate::backend::db::sqlite::projects::items_at,
    fn items_at(pool: &DbPool, viewer: Option<i64>, project: i64, include_deleted: bool) -> Result<Vec<Item>>);
native_operation!(crate::backend::db::postgres::projects::item_by_id_at, crate::backend::db::sqlite::projects::item_by_id_at,
    fn item_by_id_at(pool: &DbPool, viewer: Option<i64>, id: i64, include_deleted: bool) -> Result<Item>);
native_operation!(crate::backend::db::postgres::projects::content_items, crate::backend::db::sqlite::projects::content_items,
    fn content_items(pool: &DbPool, viewer: Option<i64>, content: &Content, include_archived: bool, offset: i64, limit: i64, count: bool) -> Result<(i64, Vec<Item>)>);

#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct Project {
    pub id: i64,
    pub owner_kind: String,
    pub owner_id: i64,
    pub number: i64,
    pub creator_id: i64,
    pub title: String,
    pub description: Option<String>,
    pub readme: String,
    pub template: bool,
    pub public: bool,
    pub closed_at: Option<DateTime<Utc>>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Summary projections never read the potentially large readme. Detail operations load it explicitly.
pub(crate) const SUMMARY_COLUMNS: &str = "p.id,p.owner_kind,p.owner_id,p.number,p.creator_id,p.title,p.description,'' AS readme,p.template,p.public,p.closed_at,p.deleted_at,p.created_at,p.updated_at";

#[derive(Default)]
pub enum ListOrder {
    #[default]
    Id,
    Number,
    Title,
    Created,
    Updated,
}
#[derive(Default)]
pub struct ListFilter {
    pub owner: Option<Owner>,
    pub repo: Option<i64>,
    pub content: Option<(bool, i64)>,
    pub titles: Vec<String>,
    pub open: bool,
    pub closed: bool,
    pub writable: bool,
    /// Exclusive keyset bounds on the project id, for cursor pages.
    pub after: Option<i64>,
    pub before: Option<i64>,
    pub order: ListOrder,
    pub descending: bool,
}
impl ListFilter {
    pub fn search(owner: Option<Owner>, query: &str) -> Self {
        let mut filter = Self {
            owner,
            ..Default::default()
        };
        for word in query.split_whitespace() {
            match word {
                "is:open" => filter.open = true,
                "is:closed" => filter.closed = true,
                _ => filter.titles.push(word.to_owned()),
            }
        }
        filter
    }
    pub(crate) fn statement(
        &self,
        postgres: bool,
    ) -> crate::backend::db::discussion_search::Statement {
        let mut sql = crate::backend::db::discussion_search::Statement::new(postgres);
        let mut conditions = vec![readable_condition(&sql.placeholder(1))];
        if let Some(owner) = self.owner {
            let (kind, id) = owner.parts();
            let kind = sql.bind(kind);
            let id = sql.bind(id.to_string());
            conditions.push(format!(
                "p.owner_kind={kind} AND p.owner_id=CAST({id} AS BIGINT)"
            ));
        }
        if let Some(repo) = self.repo {
            let repo = sql.bind(repo.to_string());
            let readable = if postgres {
                crate::backend::db::postgres::repositories::access::readable_condition()
            } else {
                crate::backend::db::sqlite::repositories::access::readable_condition()
            };
            let viewer = sql.placeholder(1);
            conditions.push(format!("EXISTS(SELECT 1 FROM project_repositories pr JOIN repositories r ON r.id=pr.repo_id LEFT JOIN users u ON u.id={viewer} WHERE pr.project_id=p.id AND pr.repo_id=CAST({repo} AS BIGINT) AND ({readable}))"));
        }
        if let Some((issue, id)) = self.content {
            let id = sql.bind(id.to_string());
            let column = if issue { "issue_id" } else { "pull_request_id" };
            let readable = if postgres {
                crate::backend::db::postgres::repositories::access::readable_condition()
            } else {
                crate::backend::db::sqlite::repositories::access::readable_condition()
            };
            let viewer = sql.placeholder(1);
            conditions.push(format!("EXISTS(SELECT 1 FROM project_items i JOIN repositories r ON r.id=i.repo_id LEFT JOIN users u ON u.id={viewer} WHERE i.project_id=p.id AND i.{column}=CAST({id} AS BIGINT) AND ({readable}))"));
        }
        if let Some(after) = self.after {
            let after = sql.bind(after.to_string());
            conditions.push(format!("p.id>CAST({after} AS BIGINT)"));
        }
        if let Some(before) = self.before {
            let before = sql.bind(before.to_string());
            conditions.push(format!("p.id<CAST({before} AS BIGINT)"));
        }
        for title in &self.titles {
            conditions.push(sql.matches("p.title", title));
        }
        if self.open {
            conditions.push("p.closed_at IS NULL".into());
        }
        if self.closed {
            conditions.push("p.closed_at IS NOT NULL".into());
        }
        if self.writable {
            let viewer = sql.placeholder(1);
            conditions.push(format!("EXISTS(SELECT 1 FROM users active WHERE active.id={viewer} AND NOT active.is_banned AND NOT active.must_change_password) AND (p.owner_user_id={viewer} OR EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=p.owner_org_id AND m.user_id={viewer} AND m.role IN ('owner','admin') AND EXISTS(SELECT 1 FROM org_memberships owners JOIN users u ON u.id=owners.user_id WHERE owners.org_id=p.owner_org_id AND owners.role='owner' AND NOT u.is_banned AND NOT u.must_change_password)))"));
        }
        sql.prefix = format!(
            "FROM projects p WHERE {}",
            conditions
                .into_iter()
                .map(|s| format!("({s})"))
                .collect::<Vec<_>>()
                .join(" AND ")
        );
        sql
    }
    pub(crate) fn order(&self, postgres: bool) -> String {
        let field = match self.order {
            ListOrder::Id => "p.id",
            ListOrder::Number => "p.number",
            ListOrder::Title if postgres => "p.title COLLATE \"C\"",
            ListOrder::Title => "p.title",
            ListOrder::Created => "p.created_at",
            ListOrder::Updated => "p.updated_at",
        };
        let direction = if self.descending { "DESC" } else { "ASC" };
        format!("{field} {direction},p.id {direction}")
    }
}

/// A project update. A field left as None keeps its stored value.
#[derive(Clone, Debug)]
pub struct Settings {
    pub title: Option<String>,
    pub description: Option<String>,
    pub public: Option<bool>,
    pub closed: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct Field {
    pub id: i64,
    pub project_id: i64,
    pub name: String,
    pub data_type: String,
    pub configuration: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewField {
    pub name: String,
    pub data_type: String,
    pub single_select_options: Option<Vec<SelectOption>>,
    pub iteration_configuration: Option<IterationConfiguration>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectOption {
    pub name: String,
    pub color: Option<String>,
    pub description: Option<String>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IterationConfiguration {
    pub start_date: chrono::NaiveDate,
    pub duration: i32,
    #[serde(default)]
    pub iterations: Vec<Iteration>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Iteration {
    pub title: String,
    pub start_date: chrono::NaiveDate,
    pub duration: i32,
}

pub fn valid_name(name: &str) -> bool {
    !name.trim().is_empty() && name.len() <= 255 && !name.chars().any(char::is_control)
}

#[derive(Clone, Copy)]
pub enum BuiltInField {
    Assignees,
    Labels,
    LinkedPullRequests,
    Milestone,
    Repository,
    Reviewers,
}
impl BuiltInField {
    pub const ALL: [Self; 6] = [
        Self::Assignees,
        Self::Labels,
        Self::LinkedPullRequests,
        Self::Milestone,
        Self::Repository,
        Self::Reviewers,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::Assignees => "Assignees",
            Self::Labels => "Labels",
            Self::LinkedPullRequests => "Linked pull requests",
            Self::Milestone => "Milestone",
            Self::Repository => "Repository",
            Self::Reviewers => "Reviewers",
        }
    }
}

impl NewField {
    pub(crate) fn configuration(&self) -> Result<String> {
        anyhow::ensure!(
            valid_name(&self.name)
                && !BuiltInField::ALL
                    .iter()
                    .any(|field| field.name().eq_ignore_ascii_case(self.name.trim())),
            Refusal::Invalid
        );
        let value = match self.data_type.as_str() {
            "text" | "number" | "date" => {
                anyhow::ensure!(
                    self.single_select_options.is_none() && self.iteration_configuration.is_none(),
                    Refusal::Invalid
                );
                json!({})
            }
            "single_select" => {
                anyhow::ensure!(self.iteration_configuration.is_none(), Refusal::Invalid);
                let options = self
                    .single_select_options
                    .as_ref()
                    .ok_or(Refusal::Invalid)?;
                anyhow::ensure!(
                    !options.is_empty() && options.len() <= 100,
                    Refusal::Invalid
                );
                let mut names = std::collections::HashSet::new();
                let mut values = Vec::new();
                for option in options {
                    anyhow::ensure!(
                        valid_name(&option.name) && names.insert(&option.name),
                        Refusal::Invalid
                    );
                    let color = option.color.as_deref().unwrap_or("GRAY");
                    anyhow::ensure!(
                        matches!(
                            color,
                            "BLUE"
                                | "GRAY"
                                | "GREEN"
                                | "ORANGE"
                                | "PINK"
                                | "PURPLE"
                                | "RED"
                                | "YELLOW"
                        ),
                        Refusal::Invalid
                    );
                    let description = option.description.as_deref().unwrap_or("");
                    anyhow::ensure!(description.len() <= 1024, Refusal::Invalid);
                    values.push(json!({"id": uuid::Uuid::new_v4().to_string(), "name": option.name, "color": color, "description": description}));
                }
                json!({"options": values})
            }
            "iteration" => {
                anyhow::ensure!(self.single_select_options.is_none(), Refusal::Invalid);
                let config = self
                    .iteration_configuration
                    .as_ref()
                    .ok_or(Refusal::Invalid)?;
                anyhow::ensure!(
                    (1..=365).contains(&config.duration) && config.iterations.len() <= 100,
                    Refusal::Invalid
                );
                let mut iterations = Vec::new();
                for iteration in &config.iterations {
                    anyhow::ensure!(
                        valid_name(&iteration.title) && (1..=365).contains(&iteration.duration),
                        Refusal::Invalid
                    );
                    iterations.push(json!({"id": uuid::Uuid::new_v4().to_string(), "title": iteration.title, "start_date": iteration.start_date, "duration": iteration.duration}));
                }
                json!({"start_date": config.start_date, "duration": config.duration, "iterations": iterations})
            }
            _ => return Err(Refusal::Invalid.into()),
        };
        Ok(value.to_string())
    }
}

native_operation!(crate::backend::db::postgres::projects::create, crate::backend::db::sqlite::projects::create,
    fn create(pool: &DbPool, actor: i64, credential: &AccountCredential<'_>, owner: Owner, title: &str, description: Option<&str>, public: bool) -> Result<Project>);
native_operation!(crate::backend::db::postgres::projects::list, crate::backend::db::sqlite::projects::list,
    fn list(pool: &DbPool, viewer: Option<i64>, filter: &ListFilter, offset: i64, limit: i64) -> Result<(i64, Vec<Project>)>);
native_operation!(crate::backend::db::postgres::projects::list_page, crate::backend::db::sqlite::projects::list_page,
    fn list_page(pool: &DbPool, viewer: Option<i64>, owner: Owner, after: i64, limit: i64) -> Result<Vec<Project>>);
native_operation!(crate::backend::db::postgres::projects::fields_page, crate::backend::db::sqlite::projects::fields_page,
    fn fields_page(pool: &DbPool, viewer: Option<i64>, project: i64, after: i64, limit: i64) -> Result<Vec<Field>>);
native_operation!(crate::backend::db::postgres::projects::get, crate::backend::db::sqlite::projects::get,
    fn get(pool: &DbPool, viewer: Option<i64>, owner: Owner, number: i64) -> Result<Project>);
native_operation!(crate::backend::db::postgres::projects::save, crate::backend::db::sqlite::projects::save,
    fn save(pool: &DbPool, actor: i64, credential: &AccountCredential<'_>, project: i64, settings: &Settings) -> Result<Project>);
native_operation!(crate::backend::db::postgres::projects::delete, crate::backend::db::sqlite::projects::delete,
    fn delete(pool: &DbPool, actor: i64, credential: &AccountCredential<'_>, project: i64) -> Result<ProjectContents>);
native_operation!(crate::backend::db::postgres::projects::fields, crate::backend::db::sqlite::projects::fields,
    fn fields(pool: &DbPool, viewer: Option<i64>, project: i64) -> Result<Vec<Field>>);
native_operation!(crate::backend::db::postgres::projects::add_field, crate::backend::db::sqlite::projects::add_field,
    fn add_field(pool: &DbPool, actor: i64, credential: &AccountCredential<'_>, project: i64, input: &NewField) -> Result<Field>);

pub(crate) fn validate_project(title: &str, description: Option<&str>) -> Result<()> {
    anyhow::ensure!(
        valid_name(title) && description.is_none_or(|s| s.len() <= 4096),
        Refusal::Invalid
    );
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct Item {
    pub id: i64,
    pub project_id: i64,
    pub creator_id: i64,
    pub repo_id: Option<i64>,
    pub issue_id: Option<i64>,
    pub pull_request_id: Option<i64>,
    pub draft_title: Option<String>,
    pub draft_body: Option<String>,
    pub field_values: String,
    #[serde(skip)]
    #[sqlx(skip)]
    pub parsed_values: std::sync::OnceLock<std::result::Result<Value, String>>,
    pub archived: bool,
    pub archived_at: Option<DateTime<Utc>>,
    pub position: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub title: String,
    pub state: String,
}
impl Item {
    pub fn content_type(&self) -> &'static str {
        if self.issue_id.is_some() {
            "Issue"
        } else if self.pull_request_id.is_some() {
            "PullRequest"
        } else {
            "DraftIssue"
        }
    }
    pub fn value(&self, field: &Field) -> Result<Value> {
        if field.data_type == "title" {
            return Ok(json!(self.title));
        }
        let values = self.parsed_values.get_or_init(|| {
            serde_json::from_str(&self.field_values).map_err(|error| error.to_string())
        });
        let values = values
            .as_ref()
            .map_err(|error| anyhow::anyhow!(error.clone()))?;
        Ok(values[field.id.to_string()].clone())
    }
}
#[derive(Clone, Debug)]
pub enum Content {
    Issue(i64),
    PullRequest(i64),
    Draft { title: String, body: Option<String> },
}
/// The fields and the readable items a project held when it was deleted.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectContents {
    pub fields: Vec<Field>,
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FieldUpdate {
    pub id: i64,
    pub value: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct View {
    pub id: i64,
    pub project_id: i64,
    pub number: i64,
    pub creator_id: i64,
    pub configuration: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewView {
    pub name: String,
    pub layout: String,
    pub filter: Option<String>,
    #[serde(default)]
    pub visible_fields: Vec<i64>,
    #[serde(default)]
    pub sort_by: Vec<(i64, String)>,
    #[serde(default)]
    pub group_by: Vec<i64>,
    #[serde(default)]
    pub vertical_group_by: Vec<i64>,
}
impl NewView {
    pub(crate) fn validate(&self, fields: &[Field]) -> Result<()> {
        anyhow::ensure!(
            valid_name(&self.name) && matches!(self.layout.as_str(), "table" | "board"),
            Refusal::Invalid
        );
        codeyard_model::project_query::query_terms(self.filter.as_deref().unwrap_or(""))?;
        anyhow::ensure!(
            self.group_by.len() <= 1
                && self.vertical_group_by.len() <= 1
                && self.visible_fields.len() <= fields.len()
                && self.sort_by.len() <= fields.len(),
            Refusal::Invalid
        );
        for id in self
            .visible_fields
            .iter()
            .chain(self.sort_by.iter().map(|(id, _)| id))
        {
            anyhow::ensure!(fields.iter().any(|f| f.id == *id), Refusal::Invalid);
        }
        for (_, direction) in &self.sort_by {
            anyhow::ensure!(
                matches!(direction.as_str(), "asc" | "desc"),
                Refusal::Invalid
            );
        }
        for id in self.group_by.iter().chain(&self.vertical_group_by) {
            anyhow::ensure!(
                fields
                    .iter()
                    .any(|f| f.id == *id && f.data_type == "single_select"),
                Refusal::Invalid
            );
        }
        Ok(())
    }
}

pub(crate) fn validate_value(field: &Field, value: &Value) -> Result<()> {
    anyhow::ensure!(field.data_type != "title", Refusal::Invalid);
    if value.is_null() {
        return Ok(());
    }
    let valid = match field.data_type.as_str() {
        "text" => value.as_str().is_some_and(|s| s.len() <= 4096),
        "number" => value.is_number(),
        "date" => value.as_str().is_some_and(|s| {
            s.len() == 10 && chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
        }),
        "single_select" | "iteration" => {
            let config = field_configuration(field)?;
            config[if field.data_type == "single_select" {
                "options"
            } else {
                "iterations"
            }]
            .as_array()
            .is_some_and(|options| options.iter().any(|o| o["id"] == *value))
        }
        _ => false,
    };
    anyhow::ensure!(valid, Refusal::Invalid);
    Ok(())
}

native_operation!(crate::backend::db::postgres::projects::can_write, crate::backend::db::sqlite::projects::can_write,
    fn can_write(pool: &DbPool, actor: i64, owner: Owner) -> Result<bool>);
native_operation!(crate::backend::db::postgres::projects::delete_field, crate::backend::db::sqlite::projects::delete_field,
    fn delete_field(pool: &DbPool, actor: i64, credential: &AccountCredential<'_>, project: i64, field: i64) -> Result<()>);
native_operation!(crate::backend::db::postgres::projects::add_item, crate::backend::db::sqlite::projects::add_item,
    fn add_item(pool: &DbPool, actor: i64, credential: &AccountCredential<'_>, project: i64, content: &Content) -> Result<Item>);
native_operation!(crate::backend::db::postgres::projects::item, crate::backend::db::sqlite::projects::item,
    fn item(pool: &DbPool, viewer: Option<i64>, project: i64, id: i64) -> Result<Option<Item>>);
native_operation!(crate::backend::db::postgres::projects::items_page, crate::backend::db::sqlite::projects::items_page,
    fn items_page(pool: &DbPool, viewer: Option<i64>, project: i64, after: i64, before: i64, limit: i64) -> Result<Vec<Item>>);
native_operation!(crate::backend::db::postgres::projects::list_window, crate::backend::db::sqlite::projects::list_window,
    fn list_window(pool: &DbPool, viewer: Option<i64>, owner: Owner, after: i64, before: i64, limit: i64) -> Result<Vec<Project>>);
native_operation!(crate::backend::db::postgres::projects::fields_window, crate::backend::db::sqlite::projects::fields_window,
    fn fields_window(pool: &DbPool, viewer: Option<i64>, project: i64, after: i64, before: i64, limit: i64) -> Result<Vec<Field>>);
native_operation!(crate::backend::db::postgres::projects::items, crate::backend::db::sqlite::projects::items,
    fn items(pool: &DbPool, viewer: Option<i64>, project: i64) -> Result<Vec<Item>>);
native_operation!(crate::backend::db::postgres::projects::update_item, crate::backend::db::sqlite::projects::update_item,
    fn update_item(pool: &DbPool, actor: i64, credential: &AccountCredential<'_>, project: i64, item: i64, updates: &[FieldUpdate]) -> Result<Item>);
native_operation!(crate::backend::db::postgres::projects::delete_item, crate::backend::db::sqlite::projects::delete_item,
    fn delete_item(pool: &DbPool, actor: i64, credential: &AccountCredential<'_>, project: i64, item: i64) -> Result<()>);
native_operation!(crate::backend::db::postgres::projects::create_view, crate::backend::db::sqlite::projects::create_view,
    fn create_view(pool: &DbPool, actor: i64, credential: &AccountCredential<'_>, project: i64, input: &NewView) -> Result<View>);
native_operation!(crate::backend::db::postgres::projects::views, crate::backend::db::sqlite::projects::views,
    fn views(pool: &DbPool, viewer: Option<i64>, project: i64) -> Result<Vec<View>>);
native_operation!(crate::backend::db::postgres::projects::link_repository, crate::backend::db::sqlite::projects::link_repository,
    fn link_repository(pool: &DbPool, actor: i64, credential: &AccountCredential<'_>, project: i64, repo: i64, linked: bool) -> Result<()>);

native_operation!(crate::backend::db::postgres::projects::link_team, crate::backend::db::sqlite::projects::link_team,
    fn link_team(pool: &DbPool, actor: i64, credential: &AccountCredential<'_>, project: i64, team: i64, linked: bool) -> Result<()>);
native_operation!(crate::backend::db::postgres::projects::by_id, crate::backend::db::sqlite::projects::by_id,
    fn by_id(pool: &DbPool, viewer: Option<i64>, project: i64) -> Result<Project>);
native_operation!(crate::backend::db::postgres::projects::field_by_id, crate::backend::db::sqlite::projects::field_by_id,
    fn field_by_id(pool: &DbPool, viewer: Option<i64>, field: i64) -> Result<Field>);
native_operation!(crate::backend::db::postgres::projects::item_by_id, crate::backend::db::sqlite::projects::item_by_id,
    fn item_by_id(pool: &DbPool, viewer: Option<i64>, item: i64) -> Result<Item>);
native_operation!(crate::backend::db::postgres::projects::metadata, crate::backend::db::sqlite::projects::metadata,
    fn metadata(pool: &DbPool, actor: i64, credential: &AccountCredential<'_>, project: i64, readme: Option<&str>, template: Option<bool>) -> Result<Project>);
native_operation!(crate::backend::db::postgres::projects::archive_item, crate::backend::db::sqlite::projects::archive_item,
    fn archive_item(pool: &DbPool, actor: i64, credential: &AccountCredential<'_>, project: i64, item: i64, archived: bool) -> Result<Item>);
native_operation!(crate::backend::db::postgres::projects::edit_draft, crate::backend::db::sqlite::projects::edit_draft,
    fn edit_draft(pool: &DbPool, actor: i64, credential: &AccountCredential<'_>, item: i64, title: Option<&str>, body: Option<&str>) -> Result<Item>);
native_operation!(crate::backend::db::postgres::projects::reorder_item, crate::backend::db::sqlite::projects::reorder_item,
    fn reorder_item(pool: &DbPool, actor: i64, credential: &AccountCredential<'_>, project: i64, item: i64, after: Option<i64>) -> Result<Item>);
native_operation!(crate::backend::db::postgres::projects::copy, crate::backend::db::sqlite::projects::copy,
    fn copy(pool: &DbPool, actor: i64, credential: &AccountCredential<'_>, source: i64, owner: Owner, title: &str, drafts: bool) -> Result<Project>);

pub fn field_configuration(field: &Field) -> Result<Value> {
    Ok(serde_json::from_str(&field.configuration)?)
}

use codeyard_model::project_query::Refusal;
