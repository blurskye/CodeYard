use crate::artifact_metadata::cacheable;
use crate::artifact_metadata::Actor;
use crate::artifact_metadata::Admission;
use crate::artifact_metadata::Artifact;
use crate::artifact_metadata::ClusterInput;
use crate::artifact_metadata::Deployment;
use crate::artifact_metadata::DeploymentInput;
use crate::artifact_metadata::Environment;
use crate::artifact_metadata::Job;
use crate::artifact_metadata::JobInput;
use crate::artifact_metadata::Mutation;
use crate::artifact_metadata::Record;
use crate::artifact_metadata::Refusal;
use crate::artifact_metadata::StorageInput;
use anyhow::Result;
use codeyard_database::pool::SqlitePool;
use serde_json::json;
use serde_json::Value;
use sqlx::Sqlite;
use sqlx::Transaction;
pub(crate) mod pages;

type Tx<'a> = Transaction<'a, Sqlite>;

async fn begin(pool: &SqlitePool) -> Result<Tx<'_>> {
    Ok(pool.begin_with("BEGIN IMMEDIATE").await?)
}

async fn credential(tx: &mut Tx<'_>, actor: Actor) -> Result<()> {
    let name: Option<String> = sqlx::query_scalar("SELECT u.username FROM users u JOIN personal_access_tokens t ON t.user_id=u.id WHERE u.id=?1 AND t.id=?2 AND t.scope='write' AND (t.expires_at IS NULL OR julianday(t.expires_at)>julianday('now')) AND NOT u.is_banned AND NOT u.must_change_password")
        .bind(actor.user).bind(actor.token).fetch_optional(&mut **tx).await?;
    let name = name.ok_or(Refusal::Denied)?;
    if name == codeyard_database::queries::ci_tokens::BOT {
        let job: Option<(i64,String)> = sqlx::query_as("SELECT r.repo_id,t.permissions FROM actions_github_tokens t JOIN ci_runs r ON r.id=t.run_id WHERE t.token_id=?1 AND (t.revoked_at IS NULL OR t.revoked_at>CAST(strftime('%s','now') AS INTEGER)-60) AND (r.status='running' OR julianday(r.completed_at)>julianday('now','-1 minute'))")
            .bind(actor.token).fetch_optional(&mut **tx).await?;
        let (repo, permissions) = job.ok_or(Refusal::Denied)?;
        let permissions: codeyard_types::permissions::Permissions =
            serde_json::from_str(&permissions)?;
        anyhow::ensure!(
            actor.repo == Some(repo) && permissions.allows("artifact-metadata", true),
            Refusal::Denied
        );
    } else {
        anyhow::ensure!(actor.repo.is_none(), Refusal::Denied);
    }
    Ok(())
}

async fn writable(tx: &mut Tx<'_>, admission: &mut Admission, org: i64, repo: i64) -> Result<()> {
    if let Some(cached) = admission.repositories.get(&(org, repo)) {
        return (*cached).map_err(Into::into);
    }
    let actor = admission.actor;
    let result: Result<()> = async {
        anyhow::ensure!(actor.repo.is_none_or(|scope| scope == repo), Refusal::Missing);
        let readable: bool = sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM repositories r JOIN users u ON u.id=?1 WHERE r.id=?2 AND r.owner_kind='org' AND r.owner_org_id=?3 AND {})",codeyard_database::queries::sqlite::repositories::access::readable_condition()))
            .bind(actor.user).bind(repo).bind(org).fetch_one(&mut **tx).await?;
        anyhow::ensure!(readable, Refusal::Missing);
        anyhow::ensure!(codeyard_database::queries::sqlite::discussions::lock_write_authority(tx, actor.user, repo).await?, Refusal::Denied);
        let belongs: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories WHERE id=?1 AND owner_kind='org' AND owner_org_id=?2)").bind(repo).bind(org).fetch_one(&mut **tx).await?;
        anyhow::ensure!(belongs, Refusal::Missing);
        Ok(())
    }.await;
    if let Some(cached) = cacheable(&result) {
        admission.repositories.insert((org, repo), cached);
    }
    result
}

async fn targets(
    tx: &mut Tx<'_>,
    admission: &mut Admission,
    org: i64,
    artifact: &Artifact,
    maximum: usize,
) -> Result<Vec<(i64, Option<i64>)>> {
    anyhow::ensure!((1..=1001).contains(&maximum), Refusal::Invalid);
    let key = (
        org,
        artifact.digest.clone(),
        artifact.github_repository.clone(),
        maximum,
    );
    if let Some(cached) = admission.targets.get(&key) {
        return cached.clone().map_err(Into::into);
    }
    let actor = admission.actor;
    let result: Result<Vec<(i64, Option<i64>)>> = async {
        let sql = crate::artifact_metadata::target_candidates_sql(false, codeyard_database::queries::sqlite::repositories::access::USER_REPO_ACCESS_SQL);
        let mut rows: Vec<(i64,Option<i64>)> = sqlx::query_as(&sql)
            .bind(org).bind(&artifact.digest).bind(actor.user).bind(actor.repo).bind(maximum as i64).fetch_all(&mut **tx).await?;
        if rows.is_empty() {
            // Filtering must not turn authoritative but inaccessible provenance into
            // permission to attach this digest to an unrelated fallback repository.
            let provenance: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM artifact_attestations a JOIN attestation_subjects s ON s.attestation_id=a.id JOIN repositories r ON r.id=a.repo_id WHERE r.owner_kind='org' AND r.owner_org_id=?1 AND s.digest=?2 AND a.predicate_type IN ('https://slsa.dev/provenance/v1','https://slsa.dev/provenance/v0.2'))")
                .bind(org).bind(&artifact.digest).fetch_one(&mut **tx).await?;
            anyhow::ensure!(!provenance, Refusal::Missing);
            let name = artifact.github_repository.as_deref().ok_or(Refusal::Missing)?;
            let repo: Option<i64> = sqlx::query_scalar("SELECT id FROM repositories WHERE owner_kind='org' AND owner_org_id=?1 AND name=?2").bind(org).bind(name).fetch_optional(&mut **tx).await?;
            rows.push((repo.ok_or(Refusal::Missing)?, None));
        }
        let mut authorized = Vec::new();
        for (repo, attestation) in rows {
            // A changed authority snapshot must fail, not silently truncate bounded fanout.
            writable(tx, admission, org, repo).await?;
            authorized.push((repo, attestation));
        }
        anyhow::ensure!(!authorized.is_empty(), Refusal::Missing);
        Ok(authorized)
    }.await;
    if let Some(cached) = cacheable(&result) {
        admission.targets.insert(key, cached);
    }
    result
}

struct NewRecord<'a> {
    actor: &'a mut Admission,
    org: i64,
    repo: i64,
    attestation: Option<i64>,
    artifact: &'a Artifact,
    kind: &'a str,
    identity: String,
    cluster: &'a str,
    environment: Option<&'a Environment>,
    document: Value,
}

async fn previous_authority(
    tx: &mut Tx<'_>,
    actor: &mut Admission,
    org: i64,
    kind: &str,
    identity: &str,
) -> Result<()> {
    let previous: Option<i64> = sqlx::query_scalar("SELECT a.repo_id FROM artifact_metadata_records d JOIN artifact_metadata a ON a.id=d.artifact_id WHERE d.org_id=?1 AND d.kind=?2 AND d.identity=?3")
        .bind(org).bind(kind).bind(identity).fetch_optional(&mut **tx).await?;
    if let Some(repo) = previous {
        writable(tx, actor, org, repo).await?;
    }
    Ok(())
}

async fn put(tx: &mut Tx<'_>, record: NewRecord<'_>) -> Result<Record> {
    previous_authority(tx, record.actor, record.org, record.kind, &record.identity).await?;
    let artifact: i64 = sqlx::query_scalar("INSERT INTO artifact_metadata(repo_id,digest,name,version) VALUES(?1,?2,?3,?4) ON CONFLICT(repo_id,digest) DO UPDATE SET digest=excluded.digest RETURNING id")
        .bind(record.repo).bind(&record.artifact.digest).bind(&record.artifact.name).bind(&record.artifact.version).fetch_one(&mut **tx).await?;
    let now = codeyard_database::queries::sqlite::time::encode(
        codeyard_database::queries::sqlite::time::now(),
    );
    let id: i64 = sqlx::query_scalar("INSERT INTO artifact_metadata_records(org_id,artifact_id,kind,identity,cluster,logical_environment,physical_environment,document,attestation_id,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10) ON CONFLICT(org_id,kind,identity) DO UPDATE SET artifact_id=excluded.artifact_id,document=excluded.document,attestation_id=excluded.attestation_id,updated_at=excluded.updated_at RETURNING id")
        .bind(record.org).bind(artifact).bind(record.kind).bind(record.identity).bind(record.cluster).bind(record.environment.map_or("",|env|env.logical_environment.as_str())).bind(record.environment.map_or("",|env|env.physical_environment.as_str())).bind(serde_json::to_string(&record.document)?).bind(record.attestation).bind(now).fetch_one(&mut **tx).await?;
    Ok(sqlx::query_as("SELECT d.id,a.repo_id,d.kind,a.digest,a.name,a.version,d.document,d.attestation_id,d.created_at,d.updated_at FROM artifact_metadata_records d JOIN artifact_metadata a ON a.id=d.artifact_id WHERE d.id=?1").bind(id).fetch_one(&mut **tx).await?)
}

fn identity(environment: &Environment, cluster: &str, deployment: &str) -> String {
    json!([
        environment.logical_environment,
        environment.physical_environment,
        cluster,
        deployment
    ])
    .to_string()
}

async fn put_deployment(
    tx: &mut Tx<'_>,
    authority: (&mut Admission, i64),
    repo: i64,
    attestation: Option<i64>,
    environment: &Environment,
    cluster: &str,
    deployment: &Deployment,
) -> Result<Record> {
    let (actor, org) = authority;
    let document = json!({"logical_environment":environment.logical_environment,"physical_environment":environment.physical_environment,"cluster":cluster,"deployment_name":deployment.deployment_name,"status":deployment.status,"tags":deployment.tags,"runtime_risks":deployment.runtime_risks});
    put(
        tx,
        NewRecord {
            actor,
            org,
            repo,
            attestation,
            artifact: &deployment.artifact,
            kind: "deployment",
            identity: identity(environment, cluster, &deployment.deployment_name),
            cluster,
            environment: Some(environment),
            document,
        },
    )
    .await
}

pub async fn deployment(
    pool: &SqlitePool,
    actor: Actor,
    org: i64,
    input: &DeploymentInput,
) -> Result<Mutation> {
    input.validate()?;
    let mut tx = begin(pool).await?;
    credential(&mut tx, actor).await?;
    let mut admission = Admission::new(actor);
    let (repo, attestation) = targets(&mut tx, &mut admission, org, &input.deployment.artifact, 1)
        .await?
        .into_iter()
        .next()
        .ok_or(Refusal::Missing)?;
    writable(&mut tx, &mut admission, org, repo).await?;
    let row = put_deployment(
        &mut tx,
        (&mut admission, org),
        repo,
        attestation,
        &input.environment,
        &input.cluster,
        &input.deployment,
    )
    .await?;
    tx.commit().await?;
    Ok(Mutation {
        records: vec![row],
        errors: vec![],
    })
}

pub async fn storage(
    pool: &SqlitePool,
    actor: Actor,
    org: i64,
    input: &StorageInput,
) -> Result<Mutation> {
    input.validate()?;
    let mut tx = begin(pool).await?;
    credential(&mut tx, actor).await?;
    let mut admission = Admission::new(actor);
    let targets = targets(&mut tx, &mut admission, org, &input.artifact, 1001).await?;
    anyhow::ensure!(targets.len() <= 1000, Refusal::TooLarge);
    for (repo, _) in &targets {
        writable(&mut tx, &mut admission, org, *repo).await?;
    }
    let mut records = Vec::new();
    for (repo, attestation) in targets {
        let mut document = json!({"registry_url":input.registry_url,"status":input.status});
        if let Some(url) = &input.artifact_url {
            document["artifact_url"] = json!(url);
        }
        if let Some(repository) = &input.repository {
            document["repository"] = json!(repository);
        }
        if let Some(path) = &input.path {
            document["path"] = json!(path);
        }
        let identity = input.identity(repo);
        records.push(
            put(
                &mut tx,
                NewRecord {
                    actor: &mut admission,
                    org,
                    repo,
                    attestation,
                    artifact: &input.artifact,
                    kind: "storage",
                    identity,
                    cluster: "",
                    environment: None,
                    document,
                },
            )
            .await?,
        );
    }
    tx.commit().await?;
    Ok(Mutation {
        records,
        errors: vec![],
    })
}

fn rejected(deployment: &Deployment, error: &anyhow::Error) -> Value {
    json!({"cause":if matches!(error.downcast_ref::<Refusal>(),Some(Refusal::Denied)) {"unauthorized"} else {"not_found"},"deployment":deployment})
}

async fn prepare(
    tx: &mut Tx<'_>,
    actor: &mut Admission,
    org: i64,
    cluster: &str,
    input: &ClusterInput,
) -> Result<(Vec<(Deployment, i64, Option<i64>)>, Vec<Value>)> {
    let mut accepted = Vec::new();
    let mut errors = Vec::new();
    for deployment in &input.deployments {
        let resolved = async {
            let (repo, attestation) = targets(tx, actor, org, &deployment.artifact, 1)
                .await?
                .into_iter()
                .next()
                .ok_or(Refusal::Missing)?;
            writable(tx, actor, org, repo).await?;
            previous_authority(
                tx,
                actor,
                org,
                "deployment",
                &identity(&input.environment, cluster, &deployment.deployment_name),
            )
            .await?;
            Ok::<_, anyhow::Error>((repo, attestation))
        }
        .await;
        match resolved {
            Ok((repo, attestation)) => accepted.push((deployment.clone(), repo, attestation)),
            Err(error)
                if input.partial_success
                    && matches!(
                        error.downcast_ref::<Refusal>(),
                        Some(Refusal::Missing | Refusal::Denied)
                    ) =>
            {
                errors.push(rejected(deployment, &error))
            }
            Err(error) => return Err(error),
        }
    }
    Ok((accepted, errors))
}

async fn apply(
    tx: &mut Tx<'_>,
    actor: &mut Admission,
    org: i64,
    cluster: &str,
    input: &ClusterInput,
) -> Result<Mutation> {
    let (accepted, errors) = prepare(tx, actor, org, cluster, input).await?;
    let mut records = Vec::new();
    for (deployment, repo, attestation) in accepted {
        records.push(
            put_deployment(
                tx,
                (&mut *actor, org),
                repo,
                attestation,
                &input.environment,
                cluster,
                &deployment,
            )
            .await?,
        );
    }
    Ok(Mutation { records, errors })
}

pub async fn cluster(
    pool: &SqlitePool,
    actor: Actor,
    org: i64,
    cluster: &str,
    input: &ClusterInput,
) -> Result<Mutation> {
    input.validate(cluster, 1000)?;
    let mut tx = begin(pool).await?;
    credential(&mut tx, actor).await?;
    let mut admission = Admission::new(actor);
    let mutation = apply(&mut tx, &mut admission, org, cluster, input).await?;
    tx.commit().await?;
    Ok(mutation)
}

pub async fn list(
    pool: &SqlitePool,
    actor: Actor,
    org: i64,
    digest: &str,
    kind: &str,
) -> Result<Vec<Record>> {
    use futures_util::TryStreamExt;
    let sql = format!("SELECT d.id,a.repo_id,d.kind,a.digest,a.name,a.version,d.document,d.attestation_id,d.created_at,d.updated_at FROM artifact_metadata_records d JOIN artifact_metadata a ON a.id=d.artifact_id JOIN repositories r ON r.id=a.repo_id JOIN users u ON u.id=?1 WHERE d.org_id=?2 AND r.owner_kind='org' AND r.owner_org_id=?2 AND a.digest=?3 AND d.kind=?4 AND (?5 IS NULL OR r.id=?5) AND {} ORDER BY d.id LIMIT 10001",codeyard_database::queries::sqlite::repositories::access::readable_condition());
    let mut stream = sqlx::query_as(&sql)
        .bind(actor.user)
        .bind(org)
        .bind(digest)
        .bind(kind)
        .bind(actor.repo)
        .fetch(pool);
    let mut rows = Vec::new();
    let mut bytes = 0;
    while let Some(row) = stream.try_next().await? {
        crate::artifact_metadata::append_record(&mut rows, row, &mut bytes)?;
    }
    Ok(rows)
}

async fn prune_jobs(tx: &mut Tx<'_>, scope: Option<(i64, i64, Option<i64>)>) -> Result<()> {
    use crate::artifact_metadata::JOB_GC_BATCH;
    use crate::artifact_metadata::JOB_HISTORY_LIMIT;
    use crate::artifact_metadata::JOB_HISTORY_SECONDS;
    let cutoff = codeyard_database::queries::sqlite::time::encode(
        codeyard_database::queries::sqlite::time::now()
            - chrono::Duration::seconds(JOB_HISTORY_SECONDS),
    );
    sqlx::query("DELETE FROM artifact_metadata_jobs WHERE id IN (SELECT id FROM artifact_metadata_jobs WHERE status IN ('completed','failed') AND completed_at<?1 ORDER BY completed_at,id LIMIT ?2)")
        .bind(cutoff).bind(JOB_GC_BATCH).execute(&mut **tx).await?;
    if let Some((actor, org, repo)) = scope {
        sqlx::query("DELETE FROM artifact_metadata_jobs WHERE id IN (SELECT id FROM artifact_metadata_jobs WHERE actor_id=?1 AND org_id=?2 AND scoped_repo IS ?3 AND status IN ('completed','failed') ORDER BY id DESC LIMIT ?4 OFFSET ?5)")
            .bind(actor).bind(org).bind(repo).bind(JOB_GC_BATCH).bind(JOB_HISTORY_LIMIT).execute(&mut **tx).await?;
    }
    Ok(())
}

pub async fn enqueue(
    pool: &SqlitePool,
    actor: Actor,
    org: i64,
    cluster: &str,
    input: &ClusterInput,
) -> Result<(i64, Vec<Value>)> {
    input.validate(cluster, 5000)?;
    let mut input = input.clone();
    input.partial_success = true;
    let mut tx = begin(pool).await?;
    credential(&mut tx, actor).await?;
    let mut admission = Admission::new(actor);
    let (accepted, errors) = prepare(&mut tx, &mut admission, org, cluster, &input).await?;
    // A batch whose every record was refused is refused as a whole, as on GitHub, so a write
    // token without access records no job. An empty batch is recorded as done.
    let noop = accepted.is_empty();
    anyhow::ensure!(!noop || input.deployments.is_empty(), Refusal::Denied);
    if !noop {
        let busy: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM artifact_metadata_jobs WHERE org_id=?1 AND cluster=?2 AND status IN ('pending','processing'))").bind(org).bind(cluster).fetch_one(&mut *tx).await?;
        anyhow::ensure!(!busy, Refusal::Busy);
    }
    input.deployments = accepted
        .into_iter()
        .map(|(deployment, _, _)| deployment)
        .collect();
    let saved = JobInput { input };
    let id = sqlx::query_scalar("INSERT INTO artifact_metadata_jobs(org_id,cluster,actor_id,token_id,scoped_repo,input,status,errors,completed_at) VALUES(?1,?2,?3,?4,?5,?6,?8,?7,CASE WHEN ?8='completed' THEN strftime('%Y-%m-%dT%H:%M:%f000Z','now') ELSE NULL END) RETURNING id")
        .bind(org).bind(cluster).bind(actor.user).bind(actor.token).bind(actor.repo).bind(if noop { String::new() } else { serde_json::to_string(&saved)? }).bind(serde_json::to_string(&errors)?).bind(if noop {"completed"} else {"pending"}).fetch_one(&mut *tx).await?;
    if noop {
        sqlx::query("UPDATE artifact_metadata_jobs SET started_at=?2,total_count=0 WHERE id=?1")
            .bind(id)
            .bind(codeyard_database::queries::sqlite::time::encode(
                codeyard_database::queries::sqlite::time::now(),
            ))
            .execute(&mut *tx)
            .await?;
    }
    prune_jobs(&mut tx, Some((actor.user, org, actor.repo))).await?;
    tx.commit().await?;
    Ok((id, errors))
}

pub async fn job(
    pool: &SqlitePool,
    actor: Actor,
    org: i64,
    cluster: &str,
    id: i64,
) -> Result<Option<Job>> {
    Ok(sqlx::query_as("SELECT * FROM artifact_metadata_jobs WHERE id=?1 AND org_id=?2 AND cluster=?3 AND actor_id=?4 AND (?5 IS NULL OR scoped_repo=?5)")
        .bind(id).bind(org).bind(cluster).bind(actor.user).bind(actor.repo).fetch_optional(pool).await?)
}

pub async fn process_job(pool: &SqlitePool, id: Option<i64>) -> Result<bool> {
    let mut tx = begin(pool).await?;
    let job: Option<Job> = sqlx::query_as("SELECT * FROM artifact_metadata_jobs WHERE status='pending' AND (?1 IS NULL OR id=?1) ORDER BY id LIMIT 1").bind(id).fetch_optional(&mut *tx).await?;
    let Some(job) = job else {
        prune_jobs(&mut tx, None).await?;
        tx.commit().await?;
        return Ok(false);
    };
    let actor = Actor {
        user: job.actor_id,
        token: job.token_id,
        repo: job.scoped_repo,
    };
    let started = codeyard_database::queries::sqlite::time::encode(
        codeyard_database::queries::sqlite::time::now(),
    );
    let result = async {
        credential(&mut tx, actor).await?;
        let saved: JobInput = serde_json::from_str(&job.input)?;
        let mut admission = Admission::new(actor);
        apply(
            &mut tx,
            &mut admission,
            job.org_id,
            &job.cluster,
            &saved.input,
        )
        .await
    }
    .await;
    match result {
        Ok(mutation) => {
            let mut errors: Vec<Value> = serde_json::from_str(&job.errors)?;
            errors.extend(mutation.errors);
            let scope = Some((job.actor_id, job.org_id, job.scoped_repo));
            sqlx::query("UPDATE artifact_metadata_jobs SET status='completed',started_at=?2,total_count=?3,errors=?4,input='',completed_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1").bind(job.id).bind(started).bind(mutation.records.len() as i64).bind(serde_json::to_string(&errors)?).execute(&mut *tx).await?;
            prune_jobs(&mut tx, scope).await?;
            tx.commit().await?;
        }
        Err(error) => {
            tx.rollback().await?;
            let message = if error.downcast_ref::<Refusal>().is_some() {
                error.to_string()
            } else {
                "Artifact metadata processing failed".to_owned()
            };
            let mut tx = begin(pool).await?;
            let scope = Some((job.actor_id, job.org_id, job.scoped_repo));
            sqlx::query("UPDATE artifact_metadata_jobs SET status='failed',started_at=?2,errors=?3,input='',completed_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id=?1 AND status='pending'").bind(job.id).bind(started).bind(json!([{"cause":message}]).to_string()).execute(&mut *tx).await?;
            prune_jobs(&mut tx, scope).await?;
            tx.commit().await?;
        }
    }
    Ok(true)
}
