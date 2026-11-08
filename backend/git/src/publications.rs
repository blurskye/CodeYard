//! Server-owned branch publication provenance. Metadata tags end at an empty tree, so Git GC
//! retains live-branch history without adding synthetic commits to contributor or commit histories.
//! Callers retain the interprocess PublicationGuard through blocking workers and scope publication
//! subprocesses with it. Prepare only after live write admission and
//! immediately before the old-value-checked ref transaction. A failed transaction resolves to
//! the previous successful publication, never to the actor who merely attempted the update.
use anyhow::{ensure, Context, Result};
use git2::{ObjectType, Oid, Repository};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, path::Path};
use uuid::Uuid;

const MAX_TAG_BYTES: usize = 32 * 1024;
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum PublicationActor {
    User(i64),
    DeployKey(i64),
}

impl PublicationActor {
    fn id(self) -> i64 {
        match self {
            Self::User(id) | Self::DeployKey(id) => id,
        }
    }

    pub fn user_id(self) -> Option<i64> {
        match self {
            Self::User(id) => Some(id),
            Self::DeployKey(_) => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BranchPublication {
    pub version: u8,
    pub id: Uuid,
    pub repository_id: i64,
    pub actor: PublicationActor,
    pub reference: String,
    pub old: String,
    pub new: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PublicationRecord {
    pub object_id: String,
    pub publication: BranchPublication,
}

#[derive(Clone)]
pub struct PublicationUpdate {
    pub reference: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

fn metadata_ref(reference: &str) -> String {
    format!(
        "refs/codeyard/publications/{}",
        hex::encode(Sha256::digest(reference.as_bytes()))
    )
}

fn checked_reference(reference: &str) -> Result<()> {
    ensure!(
        reference.starts_with("refs/heads/") && super::is_canonical_ref(reference),
        "Publication provenance requires a canonical branch reference"
    );
    Ok(())
}

fn target(repo: &Repository, reference: &str) -> Result<Option<Oid>> {
    match repo.find_reference(reference) {
        Ok(reference) => Ok(Some(
            reference
                .target()
                .context("Publication reference is symbolic")?,
        )),
        Err(error) if error.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_tag(repo: &Repository, oid: Oid) -> Result<(PublicationRecord, Option<Oid>)> {
    let (size, kind) = repo.odb()?.read_header(oid)?;
    ensure!(
        kind == ObjectType::Tag && size <= MAX_TAG_BYTES,
        "Invalid publication metadata object"
    );
    let tag = repo.find_tag(oid)?;
    let publication: BranchPublication = serde_json::from_slice(
        tag.message_bytes()
            .context("Publication metadata has no payload")?,
    )?;
    ensure!(
        publication.version == 1
            && !publication.id.is_nil()
            && publication.repository_id > 0
            && publication.actor.id() > 0
            && super::is_full_commit_id(&publication.old)
            && super::is_full_commit_id(&publication.new)
            && publication.old != publication.new,
        "Invalid publication metadata"
    );
    checked_reference(&publication.reference)?;
    ensure!(
        tag.name().ok() == Some(format!("codeyard-publication-{}", publication.id).as_str()),
        "Publication metadata identity differs from its tag"
    );
    let previous = match tag.target_type() {
        Some(ObjectType::Tag) => Some(tag.target_id()),
        Some(ObjectType::Tree) if tag.target_id().to_string() == EMPTY_TREE => None,
        _ => anyhow::bail!("Invalid publication metadata parent"),
    };
    Ok((
        PublicationRecord {
            object_id: oid.to_string(),
            publication,
        },
        previous,
    ))
}

fn matching(
    repo: &Repository,
    repository_id: i64,
    reference: &str,
    slot: Option<Oid>,
    head: Option<Oid>,
) -> Result<Option<PublicationRecord>> {
    let Some(slot) = slot else { return Ok(None) };
    let expected = head.unwrap_or(Oid::ZERO_SHA1).to_string();
    let (record, previous) = read_tag(repo, slot)?;
    if record.publication.repository_id != repository_id
        || record.publication.reference != reference
    {
        // Forks, imports and misplaced metadata never adopt another repository/branch's pusher.
        return Ok(None);
    }
    if record.publication.new == expected {
        return Ok(Some(record));
    }
    // Prepared, but not applied. Each preparation links directly to the last matching proof,
    // rather than chaining failed attempts and making reads depend on an unbounded scan.
    let Some(previous) = previous else {
        return Ok(None);
    };
    let (record, _) = read_tag(repo, previous)?;
    if record.publication.repository_id == repository_id
        && record.publication.reference == reference
        && record.publication.new == expected
    {
        Ok(Some(record))
    } else {
        Ok(None)
    }
}

/// Retain branch history only while its branch exists. Run before preparing a creation,
/// never between the preparation and publication of a batch. This bounds metadata refs by
/// the existing/proposed branch count even when users repeatedly delete and reuse names.
fn remove_absent_branches(repo: &Repository, apply: bool) -> Result<Vec<PublicationUpdate>> {
    let mut active = HashSet::new();
    for reference in repo.references_glob("refs/heads/*")? {
        if let Ok(name) = reference?.name() {
            active.insert(metadata_ref(name));
        }
    }
    let candidates = repo
        .references_glob("refs/codeyard/publications/*")?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut deletes = String::from("start\n");
    let mut changes = Vec::new();
    for reference in candidates {
        let Ok(name) = reference.name() else {
            continue;
        };
        let Some(suffix) = name.strip_prefix("refs/codeyard/publications/") else {
            continue;
        };
        if suffix.len() == 64
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            && !active.contains(name)
        {
            let expected = reference
                .target()
                .context("Publication ref must be direct")?;
            deletes.push_str(&format!("delete {name} {expected}\n"));
            changes.push(PublicationUpdate {
                reference: name.to_owned(),
                old: Some(expected.to_string()),
                new: None,
            });
        }
    }
    if apply && deletes != "start\n" {
        deletes.push_str("prepare\ncommit\n");
        let mut command = super::repository::process::git_command()?;
        command
            .arg("--git-dir")
            .arg(repo.path())
            .args(["update-ref", "--stdin"]);
        let output = super::repository::process::run_in_blocking(
            command,
            deletes.as_bytes(),
            1024 * 1024,
            std::time::Duration::from_secs(120),
        )?;
        ensure!(
            output.status.success(),
            "Git could not prune obsolete publication refs"
        );
    }
    Ok(changes)
}

pub fn read_at(
    path: &Path,
    repository_id: i64,
    reference: &str,
) -> Result<Option<PublicationRecord>> {
    read_current(&Repository::open_bare(path)?, repository_id, reference)
}

pub fn prepare_at(
    path: &Path,
    repository_id: i64,
    actor: PublicationActor,
    login: &str,
    updates: &[PublicationUpdate],
) -> Result<()> {
    prepare_at_mode(path, repository_id, actor, login, updates, true)?;
    Ok(())
}

/// Builds metadata objects without publishing refs; quorum stores journal these changes.
pub fn plan_at(
    path: &Path,
    repository_id: i64,
    actor: PublicationActor,
    login: &str,
    updates: &[PublicationUpdate],
) -> Result<Vec<PublicationUpdate>> {
    prepare_at_mode(path, repository_id, actor, login, updates, false)
}

fn prepare_at_mode(
    path: &Path,
    repository_id: i64,
    actor: PublicationActor,
    login: &str,
    updates: &[PublicationUpdate],
    apply: bool,
) -> Result<Vec<PublicationUpdate>> {
    fn oid(text: Option<&str>) -> Result<Option<Oid>> {
        Ok(text
            .map(Oid::from_str)
            .transpose()?
            .filter(|oid| !oid.is_zero()))
    }
    let repo = Repository::open_bare(path)?;
    let parsed = updates
        .iter()
        .map(|update| {
            Ok((
                update,
                oid(update.old.as_deref())?,
                oid(update.new.as_deref())?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut changes = if parsed
        .iter()
        .any(|(_, old, new)| old.is_none() && new.is_some())
    {
        remove_absent_branches(&repo, apply)?
    } else {
        Vec::new()
    };
    for (update, old, new) in parsed {
        if let Some(change) = prepare_object(
            &repo,
            repository_id,
            actor,
            login,
            &update.reference,
            old,
            new,
            apply,
        )? {
            // A recreated branch replaces the obsolete slot rather than deleting it first.
            changes.retain(|previous| previous.reference != change.reference);
            changes.push(change);
        }
    }
    Ok(changes)
}

pub(crate) fn read_current(
    repo: &Repository,
    repository_id: i64,
    reference: &str,
) -> Result<Option<PublicationRecord>> {
    checked_reference(reference)?;
    matching(
        repo,
        repository_id,
        reference,
        target(repo, &metadata_ref(reference))?,
        target(repo, reference)?,
    )
}

/// The metadata update precedes the branch CAS. It is not proof of a push until its `new`
/// identity matches the published branch. No-ops neither change the identity nor invent a push.
#[cfg(test)]
pub(crate) fn prepare(
    repo: &Repository,
    repository_id: i64,
    actor: PublicationActor,
    actor_login: &str,
    reference: &str,
    old: Option<Oid>,
    new: Option<Oid>,
) -> Result<()> {
    prepare_object(
        repo,
        repository_id,
        actor,
        actor_login,
        reference,
        old,
        new,
        true,
    )?;
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "The repository, actor, reference and both object ids are each needed to check one update."
)]
fn prepare_object(
    repo: &Repository,
    repository_id: i64,
    actor: PublicationActor,
    actor_login: &str,
    reference: &str,
    old: Option<Oid>,
    new: Option<Oid>,
    apply: bool,
) -> Result<Option<PublicationUpdate>> {
    checked_reference(reference)?;
    ensure!(
        repository_id > 0 && actor.id() > 0,
        "Invalid publication actor"
    );
    if matches!(actor, PublicationActor::User(_)) {
        ensure!(
            !actor_login.is_empty() && actor_login.len() <= 1024,
            "Invalid publication actor login"
        );
    }
    ensure!(
        target(repo, reference)? == old,
        super::RefConflict("Branch changed before publication".into())
    );
    if old == new {
        return Ok(None);
    }
    let name = metadata_ref(reference);
    let old_slot = target(repo, &name)?;
    let previous = matching(repo, repository_id, reference, old_slot, old)?;
    let parent = match previous {
        Some(record) => Oid::from_str(&record.object_id)?,
        None => repo.treebuilder(None)?.write()?,
    };
    let publication = BranchPublication {
        version: 1,
        id: Uuid::new_v4(),
        repository_id,
        actor,
        reference: reference.to_owned(),
        old: old.unwrap_or(Oid::ZERO_SHA1).to_string(),
        new: new.unwrap_or(Oid::ZERO_SHA1).to_string(),
    };
    let payload = serde_json::to_string(&publication)?;
    ensure!(
        payload.len() <= MAX_TAG_BYTES / 2,
        "Publication metadata exceeds its limit"
    );
    // Never turn a deployment credential into a user or disclose a private account email.
    let (tagger_name, email) = match actor {
        PublicationActor::User(_) => (
            actor_login.to_owned(),
            format!("{actor_login}@users.noreply"),
        ),
        PublicationActor::DeployKey(id) => (
            format!("deploy-key-{id}"),
            format!("deploy-key-{id}@credentials.codeyard.invalid"),
        ),
    };
    let signature = git2::Signature::now(&tagger_name, &email)?;
    let object = repo.find_object(parent, None)?;
    let oid = repo.tag_annotation_create(
        &format!("codeyard-publication-{}", publication.id),
        &object,
        &signature,
        &payload,
    )?;
    if apply {
        repo.reference_matching(
            &name,
            oid,
            true,
            old_slot.unwrap_or(Oid::ZERO_SHA1),
            "Prepare native branch publication",
        )?;
    }
    Ok(Some(PublicationUpdate {
        reference: name,
        old: old_slot.map(|oid| oid.to_string()),
        new: Some(oid.to_string()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Repository, Oid, Oid) {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(root.path()).unwrap();
        let (first, second) = {
            let tree = repo
                .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
                .unwrap();
            let signature = git2::Signature::now("Code author", "code@example.invalid").unwrap();
            let first = repo
                .commit(None, &signature, &signature, "first", &tree, &[])
                .unwrap();
            let parent = repo.find_commit(first).unwrap();
            let second = repo
                .commit(None, &signature, &signature, "second", &tree, &[&parent])
                .unwrap();
            (first, second)
        };
        (root, repo, first, second)
    }

    fn run_git(path: &Path, args: &[&str]) -> String {
        use crate::repository::process;
        let mut command = process::std_command(&process::executable().unwrap());
        let output = command.current_dir(path).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn planned_publications_do_not_move_refs() {
        let (root, repo, first, second) = fixture();
        let reference = "refs/heads/main";
        repo.reference(reference, first, true, "initial").unwrap();
        let changes = plan_at(
            root.path(),
            1,
            PublicationActor::User(7),
            "owner",
            &[PublicationUpdate {
                reference: reference.into(),
                old: Some(first.to_string()),
                new: Some(second.to_string()),
            }],
        )
        .unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].reference, metadata_ref(reference));
        assert_eq!(changes[0].old, None);
        assert!(repo.find_reference(&changes[0].reference).is_err());
        assert_eq!(
            repo.find_reference(reference).unwrap().target(),
            Some(first)
        );
        repo.find_tag(Oid::from_str(changes[0].new.as_deref().unwrap()).unwrap())
            .unwrap();
    }

    #[test]
    fn another_branch_cannot_adopt_a_matching_tip_or_previous_publication() {
        let (_root, repo, first, second) = fixture();
        let main = "refs/heads/main";
        let other = "refs/heads/other";
        prepare(
            &repo,
            1,
            PublicationActor::User(7),
            "alice",
            main,
            None,
            Some(first),
        )
        .unwrap();
        repo.reference(main, first, false, "initial").unwrap();
        repo.reference(other, first, false, "other").unwrap();
        let copy_slot = || {
            let slot = repo.refname_to_id(&metadata_ref(main)).unwrap();
            repo.reference(&metadata_ref(other), slot, true, "misplaced metadata")
                .unwrap();
        };
        copy_slot();
        assert!(read_current(&repo, 1, other).unwrap().is_none());
        prepare(
            &repo,
            1,
            PublicationActor::User(8),
            "bob",
            main,
            Some(first),
            Some(second),
        )
        .unwrap();
        copy_slot();
        assert!(read_current(&repo, 1, other).unwrap().is_none());
        prepare(
            &repo,
            1,
            PublicationActor::User(9),
            "carol",
            other,
            Some(first),
            Some(second),
        )
        .unwrap();
        repo.reference(other, second, true, "published").unwrap();
        assert_eq!(
            read_current(&repo, 1, other)
                .unwrap()
                .unwrap()
                .publication
                .actor,
            PublicationActor::User(9)
        );
    }

    #[test]
    fn publication_history_survives_gc_without_creating_commits_or_contributors() {
        let (root, repo, first, second) = fixture();
        let reference = "refs/heads/main";
        prepare(
            &repo,
            1,
            PublicationActor::User(7),
            "alice",
            reference,
            None,
            Some(first),
        )
        .unwrap();
        repo.reference(reference, first, false, "initial").unwrap();
        let mut proofs = vec![
            read_current(&repo, 1, reference)
                .unwrap()
                .unwrap()
                .object_id,
        ];
        let mut old = first;
        for step in 0..99 {
            let new = if old == first { second } else { first };
            let (actor, login) = if step % 2 == 0 {
                (8, "bob")
            } else {
                (7, "alice")
            };
            prepare(
                &repo,
                1,
                PublicationActor::User(actor),
                login,
                reference,
                Some(old),
                Some(new),
            )
            .unwrap();
            repo.reference(reference, new, true, "move").unwrap();
            proofs.push(
                read_current(&repo, 1, reference)
                    .unwrap()
                    .unwrap()
                    .object_id,
            );
            old = new;
        }
        assert_eq!(old, second);
        drop(repo);
        run_git(root.path(), &["gc", "--prune=now"]);
        assert_eq!(
            run_git(root.path(), &["rev-list", "--count", "--all"]).trim(),
            "2"
        );
        let contributors = run_git(root.path(), &["shortlog", "-sne", "--all"]);
        assert!(contributors.contains("Code author <code@example.invalid>"));
        assert!(!contributors.contains("alice") && !contributors.contains("bob"));
        let repo = Repository::open_bare(root.path()).unwrap();
        for object in &proofs {
            repo.find_tag(Oid::from_str(object).unwrap()).unwrap();
        }
        assert_eq!(
            read_current(&repo, 1, reference)
                .unwrap()
                .unwrap()
                .object_id,
            *proofs.last().unwrap()
        );
    }

    #[test]
    fn deleted_branch_metadata_is_bounded_and_deploy_keys_are_not_users() {
        let (root, repo, first, _) = fixture();
        let reference = "refs/heads/old";
        let creation = PublicationUpdate {
            reference: reference.into(),
            old: None,
            new: Some(first.to_string()),
        };
        prepare_at(
            root.path(),
            1,
            PublicationActor::User(7),
            "alice",
            &[creation],
        )
        .unwrap();
        repo.reference(reference, first, false, "initial").unwrap();
        let before = read_current(&repo, 1, reference).unwrap().unwrap();
        let deletion = PublicationUpdate {
            reference: reference.into(),
            old: Some(first.to_string()),
            new: None,
        };
        prepare_at(
            root.path(),
            1,
            PublicationActor::DeployKey(23),
            "",
            &[deletion],
        )
        .unwrap();
        assert_eq!(read_current(&repo, 1, reference).unwrap().unwrap(), before);
        repo.find_reference(reference).unwrap().delete().unwrap();
        let deletion = read_current(&repo, 1, reference).unwrap().unwrap();
        assert_eq!(deletion.publication.actor, PublicationActor::DeployKey(23));
        assert_eq!(deletion.publication.actor.user_id(), None);
        let creation = PublicationUpdate {
            reference: "refs/heads/new".into(),
            old: None,
            new: Some(first.to_string()),
        };
        prepare_at(
            root.path(),
            1,
            PublicationActor::User(8),
            "bob",
            &[creation],
        )
        .unwrap();
        assert_eq!(
            repo.find_reference(&metadata_ref(reference))
                .err()
                .unwrap()
                .code(),
            git2::ErrorCode::NotFound
        );
        assert_eq!(
            repo.references_glob("refs/codeyard/publications/*")
                .unwrap()
                .count(),
            1
        );
        repo.reference("refs/heads/new", first, false, "new")
            .unwrap();
        let current = read_current(&repo, 1, "refs/heads/new").unwrap().unwrap();
        assert_eq!(current.publication.actor, PublicationActor::User(8));
        assert_ne!(current.publication.id, before.publication.id);
        assert!(read_current(&repo, 2, "refs/heads/new").unwrap().is_none());
        let advertised = run_git(
            root.path(),
            &["upload-pack", "--stateless-rpc", "--advertise-refs", "."],
        );
        assert!(advertised.contains("refs/heads/new"));
        assert!(!advertised.contains("refs/codeyard/"));
    }

    #[test]
    fn failed_publications_do_not_invent_pushers_or_revive_an_old_publication() {
        let (_root, repo, first, second) = fixture();
        let reference = "refs/heads/main";
        use PublicationActor::User;
        prepare(&repo, 1, User(7), "alice", reference, None, Some(first)).unwrap();
        assert!(read_current(&repo, 1, reference).unwrap().is_none());
        repo.reference(reference, first, false, "publish first")
            .unwrap();
        let original = read_current(&repo, 1, reference).unwrap().unwrap();
        assert_eq!(original.publication.actor, User(7));
        prepare(
            &repo,
            1,
            User(8),
            "bob",
            reference,
            Some(first),
            Some(second),
        )
        .unwrap();
        assert_eq!(
            read_current(&repo, 1, reference).unwrap().unwrap(),
            original
        );
        prepare(
            &repo,
            1,
            User(9),
            "carol",
            reference,
            Some(first),
            Some(second),
        )
        .unwrap();
        assert_eq!(
            read_current(&repo, 1, reference).unwrap().unwrap(),
            original
        );
        repo.reference(reference, second, true, "publish second")
            .unwrap();
        assert_eq!(
            read_current(&repo, 1, reference)
                .unwrap()
                .unwrap()
                .publication
                .actor,
            User(9)
        );
        prepare(
            &repo,
            1,
            User(7),
            "alice",
            reference,
            Some(second),
            Some(first),
        )
        .unwrap();
        repo.reference(reference, first, true, "return to first")
            .unwrap();
        let returned = read_current(&repo, 1, reference).unwrap().unwrap();
        assert_eq!(returned.publication.actor, User(7));
        assert_ne!(returned.publication.id, original.publication.id);
        prepare(
            &repo,
            1,
            User(7),
            "alice",
            reference,
            Some(first),
            Some(first),
        )
        .unwrap();
        assert_eq!(
            read_current(&repo, 1, reference).unwrap().unwrap(),
            returned
        );
        assert!(prepare(
            &repo,
            1,
            User(8),
            "bob",
            reference,
            Some(second),
            Some(first)
        )
        .is_err());
        assert_eq!(
            read_current(&repo, 1, reference).unwrap().unwrap(),
            returned
        );
    }
}
