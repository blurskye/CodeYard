use crate::repositories::traffic::fingerprints;
use crate::repositories::traffic::new_salt;
use crate::repositories::traffic::DayKey;
use crate::repositories::traffic::Identity;
use crate::repositories::traffic::Kind;
use crate::repositories::traffic::Metric;
use crate::repositories::traffic::PopularPath;
use crate::repositories::traffic::PopularReferrer;
use crate::repositories::traffic::Summary;
use crate::repositories::traffic::Window;
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn record(
    pool: &SqlitePool,
    repo: i64,
    identity: &Identity,
    metric: &Metric,
) -> Result<()> {
    let window = Window::current();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories WHERE id=?1)")
        .bind(repo)
        .fetch_one(&mut *tx)
        .await?;
    if !exists {
        return Ok(());
    }
    for day in [&window.today, &window.yesterday] {
        sqlx::query("INSERT INTO repository_traffic_keys(repo_id,day,salt) VALUES(?1,?2,?3) ON CONFLICT(repo_id,day) DO NOTHING").bind(repo).bind(day).bind(new_salt()).execute(&mut *tx).await?;
    }
    let keys:Vec<DayKey>=sqlx::query_as("SELECT day,salt FROM repository_traffic_keys WHERE repo_id=?1 AND day BETWEEN ?2 AND ?3 ORDER BY day").bind(repo).bind(&window.first).bind(&window.today).fetch_all(&mut *tx).await?;
    let fingerprints = fingerprints(&keys, identity)?;
    let stored:Option<String>=sqlx::query_scalar("SELECT visitor FROM repository_traffic_visitors WHERE repo_id=?1 AND (day,fingerprint) IN (SELECT json_extract(value,'$.day'),json_extract(value,'$.hash') FROM json_each(?2)) ORDER BY day DESC LIMIT 1").bind(repo).bind(serde_json::to_string(&fingerprints)?).fetch_optional(&mut *tx).await?;
    let visitor = stored.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    for fingerprint in &fingerprints {
        let day = &fingerprint.day;
        if day == &window.today || day == &window.yesterday {
            sqlx::query("INSERT INTO repository_traffic_visitors(repo_id,day,fingerprint,visitor) VALUES(?1,?2,?3,?4) ON CONFLICT(repo_id,day,fingerprint) DO NOTHING").bind(repo).bind(day).bind(&fingerprint.hash).bind(&visitor).execute(&mut *tx).await?;
        }
    }
    sqlx::query("INSERT INTO repository_traffic_counts(repo_id,day,kind,visitor,path_hash,path,title,referrer,count,last_seen) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,1,strftime('%Y-%m-%dT%H:%M:%f000Z','now')) ON CONFLICT(repo_id,day,kind,visitor,path_hash,referrer) DO UPDATE SET count=count+1,title=excluded.title,last_seen=excluded.last_seen")
        .bind(repo).bind(&window.today).bind(metric.kind.as_str()).bind(&visitor).bind(&metric.path_hash).bind(&metric.path).bind(&metric.title).bind(&metric.referrer).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn prune(pool: &SqlitePool, first: &str) -> Result<u64> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let mut removed = 0;
    for table in [
        "repository_traffic_counts",
        "repository_traffic_visitors",
        "repository_traffic_keys",
    ] {
        removed+=sqlx::query(&format!("DELETE FROM {table} WHERE rowid IN (SELECT rowid FROM {table} WHERE day<?1 LIMIT 10000)")).bind(first).execute(&mut *tx).await?.rows_affected();
    }
    tx.commit().await?;
    Ok(removed)
}
pub async fn summary(
    pool: &SqlitePool,
    repo: i64,
    kind: Kind,
    window: &Window,
    weekly: bool,
) -> Result<Summary> {
    let mut tx = pool.begin().await?;
    let totals=sqlx::query_as("SELECT COALESCE(SUM(count),0) AS count,COUNT(DISTINCT visitor) AS uniques FROM repository_traffic_counts WHERE repo_id=?1 AND kind=?2 AND day BETWEEN ?3 AND ?4").bind(repo).bind(kind.as_str()).bind(&window.first).bind(&window.today).fetch_one(&mut *tx).await?;
    let bucket = if weekly {
        "date(day,'-' || ((CAST(strftime('%w',day) AS INTEGER)+6)%7) || ' days')"
    } else {
        "day"
    };
    let buckets=sqlx::query_as(&format!("SELECT {bucket} || 'T00:00:00Z' AS timestamp,SUM(count) AS count,COUNT(DISTINCT visitor) AS uniques FROM repository_traffic_counts WHERE repo_id=?1 AND kind=?2 AND day BETWEEN ?3 AND ?4 GROUP BY {bucket} ORDER BY timestamp")).bind(repo).bind(kind.as_str()).bind(&window.first).bind(&window.today).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Summary { totals, buckets })
}
pub async fn paths(pool: &SqlitePool, repo: i64, window: &Window) -> Result<Vec<PopularPath>> {
    Ok(sqlx::query_as("SELECT c.path,(SELECT latest.title FROM repository_traffic_counts latest WHERE latest.repo_id=?1 AND latest.kind='view' AND latest.path_hash=c.path_hash AND latest.day BETWEEN ?2 AND ?3 ORDER BY latest.last_seen DESC,latest.title LIMIT 1) AS title,SUM(c.count) AS count,COUNT(DISTINCT c.visitor) AS uniques FROM repository_traffic_counts c WHERE c.repo_id=?1 AND c.kind='view' AND c.day BETWEEN ?2 AND ?3 GROUP BY c.path_hash,c.path ORDER BY count DESC,c.path LIMIT 10")
        .bind(repo).bind(&window.first).bind(&window.today).fetch_all(pool).await?)
}
pub async fn referrers(
    pool: &SqlitePool,
    repo: i64,
    window: &Window,
) -> Result<Vec<PopularReferrer>> {
    Ok(sqlx::query_as("SELECT referrer,SUM(count) AS count,COUNT(DISTINCT visitor) AS uniques FROM repository_traffic_counts WHERE repo_id=?1 AND kind='view' AND day BETWEEN ?2 AND ?3 AND referrer<>'' GROUP BY referrer ORDER BY count DESC,referrer LIMIT 10")
        .bind(repo).bind(&window.first).bind(&window.today).fetch_all(pool).await?)
}
