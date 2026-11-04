//! Repository discussions use the issue/PR number allocator and live repository authority.

use crate::backend::db::discussion_search::Expr;
use crate::backend::db::discussion_search::Statement;
use anyhow::Result;
use chrono::{DateTime, Utc};
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

pub(crate) const DEFAULT_CATEGORIES: &[(&str, &str, &str, &str, bool)] = &[
    (
        "Announcements",
        "announcements",
        "📣",
        "Updates from maintainers",
        false,
    ),
    (
        "General",
        "general",
        "💬",
        "Chat about the repository",
        false,
    ),
    (
        "Ideas",
        "ideas",
        "💡",
        "Share ideas for new features",
        false,
    ),
    ("Polls", "polls", "📊", "Ask the community", false),
    ("Q&A", "q-a", "🙏", "Ask questions and find answers", true),
    (
        "Show and tell",
        "show-and-tell",
        "🙌",
        "Show something you made",
        false,
    ),
];

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Discussion {
    pub id: i64,
    pub repo_id: i64,
    pub number: i32,
    pub title: String,
    pub body: String,
    pub author_id: i64,
    pub category_id: i64,
    pub locked: bool,
    pub answer_id: Option<i64>,
    pub answer_chosen_at: Option<DateTime<Utc>>,
    pub answer_chosen_by: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Category {
    pub id: i64,
    pub repo_id: i64,
    pub name: String,
    pub slug: String,
    pub emoji: String,
    pub description: String,
    pub answerable: bool,
    pub created_at: DateTime<Utc>,
}
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Comment {
    pub id: i64,
    pub discussion_id: i64,
    pub author_id: i64,
    pub body: String,
    pub reply_to_id: Option<i64>,
    pub is_answer: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Could not resolve to a discussion")]
    Missing,
    #[error("Discussion write denied")]
    Denied,
    #[error("Invalid discussion input")]
    Invalid,
}

pub enum Write<'a> {
    Enabled(bool),
    Create {
        category: i64,
        title: &'a str,
        body: &'a str,
    },
    Edit {
        title: Option<&'a str>,
        body: Option<&'a str>,
        category: Option<i64>,
    },
    Delete,
    AddComment {
        body: &'a str,
        reply: Option<i64>,
    },
    EditComment {
        comment: i64,
        body: &'a str,
    },
    DeleteComment(i64),
    Answer {
        comment: i64,
        mark: bool,
    },
    Labels {
        labels: &'a [i64],
        add: bool,
    },
}

#[derive(Default)]
pub struct Filter {
    pub repo: Option<i64>,
    pub category: Option<i64>,
    pub answered: Option<bool>,
    pub query: Option<String>,
    pub order: Order,
    pub ascending: bool,
    pub closed_only: bool,
}
#[derive(Default)]
pub enum Order {
    #[default]
    Created,
    Updated,
    Comments,
}

impl Filter {
    pub fn search(query: &str) -> Result<Self> {
        let mut filter = Self {
            order: Order::Updated,
            ..Default::default()
        };
        let mut words = Vec::new();
        for word in
            crate::backend::db::discussion_search::tokens(query).map_err(anyhow::Error::msg)?
        {
            if let Some(sort) = word.strip_prefix("sort:") {
                let (field, direction) = sort.rsplit_once('-').ok_or(Refusal::Invalid)?;
                filter.order = match field {
                    "created" => Order::Created,
                    "updated" => Order::Updated,
                    _ => anyhow::bail!(Refusal::Invalid),
                };
                filter.ascending = match direction {
                    "asc" => true,
                    "desc" => false,
                    _ => anyhow::bail!(Refusal::Invalid),
                };
            } else {
                words.push(word);
            }
        }
        filter.query = (!words.is_empty()).then(|| words.join(" "));
        Ok(filter)
    }
    pub(crate) fn statement(
        &self,
        postgres: bool,
        readable: &str,
        reader: &crate::backend::db::repositories::credentials::Reader<'_>,
    ) -> Result<Statement> {
        let mut sql = Statement::new(postgres);
        let token = sql.bind(reader.token().unwrap_or(0).to_string());
        let session = sql.bind(reader.session().unwrap_or(""));
        let hash = sql.bind(reader.hash().unwrap_or(""));
        let scope = crate::backend::db::repositories::credentials::repository_condition(
            postgres,
            "discussions",
            crate::backend::db::repositories::credentials::Access::PublicRead,
            &format!("NULLIF({token},'0')"),
            &format!("NULLIF({session},'')"),
            &format!("NULLIF({hash},'')"),
        );
        let mut predicates = vec![readable.to_owned(), scope];
        if let Some(repo) = self.repo {
            let value = sql.bind(repo.to_string());
            predicates.push(format!("i.repo_id=CAST({value} AS BIGINT)"));
        }
        if let Some(category) = self.category {
            let value = sql.bind(category.to_string());
            predicates.push(format!("i.category_id=CAST({value} AS BIGINT)"));
        }
        if let Some(answered) = self.answered {
            predicates.push(format!(
                "i.answer_id IS {}NULL",
                if answered { "NOT " } else { "" }
            ));
        }
        if self.closed_only {
            predicates.push("false".into());
        }
        if let Some(query) = &self.query {
            predicates.push(search_expression(
                &mut sql,
                &crate::backend::db::discussion_search::expression(query)
                    .map_err(anyhow::Error::msg)?,
            )?);
        }
        sql.prefix = format!("FROM discussions i JOIN repositories r ON r.id=i.repo_id LEFT JOIN users u ON u.id={} LEFT JOIN users owners ON r.owner_kind='user' AND owners.id=r.owner_user_id LEFT JOIN organizations orgs ON r.owner_kind='org' AND orgs.id=r.owner_org_id WHERE {}", sql.placeholder(1), predicates.into_iter().map(|p| format!("({p})")).collect::<Vec<_>>().join(" AND "));
        Ok(sql)
    }
    pub(crate) fn order(&self) -> String {
        format!(
            "{} {},i.id {}",
            match self.order {
                Order::Created => "i.created_at",
                Order::Updated => "i.updated_at",
                Order::Comments =>
                    "(SELECT COUNT(*) FROM discussion_comments c WHERE c.discussion_id=i.id)",
            },
            if self.ascending { "ASC" } else { "DESC" },
            if self.ascending { "ASC" } else { "DESC" }
        )
    }
}
fn search_expression(sql: &mut Statement, expr: &Expr) -> Result<String> {
    Ok(match expr {
        Expr::And(items) | Expr::Or(items) => items
            .iter()
            .map(|e| search_expression(sql, e).map(|s| format!("({s})")))
            .collect::<Result<Vec<_>>>()?
            .join(if matches!(expr, Expr::And(_)) {
                " AND "
            } else {
                " OR "
            }),
        Expr::Not(e) => format!("NOT ({})", search_expression(sql, e)?),
        Expr::Term(key, value) => match (key.as_str(), value.as_str()) {
            ("text", _) => format!(
                "({} OR {} OR EXISTS(SELECT 1 FROM discussion_comments c WHERE c.discussion_id=i.id AND {}))",
                sql.matches("i.title", value),
                sql.matches("i.body", value),
                sql.matches("c.body", value)
            ),
            ("repo", _) => {
                let p = sql.bind(value);
                format!("lower(COALESCE(owners.username,orgs.name)||'/'||r.name)=lower({p})")
            }
            ("author", "@me") => "i.author_id=u.id".into(),
            ("author", _) => {
                let p = sql.bind(value);
                format!("i.author_id IN (SELECT id FROM users WHERE lower(username)=lower({p}))")
            }
            ("label", _) => {
                let predicate = sql.equals("l.name", value);
                format!("EXISTS(SELECT 1 FROM discussion_labels a JOIN labels l ON l.id=a.label_id WHERE a.discussion_id=i.id AND {predicate})")
            }
            ("category", _) => {
                let p = sql.bind(value);
                format!("EXISTS(SELECT 1 FROM discussion_categories c WHERE c.id=i.category_id AND (lower(c.name)=lower({p}) OR lower(c.slug)=lower({p})))")
            }
            ("is" | "state", "open") | ("type" | "is", "discussion") => "true".into(),
            ("is" | "state", "closed") => "false".into(),
            ("is", "answered") => "i.answer_id IS NOT NULL".into(),
            ("is", "unanswered") => "i.answer_id IS NULL".into(),
            _ => anyhow::bail!("Unsupported discussion search qualifier"),
        },
    })
}

native_operation!(crate::backend::db::postgres::repository_discussions::enabled, crate::backend::db::sqlite::repository_discussions::enabled,
    fn enabled(pool: &DbPool, repo: i64) -> Result<bool>);
native_operation!(crate::backend::db::postgres::repository_discussions::categories, crate::backend::db::sqlite::repository_discussions::categories,
    fn categories(pool: &DbPool, repo: i64) -> Result<Vec<Category>>);
native_operation!(crate::backend::db::postgres::repository_discussions::category_by_id, crate::backend::db::sqlite::repository_discussions::category_by_id,
    fn category_by_id(pool: &DbPool, id: i64) -> Result<Option<Category>>);
native_operation!(crate::backend::db::postgres::repository_discussions::get, crate::backend::db::sqlite::repository_discussions::get,
    fn get(pool: &DbPool, id: i64) -> Result<Option<Discussion>>);
native_operation!(crate::backend::db::postgres::repository_discussions::by_number, crate::backend::db::sqlite::repository_discussions::by_number,
    fn by_number(pool: &DbPool, repo: i64, number: i32) -> Result<Option<Discussion>>);
native_operation!(crate::backend::db::postgres::repository_discussions::comment, crate::backend::db::sqlite::repository_discussions::comment,
    fn comment(pool: &DbPool, id: i64) -> Result<Option<Comment>>);
native_operation!(crate::backend::db::postgres::repository_discussions::comments, crate::backend::db::sqlite::repository_discussions::comments,
    fn comments(pool: &DbPool, discussion: i64, reply: Option<i64>, offset: i64, limit: i64) -> Result<(i64, Vec<Comment>)>);
native_operation!(crate::backend::db::postgres::repository_discussions::labels, crate::backend::db::sqlite::repository_discussions::labels,
    fn labels(pool: &DbPool, discussion: i64) -> Result<Vec<codeyard_db_core::models::Label>>);
native_operation!(crate::backend::db::postgres::repository_discussions::list, crate::backend::db::sqlite::repository_discussions::list,
    fn list_counted(pool: &DbPool, viewer: i64, credential: &crate::backend::db::users::accounts::AccountCredential<'_>, filter: &Filter, offset: i64, limit: i64, count: bool) -> Result<(i64, Vec<Discussion>)>);
pub async fn list(
    pool: &DbPool,
    viewer: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    filter: &Filter,
    offset: i64,
    limit: i64,
) -> Result<(i64, Vec<Discussion>)> {
    list_counted(pool, viewer, credential, filter, offset, limit, true).await
}
pub async fn list_rows(
    pool: &DbPool,
    viewer: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    filter: &Filter,
    offset: i64,
    limit: i64,
) -> Result<Vec<Discussion>> {
    Ok(
        list_counted(pool, viewer, credential, filter, offset, limit, false)
            .await?
            .1,
    )
}
native_operation!(crate::backend::db::postgres::repository_discussions::write, crate::backend::db::sqlite::repository_discussions::write,
    fn write(pool: &DbPool, actor: i64, credential: &crate::backend::db::users::accounts::AccountCredential<'_>, repo: i64, discussion: Option<i64>, change: &Write<'_>) -> Result<i64>);

pub(crate) fn valid_body(body: &str) -> Result<()> {
    anyhow::ensure!(
        body.len() <= 65536 && !body.contains('\0'),
        Refusal::Invalid
    );
    Ok(())
}
