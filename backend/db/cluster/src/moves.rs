//! Moving a user or organization, with its repositories, to another cell. Both cells run
//! PostgreSQL. The steps are `begin`, `freeze`, `copy`, `switch` and `release`; the directory
//! switch and the Git copy run between them (see `codeyard cell move`). Each step can run
//! again after a crash, and a move that has not switched can be cancelled.
use crate::copy::postgres as pg;
use crate::copy::{quote, shape, Table, CHUNK_BYTES};
use anyhow::{bail, ensure, Result};
use codeyard_db_core::cells::ownership::{self, Reference, Rule};
use codeyard_model::cells::ObjectKind;
use futures_util::TryStreamExt;
use sqlx::{PgPool, Postgres, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

type Tx = Transaction<'static, Postgres>;

/// The most distinct rows one reference may need from outside the account.
const MAX_REFERENCED: usize = 100_000;
/// How long the freeze must still last when the copy commits.
const FREEZE_MARGIN_SECONDS: i64 = 30;

#[derive(Clone, Copy, Debug)]
pub struct Account {
    kind: ObjectKind,
    id: i64,
}

impl Account {
    pub fn new(kind: ObjectKind, id: i64) -> Result<Self> {
        ensure!(
            matches!(kind, ObjectKind::User | ObjectKind::Organization),
            "Only users and organizations move between cells"
        );
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
            _ => "users",
        }
    }
    fn owner_column(self) -> &'static str {
        match self.kind {
            ObjectKind::Organization => "owner_org_id",
            _ => "owner_user_id",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Pending,
    Frozen,
    Switched,
}

fn state(value: &str) -> Result<State> {
    Ok(match value {
        "pending" => State::Pending,
        "frozen" => State::Frozen,
        "switched" => State::Switched,
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
            "UPDATE ci_runs SET cancel_requested=TRUE WHERE status IN ('queued','running') AND repo_id IN (SELECT id FROM repositories WHERE NOT is_stub AND {}=$1)",
            account.owner_column()
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
        "SELECT count(*) FROM ci_runs WHERE status='running' AND repo_id IN (SELECT id FROM repositories WHERE NOT is_stub AND {}=$1)",
        account.owner_column()
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
        Some(State::Switched) => return Ok(tx.commit().await?),
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
        "SELECT count(*) FROM cell_outbox WHERE delivered_at IS NULL AND ((root_kind=$1 AND root_id=$2) OR (root_kind='repository' AND root_id IN (SELECT id FROM repositories WHERE NOT is_stub AND {}=$2)))",
        account.owner_column()
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

pub struct Repository {
    pub id: i64,
    pub disk_path: String,
}

/// The account's repositories in this cell. Repositories placed on cluster storage nodes
/// cannot move yet.
pub async fn repositories(pool: &PgPool, account: Account) -> Result<Vec<Repository>> {
    let rows: Vec<(i64, String, bool)> = sqlx::query_as(&format!(
        "SELECT id,disk_path,storage_node_id IS NOT NULL FROM repositories WHERE NOT is_stub AND {}=$1 ORDER BY id",
        account.owner_column()
    ))
    .bind(account.id)
    .fetch_all(pool)
    .await?;
    ensure!(
        rows.iter().all(|(_, _, placed)| !placed),
        "The account has repositories on cluster storage nodes; accounts with placed repositories cannot move yet"
    );
    Ok(rows
        .into_iter()
        .map(|(id, disk_path, _)| Repository { id, disk_path })
        .collect())
}

#[derive(Debug, Default)]
pub struct Copied {
    pub rows: u64,
    pub stubs: u64,
    pub shared: u64,
    pub counters_fixed: u64,
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
async fn transfer(
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
}

/// Sorts the keys one reference reaches outside the account: account and repository rows
/// become stubs, shared rows are copied, and anything else must already be in the target.
/// A reference that may be cleared on delete is cleared instead; others fail the move.
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
) -> Result<Copied> {
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
        clear(&mut to, tables, account, source_cell).await?;
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
            transfer(&mut from, &mut to, &read, Binding::Id(account.id), &insert).await?;
        ensure!(
            read_count == written,
            "{}: the target took {written} of {read_count} rows",
            to_table.name
        );
        copied.rows += written;
    }
    let repositories: Vec<i64> = sqlx::query_scalar(&format!(
        "SELECT id FROM repositories WHERE NOT is_stub AND {}=$1 ORDER BY id",
        account.owner_column()
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
                transfer(&mut from, &mut to, &read, Binding::Keys(&values), &insert).await?;
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
/// stub of `source_cell` there. Call only after `cancel` succeeded.
pub async fn discard(target: &PgPool, account: Account, source_cell: i32) -> Result<()> {
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
        clear(&mut tx, &catalog.tables, account, source_cell).await?;
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
/// accounts would be deleted with them.
pub async fn release(pool: &PgPool, account: Account) -> Result<()> {
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
    ensure!(
        state(&value)? == State::Switched,
        "The account has not switched to the target cell yet"
    );
    let mut catalog = pg::catalog(&mut tx).await?;
    catalog.tables.sort_by(|a, b| a.name.cmp(&b.name));
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
    clear(&mut tx, &catalog.tables, account, target).await?;
    sqlx::query("DELETE FROM account_moves WHERE account_kind=$1 AND account_id=$2")
        .bind(account.kind.as_str())
        .bind(account.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
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
/// home is `home`. The caller set `codeyard.move`, so no side-effect trigger runs and no
/// stored bytes are deleted.
async fn clear(tx: &mut Tx, tables: &[Table], account: Account, home: i32) -> Result<()> {
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
    sqlx::query(&format!(
        "UPDATE repositories SET is_stub=TRUE,home_cell=$2,disk_path='',storage_node_id=NULL,size_bytes=0 WHERE NOT is_stub AND {}=$1",
        account.owner_column()
    ))
    .bind(account.id)
    .bind(home)
    .execute(&mut **tx)
    .await?;
    let root = match account.kind {
        ObjectKind::Organization => "UPDATE organizations SET is_stub=TRUE,home_cell=$2,billing_email=NULL,avatar_file_id=NULL WHERE id=$1",
        _ => "UPDATE users SET is_stub=TRUE,home_cell=$2,password_hash=NULL,email=NULL,avatar_file_id=NULL,is_admin=FALSE,email_verified=FALSE WHERE id=$1",
    };
    sqlx::query(root)
        .bind(account.id)
        .bind(home)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
