//! Moving a user or organization, with its repositories, to another cell, or one repository
//! to a new owner in another cell. Both cells run PostgreSQL. The steps are `begin`,
//! `freeze`, `copy`, `switch` and `release`; the directory switch and the Git copy run
//! between them (see `codeyard cell move`). Each step can run again after a crash, and a
//! move that has not switched can be cancelled.
use crate::copy::postgres as pg;
use crate::copy::{quote, shape, Table, CHUNK_BYTES};
use anyhow::{bail, ensure, Result};
use codeyard_db_core::cells::ownership::{self, Reference, Rule};
use codeyard_model::cells::{Key, KeyKind, ObjectKind};
use futures_util::TryStreamExt;
use sqlx::{PgPool, Postgres, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

type Tx = Transaction<'static, Postgres>;

/// The most distinct rows one reference may need from outside the account.
const MAX_REFERENCED: usize = 100_000;
/// How long the freeze must still last when the copy commits.
const FREEZE_MARGIN_SECONDS: i64 = 30;
/// Also enforced on the retained cleanup paths in both baseline schemas.
const MAX_REPOSITORIES: usize = 10_000;
const MAX_CREDENTIALS: usize = 10_000;

#[derive(Clone, Copy, Debug)]
pub struct Account {
    kind: ObjectKind,
    id: i64,
}

impl Account {
    pub fn new(kind: ObjectKind, id: i64) -> Result<Self> {
        ensure!(id > 0, "Invalid account id");
        Ok(Self { kind, id })
    }
    pub fn id(self) -> i64 {
        self.id
    }
    pub fn kind(self) -> ObjectKind {
        self.kind
    }
    fn table(self) -> &'static str {
        match self.kind {
            ObjectKind::Organization => "organizations",
            ObjectKind::Repository => "repositories",
            ObjectKind::User => "users",
        }
    }
    /// Selects the repositories that move, with the account bound as `$1`.
    fn repositories(self) -> &'static str {
        match self.kind {
            ObjectKind::Organization => "owner_org_id=$1",
            ObjectKind::Repository => "id=$1",
            ObjectKind::User => "owner_user_id=$1",
        }
    }
    /// The `owner_kind`, `owner_user_id` and `owner_org_id` values of a repository it owns.
    fn owner_values(self) -> [String; 3] {
        match self.kind {
            ObjectKind::Organization => ["'org'".into(), "NULL".into(), self.id.to_string()],
            _ => ["'user'".into(), self.id.to_string(), "NULL".into()],
        }
    }
}

/// A repository moving to an owner who lives in the target cell. The source keeps a stub of
/// the repository under that owner and redirects the former name to it.
pub struct Transfer {
    former: Account,
    owner: Account,
    name: String,
    owner_row: String,
}

impl Transfer {
    pub fn former(&self) -> Account {
        self.former
    }
}

/// Checks that `owner` lives in the target cell and has no repository of the same name, and
/// reads what the source needs to keep. Returns `None` when the source already released the
/// repository, so a repeated run only finishes the cleanup.
pub async fn transfer(
    source: &PgPool,
    target: &PgPool,
    repository: Account,
    owner: &str,
) -> Result<Option<Transfer>> {
    ensure!(
        repository.kind == ObjectKind::Repository,
        "Only a repository changes owner"
    );
    let current: Option<(String, Option<i64>, String, bool, bool)> = sqlx::query_as(
        "SELECT owner_kind,CASE owner_kind WHEN 'org' THEN owner_org_id ELSE owner_user_id END,\
         name,is_stub,is_archived FROM repositories WHERE id=$1",
    )
    .bind(repository.id)
    .fetch_optional(source)
    .await?;
    let Some((kind, owner_id, name, stub, archived)) = current else {
        bail!("No such repository in this cell");
    };
    if stub {
        return Ok(None);
    }
    ensure!(!archived, "An archived repository cannot change owner");
    let owner_id = owner_id.ok_or_else(|| anyhow::anyhow!("The repository has no owner"))?;
    let former = if kind == "org" {
        Account::new(ObjectKind::Organization, owner_id)?
    } else {
        Account::new(ObjectKind::User, owner_id)?
    };
    let found: Option<(String, i64)> = sqlx::query_as(
        "SELECT 'user',id FROM users WHERE lower(username)=lower($1) AND NOT is_stub \
         UNION ALL SELECT 'organization',id FROM organizations WHERE lower(name)=lower($1) AND NOT is_stub",
    )
    .bind(owner)
    .fetch_optional(target)
    .await?;
    let Some((kind, id)) = found else {
        bail!("The new owner does not live in the target cell");
    };
    let owner = Account::new(
        ObjectKind::parse(&kind).ok_or_else(|| anyhow::anyhow!("Invalid owner kind"))?,
        id,
    )?;
    let taken: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS(SELECT 1 FROM repositories WHERE {} AND lower(name)=lower($2) AND id<>$3)",
        owner.repositories()
    ))
    .bind(owner.id)
    .bind(&name)
    .bind(repository.id)
    .fetch_one(target)
    .await?;
    ensure!(
        !taken,
        "The new owner already has a repository named {name}"
    );
    let owner_row: String = sqlx::query_scalar(&format!(
        "SELECT COALESCE(json_agg(to_jsonb(t)),'[]')::text FROM {} t WHERE id=$1",
        owner.table()
    ))
    .bind(owner.id)
    .fetch_one(target)
    .await?;
    Ok(Some(Transfer {
        former,
        owner,
        name,
        owner_row,
    }))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Pending,
    Frozen,
    Switched,
    Released,
}

fn state(value: &str) -> Result<State> {
    Ok(match value {
        "pending" => State::Pending,
        "frozen" => State::Frozen,
        "switched" => State::Switched,
        "released" => State::Released,
        _ => bail!("Invalid move state"),
    })
}

async fn cell_id(tx: &mut Tx) -> Result<i32> {
    Ok(
        sqlx::query_scalar(
            "SELECT COALESCE((SELECT cell_id FROM node_configuration WHERE id=1),0)",
        )
        .fetch_one(&mut **tx)
        .await?,
    )
}

/// The cell a database belongs to.
pub async fn cell(pool: &PgPool) -> Result<i32> {
    let mut tx = pool.begin().await?;
    let cell = cell_id(&mut tx).await?;
    tx.commit().await?;
    Ok(cell)
}

async fn set_lock_timeout(tx: &mut Tx, wait: Duration) -> Result<()> {
    sqlx::query(&format!(
        "SET LOCAL lock_timeout = '{}ms'",
        wait.as_millis().max(1)
    ))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Records the move, asks the account's running CI to stop, and waits until every
/// transaction that began earlier has ended, so each later write checks the move row.
/// Returns the state an earlier run reached.
pub async fn begin(pool: &PgPool, account: Account, target: i32, wait: Duration) -> Result<State> {
    let mut tx = pool.begin().await?;
    let cell = cell_id(&mut tx).await?;
    ensure!(cell != target, "The account already lives in cell {target}");
    let current: Option<(i32, String)> = sqlx::query_as(
        "SELECT target_cell,state FROM account_moves WHERE account_kind=$1 AND account_id=$2 FOR UPDATE",
    )
    .bind(account.kind.as_str())
    .bind(account.id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((moving_to, value)) = current {
        ensure!(
            moving_to == target,
            "The account is already moving to cell {moving_to}"
        );
        tx.commit().await?;
        let value = state(&value)?;
        if value != State::Pending {
            return Ok(value);
        }
    } else {
        // A successful move may be repeated after its final reply was lost. A stub
        // pointing at this destination proves completion, not a new move to start.
        let released: bool = sqlx::query_scalar(&format!(
            "SELECT EXISTS(SELECT 1 FROM {} WHERE id=$1 AND is_stub AND home_cell=$2)",
            account.table()
        ))
        .bind(account.id)
        .bind(target)
        .fetch_one(&mut *tx)
        .await?;
        if released {
            tx.commit().await?;
            return Ok(State::Released);
        }
        let native: bool = sqlx::query_scalar(&format!(
            "SELECT EXISTS(SELECT 1 FROM {} WHERE id=$1 AND NOT is_stub AND home_cell=$2)",
            account.table()
        ))
        .bind(account.id)
        .bind(cell)
        .fetch_one(&mut *tx)
        .await?;
        ensure!(native, "The account does not live in this cell");
        sqlx::query("INSERT INTO account_moves(account_kind,account_id,target_cell,state,expires_at) VALUES($1,$2,$3,'pending',clock_timestamp())")
            .bind(account.kind.as_str())
            .bind(account.id)
            .bind(target)
            .execute(&mut *tx)
            .await?;
        sqlx::query(&format!(
            "UPDATE ci_runs SET cancel_requested=TRUE WHERE status IN ('queued','running') AND repo_id IN (SELECT id FROM repositories WHERE NOT is_stub AND {})",
            account.repositories()
        ))
        .bind(account.id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    let marker: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(pool)
        .await?;
    wait_until(wait, "Transactions older than the move are still open", move || async move {
        let older: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND pid<>pg_backend_pid() AND backend_type='client backend' AND xact_start<$1",
        )
        .bind(marker)
        .fetch_one(pool)
        .await?;
        Ok(older == 0)
    })
    .await?;
    Ok(State::Pending)
}

async fn wait_until<F, Fut>(wait: Duration, failure: &str, mut done: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    let deadline = Instant::now() + wait;
    loop {
        if done().await? {
            return Ok(());
        }
        ensure!(Instant::now() < deadline, "{failure}; try again later");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Refuses every write to the account's rows for `hold`, then waits for the account's
/// undelivered cross-cell notices to go out. The freeze lifts by itself when `hold` passes.
pub async fn freeze(pool: &PgPool, account: Account, hold: Duration, wait: Duration) -> Result<()> {
    // A running worker must still be able to record its cancellation and final log
    // pointers. Freezing first would prevent its completion, then copy unfinished work
    // while that worker was still producing bytes in the source region.
    let running = format!(
        "SELECT count(*) FROM ci_runs WHERE status='running' AND repo_id IN (SELECT id FROM repositories WHERE NOT is_stub AND {})",
        account.repositories()
    );
    let running = running.as_str();
    wait_until(wait, "CI runs are still draining", move || async move {
        let count: i64 = sqlx::query_scalar(running)
            .bind(account.id)
            .fetch_one(pool)
            .await?;
        Ok(count == 0)
    })
    .await?;
    let mut tx = pool.begin().await?;
    set_lock_timeout(&mut tx, wait).await?;
    // FOR UPDATE waits for the writers holding the row FOR KEY SHARE; a plain update would not.
    let current: Option<String> = sqlx::query_scalar(
        "SELECT state FROM account_moves WHERE account_kind=$1 AND account_id=$2 FOR UPDATE",
    )
    .bind(account.kind.as_str())
    .bind(account.id)
    .fetch_optional(&mut *tx)
    .await?;
    match current.as_deref().map(state).transpose()? {
        None => bail!("The move was not started"),
        Some(State::Switched | State::Released) => return Ok(tx.commit().await?),
        Some(State::Pending | State::Frozen) => {
            // The exclusive move-row lock has now drained every writer admitted since
            // begin. A CI claim racing the first check either finished before this check
            // or waits for the freeze and is refused by the same write guard.
            let count: i64 = sqlx::query_scalar(running)
                .bind(account.id)
                .fetch_one(&mut *tx)
                .await?;
            ensure!(
                count == 0,
                "A CI run started while draining; retry the move"
            );
            sqlx::query("UPDATE account_moves SET state='frozen',expires_at=clock_timestamp()+make_interval(secs=>$3) WHERE account_kind=$1 AND account_id=$2")
                .bind(account.kind.as_str())
                .bind(account.id)
                .bind(hold.as_secs_f64())
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    let undelivered = format!(
        "SELECT count(*) FROM cell_outbox WHERE delivered_at IS NULL AND ((root_kind=$1 AND root_id=$2) OR (root_kind='repository' AND root_id IN (SELECT id FROM repositories WHERE NOT is_stub AND {})))",
        account.repositories().replace("$1", "$2")
    );
    let undelivered = undelivered.as_str();
    wait_until(
        wait,
        "Cross-cell notices of the account are still waiting",
        move || async move {
            let count: i64 = sqlx::query_scalar(undelivered)
                .bind(account.kind.as_str())
                .bind(account.id)
                .fetch_one(pool)
                .await?;
            Ok(count == 0)
        },
    )
    .await
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Repository {
    pub id: i64,
    pub disk_path: String,
    /// The cluster storage node that holds the repository, or none for a local directory.
    #[serde(default)]
    pub storage_node_id: Option<i64>,
}

/// The account's repositories in this cell, with the storage node that holds each one.
pub async fn repositories(pool: &PgPool, account: Account) -> Result<Vec<Repository>> {
    let released: Option<String> = sqlx::query_scalar(
        "SELECT repository_paths::text FROM account_moves WHERE account_kind=$1 AND account_id=$2 AND state='released'",
    )
    .bind(account.kind.as_str())
    .bind(account.id)
    .fetch_optional(pool)
    .await?;
    if let Some(paths) = released {
        return Ok(serde_json::from_str(&paths)?);
    }
    let rows: Vec<(i64, String, Option<i64>)> = sqlx::query_as(&format!(
        "SELECT id,disk_path,storage_node_id FROM repositories WHERE NOT is_stub AND {} ORDER BY id LIMIT {}",
        account.repositories(),
        MAX_REPOSITORIES + 1
    ))
    .bind(account.id)
    .fetch_all(pool)
    .await?;
    ensure!(
        rows.len() <= MAX_REPOSITORIES,
        "The account has too many repositories to move in one operation"
    );
    Ok(rows
        .into_iter()
        .map(|(id, disk_path, storage_node_id)| Repository {
            id,
            disk_path,
            storage_node_id,
        })
        .collect())
}

#[derive(Debug, Default)]
pub struct Copied {
    pub rows: u64,
    pub stubs: u64,
    pub shared: u64,
    pub counters_fixed: u64,
    /// Rows of a repository that changed owner and pointed at the former owner's records.
    pub dropped: u64,
}

struct ForeignKey {
    child: String,
    columns: Vec<String>,
    parent: String,
    parent_columns: Vec<String>,
    on_delete: String,
}

async fn foreign_keys(tx: &mut Tx) -> Result<Vec<ForeignKey>> {
    type ForeignKeyRow = (String, String, Vec<String>, Vec<String>, String);
    let rows: Vec<ForeignKeyRow> = sqlx::query_as(
        "SELECT c.relname::text,p.relname::text, \
         ARRAY(SELECT a.attname::text FROM unnest(con.conkey) WITH ORDINALITY k(n,i) JOIN pg_attribute a ON a.attrelid=con.conrelid AND a.attnum=k.n ORDER BY k.i), \
         ARRAY(SELECT a.attname::text FROM unnest(con.confkey) WITH ORDINALITY k(n,i) JOIN pg_attribute a ON a.attrelid=con.confrelid AND a.attnum=k.n ORDER BY k.i), \
         con.confdeltype::text \
         FROM pg_constraint con JOIN pg_class c ON c.oid=con.conrelid JOIN pg_class p ON p.oid=con.confrelid \
         JOIN pg_namespace n ON n.oid=c.relnamespace \
         WHERE con.contype='f' AND n.nspname=current_schema() ORDER BY 1,2,3",
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(child, parent, columns, parent_columns, on_delete)| ForeignKey {
                child,
                columns,
                parent,
                parent_columns,
                on_delete,
            },
        )
        .collect())
}

fn is_root(table: &str) -> bool {
    matches!(table, "users" | "organizations" | "repositories")
}

fn column_list(alias: &str, columns: &[String]) -> String {
    columns
        .iter()
        .map(|column| format!("{alias}.{}", quote(column)))
        .collect::<Vec<_>>()
        .join(",")
}

fn column_type<'a>(tables: &'a [Table], table: &str, column: &str) -> Result<&'a str> {
    tables
        .iter()
        .find(|candidate| candidate.name == table)
        .and_then(|table| {
            table
                .columns
                .iter()
                .find(|candidate| candidate.name == column)
        })
        .map(|column| column.ty.as_str())
        .ok_or_else(|| anyhow::anyhow!("Unknown column {table}.{column}"))
}

/// The key values, as text, that rows of `child` reach in `parent` through `columns`.
/// A single column gives its value; several give a JSON array.
fn key_text(alias: &str, columns: &[String]) -> String {
    match columns {
        [column] => format!("{alias}.{}::text", quote(column)),
        _ => format!("json_build_array({})::text", column_list(alias, columns)),
    }
}

/// The values of `keys` (from `key_text`) that `table` lacks in `columns`.
async fn missing(
    tx: &mut Tx,
    tables: &[Table],
    table: &str,
    columns: &[String],
    keys: &[String],
) -> Result<Vec<String>> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let mut parts = Vec::new();
    for (index, column) in columns.iter().enumerate() {
        let ty = column_type(tables, table, column)?;
        let value = if columns.len() == 1 {
            "x".to_owned()
        } else {
            format!("(x::jsonb->>{index})")
        };
        parts.push(format!("p.{}=CAST({value} AS {ty})", quote(column)));
    }
    let sql = format!(
        "SELECT x FROM unnest($1::text[]) x WHERE NOT EXISTS(SELECT 1 FROM {} p WHERE {})",
        quote(table),
        parts.join(" AND ")
    );
    Ok(sqlx::query_scalar(&sql)
        .bind(keys)
        .fetch_all(&mut **tx)
        .await?)
}

/// Streams the rows `read` selects from the source into the target and returns how many
/// were read and how many the target took.
async fn copy_rows(
    from: &mut Tx,
    to: &mut Tx,
    read: &str,
    bind: Binding<'_>,
    insert: &str,
) -> Result<(u64, u64)> {
    let query = sqlx::query_scalar::<_, String>(read);
    let query = match bind {
        Binding::Id(id) => query.bind(id),
        Binding::Keys(keys) => query.bind(keys),
    };
    let mut rows = query.fetch(&mut **from);
    let (mut read_count, mut written) = (0, 0);
    let mut chunk = String::new();
    loop {
        let row = rows.try_next().await?;
        if let Some(row) = &row {
            chunk.push(if chunk.is_empty() { '[' } else { ',' });
            chunk.push_str(row);
            read_count += 1;
        }
        if !chunk.is_empty() && (row.is_none() || chunk.len() >= CHUNK_BYTES) {
            chunk.push(']');
            written += sqlx::query(insert)
                .bind(&chunk)
                .execute(&mut **to)
                .await?
                .rows_affected();
            chunk.clear();
        }
        if row.is_none() {
            return Ok((read_count, written));
        }
    }
}

enum Binding<'a> {
    Id(i64),
    Keys(&'a [String]),
}

/// Rows outside the account that the copied rows reference, by table.
#[derive(Default)]
struct Needed {
    accounts: BTreeMap<&'static str, BTreeSet<i64>>,
    shared: Vec<(String, String, Vec<String>)>,
    dropped: u64,
}

/// Sorts the keys one reference reaches outside the account: account and repository rows
/// become stubs, shared rows are copied, and anything else must already be in the target.
/// A reference that may be cleared on delete is cleared instead. A repository that changes
/// owner drops its other rows that point at the former owner's records (team grants, runner
/// groups, organization values and properties), as a transfer does; others fail the move.
async fn place(
    to: &mut Tx,
    tables: &[Table],
    key: &ForeignKey,
    keys: Vec<String>,
    owned: Option<&str>,
    account: Account,
    needed: &mut Needed,
) -> Result<()> {
    ensure!(
        keys.len() <= MAX_REFERENCED,
        "{}.{} references more than {MAX_REFERENCED} rows outside the account",
        key.child,
        key.columns.join(",")
    );
    let absent = missing(to, tables, &key.parent, &key.parent_columns, &keys).await?;
    if absent.is_empty() {
        return Ok(());
    }
    let single = key.parent_columns.len() == 1;
    let root = match key.parent.as_str() {
        "users" => Some("users"),
        "organizations" => Some("organizations"),
        "repositories" => Some("repositories"),
        _ => None,
    };
    if let (Some(root), true) = (root, single && key.parent_columns[0] == "id") {
        let ids = needed.accounts.entry(root).or_default();
        for value in absent {
            ids.insert(value.parse()?);
        }
    } else if single && matches!(ownership::rule(&key.parent), Some(Rule::Shared)) {
        needed
            .shared
            .push((key.parent.clone(), key.parent_columns[0].clone(), absent));
    } else if key.on_delete == "n" && key.columns.len() == 1 && owned.is_some() {
        let ty = column_type(tables, &key.child, &key.columns[0])?;
        let column = quote(&key.columns[0]);
        sqlx::query(&format!(
            "UPDATE {} t0 SET {column}=NULL WHERE ({}) AND t0.{column}=ANY(CAST($2::text[] AS {ty}[]))",
            quote(&key.child),
            owned.unwrap_or("FALSE")
        ))
        .bind(account.id)
        .bind(&absent)
        .execute(&mut **to)
        .await?;
    } else if let (ObjectKind::Repository, Some(owned)) = (account.kind, owned) {
        needed.dropped += sqlx::query(&format!(
            "DELETE FROM {} t0 WHERE ({owned}) AND {}=ANY($2::text[])",
            quote(&key.child),
            key_text("t0", &key.columns)
        ))
        .bind(account.id)
        .bind(&absent)
        .execute(&mut **to)
        .await?
        .rows_affected();
    } else {
        bail!(
            "{}.{} references {} rows that the target cell does not have",
            key.child,
            key.columns.join(","),
            key.parent
        );
    }
    Ok(())
}

/// The column list each stub keeps, as in `codeyard_db_core::cells::stubs`.
fn stub_insert(table: &str) -> &'static str {
    match table {
        "users" => "INSERT INTO users(id,username,display_name,bio,location,website,is_banned,must_change_password,two_factor_enabled,home_cell,is_stub,created_at,updated_at) \
            SELECT s.id,s.username,s.display_name,s.bio,s.location,s.website,s.is_banned,s.must_change_password,s.two_factor_enabled,s.home_cell,TRUE,s.created_at,s.updated_at \
            FROM jsonb_populate_recordset(NULL::users,$1::jsonb) s ON CONFLICT(id) DO NOTHING",
        "organizations" => "INSERT INTO organizations(id,name,display_name,description,is_verified,home_cell,is_stub,created_at,updated_at) \
            SELECT s.id,s.name,s.display_name,s.description,s.is_verified,s.home_cell,TRUE,s.created_at,s.updated_at \
            FROM jsonb_populate_recordset(NULL::organizations,$1::jsonb) s ON CONFLICT(id) DO NOTHING",
        _ => "INSERT INTO repositories(id,owner_kind,owner_user_id,owner_org_id,name,description,website,is_private,is_archived,is_fork,is_gist,is_template,is_internal,default_branch,home_cell,stars_count,forks_count,watchers_count,open_issues_count,allow_merge_commit,allow_squash_merge,allow_rebase_merge,created_at,updated_at,is_stub,disk_path,forked_from_id) \
            SELECT s.id,s.owner_kind,s.owner_user_id,s.owner_org_id,s.name,s.description,s.website,s.is_private,s.is_archived,s.is_fork,s.is_gist,s.is_template,s.is_internal,s.default_branch,s.home_cell,s.stars_count,s.forks_count,s.watchers_count,s.open_issues_count,s.allow_merge_commit,s.allow_squash_merge,s.allow_rebase_merge,s.created_at,s.updated_at,TRUE,'',(SELECT r.id FROM repositories r WHERE r.id=s.forked_from_id) \
            FROM jsonb_populate_recordset(NULL::repositories,$1::jsonb) s ON CONFLICT(id) DO NOTHING",
    }
}

/// Adds stubs for the accounts and repositories the copied rows reference, repositories
/// first since their owners need stubs too.
async fn add_stubs(from: &mut Tx, to: &mut Tx, target: i32, needed: &mut Needed) -> Result<u64> {
    let mut added = 0;
    for table in ["repositories", "users", "organizations"] {
        let ids: Vec<i64> = needed
            .accounts
            .remove(table)
            .unwrap_or_default()
            .into_iter()
            .collect();
        for ids in ids.chunks(1000) {
            // Owners added for repository stubs may already be in the target.
            let ids: Vec<i64> = sqlx::query_scalar(&format!(
                "SELECT x FROM unnest($1::bigint[]) x WHERE NOT EXISTS(SELECT 1 FROM {table} t WHERE t.id=x)"
            ))
            .bind(ids)
            .fetch_all(&mut **to)
            .await?;
            if ids.is_empty() {
                continue;
            }
            let ids = ids.as_slice();
            let local: i64 = sqlx::query_scalar(&format!(
                "SELECT count(*) FROM {table} WHERE id=ANY($1) AND home_cell=$2"
            ))
            .bind(ids)
            .bind(target)
            .fetch_one(&mut **from)
            .await?;
            ensure!(
                local == 0,
                "Rows reference {table} of the target cell that the target does not have"
            );
            let rows: String = sqlx::query_scalar(&format!(
                "SELECT COALESCE(json_agg(to_jsonb(t)),'[]')::text FROM {table} t WHERE id=ANY($1)"
            ))
            .bind(ids)
            .fetch_one(&mut **from)
            .await?;
            added += sqlx::query(stub_insert(table))
                .bind(&rows)
                .execute(&mut **to)
                .await?
                .rows_affected();
            if table == "repositories" {
                let owners: Vec<(Option<i64>, Option<i64>)> = sqlx::query_as(
                    "SELECT owner_user_id,owner_org_id FROM repositories WHERE id=ANY($1)",
                )
                .bind(ids)
                .fetch_all(&mut **from)
                .await?;
                for (user, organization) in owners {
                    if let Some(user) = user {
                        needed.accounts.entry("users").or_default().insert(user);
                    }
                    if let Some(organization) = organization {
                        needed
                            .accounts
                            .entry("organizations")
                            .or_default()
                            .insert(organization);
                    }
                }
            }
        }
    }
    Ok(added)
}

/// Refuses an active destination before the CLI touches its Git directories. The caller
/// holds the destination cell's move lease until it finishes; `copy` repeats this check
/// under row locks before loading data.
pub async fn check_target(target: &PgPool, account: Account, source_cell: i32) -> Result<()> {
    let allowed: bool = sqlx::query_scalar(&format!(
        "SELECT NOT EXISTS(SELECT 1 FROM {table} WHERE id=$2 AND NOT is_stub) \
         AND NOT EXISTS(SELECT 1 FROM account_moves WHERE account_kind=$1 AND account_id=$2) \
         OR EXISTS(SELECT 1 FROM account_moves WHERE account_kind=$1 AND account_id=$2 AND state='receiving' AND target_cell=$3)",
        table = account.table()
    ))
    .bind(account.kind.as_str())
    .bind(account.id)
    .bind(source_cell)
    .fetch_one(target)
    .await?;
    ensure!(
        allowed,
        "The target account is already active or belongs to another move"
    );
    Ok(())
}

/// The storage node that holds an earlier run's copy of these repositories in this cell.
pub async fn placed_node(pool: &PgPool, repositories: &[i64]) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT storage_node_id FROM repositories WHERE id=ANY($1) AND NOT is_stub AND storage_node_id IS NOT NULL ORDER BY id LIMIT 1",
    )
    .bind(repositories)
    .fetch_optional(pool)
    .await?)
}

/// Gives the account's copied repositories to a storage node of a target cell that writes
/// through a quorum. Each starts empty there and is published through the quorum, which
/// requires write version zero. A repeated run may place them again on the same node.
pub async fn place_repositories(
    target: &PgPool,
    account: Account,
    source_cell: i32,
    node: i64,
) -> Result<()> {
    let mut tx = target.begin().await?;
    sqlx::query("SET LOCAL codeyard.move = 'load'")
        .execute(&mut *tx)
        .await?;
    let receiving: Option<i32> = sqlx::query_scalar(
        "SELECT target_cell FROM account_moves WHERE account_kind=$1 AND account_id=$2 AND state='receiving' FOR UPDATE",
    )
    .bind(account.kind.as_str())
    .bind(account.id)
    .fetch_optional(&mut *tx)
    .await?;
    ensure!(
        receiving == Some(source_cell),
        "The target cell is not receiving the account"
    );
    sqlx::query(&format!(
        "UPDATE repositories SET storage_node_id=$2,write_version=0 WHERE NOT is_stub AND storage_node_id IS DISTINCT FROM $2 AND {}",
        account.repositories()
    ))
    .bind(account.id)
    .bind(node)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Whether this cell is receiving `repository` in a move that has not activated yet: the
/// target storage node may then publish the copied refs, and nothing else may.
pub async fn receiving(pool: &PgPool, repository: i64) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM account_moves m JOIN repositories r ON r.id=$1 AND NOT r.is_stub
        WHERE m.state='receiving' AND ((m.account_kind='repository' AND m.account_id=r.id)
           OR (m.account_kind=CASE WHEN r.owner_kind='org' THEN 'organization' ELSE 'user' END
               AND m.account_id=CASE WHEN r.owner_kind='org' THEN r.owner_org_id ELSE r.owner_user_id END)))",
    )
    .bind(repository)
    .fetch_one(pool)
    .await?)
}

/// Loads the account into the target cell in one transaction while the source is frozen:
/// its own rows (the account and repository rows over any stubs), stubs for the accounts
/// and repositories they reference, and the shared rows they need. `paths` gives each
/// repository's Git directory in the target. Queued CI is cancelled, counters are counted
/// again, and search is queued. A copy from an earlier run that never switched is replaced.
pub async fn copy(
    source: &PgPool,
    target: &PgPool,
    account: Account,
    target_cell: i32,
    paths: &BTreeMap<i64, String>,
    transfer: Option<&Transfer>,
) -> Result<Copied> {
    ensure!(
        (account.kind == ObjectKind::Repository) == transfer.is_some(),
        "A repository moves only to a new owner"
    );
    // Retain the move row independently of the read snapshot. Cancelling or renewing the
    // freeze must not race a copy made under the previous freeze. Writers may still take
    // KEY SHARE and see its expiry; the final deadline check rejects an expired snapshot.
    let mut control = source.begin().await?;
    set_lock_timeout(&mut control, Duration::from_secs(15)).await?;
    let current: Option<String> = sqlx::query_scalar(
        "SELECT state FROM account_moves WHERE account_kind=$1 AND account_id=$2 FOR SHARE",
    )
    .bind(account.kind.as_str())
    .bind(account.id)
    .fetch_optional(&mut *control)
    .await?;
    ensure!(
        current.as_deref() == Some("frozen"),
        "The account is not frozen"
    );
    let mut from = source.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *from)
        .await?;
    let source_cell = cell_id(&mut from).await?;
    let frozen: Option<(String, bool)> = sqlx::query_as(
        "SELECT state,expires_at>clock_timestamp()+make_interval(secs=>$3) FROM account_moves WHERE account_kind=$1 AND account_id=$2",
    )
    .bind(account.kind.as_str())
    .bind(account.id)
    .bind(FREEZE_MARGIN_SECONDS as f64)
    .fetch_optional(&mut *from)
    .await?;
    ensure!(
        matches!(&frozen, Some((state, true)) if state == "frozen"),
        "The account is not frozen; run the move again"
    );
    let mut to = target.begin().await?;
    for statement in [
        "SET CONSTRAINTS ALL DEFERRED",
        "SET LOCAL codeyard.move = 'load'",
        "SET LOCAL lock_timeout = '15s'",
    ] {
        sqlx::query(statement).execute(&mut *to).await?;
    }
    ensure!(
        cell_id(&mut to).await? == target_cell,
        "The target database belongs to another cell"
    );
    let mut source_catalog = pg::catalog(&mut from).await?;
    let mut target_catalog = pg::catalog(&mut to).await?;
    source_catalog.tables.sort_by(|a, b| a.name.cmp(&b.name));
    target_catalog.tables.sort_by(|a, b| a.name.cmp(&b.name));
    ensure!(
        source_catalog.migrations == target_catalog.migrations
            && shape(&source_catalog.tables) == shape(&target_catalog.tables),
        "The two cells run different schema versions"
    );
    let tables = &target_catalog.tables;
    let earlier: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS(SELECT 1 FROM {} WHERE id=$1 AND NOT is_stub)",
        account.table()
    ))
    .bind(account.id)
    .fetch_one(&mut *to)
    .await?;
    // A committed copy is not a second writable home. Its guard survives process loss
    // and has no expiry; only a switched source can activate it. A retry may replace only
    // our own receiving copy, never an already active account in the destination.
    let receiving: Option<(String, i32)> = sqlx::query_as(
        "SELECT state,target_cell FROM account_moves WHERE account_kind=$1 AND account_id=$2 FOR UPDATE",
    )
    .bind(account.kind.as_str())
    .bind(account.id)
    .fetch_optional(&mut *to)
    .await?;
    if let Some((state, from_cell)) = receiving {
        ensure!(
            state == "receiving" && from_cell == source_cell,
            "Another move owns the target account"
        );
    } else {
        ensure!(!earlier, "The target account is already active");
        sqlx::query("INSERT INTO account_moves(account_kind,account_id,target_cell,state,expires_at) VALUES($1,$2,$3,'receiving',clock_timestamp())")
            .bind(account.kind.as_str())
            .bind(account.id)
            .bind(source_cell)
            .execute(&mut *to)
            .await?;
    }
    if earlier {
        clear(
            &mut to,
            tables,
            account,
            source_cell,
            transfer.map(|transfer| transfer.former),
        )
        .await?;
    }

    let mut copied = Copied::default();
    let mut order: Vec<(&Table, &Table)> = source_catalog.tables.iter().zip(tables).collect();
    order.sort_by_key(|(table, _)| match table.name.as_str() {
        name if name == account.table() => 0,
        "repositories" => 1,
        _ => 2,
    });
    for (from_table, to_table) in order {
        let Some(owned) = ownership::owned_rows(&from_table.name, account.kind) else {
            continue;
        };
        let read = format!("{} t0 WHERE {owned}", pg::read_sql(from_table));
        let insert = if is_root(&to_table.name) {
            let mut fixed = vec![("home_cell", target_cell.to_string())];
            if to_table.name == "repositories" {
                fixed.push(("storage_node_id", "NULL".to_owned()));
                if let Some(transfer) = transfer {
                    let [kind, user, organization] = transfer.owner.owner_values();
                    fixed.push(("owner_kind", kind));
                    fixed.push(("owner_user_id", user));
                    fixed.push(("owner_org_id", organization));
                }
            }
            let updates = to_table
                .columns
                .iter()
                .filter(|column| column.name != "id")
                .map(|column| format!("{0}=EXCLUDED.{0}", quote(&column.name)))
                .collect::<Vec<_>>()
                .join(",");
            let fixed: Vec<(&str, &str)> = fixed.iter().map(|(n, v)| (*n, v.as_str())).collect();
            pg::insert_sql_with(
                to_table,
                &fixed,
                &format!(
                    " ON CONFLICT(id) DO UPDATE SET {updates} WHERE {}.is_stub",
                    quote(&to_table.name)
                ),
            )
        } else {
            pg::insert_sql_with(to_table, &[], "")
        };
        let (read_count, written) =
            copy_rows(&mut from, &mut to, &read, Binding::Id(account.id), &insert).await?;
        ensure!(
            read_count == written,
            "{}: the target took {written} of {read_count} rows",
            to_table.name
        );
        copied.rows += written;
    }
    let repositories: Vec<i64> = sqlx::query_scalar(&format!(
        "SELECT id FROM repositories WHERE NOT is_stub AND {} ORDER BY id",
        account.repositories()
    ))
    .bind(account.id)
    .fetch_all(&mut *to)
    .await?;
    ensure!(
        repositories.len() == paths.len() && repositories.iter().all(|id| paths.contains_key(id)),
        "The repository copies do not match the account's repositories"
    );
    for (id, path) in paths {
        sqlx::query("UPDATE repositories SET disk_path=$2 WHERE id=$1")
            .bind(id)
            .bind(path)
            .execute(&mut *to)
            .await?;
    }

    // References that leave the account.
    let keys = foreign_keys(&mut to).await?;
    let mut needed = Needed::default();
    for key in &keys {
        let Some(owned) = ownership::owned_rows(&key.child, account.kind) else {
            continue;
        };
        // The copy clears the storage node; the source's node is not a target row.
        if key.child == "repositories" && key.columns == ["storage_node_id"] {
            continue;
        }
        let columns = column_list("t0", &key.columns);
        let present = key
            .columns
            .iter()
            .map(|column| format!("t0.{} IS NOT NULL", quote(column)))
            .collect::<Vec<_>>()
            .join(" AND ");
        let outside = match ownership::owned_rows(&key.parent, account.kind) {
            Some(parent) => format!(
                " AND ({columns}) NOT IN (SELECT {} FROM {} t0 WHERE {parent})",
                column_list("t0", &key.parent_columns),
                quote(&key.parent)
            ),
            None => String::new(),
        };
        let found: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT DISTINCT {} FROM {} t0 WHERE ({owned}) AND {present}{outside} LIMIT {}",
            key_text("t0", &key.columns),
            quote(&key.child),
            MAX_REFERENCED + 1
        ))
        .bind(account.id)
        .fetch_all(&mut *from)
        .await?;
        place(
            &mut to,
            tables,
            key,
            found,
            Some(owned.as_str()),
            account,
            &mut needed,
        )
        .await?;
    }
    // Shared rows, and what they reference in turn.
    let mut rounds = 0;
    while !needed.shared.is_empty() {
        rounds += 1;
        ensure!(rounds <= 10, "Shared rows reference each other too deeply");
        for (table, column, values) in std::mem::take(&mut needed.shared) {
            let source_table = source_catalog
                .tables
                .iter()
                .find(|candidate| candidate.name == table)
                .ok_or_else(|| anyhow::anyhow!("Unknown table {table}"))?;
            let to_table = tables
                .iter()
                .find(|candidate| candidate.name == table)
                .ok_or_else(|| anyhow::anyhow!("Unknown table {table}"))?;
            let ty = column_type(tables, &table, &column)?;
            let selection = format!("t0.{}=ANY(CAST($1::text[] AS {ty}[]))", quote(&column));
            let read = format!("{} t0 WHERE {selection}", pg::read_sql(source_table));
            let insert = pg::insert_sql_with(to_table, &[], " ON CONFLICT DO NOTHING");
            let (_, written) =
                copy_rows(&mut from, &mut to, &read, Binding::Keys(&values), &insert).await?;
            copied.shared += written;
            for key in keys.iter().filter(|key| key.child == table) {
                let present = key
                    .columns
                    .iter()
                    .map(|column| format!("t0.{} IS NOT NULL", quote(column)))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                let found: Vec<String> = sqlx::query_scalar(&format!(
                    "SELECT DISTINCT {} FROM {} t0 WHERE {selection} AND {present} LIMIT {}",
                    key_text("t0", &key.columns),
                    quote(&table),
                    MAX_REFERENCED + 1
                ))
                .bind(&values)
                .fetch_all(&mut *from)
                .await?;
                place(&mut to, tables, key, found, None, account, &mut needed).await?;
            }
        }
    }
    copied.stubs = add_stubs(&mut from, &mut to, target_cell, &mut needed).await?;
    copied.dropped = needed.dropped;
    if let Some(transfer) = transfer {
        let personal = (transfer.owner.kind == ObjectKind::User).then_some(transfer.owner.id);
        codeyard_db_core::queries::postgres::repositories::records::leave_former_owner(
            &mut to, account.id, personal,
        )
        .await?;
        // Runners registered for the repository answered to the former owner and cell.
        codeyard_db_core::queries::postgres::runners::remove_repository_runners(
            &mut to, account.id,
        )
        .await?;
    }

    // Running CI was drained before the freeze. Never claim that an execution stopped
    // merely because its copied row says so; its source worker might still be producing bytes.
    let running: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM ci_runs WHERE repo_id=ANY($1) AND status='running')",
    )
    .bind(&repositories)
    .fetch_one(&mut *to)
    .await?;
    ensure!(!running, "Running CI must drain before its rows can move");
    sqlx::query("UPDATE ci_runs SET status='cancelled',error_message='Cancelled because the account moved to another cell',completed_at=COALESCE(completed_at,clock_timestamp()),worker=NULL,heartbeat_at=NULL WHERE repo_id=ANY($1) AND status='queued'")
        .bind(&repositories)
        .execute(&mut *to)
        .await?;
    sqlx::query("UPDATE actions_jobs SET status='completed',conclusion='cancelled',completed_at=COALESCE(completed_at,clock_timestamp()) WHERE status<>'completed' AND run_id IN (SELECT id FROM ci_runs WHERE repo_id=ANY($1))")
        .bind(&repositories)
        .execute(&mut *to)
        .await?;
    // Counters travel as values; count them again here and keep the counted value.
    for (counter, rows) in [
        ("stars_count", "SELECT count(*) FROM stars s WHERE s.repo_id=r.id"),
        (
            "watchers_count",
            "SELECT count(*) FROM watches w WHERE w.repo_id=r.id AND w.level IN ('watching','custom')",
        ),
    ] {
        copied.counters_fixed += sqlx::query(&format!(
            "UPDATE repositories r SET {counter}=({rows}) WHERE r.id=ANY($1) AND r.{counter}<>({rows})"
        ))
        .bind(&repositories)
        .execute(&mut *to)
        .await?
        .rows_affected();
    }
    // The search index learns the new home from the target's events.
    sqlx::query("SELECT queue_search(r.id,'repository',NULL) FROM repositories r WHERE r.id=ANY($1) AND NOT r.is_gist AND COALESCE((SELECT search_index FROM node_configuration WHERE id=1),FALSE)")
        .bind(&repositories)
        .execute(&mut *to)
        .await?;
    for (table, item) in [("issues", "issue"), ("pull_requests", "pull")] {
        sqlx::query(&format!("SELECT queue_search(i.repo_id,'{item}',i.id) FROM {table} i WHERE i.repo_id=ANY($1) AND COALESCE((SELECT search_index FROM node_configuration WHERE id=1),FALSE)"))
            .bind(&repositories)
            .execute(&mut *to)
            .await?;
    }
    from.commit().await?;
    // The copy is only good while no write reached the source since the snapshot.
    let still: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM account_moves WHERE account_kind=$1 AND account_id=$2 AND state='frozen' AND expires_at>clock_timestamp()+make_interval(secs=>$3))")
        .bind(account.kind.as_str())
        .bind(account.id)
        .bind(FREEZE_MARGIN_SECONDS as f64)
        .fetch_one(source)
        .await?;
    ensure!(
        still,
        "The freeze ran out during the copy; run the move again with a longer freeze"
    );
    to.commit().await?;
    control.commit().await?;
    Ok(copied)
}

/// Enables the copied account only after the source durably refuses further writes.
/// Repeating activation after a lost reply is safe; an active target is never recopied.
pub async fn activate(source: &PgPool, target: &PgPool, account: Account) -> Result<()> {
    let mut from = source.begin().await?;
    set_lock_timeout(&mut from, Duration::from_secs(15)).await?;
    let switched: Option<(String, i32)> = sqlx::query_as(
        "SELECT state,target_cell FROM account_moves WHERE account_kind=$1 AND account_id=$2 FOR SHARE",
    )
    .bind(account.kind.as_str())
    .bind(account.id)
    .fetch_optional(&mut *from)
    .await?;
    let mut to = target.begin().await?;
    set_lock_timeout(&mut to, Duration::from_secs(15)).await?;
    let target_cell = cell_id(&mut to).await?;
    ensure!(
        matches!(switched, Some((ref state, cell)) if state == "switched" && cell == target_cell),
        "The source has not switched to this cell"
    );
    let source_cell = cell_id(&mut from).await?;
    let receiving: Option<(String, i32)> = sqlx::query_as(
        "SELECT state,target_cell FROM account_moves WHERE account_kind=$1 AND account_id=$2 FOR UPDATE",
    )
    .bind(account.kind.as_str())
    .bind(account.id)
    .fetch_optional(&mut *to)
    .await?;
    ensure!(
        receiving
            .as_ref()
            .is_none_or(|(state, cell)| state == "receiving" && *cell == source_cell),
        "Another move owns the target account"
    );
    let native: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS(SELECT 1 FROM {} WHERE id=$1 AND NOT is_stub AND home_cell=$2)",
        account.table()
    ))
    .bind(account.id)
    .bind(target_cell)
    .fetch_one(&mut *to)
    .await?;
    ensure!(native, "The target copy is missing");
    sqlx::query("DELETE FROM account_moves WHERE account_kind=$1 AND account_id=$2")
        .bind(account.kind.as_str())
        .bind(account.id)
        .execute(&mut *to)
        .await?;
    to.commit().await?;
    from.commit().await?;
    Ok(())
}

/// Marks the move switched: the source refuses the account's writes from now on, until
/// `release` turns its rows into stubs. Fails when the freeze ran out.
pub async fn switch(pool: &PgPool, account: Account) -> Result<()> {
    let changed = sqlx::query("UPDATE account_moves SET state='switched' WHERE account_kind=$1 AND account_id=$2 AND (state='switched' OR (state='frozen' AND expires_at>clock_timestamp()))")
        .bind(account.kind.as_str())
        .bind(account.id)
        .execute(pool)
        .await?
        .rows_affected();
    ensure!(
        changed == 1,
        "The freeze ran out before the switch; run the move again"
    );
    Ok(())
}

/// Ends a move that has not switched, so the account's writes resume here. Returns false
/// when there was nothing to cancel.
pub async fn cancel(pool: &PgPool, account: Account) -> Result<bool> {
    Ok(sqlx::query(
        "DELETE FROM account_moves WHERE account_kind=$1 AND account_id=$2 AND state IN ('pending','frozen')",
    )
    .bind(account.kind.as_str())
    .bind(account.id)
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

/// Removes a copy that never switched from the target, turning the account back into a
/// stub of `source_cell` there; a repository gets its `former` owner back. Call only after
/// `cancel` succeeded.
pub async fn discard(
    target: &PgPool,
    account: Account,
    source_cell: i32,
    former: Option<Account>,
) -> Result<()> {
    let mut tx = target.begin().await?;
    for statement in [
        "SET CONSTRAINTS ALL DEFERRED",
        "SET LOCAL codeyard.move = 'release'",
        "SET LOCAL lock_timeout = '15s'",
    ] {
        sqlx::query(statement).execute(&mut *tx).await?;
    }
    let copied: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS(SELECT 1 FROM {} WHERE id=$1 AND NOT is_stub)",
        account.table()
    ))
    .bind(account.id)
    .fetch_one(&mut *tx)
    .await?;
    let receiving: Option<(String, i32)> = sqlx::query_as(
        "SELECT state,target_cell FROM account_moves WHERE account_kind=$1 AND account_id=$2 FOR UPDATE",
    )
    .bind(account.kind.as_str())
    .bind(account.id)
    .fetch_optional(&mut *tx)
    .await?;
    ensure!(
        receiving
            .as_ref()
            .is_none_or(|(state, cell)| state == "receiving" && *cell == source_cell),
        "Another move owns the target account"
    );
    ensure!(
        !copied || receiving.is_some(),
        "An active target cannot be discarded"
    );
    if copied {
        let mut catalog = pg::catalog(&mut tx).await?;
        catalog.tables.sort_by(|a, b| a.name.cmp(&b.name));
        clear(&mut tx, &catalog.tables, account, source_cell, former).await?;
    }
    sqlx::query(
        "DELETE FROM account_moves WHERE account_kind=$1 AND account_id=$2 AND state='receiving'",
    )
    .bind(account.kind.as_str())
    .bind(account.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// After the directory sends the account to the target, deletes its rows here and keeps
/// the account and its repositories as stubs of the target. Refuses while rows of other
/// accounts would be deleted with them. A transferred repository's stub belongs to its new
/// owner, and its former name redirects to it.
pub async fn release(pool: &PgPool, account: Account, transfer: Option<&Transfer>) -> Result<()> {
    let mut tx = pool.begin().await?;
    for statement in [
        "SET CONSTRAINTS ALL DEFERRED",
        "SET LOCAL codeyard.move = 'release'",
        "SET LOCAL lock_timeout = '15s'",
    ] {
        sqlx::query(statement).execute(&mut *tx).await?;
    }
    let current: Option<(String, i32)> = sqlx::query_as(
        "SELECT state,target_cell FROM account_moves WHERE account_kind=$1 AND account_id=$2 FOR UPDATE",
    )
    .bind(account.kind.as_str())
    .bind(account.id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((value, target)) = current else {
        let released: bool = sqlx::query_scalar(&format!(
            "SELECT EXISTS(SELECT 1 FROM {} WHERE id=$1 AND is_stub)",
            account.table()
        ))
        .bind(account.id)
        .fetch_one(&mut *tx)
        .await?;
        ensure!(released, "The move was not started");
        return Ok(());
    };
    if state(&value)? == State::Released {
        tx.commit().await?;
        return Ok(());
    }
    ensure!(
        state(&value)? == State::Switched,
        "The account has not switched to the target cell yet"
    );
    let mut catalog = pg::catalog(&mut tx).await?;
    catalog.tables.sort_by(|a, b| a.name.cmp(&b.name));
    if transfer.is_some() {
        // The former owner's inventory of the repository's artifacts stays behind, as in a
        // transfer within the cell.
        sqlx::query("DELETE FROM artifact_metadata_records WHERE artifact_id IN (SELECT id FROM artifact_metadata WHERE repo_id=$1)")
            .bind(account.id)
            .execute(&mut *tx)
            .await?;
    }
    for key in foreign_keys(&mut tx).await? {
        // Clearing a reference on delete is harmless; anything else would delete or block.
        if is_root(&key.parent) || matches!(key.on_delete.as_str(), "n" | "d") {
            continue;
        }
        let Some(parent) = ownership::owned_rows(&key.parent, account.kind) else {
            continue;
        };
        let child =
            ownership::owned_rows(&key.child, account.kind).unwrap_or_else(|| "FALSE".to_owned());
        let outside: bool = sqlx::query_scalar(&format!(
            "SELECT EXISTS(SELECT 1 FROM {} t0 WHERE ({}) IN (SELECT {} FROM {} t0 WHERE {parent}) AND NOT ({child}))",
            quote(&key.child),
            column_list("t0", &key.columns),
            column_list("t0", &key.parent_columns),
            quote(&key.parent)
        ))
        .bind(account.id)
        .fetch_one(&mut *tx)
        .await?;
        ensure!(
            !outside,
            "Rows of {} outside the account reference its {} rows; the move cannot release them",
            key.child,
            key.parent
        );
    }
    // Keep source paths in the same transaction that clears the rows. The CLI can
    // then resume filesystem cleanup after release, even though the stubs have no path.
    let paths: Vec<(i64, String, Option<i64>)> = sqlx::query_as(&format!(
        "SELECT id,disk_path,storage_node_id FROM repositories WHERE NOT is_stub AND {} ORDER BY id LIMIT {}",
        account.repositories(),
        MAX_REPOSITORIES + 1
    ))
    .bind(account.id)
    .fetch_all(&mut *tx)
    .await?;
    ensure!(
        paths.len() <= MAX_REPOSITORIES,
        "The account has too many repositories to move in one operation"
    );
    let paths: Vec<Repository> = paths
        .into_iter()
        .map(|(id, disk_path, storage_node_id)| Repository {
            id,
            disk_path,
            storage_node_id,
        })
        .collect();
    sqlx::query("UPDATE account_moves SET state='released',repository_paths=$3::jsonb WHERE account_kind=$1 AND account_id=$2")
        .bind(account.kind.as_str())
        .bind(account.id)
        .bind(serde_json::to_string(&paths)?)
        .execute(&mut *tx)
        .await?;
    if let Some(transfer) = transfer {
        sqlx::query(stub_insert(transfer.owner.table()))
            .bind(&transfer.owner_row)
            .execute(&mut *tx)
            .await?;
        // Packages stay in the former owner's namespace, as in a transfer within the cell.
        sqlx::query("UPDATE packages SET repo_id=NULL,visibility=CASE WHEN (SELECT is_private FROM repositories WHERE id=$1) THEN 'private' ELSE visibility END,updated_at=extract(epoch FROM clock_timestamp())::bigint WHERE repo_id=$1")
            .bind(account.id)
            .execute(&mut *tx)
            .await?;
        let [kind, ..] = transfer.former.owner_values();
        sqlx::query(&format!(
            "INSERT INTO repository_redirects(owner_kind,owner_id,name_key,repo_id) VALUES({kind},$1,lower($2),$3) \
             ON CONFLICT(owner_kind,owner_id,name_key) DO UPDATE SET repo_id=EXCLUDED.repo_id"
        ))
        .bind(transfer.former.id)
        .bind(&transfer.name)
        .bind(account.id)
        .execute(&mut *tx)
        .await?;
    }
    if account.kind == ObjectKind::User {
        // Requests that still bring these tokens here are answered by the new home.
        sqlx::query("INSERT INTO moved_credentials(token_hash) SELECT token_hash FROM personal_access_tokens WHERE user_id=$1 ON CONFLICT DO NOTHING")
            .bind(account.id)
            .execute(&mut *tx)
            .await?;
    }
    clear(
        &mut tx,
        &catalog.tables,
        account,
        target,
        transfer.map(|transfer| transfer.owner),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// The hashes of a moving user's live tokens. The directory maps each to the user, so
/// routers and other cells find a token's new home although its prefix names this cell.
pub async fn credentials(source: &PgPool, account: Account) -> Result<Vec<Key>> {
    if account.kind != ObjectKind::User {
        return Ok(Vec::new());
    }
    let hashes: Vec<String> = sqlx::query_scalar(
        "SELECT token_hash FROM personal_access_tokens WHERE user_id=$1 \
         AND (expires_at IS NULL OR expires_at>clock_timestamp()) ORDER BY id LIMIT $2",
    )
    .bind(account.id)
    .bind(MAX_CREDENTIALS as i64 + 1)
    .fetch_all(source)
    .await?;
    ensure!(
        hashes.len() <= MAX_CREDENTIALS,
        "The account has more than {MAX_CREDENTIALS} live tokens"
    );
    hashes
        .iter()
        .map(|hash| Key::new(KeyKind::Credential, hash))
        .collect()
}

/// One stored object that the account's rows name.
#[derive(sqlx::FromRow, Debug)]
pub struct StoredObject {
    pub object_key: String,
    pub bytes: i64,
    /// The SHA-256 the row recorded, when it records one.
    pub sha256: Option<String>,
    pub content_type: String,
}

/// The account's stored objects as one query that binds the account as `$1`: avatars,
/// release assets, Actions artifact and cache parts, CI log chunks, CodeQL databases and,
/// unless `unique_only`, package blobs. Package blob keys come from their digests, so
/// another account's package may name the same bytes.
fn stored_objects(kind: ObjectKind, unique_only: bool) -> String {
    let owned =
        |table: &str| ownership::owned_rows(table, kind).unwrap_or_else(|| "FALSE".to_owned());
    let file = |table: &str, column: &str| {
        format!(
            "SELECT o.object_key,o.file_size AS bytes,o.sha256_hex AS sha256,o.content_type FROM {table} t0 \
             JOIN files o ON o.id=t0.{column} AND o.is_active WHERE {}",
            owned(table)
        )
    };
    let mut parts = vec![
        file("users", "avatar_file_id"),
        file("organizations", "avatar_file_id"),
        file("release_assets", "file_id"),
        format!(
            "SELECT t0.object_key,t0.size,NULL,'application/octet-stream' FROM actions_blob_parts t0 WHERE t0.ready AND {}",
            owned("actions_blob_parts")
        ),
        format!(
            "SELECT t0.object_key,t0.object_bytes,t0.object_digest,'application/octet-stream' FROM ci_log_chunks t0 WHERE {}",
            owned("ci_log_chunks")
        ),
        format!(
            "SELECT t0.object_key,t0.size,NULL,'application/zip' FROM codeql_databases t0 WHERE {}",
            owned("codeql_databases")
        ),
    ];
    if !unique_only {
        for table in [
            "package_version_blobs",
            "package_files",
            "package_blob_grants",
        ] {
            parts.push(format!(
                "SELECT 'packages/sha256/'||regexp_replace(o.digest,'^sha256:',''),o.size,regexp_replace(o.digest,'^sha256:',''),'application/octet-stream' \
                 FROM {table} t0 JOIN package_blobs o ON o.digest=t0.digest AND o.ready AND NOT o.deleting WHERE {}",
                owned(table)
            ));
        }
    }
    parts.join(" UNION ")
}

/// A page of the account's stored objects with keys after `after`, in key order.
pub async fn objects(
    pool: &PgPool,
    account: Account,
    after: &str,
    limit: i64,
) -> Result<Vec<StoredObject>> {
    Ok(sqlx::query_as(&format!(
        "SELECT object_key,bytes,sha256,content_type FROM ({}) o WHERE object_key>$2 ORDER BY object_key LIMIT $3",
        stored_objects(account.kind, false)
    ))
    .bind(account.id)
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// After the account's bytes were copied to another region's store and the move switched,
/// queues its uniquely named objects here for deletion after the restore window, as a
/// deletion would. Package blobs stay for the package collector, which keeps the ones that
/// packages left in this cell still use. Repeating it is safe.
pub async fn retire_objects(pool: &PgPool, account: Account) -> Result<u64> {
    let mut tx = pool.begin().await?;
    let current: Option<String> = sqlx::query_scalar(
        "SELECT state FROM account_moves WHERE account_kind=$1 AND account_id=$2 FOR SHARE",
    )
    .bind(account.kind.as_str())
    .bind(account.id)
    .fetch_optional(&mut *tx)
    .await?;
    ensure!(
        current.as_deref() == Some("switched"),
        "Stored objects are retired only between the switch and the release"
    );
    let queued = sqlx::query(&format!(
        "INSERT INTO pending_object_deletions(object_key,delete_after) \
         SELECT object_key,clock_timestamp()+make_interval(secs=>$2) FROM ({}) o ON CONFLICT(object_key) DO NOTHING",
        stored_objects(account.kind, true)
    ))
    .bind(account.id)
    .bind(codeyard_db_core::queries::files::RESTORE_WINDOW_SECONDS as f64)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(queued)
}

/// The CLI calls this only after deleting every retained source Git directory. Keep
/// the release record on any cleanup failure, so another invocation can finish it.
pub async fn finish(pool: &PgPool, account: Account) -> Result<()> {
    sqlx::query(
        "DELETE FROM account_moves WHERE account_kind=$1 AND account_id=$2 AND state='released'",
    )
    .bind(account.kind.as_str())
    .bind(account.id)
    .execute(pool)
    .await?;
    Ok(())
}

/// How many parent rows a table's rows hang below; children are deleted first.
fn depth(table: &str) -> usize {
    match ownership::rule(table) {
        Some(Rule::Owned(references)) => references
            .iter()
            .map(|reference| match reference {
                Reference::Parent(_, parent) if *parent != table => 1 + depth(parent),
                _ => 0,
            })
            .max()
            .unwrap_or(0),
        _ => 0,
    }
}

/// Deletes the account's rows and turns the account and its repositories into stubs whose
/// home is `home`; a moving repository's stub belongs to `owner`. The caller set
/// `codeyard.move`, so no side-effect trigger runs and no stored bytes are deleted.
async fn clear(
    tx: &mut Tx,
    tables: &[Table],
    account: Account,
    home: i32,
    owner: Option<Account>,
) -> Result<()> {
    let mut owned: Vec<(usize, &str, String)> = tables
        .iter()
        .filter(|table| !is_root(&table.name))
        .filter_map(|table| {
            ownership::owned_rows(&table.name, account.kind)
                .map(|condition| (depth(&table.name), table.name.as_str(), condition))
        })
        .collect();
    owned.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    for (_, table, condition) in owned {
        sqlx::query(&format!(
            "DELETE FROM {} t0 WHERE {condition}",
            quote(table)
        ))
        .bind(account.id)
        .execute(&mut **tx)
        .await?;
    }
    // Cluster rows are not owned rows. Without them the storage nodes no longer need
    // the stubs' Git copies and retire them, as after a deletion.
    for table in [
        "repository_writes",
        "repository_voters",
        "repository_write_control",
        "repository_committed_refs",
        "repository_replicas",
        "repository_moves",
        "node_drain_repositories",
    ] {
        sqlx::query(&format!(
            "DELETE FROM {table} WHERE repository_id IN (SELECT id FROM repositories WHERE NOT is_stub AND {})",
            account.repositories()
        ))
        .bind(account.id)
        .execute(&mut **tx)
        .await?;
    }
    let owner = owner
        .map(|owner| {
            let [kind, user, organization] = owner.owner_values();
            format!(",owner_kind={kind},owner_user_id={user},owner_org_id={organization}")
        })
        .unwrap_or_default();
    sqlx::query(&format!(
        "UPDATE repositories SET is_stub=TRUE,home_cell=$2,disk_path='',storage_node_id=NULL,size_bytes=0{owner} WHERE NOT is_stub AND {}",
        account.repositories()
    ))
    .bind(account.id)
    .bind(home)
    .execute(&mut **tx)
    .await?;
    let root = match account.kind {
        ObjectKind::Organization => "UPDATE organizations SET is_stub=TRUE,home_cell=$2,billing_email=NULL,avatar_file_id=NULL WHERE id=$1",
        ObjectKind::Repository => return Ok(()),
        ObjectKind::User => "UPDATE users SET is_stub=TRUE,home_cell=$2,password_hash=NULL,email=NULL,avatar_file_id=NULL,is_admin=FALSE,email_verified=FALSE WHERE id=$1",
    };
    sqlx::query(root)
        .bind(account.id)
        .bind(home)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
