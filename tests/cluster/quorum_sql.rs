//! SQL quorum contracts run unchanged on both engines; no Git or network coordinator.
#![cfg(not(target_arch = "wasm32"))]
use crate::support::native_database as native;
use chrono::{DateTime, Utc};
use codeyard::backend::{
    cluster::NodeKey,
    db::{
        self,
        cluster::writes,
        models::{RefChange, RefValue},
        DbPool,
    },
};
use uuid::Uuid;

const OLD: &str = "1111111111111111111111111111111111111111";
const NEW: &str = "2222222222222222222222222222222222222222";
const MAIN: &str = "refs/heads/main";

struct Fixture {
    repo: i64,
    owner: i64,
    primary: i64,
    voters: Vec<i64>,
}

fn initial_refs() -> Vec<RefValue> {
    vec![
        RefValue {
            name: "HEAD".into(),
            kind: "symbolic".into(),
            target: MAIN.into(),
        },
        RefValue {
            name: MAIN.into(),
            kind: "oid".into(),
            target: OLD.into(),
        },
        RefValue {
            name: "refs/heads/remove".into(),
            kind: "oid".into(),
            target: OLD.into(),
        },
    ]
}

fn change() -> RefChange {
    RefChange {
        name: MAIN.into(),
        kind: "oid".into(),
        old_target: Some(OLD.into()),
        new_target: Some(NEW.into()),
    }
}

async fn node(pool: &DbPool, name: &str) -> i64 {
    db::cluster::register_own_node(
        pool,
        &NodeKey::generate().public_hex(),
        name,
        &["storage".to_owned()],
        "test",
    )
    .await
    .unwrap()
    .id
}

async fn configure(pool: &DbPool, setting: &str) -> Result<(), sqlx::Error> {
    match pool {
        DbPool::Postgres(raw) => {
            sqlx::query("UPDATE node_configuration SET write_quorum=$1 WHERE id=1")
                .bind(setting)
                .execute(raw)
                .await?;
        }
        DbPool::Sqlite(raw) => {
            sqlx::query("UPDATE node_configuration SET write_quorum=$1 WHERE id=1")
                .bind(setting)
                .execute(raw)
                .await?;
        }
    };
    Ok(())
}

async fn clock(pool: &DbPool) -> DateTime<Utc> {
    match pool {
        DbPool::Postgres(raw) => sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(raw)
            .await
            .unwrap(),
        DbPool::Sqlite(raw) => sqlx::query_scalar("SELECT strftime('%Y-%m-%dT%H:%M:%f000Z','now')")
            .fetch_one(raw)
            .await
            .unwrap(),
    }
}

async fn setup(pool: &DbPool, count: usize, setting: &str) -> Fixture {
    db::node_config::upsert_node_configuration(pool, "public", true)
        .await
        .unwrap();
    configure(pool, setting).await.unwrap();
    let mut voters = Vec::new();
    for name in ["store-a", "store-b", "store-c"].into_iter().take(count) {
        voters.push(node(pool, name).await);
    }
    let primary = voters[0];
    let owner = db::users::accounts::create_user(pool, "quorum-owner", None)
        .await
        .unwrap()
        .id;
    let repo = db::repositories::records::create_repository(
        pool,
        owner,
        "quorum",
        None,
        false,
        &format!("/srv/repos/{}.git", Uuid::new_v4()),
    )
    .await
    .unwrap()
    .id;
    assert!(db::cluster::place_repository(pool, repo, primary)
        .await
        .unwrap());
    writes::bootstrap_writes(pool, repo, primary, &voters, &initial_refs())
        .await
        .unwrap();
    let view = writes::recovery_view(pool, repo).await.unwrap();
    assert!(!view.serving);
    assert_eq!((view.term, view.committed_version), (1, 0));
    assert_eq!(
        view.voters
            .iter()
            .filter(|v| v.applied_version == 0)
            .count(),
        1
    );
    if count > 1 {
        assert!(!writes::enable_write_family(pool, &[(repo, 1)])
            .await
            .unwrap());
    }
    for voter in &voters {
        assert!(writes::mark_write_applied(pool, repo, *voter, 1, 0)
            .await
            .unwrap());
    }
    assert!(writes::enable_write_family(pool, &[(repo, 1)])
        .await
        .unwrap());
    Fixture {
        repo,
        owner,
        primary,
        voters,
    }
}

async fn begin(pool: &DbPool, f: &Fixture) -> String {
    let id = Uuid::new_v4().to_string();
    writes::begin_write(pool, f.repo, f.primary, 1, &id, &[change()])
        .await
        .unwrap();
    id
}

async fn votes(pool: &DbPool, f: &Fixture, id: &str) {
    for voter in f.voters.iter().take(2) {
        assert!(writes::vote_write(pool, id, *voter, 1).await.unwrap());
    }
}

async fn happy_path(pool: &DbPool, count: usize, setting: &str) {
    let f = setup(pool, count, setting).await;
    let mut changes = vec![
        change(),
        RefChange {
            name: "refs/heads/remove".into(),
            kind: "oid".into(),
            old_target: Some(OLD.into()),
            new_target: None,
        },
        RefChange {
            name: "refs/codeyard/retained/test".into(),
            kind: "oid".into(),
            old_target: None,
            new_target: Some(NEW.into()),
        },
        RefChange {
            name: "HEAD".into(),
            kind: "symbolic".into(),
            old_target: Some(MAIN.into()),
            new_target: Some("refs/heads/next".into()),
        },
    ];
    let id = Uuid::new_v4().to_string();
    let before = clock(pool).await;
    let write = writes::begin_write(pool, f.repo, f.primary, 1, &id, &changes)
        .await
        .unwrap();
    let after = clock(pool).await;
    assert_eq!(write.required_votes, 2);
    assert_eq!(write.base_version, 0);
    assert_eq!(write.decision, "pending");
    assert!(write.deadline >= before + chrono::Duration::seconds(10));
    assert!(write.deadline <= after + chrono::Duration::seconds(10));
    changes.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(write.changes, changes);
    assert_eq!(write.manifest_hash.len(), 64);
    assert!(write.votes.is_empty());
    assert!(!writes::finish_write(pool, &id, 1).await.unwrap());
    assert!(writes::vote_write(pool, &id, f.voters[0], 1).await.unwrap());
    assert_eq!(
        writes::load_write(pool, &id)
            .await
            .unwrap()
            .unwrap()
            .votes
            .len(),
        1
    );
    let prepared = writes::recovery_view(pool, f.repo).await.unwrap();
    assert_eq!(prepared.committed_version, 0);
    assert_eq!(
        prepared
            .refs
            .iter()
            .find(|v| v.name == MAIN)
            .unwrap()
            .target,
        OLD
    );
    assert!(writes::vote_write(pool, &id, f.voters[1], 1).await.unwrap());
    let decided = writes::decide_write(pool, &id, f.primary, 1, true)
        .await
        .unwrap();
    assert_eq!(decided.decision, "commit");
    assert_eq!(decided.committed_version, Some(1));
    assert_eq!(decided.votes.len(), 2);
    assert!(!decided.finished);
    assert!(!writes::vote_write(pool, &id, f.primary, 1).await.unwrap());
    assert!(!writes::finish_write(pool, &id, 1).await.unwrap());
    let committed = writes::recovery_view(pool, f.repo).await.unwrap();
    assert_eq!(committed.committed_version, 1);
    assert_eq!(
        committed
            .refs
            .iter()
            .find(|v| v.name == MAIN)
            .unwrap()
            .target,
        NEW
    );
    assert_eq!(
        committed
            .refs
            .iter()
            .find(|v| v.name == "HEAD")
            .unwrap()
            .target,
        "refs/heads/next"
    );
    assert!(!committed.refs.iter().any(|v| v.name == "refs/heads/remove"));
    assert!(committed
        .refs
        .iter()
        .any(|v| v.name == "refs/codeyard/retained/test"));
    assert_eq!(
        db::cluster::get_write_version(pool, f.repo).await.unwrap(),
        Some(0)
    );
    for voter in f.voters.iter().take(2) {
        assert!(writes::mark_write_applied(pool, f.repo, *voter, 1, 1)
            .await
            .unwrap());
    }
    assert!(!writes::mark_write_applied(pool, f.repo, f.primary, 1, 0)
        .await
        .unwrap());
    assert!(!writes::mark_write_applied(pool, f.repo, f.primary, 1, 2)
        .await
        .unwrap());
    assert!(writes::finish_write(pool, &id, 1).await.unwrap());
    assert!(writes::recovery_view(pool, f.repo)
        .await
        .unwrap()
        .unfinished
        .is_none());
    assert!(
        writes::load_write(pool, &id)
            .await
            .unwrap()
            .unwrap()
            .finished
    );
}

async fn two_voters(pool: &DbPool) {
    happy_path(pool, 2, "2").await;
}
async fn three_voters(pool: &DbPool) {
    happy_path(pool, 3, "majority").await;
}

async fn duplicate_vote(pool: &DbPool) {
    let f = setup(pool, 3, "majority").await;
    let id = begin(pool, &f).await;
    let outsider = node(pool, "non-voter").await;
    assert!(!writes::vote_write(pool, &id, outsider, 1).await.unwrap());
    assert!(writes::vote_write(pool, &id, f.primary, 1).await.unwrap());
    assert!(!writes::vote_write(pool, &id, f.primary, 1).await.unwrap());
    assert_eq!(
        writes::load_write(pool, &id)
            .await
            .unwrap()
            .unwrap()
            .votes
            .len(),
        1
    );
    let decided = writes::decide_write(pool, &id, f.primary, 1, true)
        .await
        .unwrap();
    assert_eq!(
        decided.decision, "abort",
        "one voter cannot manufacture a majority"
    );
    assert_eq!(decided.committed_version, None);
    assert_eq!(
        writes::recovery_view(pool, f.repo)
            .await
            .unwrap()
            .committed_version,
        0
    );
}

async fn deadline(pool: &DbPool) {
    let f = setup(pool, 3, "majority").await;
    let id = begin(pool, &f).await;
    votes(pool, &f, &id).await;
    let sql = "UPDATE repository_writes SET deadline='2000-01-01T00:00:00.000000Z' WHERE id=$1";
    match pool {
        DbPool::Postgres(raw) => {
            sqlx::query(sql).bind(&id).execute(raw).await.unwrap();
        }
        DbPool::Sqlite(raw) => {
            sqlx::query(sql).bind(&id).execute(raw).await.unwrap();
        }
    }
    assert!(!writes::vote_write(pool, &id, f.voters[2], 1).await.unwrap());
    let decided = writes::decide_write(pool, &id, f.primary, 1, true)
        .await
        .unwrap();
    assert_eq!(decided.decision, "abort");
    assert_eq!(decided.committed_version, None);
    let view = writes::recovery_view(pool, f.repo).await.unwrap();
    assert_eq!(view.committed_version, 0);
    assert_eq!(
        view.refs.iter().find(|v| v.name == MAIN).unwrap().target,
        OLD
    );
    assert!(writes::finish_write(pool, &id, 1).await.unwrap());
}

async fn decision_race(pool: &DbPool) {
    let f = setup(pool, 2, "majority").await;
    let id = begin(pool, &f).await;
    votes(pool, &f, &id).await;
    let (commit, abort) = tokio::join!(
        writes::decide_write(pool, &id, f.primary, 1, true),
        writes::decide_write(pool, &id, f.primary, 1, false),
    );
    let commit = commit.unwrap();
    let abort = abort.unwrap();
    assert_eq!(
        commit, abort,
        "both replies must name the same irreversible decision"
    );
    assert!(matches!(commit.decision.as_str(), "commit" | "abort"));
    let view = writes::recovery_view(pool, f.repo).await.unwrap();
    if commit.decision == "commit" {
        assert_eq!(commit.committed_version, Some(1));
        assert_eq!(view.committed_version, 1);
        assert_eq!(
            view.refs.iter().find(|v| v.name == MAIN).unwrap().target,
            NEW
        );
    } else {
        assert_eq!(commit.committed_version, None);
        assert_eq!(view.committed_version, 0);
        assert_eq!(
            view.refs.iter().find(|v| v.name == MAIN).unwrap().target,
            OLD
        );
    }
    assert_eq!(view.unfinished.unwrap(), commit);
    assert_eq!(
        writes::decide_write(pool, &id, f.primary, 1, commit.decision != "commit")
            .await
            .unwrap(),
        commit
    );
}

async fn stale_terms(pool: &DbPool) {
    let f = setup(pool, 2, "majority").await;
    let id = begin(pool, &f).await;
    assert!(writes::decide_write(pool, &id, f.voters[1], 1, true)
        .await
        .is_err());
    assert!(writes::switch_write_family(
        pool,
        f.repo,
        f.primary,
        &[(f.repo, 1)],
        f.voters[1],
        &f.voters
    )
    .await
    .unwrap());
    assert!(writes::begin_write(
        pool,
        f.repo,
        f.primary,
        1,
        &Uuid::new_v4().to_string(),
        &[change()]
    )
    .await
    .is_err());
    assert!(!writes::vote_write(pool, &id, f.primary, 1).await.unwrap());
    for commit in [true, false] {
        assert!(writes::decide_write(pool, &id, f.primary, 1, commit)
            .await
            .is_err());
    }
    assert!(!writes::mark_write_applied(pool, f.repo, f.primary, 1, 0)
        .await
        .unwrap());
    assert!(!writes::finish_write(pool, &id, 1).await.unwrap());
    assert!(!writes::switch_write_family(
        pool,
        f.repo,
        f.voters[1],
        &[(f.repo, 1)],
        f.primary,
        &f.voters
    )
    .await
    .unwrap());
    assert!(!writes::enable_write_family(pool, &[(f.repo, 1)])
        .await
        .unwrap());
    let view = writes::recovery_view(pool, f.repo).await.unwrap();
    assert_eq!((view.term, view.primary_id), (2, f.voters[1]));
    assert_eq!(view.unfinished.unwrap().decision, "abort");
    assert!(!view.serving);
    assert!(!writes::vote_write(pool, &id, f.primary, 2).await.unwrap());
    assert!(writes::finish_write(pool, &id, 2).await.unwrap());
}

async fn unfinished_slot(pool: &DbPool) {
    let f = setup(pool, 2, "majority").await;
    let first = begin(pool, &f).await;
    let second = Uuid::new_v4().to_string();
    assert!(
        writes::begin_write(pool, f.repo, f.primary, 1, &second, &[change()])
            .await
            .is_err()
    );
    writes::decide_write(pool, &first, f.primary, 1, false)
        .await
        .unwrap();
    assert!(
        writes::begin_write(pool, f.repo, f.primary, 1, &second, &[change()])
            .await
            .is_err()
    );
    assert!(writes::finish_write(pool, &first, 1).await.unwrap());
    let next = writes::begin_write(pool, f.repo, f.primary, 1, &second, &[change()])
        .await
        .unwrap();
    assert_eq!(next.base_version, 0);
    assert_eq!(next.decision, "pending");
}

async fn lost_reply(pool: &DbPool) {
    let f = setup(pool, 3, "majority").await;
    let id = begin(pool, &f).await;
    votes(pool, &f, &id).await;
    // Discard the reply, as if the connection vanished after COMMIT.
    writes::decide_write(pool, &id, f.primary, 1, true)
        .await
        .unwrap();
    let recovered = writes::recovery_view(pool, f.repo).await.unwrap();
    let write = recovered.unfinished.unwrap();
    assert_eq!(write.id, id);
    assert_eq!(write.decision, "commit");
    assert_eq!(write.committed_version, Some(recovered.committed_version));
    assert_eq!(write, writes::load_write(pool, &id).await.unwrap().unwrap());
    assert_eq!(
        writes::decide_write(pool, &id, f.primary, 1, false)
            .await
            .unwrap(),
        write
    );
    assert_eq!(
        recovered
            .refs
            .iter()
            .find(|v| v.name == MAIN)
            .unwrap()
            .target,
        NEW
    );
}

async fn family_switch(pool: &DbPool) {
    let f = setup(pool, 3, "majority").await;
    let disk_path = format!("/srv/repos/{}.git", Uuid::new_v4());
    let fork = db::repositories::creation::insert_repository(
        pool,
        db::repositories::creation::NewRepository {
            owner: db::repositories::creation::RepositoryCreationOwner::personal(f.owner),
            name: "fork",
            description: None,
            is_private: false,
            disk_path: &disk_path,
            storage_node_id: Some(f.primary),
            source_id: Some(f.repo),
        },
    )
    .await
    .unwrap()
    .unwrap()
    .id;
    writes::bootstrap_writes(pool, fork, f.primary, &f.voters, &initial_refs())
        .await
        .unwrap();
    for voter in &f.voters {
        assert!(writes::mark_write_applied(pool, fork, *voter, 1, 0)
            .await
            .unwrap());
    }
    let terms = [(f.repo, 1), (fork, 1)];
    assert!(writes::enable_write_family(pool, &terms).await.unwrap());
    let pending = begin(pool, &f).await;
    let committed = Uuid::new_v4().to_string();
    writes::begin_write(pool, fork, f.primary, 1, &committed, &[change()])
        .await
        .unwrap();
    votes(pool, &f, &committed).await;
    assert_eq!(
        writes::decide_write(pool, &committed, f.primary, 1, true)
            .await
            .unwrap()
            .decision,
        "commit"
    );
    let target = f.voters[1];
    assert!(!writes::switch_write_family(
        pool,
        f.repo,
        f.primary,
        &[(f.repo, 1)],
        target,
        &f.voters
    )
    .await
    .unwrap());
    assert!(
        !writes::switch_write_family(pool, f.repo, target, &terms, target, &f.voters)
            .await
            .unwrap()
    );
    assert!(!writes::switch_write_family(
        pool,
        f.repo,
        f.primary,
        &[(f.repo, 1), (fork, 2)],
        target,
        &f.voters
    )
    .await
    .unwrap());
    assert_eq!(writes::recovery_view(pool, f.repo).await.unwrap().term, 1);
    assert!(
        writes::switch_write_family(pool, f.repo, f.primary, &terms, target, &f.voters)
            .await
            .unwrap()
    );
    let new_terms = [(fork, 2), (f.repo, 2)];
    for repo in [f.repo, fork] {
        let view = writes::recovery_view(pool, repo).await.unwrap();
        assert_eq!(
            (view.term, view.primary_id, view.serving),
            (2, target, false)
        );
        assert!(view
            .voters
            .iter()
            .all(|v| v.term == 2 && v.applied_version == -1));
        assert!(writes::begin_write(
            pool,
            repo,
            target,
            2,
            &Uuid::new_v4().to_string(),
            &[change()]
        )
        .await
        .is_err());
    }
    assert_eq!(
        writes::load_write(pool, &pending)
            .await
            .unwrap()
            .unwrap()
            .decision,
        "abort"
    );
    assert_eq!(
        writes::load_write(pool, &committed)
            .await
            .unwrap()
            .unwrap()
            .decision,
        "commit"
    );
    assert!(!writes::enable_write_family(pool, &new_terms).await.unwrap());
    assert!(writes::finish_write(pool, &pending, 2).await.unwrap());
    assert!(!writes::finish_write(pool, &committed, 2).await.unwrap());
    for voter in f.voters.iter().take(2) {
        assert!(writes::mark_write_applied(pool, f.repo, *voter, 2, 0)
            .await
            .unwrap());
        assert!(writes::mark_write_applied(pool, fork, *voter, 2, 1)
            .await
            .unwrap());
    }
    assert!(writes::finish_write(pool, &committed, 2).await.unwrap());
    assert!(writes::enable_write_family(pool, &new_terms).await.unwrap());
    assert!(writes::recovery_view(pool, f.repo).await.unwrap().serving);
    assert!(writes::recovery_view(pool, fork).await.unwrap().serving);
    // A restart on the same primary also fences all old term directories.
    assert!(
        writes::switch_write_family(pool, f.repo, target, &new_terms, target, &f.voters)
            .await
            .unwrap()
    );
    assert_eq!(writes::recovery_view(pool, fork).await.unwrap().term, 3);
    assert!(!writes::recovery_view(pool, f.repo).await.unwrap().serving);
}

async fn cascades(pool: &DbPool) {
    let f = setup(pool, 2, "majority").await;
    let id = begin(pool, &f).await;
    votes(pool, &f, &id).await;
    // Node references restrict deletion while a repository uses them.
    match pool {
        DbPool::Postgres(raw) => assert!(sqlx::query("DELETE FROM nodes WHERE id=$1")
            .bind(f.voters[1])
            .execute(raw)
            .await
            .is_err()),
        DbPool::Sqlite(raw) => assert!(sqlx::query("DELETE FROM nodes WHERE id=$1")
            .bind(f.voters[1])
            .execute(raw)
            .await
            .is_err()),
    }
    db::repositories::records::delete_repository(pool, f.owner, f.repo)
        .await
        .unwrap();
    assert!(writes::load_write(pool, &id).await.unwrap().is_none());
    for table in [
        "repository_write_control",
        "repository_voters",
        "repository_writes",
        "repository_write_refs",
        "repository_write_votes",
        "repository_committed_refs",
    ] {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        let count: i64 = match pool {
            DbPool::Postgres(raw) => sqlx::query_scalar(&sql).fetch_one(raw).await.unwrap(),
            DbPool::Sqlite(raw) => sqlx::query_scalar(&sql).fetch_one(raw).await.unwrap(),
        };
        assert_eq!(count, 0, "{table}");
    }
}

async fn input_and_config(pool: &DbPool) {
    let f = setup(pool, 2, "majority").await;
    for setting in ["", "0", "01", "1000", "MAJORITY", " 2", "2 ", "-1"] {
        assert!(configure(pool, setting).await.is_err(), "{setting:?}");
    }
    for refs in [
        vec![],
        vec![change(), change()],
        vec![RefChange {
            name: "refs/heads/../bad".into(),
            ..change()
        }],
        vec![RefChange {
            old_target: Some(NEW.into()),
            new_target: Some(OLD.into()),
            ..change()
        }],
    ] {
        assert!(writes::begin_write(
            pool,
            f.repo,
            f.primary,
            1,
            &Uuid::new_v4().to_string(),
            &refs
        )
        .await
        .is_err());
    }
    assert!(
        writes::begin_write(pool, f.repo, f.primary, 1, "not-a-uuid", &[change()])
            .await
            .is_err()
    );
    assert!(
        writes::bootstrap_writes(pool, f.repo, f.primary, &f.voters, &initial_refs())
            .await
            .is_err()
    );
    assert!(writes::switch_write_family(
        pool,
        f.repo,
        f.primary,
        &[(f.repo, 1)],
        f.primary,
        &[f.primary, f.primary]
    )
    .await
    .is_err());
    configure(pool, "3").await.unwrap();
    assert!(writes::begin_write(
        pool,
        f.repo,
        f.primary,
        1,
        &Uuid::new_v4().to_string(),
        &[change()]
    )
    .await
    .is_err());
    configure(pool, "majority").await.unwrap();
    let singleton = db::repositories::records::create_repository(
        pool,
        f.owner,
        "singleton",
        None,
        false,
        &format!("/srv/repos/{}.git", Uuid::new_v4()),
    )
    .await
    .unwrap()
    .id;
    assert!(db::cluster::place_repository(pool, singleton, f.primary)
        .await
        .unwrap());
    assert!(
        writes::bootstrap_writes(pool, singleton, f.primary, &[f.primary], &initial_refs())
            .await
            .is_err()
    );
    configure(pool, "1").await.unwrap();
    assert!(!writes::switch_write_family(
        pool,
        f.repo,
        f.primary,
        &[(f.repo, 1)],
        f.primary,
        &[f.primary]
    )
    .await
    .unwrap());
    writes::bootstrap_writes(pool, singleton, f.primary, &[f.primary], &initial_refs())
        .await
        .unwrap();
    assert!(writes::enable_write_family(pool, &[(singleton, 1)])
        .await
        .unwrap());
    let id = begin(pool, &f).await;
    assert_eq!(
        writes::load_write(pool, &id)
            .await
            .unwrap()
            .unwrap()
            .required_votes,
        1
    );
    // The required count is frozen at preparation, even if configuration changes later.
    configure(pool, "2").await.unwrap();
    assert!(writes::vote_write(pool, &id, f.primary, 1).await.unwrap());
    assert_eq!(
        writes::decide_write(pool, &id, f.primary, 1, true)
            .await
            .unwrap()
            .decision,
        "commit"
    );
}

macro_rules! engine_pair {
    ($sqlite:ident, $postgres:ident, $contract:ident) => {
        #[tokio::test]
        async fn $sqlite() {
            let database = native::NativeDatabase::new().await;
            $contract(&database.pool).await;
        }
        #[tokio::test]
        async fn $postgres() {
            let mut database = native::PostgresDatabase::new().await;
            $contract(&database.pool).await;
            database.finish().await;
        }
    };
}

engine_pair!(sqlite_two_of_two, postgres_two_of_two, two_voters);
engine_pair!(sqlite_two_of_three, postgres_two_of_three, three_voters);
engine_pair!(
    sqlite_duplicate_vote,
    postgres_duplicate_vote,
    duplicate_vote
);
engine_pair!(sqlite_deadline, postgres_deadline, deadline);
engine_pair!(
    sqlite_abort_commit_race,
    postgres_abort_commit_race,
    decision_race
);
engine_pair!(sqlite_stale_terms, postgres_stale_terms, stale_terms);
engine_pair!(
    sqlite_unfinished_slot,
    postgres_unfinished_slot,
    unfinished_slot
);
engine_pair!(sqlite_lost_reply, postgres_lost_reply, lost_reply);
engine_pair!(sqlite_family_switch, postgres_family_switch, family_switch);
engine_pair!(
    sqlite_repository_cascades,
    postgres_repository_cascades,
    cascades
);
engine_pair!(
    sqlite_input_and_config,
    postgres_input_and_config,
    input_and_config
);
