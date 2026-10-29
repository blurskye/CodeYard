//! A bounded search expression shared by native database implementations. Values are always bound;
//! only the closed vocabulary below can become SQL. Search sees published discussion content only.
use super::DbPool;
use chrono::{DateTime, NaiveDate, Utc};

#[derive(Debug, Clone)]
pub(crate) enum Expr {
    Term(String, String),
    And(Vec<Self>),
    Or(Vec<Self>),
    Not(Box<Self>),
}
#[derive(Debug, Clone)]
pub struct Search {
    expr: Expr,
    fields: Vec<&'static str>,
    policy_token: Option<i64>,
}
#[derive(Default)]
pub struct Collection {
    pub relation: Relation,
    pub state: Option<String>,
    pub labels: Vec<String>,
    pub since: Option<DateTime<Utc>>,
    pub organization: Option<String>,
    /// Owned, directly collaborating, and organization-member repositories, respectively.
    pub scopes: Option<[bool; 3]>,
    pub pulls: bool,
}
#[derive(Default)]
pub enum Relation {
    #[default]
    All,
    Assigned,
    Created,
    Mentioned,
    Subscribed,
}

#[derive(Clone, Copy)]
pub enum Sort {
    Best,
    Created,
    Updated,
    Comments,
    Reactions,
    Reaction(Reaction),
    Interactions,
}
#[derive(Clone, Copy)]
pub enum Reaction {
    Up,
    Down,
    Laugh,
    Confused,
    Heart,
    Hooray,
}
impl Reaction {
    fn content(self) -> &'static str {
        match self {
            Self::Up => "+1",
            Self::Down => "-1",
            Self::Laugh => "laugh",
            Self::Confused => "confused",
            Self::Heart => "heart",
            Self::Hooray => "hooray",
        }
    }
}
impl Sort {
    pub fn parse(value: Option<&str>) -> Result<Self, &'static str> {
        Ok(match value.unwrap_or("") {
            "" => Self::Best,
            "created" => Self::Created,
            "updated" => Self::Updated,
            "comments" => Self::Comments,
            "reactions" => Self::Reactions,
            "interactions" => Self::Interactions,
            "reactions-+1" => Self::Reaction(Reaction::Up),
            "reactions--1" => Self::Reaction(Reaction::Down),
            "reactions-smile" => Self::Reaction(Reaction::Laugh),
            "reactions-thinking_face" => Self::Reaction(Reaction::Confused),
            "reactions-heart" => Self::Reaction(Reaction::Heart),
            "reactions-tada" => Self::Reaction(Reaction::Hooray),
            _ => return Err("Invalid search sort"),
        })
    }
    pub(crate) fn sql(self) -> String {
        match self {
            Self::Best=>"i.score".into(),Self::Created=>"i.created_at".into(),Self::Updated=>"i.updated_at".into(),Self::Comments=>"i.comments".into(),Self::Reactions=>"i.reactions".into(),Self::Interactions=>"(i.comments+i.reactions)".into(),
            Self::Reaction(content)=>format!("(SELECT COUNT(*) FROM reactions x WHERE x.subject_id=i.id AND x.subject_kind=CASE WHEN i.pull_request THEN 'pull_request' ELSE 'issue' END AND x.content='{}')",content.content()),
        }
    }
}

pub(crate) fn tokens(query: &str) -> Result<Vec<String>, &'static str> {
    if query.trim().is_empty() || query.len() > 4096 || query.contains('\0') {
        return Err("Search query must contain 1–4096 bytes of text");
    }
    let mut result = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for ch in query.chars() {
        if escaped {
            word.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quoted {
            escaped = true;
            word.push(ch);
            continue;
        }
        if ch == '"' {
            quoted = !quoted;
            word.push(ch);
            continue;
        }
        if !quoted && (ch.is_whitespace() || matches!(ch, '(' | ')')) {
            if !word.is_empty() {
                result.push(std::mem::take(&mut word));
            }
            if !ch.is_whitespace() {
                result.push(ch.to_string());
            }
        } else {
            word.push(ch);
        }
    }
    if quoted || escaped {
        return Err("Unterminated quoted search text");
    }
    if !word.is_empty() {
        result.push(word);
    }
    if result.len() > 64 {
        return Err("Search query is too complex");
    }
    Ok(result)
}
fn term(word: &str) -> Result<(String, String), &'static str> {
    let (key, value) = if word.starts_with('"') {
        ("text", word)
    } else {
        word.split_once(':').unwrap_or(("text", word))
    };
    let value = if value.starts_with('"') && value.ends_with('"') {
        &value[1..value.len() - 1]
    } else {
        value
    };
    let value = value.replace("\\\"", "\"").replace("\\\\", "\\");
    if value.is_empty() {
        return Err("Search qualifier cannot be empty");
    }
    Ok((key.to_ascii_lowercase(), value))
}
struct Parser {
    tokens: Vec<String>,
    at: usize,
}
impl Parser {
    fn or(&mut self, depth: usize) -> Result<Expr, &'static str> {
        if depth > 8 {
            return Err("Search query is too deeply nested");
        }
        let mut values = vec![self.and(depth)?];
        while self.tokens.get(self.at).is_some_and(|word| word == "OR") {
            self.at += 1;
            values.push(self.and(depth)?);
        }
        Ok(if values.len() == 1 {
            values.remove(0)
        } else {
            Expr::Or(values)
        })
    }
    fn and(&mut self, depth: usize) -> Result<Expr, &'static str> {
        let mut values = Vec::new();
        while let Some(word) = self.tokens.get(self.at) {
            if matches!(word.as_str(), "OR" | ")") {
                break;
            }
            if word == "AND" {
                if values.is_empty() {
                    return Err("Missing search operand");
                }
                self.at += 1;
            }
            values.push(self.factor(depth)?);
        }
        if values.is_empty() {
            return Err("Missing search operand");
        }
        // Multiple repositories or owners in one conjunction select their union. Labels remain AND.
        for field in [
            "repo",
            "user",
            "org",
            "author",
            "assignee",
            "mentions",
            "commenter",
            "involves",
        ] {
            let mut alternatives = Vec::new();
            values.retain(|expr| {
                if matches!(expr,Expr::Term(key,_) if key==field) {
                    alternatives.push(expr.clone());
                    false
                } else {
                    true
                }
            });
            if alternatives.len() == 1 {
                values.push(alternatives.remove(0));
            } else if !alternatives.is_empty() {
                values.push(Expr::Or(alternatives));
            }
        }
        for group in [
            &["public", "private", "internal"][..],
            &["open", "closed"][..],
            &["locked", "unlocked"][..],
            &["merged", "unmerged"][..],
        ] {
            let mut alternatives = Vec::new();
            values.retain(|expr| {
                if matches!(expr,Expr::Term(key,value) if key=="is" && group.contains(&value.as_str())) {
                    alternatives.push(expr.clone()); false
                } else { true }
            });
            if !alternatives.is_empty() {
                values.push(Expr::Or(alternatives));
            }
        }
        Ok(if values.len() == 1 {
            values.remove(0)
        } else {
            Expr::And(values)
        })
    }
    fn factor(&mut self, depth: usize) -> Result<Expr, &'static str> {
        if depth > 8 {
            return Err("Search query is too deeply nested");
        }
        let word = self
            .tokens
            .get(self.at)
            .ok_or("Missing search operand")?
            .clone();
        self.at += 1;
        match word.as_str() {
            "NOT" => Ok(Expr::Not(Box::new(self.factor(depth + 1)?))),
            "(" => {
                let expr = self.or(depth + 1)?;
                if self.tokens.get(self.at).is_none_or(|word| word != ")") {
                    return Err("Unbalanced search parentheses");
                }
                self.at += 1;
                Ok(expr)
            }
            ")" | "AND" | "OR" => Err("Missing search operand"),
            _ => {
                let (negative, word) = word
                    .strip_prefix('-')
                    .map_or((false, word.as_str()), |word| (true, word));
                let (key, value) = term(word)?;
                let expr = Expr::Term(key, value);
                Ok(if negative {
                    Expr::Not(Box::new(expr))
                } else {
                    expr
                })
            }
        }
    }
}

pub(crate) fn expression(query: &str) -> Result<Expr, &'static str> {
    parse_words(tokens(query)?)
}
fn parse_words(words: Vec<String>) -> Result<Expr, &'static str> {
    let mut parser = Parser {
        tokens: words,
        at: 0,
    };
    let expr = parser.or(0)?;
    if parser.at != parser.tokens.len() {
        return Err("Unbalanced search expression");
    }
    Ok(expr)
}

impl Search {
    /// Apply classic-token organization policy before counting/paging, in the search snapshot.
    /// Other credential bounds still require the caller's resource-specific grant checks.
    pub fn with_classic_policy(mut self, token: Option<i64>) -> Self {
        self.policy_token = token;
        self
    }

    /// GraphQL carries the search order in the query text rather than REST's separate parameters.
    pub fn parse_ordered(query: &str) -> Result<(Self, Sort, bool), &'static str> {
        let mut words = tokens(query)?;
        let mut sort = Sort::Best;
        let mut ascending = false;
        let mut error = None;
        words.retain(|word| {
            if let Ok((key, value)) = term(word) {
                if key == "sort" {
                    let (name, direction) = value.rsplit_once('-').unwrap_or((&value, "desc"));
                    match Sort::parse(Some(name)) {
                        Ok(value) => sort = value,
                        Err(value) => error = Some(value),
                    }
                    ascending = direction == "asc";
                    if !matches!(direction, "asc" | "desc") {
                        error = Some("Invalid search order");
                    }
                    return false;
                }
            }
            true
        });
        if let Some(error) = error {
            return Err(error);
        }
        Ok((Self::parse(&words.join(" "))?, sort, ascending))
    }

    pub fn collection(filter: Collection) -> Result<Self, &'static str> {
        let mut values = Vec::new();
        let relation = match filter.relation {
            Relation::All => None,
            Relation::Assigned => Some("assignee"),
            Relation::Created => Some("author"),
            Relation::Mentioned => Some("mentions"),
            Relation::Subscribed => Some("subscribed"),
        };
        if let Some(key) = relation {
            values.push(Expr::Term(key.into(), "@me".into()));
        }
        if let Some(state) = filter.state {
            values.push(Expr::Term("state".into(), state));
        }
        for label in filter.labels {
            values.push(Expr::Term("exact-label".into(), label));
        }
        if let Some(since) = filter.since {
            values.push(Expr::Term(
                "updated".into(),
                format!(">{}", since.to_rfc3339()),
            ));
        }
        if let Some(org) = filter.organization {
            values.push(Expr::Term("org".into(), org));
        }
        if let Some(scopes) = filter.scopes {
            let selections = ["owned", "collab", "orgs"]
                .into_iter()
                .zip(scopes)
                .filter(|(_, selected)| *selected)
                .map(|(key, _)| Expr::Term(key.into(), "true".into()))
                .collect();
            values.push(Expr::Or(selections));
        }
        if !filter.pulls {
            values.push(Expr::Term("type".into(), "issue".into()));
        }
        let search = Self {
            expr: Expr::And(values),
            fields: Vec::new(),
            policy_token: None,
        };
        search.statement(false, "TRUE")?;
        Ok(search)
    }
    pub fn parse(query: &str) -> Result<Self, &'static str> {
        let mut expr = expression(query)?;
        let mut names = Vec::new();
        super::catalog_search::extract_fields(&mut expr, &mut names)?;
        let mut fields = names
            .iter()
            .map(|field| match field.as_str() {
                "title" => Ok("title"),
                "body" => Ok("body"),
                "comments" => Ok("comments"),
                _ => Err("Invalid search field"),
            })
            .collect::<Result<Vec<_>, _>>()?;
        if fields.is_empty() {
            fields = vec!["title", "body", "comments"];
        }
        let search = Self { expr, fields, policy_token: None };
        // Validate before admission to a database connection.
        search.statement(false, "TRUE")?;
        Ok(search)
    }
    pub fn needs_viewer(&self) -> bool {
        fn needs(expr: &Expr) -> bool {
            match expr {
                Expr::Term(_, value) => value == "@me",
                Expr::And(v) | Expr::Or(v) => v.iter().any(needs),
                Expr::Not(v) => needs(v),
            }
        }
        needs(&self.expr)
    }
    pub(crate) fn statement(
        &self,
        postgres: bool,
        readable: &str,
    ) -> Result<Statement, &'static str> {
        let mut sql = Statement {
            postgres,
            args: Vec::new(),
            prefix: String::new(),
        };
        let readable = sql.repository_policy(readable, self.policy_token);
        let predicate = sql.expr(&self.expr, &self.fields)?;
        let mut text = Vec::new();
        positive_text(&self.expr, false, &mut text);
        let mut scores = Vec::new();
        for value in text {
            for field in &self.fields {
                let hit = sql.text(field, value);
                scores.push(format!(
                    "CASE WHEN {hit} THEN {} ELSE 0 END",
                    if *field == "title" { 3 } else { 1 }
                ));
            }
        }
        let score = if scores.is_empty() {
            "1".into()
        } else {
            scores.join("+")
        };
        sql.prefix = format!(
            r"WITH items AS (
            SELECT id,repo_id,number,author_id,title,body,state,locked,milestone_id,created_at,updated_at,closed_at,NULL AS merged_at,false AS draft,false AS merged,false AS pull_request,NULL AS base_branch,NULL AS head_branch FROM issues
            UNION ALL SELECT id,repo_id,number,author_id,title,body,CASE WHEN state='open' THEN 'open' ELSE 'closed' END,locked,milestone_id,created_at,updated_at,closed_at,merged_at,is_draft,state='merged',true,base_branch,head_branch FROM pull_requests
        ), assignments AS (
            SELECT issue_id AS item_id,user_id,false AS pull_request FROM issue_assignees UNION ALL SELECT pull_request_id,user_id,true FROM pr_assignees
        ), item_labels AS (
            SELECT issue_id AS item_id,label_id,false AS pull_request FROM issue_labels UNION ALL SELECT pull_request_id,label_id,true FROM pr_labels
        ), discussion_comments AS (
            SELECT issue_id AS item_id,author_id,body,false AS pull_request,true AS conversation FROM issue_comments
            UNION ALL SELECT c.pull_request_id,c.author_id,c.body,true,(c.path IS NULL AND c.review_id IS NULL) FROM pr_comments c LEFT JOIN pr_reviews v ON v.id=c.review_id WHERE c.review_id IS NULL OR v.state<>'pending'
            UNION ALL SELECT pull_request_id,author_id,body,true,false FROM pr_reviews WHERE state<>'pending'
        ), stats AS (
            SELECT i.*,(SELECT COUNT(*) FROM discussion_comments c WHERE c.item_id=i.id AND c.pull_request=i.pull_request AND c.conversation) AS comments,
            (SELECT COUNT(*) FROM reactions x WHERE x.subject_id=i.id AND x.subject_kind=CASE WHEN i.pull_request THEN 'pull_request' ELSE 'issue' END) AS reactions FROM items i
        ), matched AS (
            SELECT i.*,({score}) AS score FROM stats i JOIN repositories r ON r.id=i.repo_id LEFT JOIN users u ON u.id={viewer}
            LEFT JOIN users owners ON r.owner_kind='user' AND owners.id=r.owner_user_id LEFT JOIN organizations orgs ON r.owner_kind='org' AND orgs.id=r.owner_org_id
            WHERE ({readable}) AND ({predicate})
        )",
            viewer = if postgres { "$1" } else { "?1" }
        );
        Ok(sql)
    }
}
pub(crate) fn positive_text<'a>(expr: &'a Expr, negative: bool, values: &mut Vec<&'a str>) {
    match expr {
        Expr::Term(key, value) if key == "text" && !negative => values.push(value),
        Expr::And(v) | Expr::Or(v) => {
            for expr in v {
                positive_text(expr, negative, values);
            }
        }
        Expr::Not(v) => positive_text(v, !negative, values),
        _ => {}
    }
}

pub(crate) fn escape(value: &str) -> String {
    let mut escaped = String::new();
    for ch in value.chars() {
        if r"\.+*?()|[]{}^$#&-~".contains(ch) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

pub(crate) struct Statement {
    pub prefix: String,
    pub args: Vec<String>,
    postgres: bool,
}
impl Statement {
    pub(crate) fn new(postgres: bool) -> Self {
        Self {
            prefix: String::new(),
            args: Vec::new(),
            postgres,
        }
    }
    pub fn placeholder(&self, index: usize) -> String {
        format!("{}{index}", if self.postgres { '$' } else { '?' })
    }
    pub(crate) fn repository_policy(&mut self, readable: &str, token: Option<i64>) -> String {
        let Some(token) = token else { return readable.to_owned() };
        let token = self.bind(token.to_string());
        let allowed = super::fine_grained_tokens::classic_allowed_condition(&token, "r.owner_org_id");
        format!("({readable}) AND (NOT r.is_private OR ({allowed}))")
    }
    pub(crate) fn bind(&mut self, value: impl Into<String>) -> String {
        self.args.push(value.into());
        self.placeholder(self.args.len() + 1)
    }
    pub(crate) fn matches(&mut self, column: &str, value: &str) -> String {
        let value = self.bind(format!("(?i){}", escape(value)));
        format!(
            "COALESCE({column},'') {} {value}",
            if self.postgres { "~" } else { "REGEXP" }
        )
    }
    fn text(&mut self, field: &str, value: &str) -> String {
        if field == "comments" {
            let predicate = self.matches("c.body", value);
            format!("EXISTS (SELECT 1 FROM discussion_comments c WHERE c.item_id=i.id AND c.pull_request=i.pull_request AND {predicate})")
        } else {
            self.matches(&format!("i.{field}"), value)
        }
    }
    fn person(&mut self, column: &str, value: &str) -> String {
        if value == "@me" {
            format!("{column}=u.id")
        } else {
            let value = self.bind(value);
            format!("{column} IN (SELECT id FROM users WHERE lower(username)=lower({value}))")
        }
    }
    fn expr(&mut self, expr: &Expr, fields: &[&str]) -> Result<String, &'static str> {
        Ok(match expr {
            Expr::And(v)|Expr::Or(v)=>{if v.is_empty(){return Ok(if matches!(expr,Expr::And(_)){"true"}else{"false"}.into());}let parts=v.iter().map(|expr|self.expr(expr,fields).map(|s|format!("({s})"))).collect::<Result<Vec<_>,_>>()?;parts.join(if matches!(expr,Expr::And(_)){" AND "}else{" OR "})},
            Expr::Not(v)=>format!("NOT COALESCE(({}),false)",self.expr(v,fields)?),
            Expr::Term(key,value)=>match key.as_str(){
                "text"=>fields.iter().map(|field|self.text(field,value)).collect::<Vec<_>>().join(" OR "),
                "repo"=>{if !value.contains('/'){return Err("Repository search must name owner/repository");}let value=self.bind(value);format!("lower(COALESCE(owners.username,orgs.name)||'/'||r.name)=lower({value})")},
                "user"|"org"=>{if value=="@me" {if key=="org" {return Err("An organization name is required");} "r.owner_kind='user' AND r.owner_user_id=u.id".into()} else {let p=self.bind(value);format!("{}lower(COALESCE(owners.username,orgs.name))=lower({p})",if key=="org"{"r.owner_kind='org' AND "}else{""})}},
                "author"=>if let Some(app)=value.strip_prefix("app/") {let p=self.bind(app);format!("i.author_id IN (SELECT bot_id FROM github_apps WHERE lower(slug)=lower({p}))")} else {self.person("i.author_id",value)},
                "base"=>{let p=self.bind(value);format!("i.pull_request AND EXISTS(SELECT 1 FROM pull_requests pr WHERE pr.id=i.id AND lower(pr.base_branch)=lower({p}))")},
                // As on GitHub, head matches the branches whose names begin with the value.
                "head"=>{let p=self.bind(value);format!("i.pull_request AND EXISTS(SELECT 1 FROM pull_requests pr WHERE pr.id=i.id AND lower(substr(pr.head_branch,1,length({p})))=lower({p}))")},
                "language"=>{let p=self.bind(value);format!("EXISTS(SELECT 1 FROM repo_languages l WHERE l.repo_id=r.id AND lower(l.language)=lower({p}))")},
                "team"=>{let (org,team)=value.split_once('/').ok_or("Team must name organization/team")?;let org=self.bind(org);let team=self.bind(team);let visible=self.team_visible();let pattern=self.bind(format!("(?i)(^|[^A-Za-z0-9_])@{}([^A-Za-z0-9_/-]|$)",escape(value)));let op=if self.postgres{"~"}else{"REGEXP"};format!("EXISTS(SELECT 1 FROM teams t JOIN organizations o ON o.id=t.org_id WHERE lower(o.name)=lower({org}) AND lower(t.slug)=lower({team}) AND {visible}) AND (i.body {op} {pattern} OR EXISTS(SELECT 1 FROM discussion_comments c WHERE c.item_id=i.id AND c.pull_request=i.pull_request AND c.body {op} {pattern}))")},
                "team-review-requested"=>{let (org,team)=value.split_once('/').ok_or("Team must name organization/team")?;let org=self.bind(org);let team=self.bind(team);let visible=self.team_visible();format!("i.pull_request AND EXISTS(SELECT 1 FROM pr_team_reviewers v JOIN teams t ON t.id=v.team_id JOIN organizations o ON o.id=t.org_id WHERE v.pull_request_id=i.id AND lower(o.name)=lower({org}) AND lower(t.slug)=lower({team}) AND {visible})")},
                "reviewed-by"=>{let who=self.person("v.author_id",value);format!("i.pull_request AND EXISTS(SELECT 1 FROM pr_reviews v WHERE v.pull_request_id=i.id AND v.state<>'pending' AND {who})")},
                "review"=>self.review(value)?,
                "status"=>self.status(value)?,
                "project"=>self.project(Some(value))?,
                "assignee"=>{let who=self.person("a.user_id",value);format!("EXISTS (SELECT 1 FROM assignments a WHERE a.item_id=i.id AND a.pull_request=i.pull_request AND {who})")},
                "review-requested"=>{let who=self.person("v.user_id",value);format!("i.pull_request AND EXISTS (SELECT 1 FROM pr_reviewers v WHERE v.pull_request_id=i.id AND {who})")},
                "commenter"=>{let who=self.person("c.author_id",value);format!("EXISTS (SELECT 1 FROM discussion_comments c WHERE c.item_id=i.id AND c.pull_request=i.pull_request AND {who})")},
                "mentions"=>self.mentions(value),
                "milestone-number"=>{let p=self.bind(value);format!("EXISTS (SELECT 1 FROM milestones m WHERE m.id=i.milestone_id AND CAST(m.number AS TEXT)={p})")},

                "involves"=>{let author=self.person("i.author_id",value);let assigned=self.person("a.user_id",value);let commenter=self.person("c.author_id",value);let mentioned=self.mentions(value);format!("({author} OR EXISTS (SELECT 1 FROM assignments a WHERE a.item_id=i.id AND a.pull_request=i.pull_request AND {assigned}) OR EXISTS (SELECT 1 FROM discussion_comments c WHERE c.item_id=i.id AND c.pull_request=i.pull_request AND {commenter}) OR {mentioned})")},
                "owned"=>"r.owner_kind='user' AND r.owner_user_id=u.id".into(),
                "collab"=>"EXISTS (SELECT 1 FROM repo_access a WHERE a.repo_id=r.id AND a.user_id=u.id AND a.access_level>=1)".into(),
                "orgs"=>"r.owner_kind='org' AND EXISTS (SELECT 1 FROM org_memberships a WHERE a.org_id=r.owner_org_id AND a.user_id=u.id)".into(),
                "subscribed"=>{
                    let participation=self.expr(&Expr::Term("involves".into(),value.clone()),fields)?;
                    format!("NOT EXISTS (SELECT 1 FROM watches w WHERE w.repo_id=r.id AND w.user_id=u.id AND w.level='ignoring') AND ({participation} OR EXISTS (SELECT 1 FROM watches w WHERE w.repo_id=r.id AND w.user_id=u.id AND (w.level='watching' OR (w.level='custom' AND (','||w.events||',') LIKE CASE WHEN i.pull_request THEN '%,pulls,%' ELSE '%,issues,%' END))))")
                },
                "exact-label"=>{let p=self.bind(value);format!("EXISTS (SELECT 1 FROM item_labels a JOIN labels l ON l.id=a.label_id WHERE a.item_id=i.id AND a.pull_request=i.pull_request AND lower(l.name)=lower({p}))")},
                "label"=>{let mut labels=Vec::new();for value in value.split(','){let p=self.bind(value);labels.push(format!("lower(l.name)=lower({p})"));}format!("EXISTS (SELECT 1 FROM item_labels a JOIN labels l ON l.id=a.label_id WHERE a.item_id=i.id AND a.pull_request=i.pull_request AND ({}))",labels.join(" OR "))},
                "milestone"=>{let p=self.bind(value);format!("EXISTS (SELECT 1 FROM milestones m WHERE m.id=i.milestone_id AND lower(m.title)=lower({p}))")},
                "state"=>match value.as_str(){"open"=>"i.state='open'".into(),"closed"=>"i.state='closed'".into(),_=>return Err("Invalid issue state")},
                "type"=>match value.as_str(){"issue"=>"NOT i.pull_request".into(),"pr"|"pull-request"=>"i.pull_request".into(),_=>return Err("Invalid discussion type")},
                "is"=>match value.as_str(){"issue"=>"NOT i.pull_request".into(),"pr"|"pull-request"=>"i.pull_request".into(),"open"=>"i.state='open'".into(),"closed"=>"i.state='closed'".into(),"merged"=>"i.merged".into(),"unmerged"=>"i.pull_request AND NOT i.merged".into(),"draft"=>"i.draft".into(),"locked"=>"i.locked".into(),"unlocked"=>"NOT i.locked".into(),"public"=>"NOT r.is_private".into(),"private"=>"r.is_private".into(),"internal"=>"false".into(),_=>return Err("Invalid is qualifier")},
                "draft"|"archived"=>{if !matches!(value.as_str(),"true"|"false"){return Err("Invalid boolean qualifier");}let column=if key=="draft"{"i.draft"}else{"r.is_archived"};format!("{}{column}",if value=="false"{"NOT "}else{""})},
                "no"=>match value.as_str(){"assignee"=>"NOT EXISTS (SELECT 1 FROM assignments a WHERE a.item_id=i.id AND a.pull_request=i.pull_request)".into(),"label"=>"NOT EXISTS (SELECT 1 FROM item_labels a WHERE a.item_id=i.id AND a.pull_request=i.pull_request)".into(),"milestone"=>"i.milestone_id IS NULL".into(),"project"=>format!("NOT ({})",self.project(None)?),_=>return Err("Invalid no qualifier")},
                "created"|"updated"|"closed"|"merged"=>self.range(&format!("i.{key}_at"),value,true)?,
                "comments"|"reactions"|"interactions"=>self.range(if key=="interactions"{"(i.comments+i.reactions)"}else if key=="comments"{"i.comments"}else{"i.reactions"},value,false)?,
                _=>return Err("Unsupported search qualifier"),
            },
        })
    }
    fn team_visible(&self) -> String {
        "(t.privacy='visible' OR EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=t.org_id AND m.user_id=u.id AND m.role IN ('owner','admin')) OR EXISTS(SELECT 1 FROM effective_team_memberships tm JOIN org_memberships m ON m.org_id=t.org_id AND m.user_id=tm.user_id WHERE tm.team_id=t.id AND tm.user_id=u.id AND m.role IN ('owner','admin','member')))".into()
    }
    fn project(&mut self, value: Option<&str>) -> Result<String, &'static str> {
        let scope = if let Some(value) = value {
            let (owner, number) = value
                .rsplit_once('/')
                .ok_or("Project must name owner/number")?;
            let number = number
                .parse::<i64>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or("Invalid project number")?;
            let owner = self.bind(owner);
            let number = self.bind(number.to_string());
            format!(" AND p.number=CAST({number} AS BIGINT) AND (EXISTS(SELECT 1 FROM users po WHERE po.id=p.owner_user_id AND lower(po.username)=lower({owner})) OR EXISTS(SELECT 1 FROM organizations po WHERE po.id=p.owner_org_id AND lower(po.name)=lower({owner})))")
        } else {
            String::new()
        };
        let readable = super::projects::readable_condition("u.id");
        Ok(format!("EXISTS(SELECT 1 FROM project_items pi JOIN projects p ON p.id=pi.project_id WHERE ((i.pull_request AND pi.pull_request_id=i.id) OR (NOT i.pull_request AND pi.issue_id=i.id)) AND {readable}{scope})"))
    }
    fn review(&self, value: &str) -> Result<String, &'static str> {
        let decisions = "SELECT v.state FROM pr_reviews v WHERE v.pull_request_id=i.id AND v.state NOT IN ('pending','commented') AND NOT EXISTS(SELECT 1 FROM pr_reviews newer WHERE newer.pull_request_id=v.pull_request_id AND newer.author_id=v.author_id AND newer.state NOT IN ('pending','commented') AND (newer.created_at>v.created_at OR (newer.created_at=v.created_at AND newer.id>v.id)))";
        let approved =
            format!("EXISTS(SELECT 1 FROM ({decisions}) decisions WHERE state='approved')");
        let changes = format!(
            "EXISTS(SELECT 1 FROM ({decisions}) decisions WHERE state='changes_requested')"
        );
        let predicate = match value {
            "none" => "NOT EXISTS(SELECT 1 FROM pr_reviews v WHERE v.pull_request_id=i.id AND v.state<>'pending')".into(),
            "required" => "(EXISTS(SELECT 1 FROM pr_reviewers v WHERE v.pull_request_id=i.id) OR EXISTS(SELECT 1 FROM pr_team_reviewers v WHERE v.pull_request_id=i.id))".into(),
            "approved" => format!("{approved} AND NOT ({changes})"),
            "changes_requested" => changes,
            _ => return Err("Invalid review qualifier"),
        };
        Ok(format!("i.pull_request AND ({predicate})"))
    }
    fn status(&self, value: &str) -> Result<String, &'static str> {
        let source = "commit_statuses s JOIN pull_requests pr ON pr.repo_id=s.repo_id AND pr.head_sha=s.sha WHERE pr.id=i.id";
        let failure = format!("EXISTS(SELECT 1 FROM {source} AND s.state IN ('error','failure'))");
        let pending = format!("EXISTS(SELECT 1 FROM {source} AND s.state='pending')");
        let predicate = match value {
            "failure" => failure,
            "pending" => {
                format!("NOT ({failure}) AND ({pending} OR NOT EXISTS(SELECT 1 FROM {source}))")
            }
            "success" => {
                format!("EXISTS(SELECT 1 FROM {source}) AND NOT ({failure}) AND NOT ({pending})")
            }
            _ => return Err("Invalid status qualifier"),
        };
        Ok(format!("i.pull_request AND ({predicate})"))
    }
    fn mentions(&mut self, value: &str) -> String {
        // Mention matching has username boundaries and includes published comments, never drafts.
        let pattern = if value == "@me" {
            "'(?i)(^|[^A-Za-z0-9_])@'||u.username||'([^A-Za-z0-9_-]|$)'".to_owned()
        } else {
            self.bind(format!(
                "(?i)(^|[^A-Za-z0-9_])@{}([^A-Za-z0-9_-]|$)",
                escape(value)
            ))
        };
        let op = if self.postgres { "~" } else { "REGEXP" };
        format!("(i.body {op} ({pattern}) OR EXISTS (SELECT 1 FROM discussion_comments c WHERE c.item_id=i.id AND c.pull_request=i.pull_request AND c.body {op} ({pattern})))")
    }
    pub(crate) fn range(
        &mut self,
        column: &str,
        value: &str,
        date: bool,
    ) -> Result<String, &'static str> {
        if let Some((lo, hi)) = value.split_once("..") {
            if lo == "*" && hi == "*" {
                return Err("Unbounded search range");
            }
            let mut parts = Vec::new();
            if lo != "*" {
                parts.push(self.comparison(column, ">=", lo, date)?);
            }
            if hi != "*" {
                parts.push(self.comparison(column, "<=", hi, date)?);
            }
            return Ok(parts.join(" AND "));
        }
        let (op, value) = ["<=", ">=", "<", ">", "="]
            .iter()
            .find_map(|op| value.strip_prefix(op).map(|v| (*op, v)))
            .unwrap_or(("=", value));
        self.comparison(column, op, value, date)
    }
    fn comparison(
        &mut self,
        column: &str,
        op: &str,
        value: &str,
        date: bool,
    ) -> Result<String, &'static str> {
        if !date {
            let value = value
                .parse::<u64>()
                .ok()
                .filter(|v| *v <= i64::MAX as u64)
                .ok_or("Invalid numeric search range")?;
            let p = self.bind(value.to_string());
            return Ok(format!("{column}{op}CAST({p} AS BIGINT)"));
        }
        let day = NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .ok()
            .filter(|d| d.format("%Y-%m-%d").to_string() == value);
        let date = day
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .map(|d| d.and_utc())
            .or_else(|| {
                DateTime::parse_from_rfc3339(value)
                    .ok()
                    .map(|d| d.with_timezone(&Utc))
            })
            .ok_or("Invalid date search range")?;
        let next = date
            .checked_add_signed(chrono::Duration::days(1))
            .ok_or("Invalid date search range")?;
        let (op, date) = if day.is_some() {
            match op {
                "=" => {
                    let a = self.date(column, ">=", date);
                    let b = self.date(column, "<", next);
                    return Ok(format!("{a} AND {b}"));
                }
                "<=" => ("<", next),
                ">" => (">=", next),
                _ => (op, date),
            }
        } else {
            (op, date)
        };
        Ok(self.date(column, op, date))
    }
    fn date(&mut self, column: &str, op: &str, date: DateTime<Utc>) -> String {
        let p = self.bind(date.to_rfc3339_opts(chrono::SecondsFormat::Micros, true));
        if self.postgres {
            format!("{column}{op}CAST({p} AS TIMESTAMPTZ)")
        } else {
            format!("julianday({column}){op}julianday({p})")
        }
    }
}

pub struct Results {
    pub total: i64,
    pub items: Vec<(i64, i64, bool, i64)>,
}
native_operation!(crate::backend::db::postgres::discussion_search::search,
    crate::backend::db::sqlite::discussion_search::search,
    fn search(pool:&DbPool,viewer:Option<i64>,query:&Search,sort:Sort,ascending:bool,offset:i64,limit:i64)->anyhow::Result<Results>);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expressions_are_bounded_and_values_stay_out_of_sql() {
        let query = Search::parse(
            "repo:alice/proj (\"hello world\" OR label:\"a' bug\") -is:closed created:>=2020-01-01",
        )
        .unwrap();
        let sql = query.statement(true, "r.id>0").unwrap();
        assert!(!sql.prefix.contains("a' bug"));
        assert!(sql.args.iter().any(|s| s == "a' bug"));
        // Restricted classic-token repositories must be excluded by the counted relation itself.
        for postgres in [true, false] {
            let guarded = query.clone().with_classic_policy(Some(987654321)).statement(postgres, "r.id>0").unwrap();
            assert!(guarded.prefix.contains("organization_token_policies"));
            assert!(guarded.prefix.contains("NOT r.is_private OR"));
            assert!(guarded.args.iter().any(|arg| arg == "987654321"));
            assert!(!guarded.prefix.contains("987654321"));
        }
        for q in [
            "",
            "foo OR",
            "(foo",
            "foo)",
            "created:bad",
            "comments:no",
            "is:bad",
            "unknown:foo",
            "in:secret foo",
        ] {
            assert!(Search::parse(q).is_err(), "{q}");
        }
    }
    #[test]
    fn repeated_repos_are_alternatives_and_labels_are_conjunctions() {
        let query = Search::parse("repo:a/b repo:c/d label:one label:two").unwrap();
        let sql = query.statement(false, "TRUE").unwrap();
        assert!(sql.prefix.contains(" OR "));
        assert!(sql.args.contains(&"one".into()));
        assert!(sql.args.contains(&"two".into()));
    }
}
