//! A base owns its pull request; the head cell retains explicit publication consent.
use anyhow::{ensure, Result};
use codeyard_model::cells::{Object, ObjectKind};
use codeyard_support::cells::Prefix;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub base_repo_id: i64,
    pub number: i32,
    pub head_repo_id: i64,
    pub head_branch: String,
}
impl Binding {
    pub fn validate(&self) -> Result<()> {
        Prefix::for_id(self.base_repo_id)?;
        Prefix::for_id(self.head_repo_id)?;
        ensure!(
            self.base_repo_id != self.head_repo_id
                && self.number > 0
                && self.head_branch.chars().count() <= 255
                && codeyard_git::branch_ref(&self.head_branch).is_some(),
            "Invalid cross-cell pull binding"
        );
        Ok(())
    }
    pub fn base(&self) -> Object {
        Object {
            kind: ObjectKind::Repository,
            id: self.base_repo_id,
        }
    }
    pub fn head(&self) -> Object {
        Object {
            kind: ObjectKind::Repository,
            id: self.head_repo_id,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub binding: Binding,
    pub publisher_id: i64,
    pub maintainer_edit_granted_by_id: Option<i64>,
    pub actor_id: Option<i64>,
}
impl Grant {
    pub fn validate(&self, binding: &Binding) -> Result<()> {
        self.binding.validate()?;
        ensure!(&self.binding == binding, "Pull grant changed its binding");
        Prefix::for_id(self.publisher_id)?;
        for id in self
            .maintainer_edit_granted_by_id
            .into_iter()
            .chain(self.actor_id)
        {
            Prefix::for_id(id)?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Push {
    pub base_repo_id: i64,
    pub head_repo_id: i64,
    pub version: i64,
}
impl Push {
    pub fn validate(&self) -> Result<()> {
        Prefix::for_id(self.base_repo_id)?;
        Prefix::for_id(self.head_repo_id)?;
        ensure!(
            self.base_repo_id != self.head_repo_id && self.version >= 0,
            "Invalid foreign push"
        );
        Ok(())
    }
    pub fn base(&self) -> Object {
        Object {
            kind: ObjectKind::Repository,
            id: self.base_repo_id,
        }
    }
    pub fn head(&self) -> Object {
        Object {
            kind: ObjectKind::Repository,
            id: self.head_repo_id,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub id: i64,
    pub change: Push,
}
impl Event {
    pub fn validate(&self) -> Result<()> {
        Prefix::for_id(self.id)?;
        self.change.validate()
    }
}
/// The listener checks the live sending key, both directory roots and an event's issuer.
pub trait Admission: Send + Sync {
    fn check<'a>(
        &'a self,
        source: Object,
        target: Object,
        event: Option<i64>,
    ) -> BoxFuture<'a, Result<Prefix>>;
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadRequest {
    pub binding: Binding,
    pub publisher_id: i64,
    pub maximum_bytes: u64,
    pub timeout_seconds: u64,
}
impl HeadRequest {
    pub fn validate(&self) -> Result<()> {
        self.binding.validate()?;
        Prefix::for_id(self.publisher_id)?;
        ensure!(
            (1..=64 * 1024 * 1024 * 1024).contains(&self.maximum_bytes)
                && (1..=300).contains(&self.timeout_seconds),
            "Invalid pull snapshot limits"
        );
        Ok(())
    }
    pub fn validate_snapshot(&self, snapshot: &super::repositories::Snapshot) -> Result<()> {
        self.validate()?;
        ensure!(
            snapshot.repository_id == self.binding.head_repo_id
                && snapshot.reader_id == self.publisher_id
                && snapshot.refs.len() <= 1
                && codeyard_git::branch_ref(&snapshot.default_branch).is_some(),
            "Invalid pull snapshot source"
        );
        for (name, oid) in &snapshot.refs {
            ensure!(
                name == &format!("refs/heads/{}", self.binding.head_branch)
                    && codeyard_git::is_full_commit_id(oid),
                "Unrequested pull snapshot reference"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Refresh {
    pub push: Push,
    pub after: i64,
}
impl Refresh {
    pub fn validate(&self) -> Result<()> {
        self.push.validate()?;
        ensure!(self.after >= 0, "Invalid pull refresh cursor");
        if self.after > 0 {
            Prefix::for_id(self.after)?;
        }
        Ok(())
    }
}
