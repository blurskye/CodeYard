use anyhow::ensure;
use anyhow::Result;
use codeyard_database::models::RecoveryView;
use codeyard_database::models::RefChange;
use codeyard_database::models::RefValue;
use codeyard_database::models::WriteRecord;
use codeyard_database::models::WriteSettings;
use codeyard_database::native_operation;
use codeyard_database::DbPool;
use sha2::Digest;
use sha2::Sha256;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

// Explicit legacy mode is effective only before any repository has a quorum term.
// An inconsistent downgrade remains fenced instead of opening old, unjournalled paths.
pub(crate) const EFFECTIVE_SETTINGS: &str = "SELECT CASE WHEN write_quorum='1' AND NOT EXISTS(SELECT 1 FROM repository_write_control) THEN NULL ELSE write_quorum END AS write_quorum,write_timeout_seconds FROM node_configuration WHERE id=1";

pub(crate) const MAX_VOTERS: usize = 999;
pub(crate) const MAX_FAMILY: usize = 10_000;
pub(crate) const MAX_REFS: usize = codeyard_git::repository::MAX_MIRROR_REFS;
const MAX_REF_BYTES: usize = codeyard_git::repository::MAX_MIRROR_REF_BYTES;

pub(crate) const MOVING: &str =
    "This account is moving to another cell; try again in a few minutes";

/// Whether a move row in this state refuses quorum writes. Quorum tables have no move
/// guard trigger, so begin and decide check the owner's move inside their transaction.
/// A receiving copy is the move's own publication and is not refused.
pub(crate) fn move_blocks_writes(state: &str, unexpired: bool) -> bool {
    matches!(state, "switched" | "released") || (state == "frozen" && unexpired)
}

native_operation!(crate::postgres::cluster::writes::write_settings,
    crate::sqlite::cluster::writes::write_settings,
    fn write_settings(pool: &DbPool) -> Result<WriteSettings>);

native_operation!(crate::postgres::cluster::writes::bootstrap_writes,
    crate::sqlite::cluster::writes::bootstrap_writes,
    /// Bootstraps once, under a write pause, from a verified complete primary. Serving stays off.
    fn bootstrap_writes(pool: &DbPool, repo: i64, primary: i64, voters: &[i64], refs: &[RefValue]) -> Result<()>);
native_operation!(crate::postgres::cluster::writes::enable_initialized_repository,
    crate::sqlite::cluster::writes::enable_initialized_repository,
    /// Enables a fresh empty term only after all voters have durably reconstructed it.
    fn enable_initialized_repository(pool: &DbPool, repo: i64, primary: i64) -> Result<bool>);
native_operation!(crate::postgres::cluster::writes::begin_write,
    crate::sqlite::cluster::writes::begin_write,
    fn begin_write(pool: &DbPool, repo: i64, primary: i64, term: i64, id: &str, refs: &[RefChange]) -> Result<WriteRecord>);
native_operation!(crate::postgres::cluster::writes::vote_write,
    crate::sqlite::cluster::writes::vote_write,
    /// A caller records only its own durably prepared vote. Duplicates return false.
    fn vote_write(pool: &DbPool, id: &str, node: i64, term: i64) -> Result<bool>);
native_operation!(crate::postgres::cluster::writes::decide_write,
    crate::sqlite::cluster::writes::decide_write,
    /// Failed commit preconditions decide abort. An existing decision is returned unchanged.
    fn decide_write(pool: &DbPool, id: &str, primary: i64, term: i64, commit: bool) -> Result<WriteRecord>);
native_operation!(crate::postgres::cluster::writes::load_write,
    crate::sqlite::cluster::writes::load_write,
    fn load_write(pool: &DbPool, id: &str) -> Result<Option<WriteRecord>>);
native_operation!(crate::postgres::cluster::writes::recovery_view,
    crate::sqlite::cluster::writes::recovery_view,
    fn recovery_view(pool: &DbPool, repo: i64) -> Result<RecoveryView>);
native_operation!(crate::postgres::cluster::writes::mark_write_applied,
    crate::sqlite::cluster::writes::mark_write_applied,
    fn mark_write_applied(pool: &DbPool, repo: i64, node: i64, term: i64, version: i64) -> Result<bool>);
native_operation!(crate::postgres::cluster::writes::finish_write,
    crate::sqlite::cluster::writes::finish_write,
    /// Releases the unfinished slot after abort cleanup or durable primary/quorum publication.
    fn finish_write(pool: &DbPool, id: &str, current_term: i64) -> Result<bool>);
native_operation!(crate::postgres::cluster::writes::switch_write_family,
    crate::sqlite::cluster::writes::switch_write_family,
    /// Fences the whole family, aborts pending work and requires reconstruction before serving.
    fn switch_write_family(pool: &DbPool, repo: i64, expected_primary: i64, terms: &[(i64, i64)], target: i64, voters: &[i64]) -> Result<bool>);
native_operation!(crate::postgres::cluster::writes::enable_write_family,
    crate::sqlite::cluster::writes::enable_write_family,
    fn enable_write_family(pool: &DbPool, terms: &[(i64, i64)]) -> Result<bool>);

native_operation!(crate::postgres::cluster::writes::write_family,
    crate::sqlite::cluster::writes::write_family,
    fn write_family(pool: &DbPool, repo: i64) -> Result<Vec<(i64, Option<i64>, i64)>>);

native_operation!(crate::postgres::cluster::writes::held_voters,
    crate::sqlite::cluster::writes::held_voters,
    /// Bounded voting holdings for takeover, independently of asynchronous copy policy.
    fn held_voters(pool: &DbPool, node: i64) -> Result<Vec<codeyard_database::models::HeldReplica>>);

native_operation!(crate::postgres::cluster::writes::write_family_terms,
    crate::sqlite::cluster::writes::write_family_terms,
    /// Current family terms for enabling already-reconstructed copies in one transaction.
    fn write_family_terms(pool: &DbPool, repo: i64) -> Result<Vec<(i64, i64)>>);

pub fn validate_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 36 && uuid::Uuid::parse_str(id).is_ok(),
        "invalid write id"
    );
    Ok(())
}

pub(crate) fn validate_voters(primary: i64, voters: &[i64]) -> Result<()> {
    ensure!(
        !voters.is_empty() && voters.len() <= MAX_VOTERS,
        "invalid voter count"
    );
    let unique: BTreeSet<_> = voters.iter().copied().collect();
    ensure!(
        unique.len() == voters.len()
            && unique.contains(&primary)
            && unique.iter().all(|id| *id > 0),
        "invalid voters"
    );
    Ok(())
}

pub(crate) fn validate_terms(terms: &[(i64, i64)]) -> Result<()> {
    ensure!(
        !terms.is_empty() && terms.len() <= MAX_FAMILY,
        "invalid family size"
    );
    let unique: BTreeSet<_> = terms.iter().map(|(id, _)| *id).collect();
    ensure!(
        unique.len() == terms.len() && terms.iter().all(|(id, term)| *id > 0 && *term > 0),
        "invalid family terms"
    );
    Ok(())
}

pub fn required_votes(setting: &str, count: usize) -> Result<i64> {
    ensure!(count > 0 && count <= MAX_VOTERS, "invalid voter count");
    ensure!(
        count > 1 || setting == "1",
        "one voter requires write_quorum=1"
    );
    let required = if setting == "majority" {
        count as i64 / 2 + 1
    } else {
        ensure!(
            (1..=3).contains(&setting.len())
                && !setting.starts_with('0')
                && setting.bytes().all(|b| b.is_ascii_digit()),
            "invalid write quorum"
        );
        setting.parse::<i64>()?
    };
    ensure!(
        required > 0 && required <= count as i64,
        "write quorum exceeds fixed voters"
    );
    Ok(required)
}

fn validate_ref(name: &str, kind: &str, target: Option<&str>) -> Result<()> {
    ensure!(
        name.len() <= 4096
            && (name == "HEAD"
                || (name.starts_with("refs/") && git2::Reference::is_valid_name(name))),
        "invalid reference name"
    );
    ensure!(matches!(kind, "oid" | "symbolic"), "invalid reference kind");
    if let Some(target) = target {
        ensure!(target.len() <= 4096, "reference target too long");
        if kind == "oid" {
            ensure!(
                target.len() == 40
                    && target
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    && target.bytes().any(|b| b != b'0'),
                "invalid reference oid"
            );
        } else {
            ensure!(
                target.starts_with("refs/") && git2::Reference::is_valid_name(target),
                "invalid symbolic reference"
            );
        }
    }
    Ok(())
}

pub fn validate_values(refs: &[RefValue]) -> Result<()> {
    ensure!(refs.len() <= MAX_REFS, "too many references");
    let mut names = BTreeSet::new();
    let mut bytes = 0;
    for value in refs {
        validate_ref(&value.name, &value.kind, Some(&value.target))?;
        ensure!(names.insert(&value.name), "duplicate reference");
        bytes += value.name.len() + value.target.len();
    }
    ensure!(bytes <= MAX_REF_BYTES, "reference manifest too large");
    Ok(())
}

pub fn validate_changes(refs: &[RefChange]) -> Result<()> {
    ensure!(
        !refs.is_empty() && refs.len() <= MAX_REFS,
        "invalid reference count"
    );
    let mut names = BTreeSet::new();
    let mut bytes = 0;
    for change in refs {
        validate_ref(&change.name, &change.kind, change.old_target.as_deref())?;
        validate_ref(&change.name, &change.kind, change.new_target.as_deref())?;
        ensure!(
            names.insert(&change.name) && change.old_target != change.new_target,
            "duplicate or unchanged reference"
        );
        bytes += change.name.len()
            + change.old_target.as_ref().map_or(0, String::len)
            + change.new_target.as_ref().map_or(0, String::len);
    }
    ensure!(bytes <= MAX_REF_BYTES, "reference manifest too large");
    Ok(())
}

pub(crate) fn changed_refs(current: Vec<RefValue>, changes: &[RefChange]) -> Result<Vec<RefValue>> {
    let mut refs: BTreeMap<_, _> = current
        .into_iter()
        .map(|value| (value.name.clone(), value))
        .collect();
    for change in changes {
        let old = refs.get(&change.name);
        ensure!(
            old.map(|value| value.target.as_str()) == change.old_target.as_deref()
                && old.is_none_or(|value| value.kind == change.kind),
            "reference old value mismatch"
        );
        if let Some(target) = &change.new_target {
            refs.insert(
                change.name.clone(),
                RefValue {
                    name: change.name.clone(),
                    kind: change.kind.clone(),
                    target: target.clone(),
                },
            );
        } else {
            refs.remove(&change.name);
        }
    }
    let refs: Vec<_> = refs.into_values().collect();
    validate_values(&refs)?;
    Ok(refs)
}

pub fn manifest_hash(
    repo: i64,
    primary: i64,
    term: i64,
    base: i64,
    refs: &[RefChange],
) -> Result<String> {
    let mut refs: Vec<_> = refs.iter().collect();
    refs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&(
        repo, primary, term, base, refs,
    ))?)))
}
