//! Bounded lexical catalog searches. Repository visibility is part of every SQL predicate,
//! count and ordering operation, never a filter on an already paginated collection.
use super::{
    discussion_search::{self, Expr, Statement},
    DbPool,
};
use chrono::{DateTime, Utc};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Repositories,
    Labels,
    Users,
    Topics,
}
#[derive(Clone, Debug)]
pub struct Search {
    kind: Kind,
    expr: Expr,
    fields: Vec<String>,
    git: Option<String>,
}
#[derive(Debug, sqlx::FromRow)]
pub struct Hit {
    pub key: String,
    pub repository_id: Option<i64>,
    pub score: i64,
}
pub struct Results {
    pub total: i64,
    pub items: Vec<Hit>,
}
pub struct Options {
    pub repository: Option<i64>,
    pub sort: Option<String>,
    pub ascending: bool,
    pub offset: i64,
    pub limit: i64,
}
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Topic {
    pub name: String,
    pub display_name: Option<String>,
    pub short_description: Option<String>,
    pub description: Option<String>,
    pub created_by: Option<String>,
    pub released: Option<String>,
    pub featured: bool,
    pub curated: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub repository_count: i64,
}
impl Search {
    pub fn parse(kind: Kind, text: &str) -> Result<Self, &'static str> {
        let mut expr = discussion_search::expression(text)?;
        if kind == Kind::Repositories
            && !has(&expr, "fork", None)
            && !has(&expr, "is", Some("fork"))
            && !has(&expr, "repo", None)
        {
            expr = Expr::And(vec![expr, Expr::Term("fork".into(), "false".into())]);
        }
        Self::from_expression(kind, expr)
    }
    pub fn needs_git(&self) -> bool {
        self.kind == Kind::Repositories
            && (self.fields.iter().any(|f| f == "readme") || has(&self.expr, "license", None))
    }
    pub fn with_git(mut self, facts: String) -> Self {
        self.git = Some(facts);
        self
    }
    pub fn git_candidates(&self) -> Self {
        fn broader(expr: &Expr, readme: bool, negative: bool) -> Expr {
            match expr {
                Expr::Term(k, _) if k == "license" || (readme && k == "text") => {
                    if negative {
                        Expr::Or(Vec::new())
                    } else {
                        Expr::And(Vec::new())
                    }
                }
                Expr::And(v) => Expr::And(v.iter().map(|e| broader(e, readme, negative)).collect()),
                Expr::Or(v) => Expr::Or(v.iter().map(|e| broader(e, readme, negative)).collect()),
                Expr::Not(v) => Expr::Not(Box::new(broader(v, readme, !negative))),
                _ => expr.clone(),
            }
        }
        Self {
            kind: self.kind,
            expr: broader(&self.expr, self.fields.iter().any(|f| f == "readme"), false),
            fields: vec!["name".into(), "description".into()],
            git: None,
        }
    }
    pub fn needs_viewer(&self) -> bool {
        has(&self.expr, "user", Some("@me"))
    }
    pub(crate) fn from_expression(kind: Kind, mut expr: Expr) -> Result<Self, &'static str> {
        let mut fields = Vec::new();
        extract_fields(&mut expr, &mut fields)?;
        if fields.is_empty() {
            fields = match kind {
                Kind::Repositories | Kind::Labels => vec!["name".into(), "description".into()],
                Kind::Users => vec!["login".into(), "fullname".into()],
                Kind::Topics => vec!["name".into(), "display_name".into(), "description".into()],
            };
        }
        let search = Self {
            kind,
            expr,
            fields,
            git: None,
        };
        for field in &search.fields {
            search.field(field)?;
        }
        search.statement(false, "TRUE", None)?;
        Ok(search)
    }
    pub(crate) fn statement(
        &self,
        postgres: bool,
        readable: &str,
        repository: Option<i64>,
    ) -> Result<Statement, &'static str> {
        let mut sql = Statement::new(postgres);
        let predicate = self.expr(&mut sql, &self.expr)?;
        let mut words = Vec::new();
        discussion_search::positive_text(&self.expr, false, &mut words);
        let mut scores = Vec::new();
        for word in words {
            scores.push(format!(
                "CASE WHEN ({}) THEN 1 ELSE 0 END",
                self.text(&mut sql, word)?
            ));
        }
        let score = if scores.is_empty() {
            "0".into()
        } else {
            scores.join("+")
        };
        let viewer = sql.placeholder(1);
        let visible=format!("visible AS (SELECT r.*,CASE WHEN r.owner_kind='org' THEN orgs.name ELSE owners.username END AS owner_name FROM repositories r LEFT JOIN users u ON u.id={viewer} LEFT JOIN users owners ON r.owner_kind='user' AND owners.id=r.owner_user_id LEFT JOIN organizations orgs ON r.owner_kind='org' AND orgs.id=r.owner_org_id WHERE {readable})");
        let (source,key,repo,extra)=match self.kind{
            Kind::Repositories=>("visible i".to_owned(),"CAST(i.id AS TEXT)","i.id",String::new()),
            Kind::Labels=>{let filter=if let Some(id)=repository{if id<=0{return Err("Invalid repository_id");}let p=sql.bind(id.to_string());format!(" AND i.repo_id=CAST({p} AS BIGINT)")}else{String::new()};("labels i JOIN visible r ON r.id=i.repo_id".into(),"CAST(i.id AS TEXT)","i.repo_id",filter)},
            Kind::Users=>("people i".into(),"i.key",if postgres{"NULL::BIGINT"}else{"CAST(NULL AS BIGINT)"},String::new()),
            Kind::Topics=>("topics i JOIN (SELECT rt.topic,COUNT(*) AS repository_count FROM repository_topics rt JOIN visible r ON r.id=rt.repo_id GROUP BY rt.topic) usage ON usage.topic=i.name".into(),"i.name",if postgres{"NULL::BIGINT"}else{"CAST(NULL AS BIGINT)"},String::new()),
        };
        let people = if self.kind == Kind::Users {
            format!(
                r#", people AS (
            SELECT 'u:'||CAST(p.id AS TEXT) AS key,p.username AS login,p.display_name AS fullname,p.bio AS description,p.location,p.created_at,p.updated_at,'user' AS kind,
            CASE WHEN EXISTS(SELECT 1 FROM user_emails e WHERE e.user_id=p.id AND lower(e.email)=lower(s.public_email) AND e.verified) THEN s.public_email WHEN s.primary_visibility='public' AND p.email_verified AND (NULLIF(s.public_email,'') IS NULL OR lower(s.public_email)=lower(p.email)) THEN p.email ELSE NULL END AS email,
            (SELECT COUNT(*) FROM follows f WHERE f.following_id=p.id) AS followers,
            (SELECT COUNT(*) FROM repositories x WHERE x.owner_kind='user' AND x.owner_user_id=p.id AND NOT x.is_private AND NOT x.is_gist) AS repositories
            FROM users p LEFT JOIN user_profile_settings s ON s.user_id=p.id WHERE NOT p.is_banned AND NOT p.must_change_password AND p.username<>'{bot}'
            UNION ALL SELECT 'o:'||CAST(o.id AS TEXT),o.name,o.display_name,o.description,NULL,o.created_at,o.updated_at,'org',NULL,0,
            (SELECT COUNT(*) FROM repositories x WHERE x.owner_kind='org' AND x.owner_org_id=o.id AND NOT x.is_private AND NOT x.is_gist)
            FROM organizations o WHERE EXISTS(SELECT 1 FROM org_memberships m JOIN users own ON own.id=m.user_id WHERE m.org_id=o.id AND m.role='owner' AND NOT own.is_banned AND NOT own.must_change_password))"#,
                bot = super::ci_tokens::BOT
            )
        } else {
            String::new()
        };
        let git = sql.bind(self.git.as_deref().unwrap_or("[]"));
        let git = if postgres {
            format!("search_git AS (SELECT * FROM jsonb_to_recordset(CAST({git} AS JSONB)) AS g(repo_id BIGINT,readme TEXT,license TEXT))")
        } else {
            format!("search_git AS (SELECT CAST(json_extract(value,'$.repo_id') AS BIGINT) AS repo_id,json_extract(value,'$.readme') AS readme,json_extract(value,'$.license') AS license FROM json_each({git}))")
        };
        let restricted = if self.git.is_some() {
            " AND i.id IN (SELECT repo_id FROM search_git)"
        } else {
            ""
        };
        sql.prefix=format!("WITH {git}, {visible}{people}, matched AS (SELECT {key} AS key,{repo} AS repository_id,CAST(({score}) AS BIGINT) AS score FROM {source} WHERE ({predicate}){extra}{restricted})");
        Ok(sql)
    }
    fn field(&self, field: &str) -> Result<&str, &'static str> {
        Ok(match (self.kind, field) {
            (Kind::Repositories | Kind::Labels | Kind::Topics, "name") => "i.name",
            (Kind::Repositories | Kind::Labels | Kind::Topics, "description") => "i.description",
            (Kind::Repositories, "readme") => {
                "(SELECT g.readme FROM search_git g WHERE g.repo_id=i.id)"
            }
            (Kind::Users, "login") => "i.login",
            (Kind::Users, "name" | "fullname") => "i.fullname",
            (Kind::Users, "email") => "i.email",
            (Kind::Topics, "display_name") => "i.display_name",
            (Kind::Topics, "short_description") => "i.short_description",
            _ => return Err("Invalid search field"),
        })
    }
    fn text(&self, sql: &mut Statement, value: &str) -> Result<String, &'static str> {
        self.fields
            .iter()
            .map(|field| Ok(sql.matches(self.field(field)?, value)))
            .collect::<Result<Vec<_>, _>>()
            .map(|parts| parts.join(" OR "))
    }
    fn expr(&self, sql: &mut Statement, expr: &Expr) -> Result<String, &'static str> {
        Ok(match expr {
            Expr::And(values) | Expr::Or(values) => {
                let parts = values
                    .iter()
                    .map(|e| self.expr(sql, e).map(|s| format!("({s})")))
                    .collect::<Result<Vec<_>, _>>()?;
                if parts.is_empty() {
                    if matches!(expr, Expr::And(_)) {
                        "TRUE".into()
                    } else {
                        "FALSE".into()
                    }
                } else {
                    parts.join(if matches!(expr, Expr::And(_)) {
                        " AND "
                    } else {
                        " OR "
                    })
                }
            }
            Expr::Not(expr) => format!("NOT COALESCE(({}),FALSE)", self.expr(sql, expr)?),
            Expr::Term(key, value) => {
                let r = if self.kind == Kind::Repositories {
                    "i"
                } else {
                    "r"
                };
                let repo = matches!(self.kind, Kind::Repositories | Kind::Labels);
                match key.as_str() {
                    "text" => self.text(sql, value)?,
                    "repo" if repo => {
                        if !value.contains('/') {
                            return Err("Repository search must name owner/repository");
                        }
                        let p = sql.bind(value);
                        format!("lower({r}.owner_name||'/'||{r}.name)=lower({p})")
                    }
                    "user" | "org" if repo => {
                        if value == "@me" {
                            if key == "org" {
                                return Err("An organization name is required");
                            }
                            format!(
                                "{r}.owner_kind='user' AND {r}.owner_user_id={}",
                                sql.placeholder(1)
                            )
                        } else {
                            let p = sql.bind(value);
                            format!(
                                "{}lower({r}.owner_name)=lower({p})",
                                if key == "org" { format!("{r}.owner_kind='org' AND ") } else { String::new() }
                            )
                        }
                    }
                    "is" if repo => match value.as_str() {
                        "public" => format!("NOT {r}.is_private"),
                        "private" => format!("{r}.is_private"),
                        "internal" => "FALSE".into(),
                        "archived" => format!("{r}.is_archived"),
                        "fork" => format!("{r}.is_fork"),
                        _ => return Err("Invalid repository is qualifier"),
                    },
                    "archived" if repo => boolean(&format!("{r}.is_archived"), value)?,
                    "fork" if repo => match value.as_str() {
                        "true" => "TRUE".into(),
                        "false" => format!("NOT {r}.is_fork"),
                        "only" => format!("{r}.is_fork"),
                        _ => return Err("Invalid fork qualifier"),
                    },
                    "topic" if repo => {
                        let p = sql.bind(value);
                        format!("EXISTS(SELECT 1 FROM repository_topics t WHERE t.repo_id={r}.id AND lower(t.topic)=lower({p}))")
                    }
                    "language" if repo => {
                        let p = sql.bind(value);
                        format!("EXISTS(SELECT 1 FROM repo_languages l WHERE l.repo_id={r}.id AND lower(l.language)=lower({p}))")
                    }
                    "license" if repo => { let p=sql.bind(value); format!("EXISTS(SELECT 1 FROM search_git g WHERE g.repo_id={r}.id AND lower(g.license)=lower({p}))") },
                    "pushed" if repo => sql.range(&format!("COALESCE((SELECT MAX(p.created_at) FROM repository_pushes p WHERE p.repo_id={r}.id),{r}.created_at)"),value,true)?,
                    "topics" if repo => sql.range(&format!("(SELECT COUNT(*) FROM repository_topics t WHERE t.repo_id={r}.id)"),value,false)?,
                    "good-first-issues" | "help-wanted-issues" if repo => sql.range(&label_count(r,if key=="good-first-issues" {"good first issue"} else {"help wanted"}),value,false)?,
                    "stars" | "forks" | "size" | "followers" if repo => sql.range(
                        &match key.as_str() {
                            "stars" => format!("{r}.stars_count"),
                            "forks" => format!("{r}.forks_count"),
                            "followers" => format!("(SELECT COUNT(*) FROM watches w WHERE w.repo_id={r}.id AND w.level IN ('watching','custom'))"),
                            _ => format!("({r}.size_bytes/1024)"),
                        },
                        value,
                        false,
                    )?,
                    "created" | "updated" => sql.range(
                        &if self.kind == Kind::Labels && key == "updated" {
                            "COALESCE(i.updated_at,i.created_at)".into()
                        } else {
                            format!("i.{key}_at")
                        },
                        value,
                        true,
                    )?,
                    "type" if self.kind == Kind::Users => {
                        let kind = match value.as_str() {
                            "user" => "user",
                            "org" | "organization" => "org",
                            _ => return Err("Invalid account type"),
                        };
                        format!("i.kind='{kind}'")
                    }
                    "location" if self.kind == Kind::Users => sql.matches("i.location", value),
                    "followers" | "repos" if self.kind == Kind::Users => sql.range(
                        if key == "followers" {
                            "i.followers"
                        } else {
                            "i.repositories"
                        },
                        value,
                        false,
                    )?,
                    "repositories" if self.kind == Kind::Topics => {
                        sql.range("usage.repository_count", value, false)?
                    }
                    "is" if self.kind == Kind::Topics => match value.as_str() {
                        "featured" => "i.featured".into(),
                        "curated" => "i.curated".into(),
                        _ => return Err("Invalid topic is qualifier"),
                    },
                    _ => return Err("Unsupported search qualifier"),
                }
            }
        })
    }
    pub(crate) fn order(
        &self,
        sort: Option<&str>,
        ascending: bool,
    ) -> Result<String, &'static str> {
        let sort = sort.unwrap_or("");
        let direction = if ascending { "ASC" } else { "DESC" };
        let field=match(self.kind,sort){(_,"")=>return Ok(format!("m.score {direction},m.key ASC")),(Kind::Repositories,"stars")=>"i.stars_count",(Kind::Repositories,"forks")=>"i.forks_count",(Kind::Repositories,"updated")=>"i.updated_at",(Kind::Repositories,"help-wanted-issues")=>"(SELECT COUNT(*) FROM issues x WHERE x.repo_id=i.id AND x.state='open' AND EXISTS(SELECT 1 FROM issue_labels il JOIN labels l ON l.id=il.label_id WHERE il.issue_id=x.id AND lower(l.name)='help wanted'))",(Kind::Labels,"created")=>"i.created_at",(Kind::Labels,"updated")=>"COALESCE(i.updated_at,i.created_at)",(Kind::Users,"followers")=>"i.followers",(Kind::Users,"repositories")=>"i.repositories",(Kind::Users,"joined")=>"i.created_at",_=>return Err("Invalid search sort")};
        let source = match self.kind {
            Kind::Repositories => "visible i",
            Kind::Labels => "labels i",
            Kind::Users => "people i",
            Kind::Topics => return Err("Invalid search sort"),
        };
        let key = if self.kind == Kind::Users {
            "i.key"
        } else {
            "CAST(i.id AS TEXT)"
        };
        Ok(format!(
            "(SELECT {field} FROM {source} WHERE {key}=m.key) {direction},m.key ASC"
        ))
    }
}
pub(crate) fn has(expr: &Expr, key: &str, value: Option<&str>) -> bool {
    match expr {
        Expr::Term(k, v) => k == key && value.is_none_or(|value| v == value),
        Expr::And(v) | Expr::Or(v) => v.iter().any(|e| has(e, key, value)),
        Expr::Not(v) => has(v, key, value),
    }
}
fn boolean(column: &str, value: &str) -> Result<String, &'static str> {
    match value {
        "true" => Ok(column.into()),
        "false" => Ok(format!("NOT {column}")),
        _ => Err("Invalid boolean qualifier"),
    }
}
fn label_count(repo: &str, label: &str) -> String {
    format!("(SELECT COUNT(*) FROM issues x WHERE x.repo_id={repo}.id AND x.state='open' AND EXISTS(SELECT 1 FROM issue_labels il JOIN labels l ON l.id=il.label_id WHERE il.issue_id=x.id AND lower(l.name)='{label}'))")
}
fn contains_in(expr: &Expr) -> bool {
    match expr {
        Expr::Term(k, _) => k == "in",
        Expr::And(v) | Expr::Or(v) => v.iter().any(contains_in),
        Expr::Not(v) => contains_in(v),
    }
}
pub(crate) fn extract_fields(
    expr: &mut Expr,
    fields: &mut Vec<String>,
) -> Result<(), &'static str> {
    match expr {
        Expr::Term(k, v) if k == "in" => {
            fields.extend(v.split(',').map(str::to_owned));
            *expr = Expr::And(Vec::new());
        }
        Expr::And(v) => {
            for expr in v {
                extract_fields(expr, fields)?;
            }
        }
        Expr::Or(v) if v.iter().all(|e| matches!(e, Expr::Term(k, _) if k=="in")) => {
            for expr in v.iter_mut() {
                extract_fields(expr, fields)?;
            }
            *expr = Expr::And(Vec::new());
        }
        Expr::Or(_) | Expr::Not(_) if contains_in(expr) => {
            return Err("in must be a positive top-level qualifier")
        }
        _ => {}
    }
    Ok(())
}

native_operation!(crate::backend::db::postgres::catalog_search::search,crate::backend::db::sqlite::catalog_search::search,
    fn search(pool:&DbPool,viewer:Option<i64>,query:&Search,options:&Options)->anyhow::Result<Results>);
native_operation!(crate::backend::db::postgres::catalog_search::topic,crate::backend::db::sqlite::catalog_search::topic,
    fn topic(pool:&DbPool,viewer:Option<i64>,name:&str)->anyhow::Result<Option<Topic>>);
