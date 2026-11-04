use anyhow::{bail, ensure, Result};
use codeyard_actions::workflow::PERMISSION_SCOPES as SCOPES;
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::collections::BTreeMap;
#[derive(Clone, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permissions(pub BTreeMap<String, u8>);

impl Permissions {
    pub fn allows(&self, scope: &str, write: bool) -> bool {
        self.0.get(scope).copied().unwrap_or(0) >= if write { 2 } else { 1 }
    }

    pub fn parse(value: Option<&Value>, default: &Self, fork: bool) -> Result<Self> {
        let mut permissions = match value {
            None => default.clone(),
            Some(Value::String(value)) if value == "read-all" || value == "write-all" => Self(
                SCOPES
                    .iter()
                    .map(|scope| {
                        (
                            (*scope).into(),
                            match (*scope, value.as_str()) {
                                ("models" | "metadata" | "vulnerability-alerts", _) => 1,
                                ("id-token", "read-all") => 0,
                                (_, "write-all") => 2,
                                _ => 1,
                            },
                        )
                    })
                    .collect(),
            ),
            Some(Value::Mapping(values)) => {
                let mut result = BTreeMap::new();
                for (name, level) in values {
                    let name = name.as_str().unwrap_or("");
                    ensure!(SCOPES.contains(&name), "unknown workflow permission");
                    let level = match level.as_str() {
                        Some("none") => 0,
                        Some("read") if name != "id-token" => 1,
                        Some("write")
                            if codeyard_actions::workflow::permission_level_allowed(
                                name, "write",
                            ) =>
                        {
                            2
                        }
                        _ => bail!("invalid workflow permission"),
                    };
                    result.insert(name.to_owned(), level);
                }
                Self(result)
            }
            _ => bail!("invalid workflow permissions"),
        };
        permissions.0.insert("metadata".into(), 1);
        if fork {
            for level in permissions.0.values_mut() {
                *level = (*level).min(1);
            }
            permissions.0.insert("id-token".into(), 0);
        }
        Ok(permissions)
    }

    pub fn jobs(bytes: &[u8], permissive: bool, fork: bool) -> Result<BTreeMap<String, Self>> {
        let workflow = crate::workflow_yaml::parse_yaml(bytes)?;
        Self::jobs_from_yaml(&workflow, permissive, fork)
    }

    pub fn jobs_from_yaml(
        workflow: &Value,
        permissive: bool,
        fork: bool,
    ) -> Result<BTreeMap<String, Self>> {
        let default = if permissive {
            Self(
                SCOPES
                    .iter()
                    .filter(|scope| **scope != "id-token")
                    .map(|scope| {
                        (
                            (*scope).into(),
                            if matches!(*scope, "models" | "metadata" | "vulnerability-alerts") {
                                1
                            } else {
                                2
                            },
                        )
                    })
                    .collect(),
            )
        } else {
            Self(BTreeMap::from([
                ("contents".into(), 1),
                ("packages".into(), 1),
                ("metadata".into(), 1),
            ]))
        };
        let workflow_default = Self::parse(workflow.get("permissions"), &default, fork)?;
        let jobs = workflow
            .get("jobs")
            .and_then(Value::as_mapping)
            .ok_or_else(|| anyhow::anyhow!("workflow has no jobs"))?;
        jobs.iter()
            .map(|(id, job)| {
                Ok((
                    id.as_str()
                        .ok_or_else(|| anyhow::anyhow!("invalid job id"))?
                        .into(),
                    Self::parse(job.get("permissions"), &workflow_default, fork)?,
                ))
            })
            .collect()
    }

    pub fn intersection(jobs: impl Iterator<Item = Self>) -> Self {
        let mut jobs = jobs;
        let Some(mut result) = jobs.next() else {
            return Self::default();
        };
        for job in jobs {
            for (scope, level) in &mut result.0 {
                *level = (*level).min(job.0.get(scope).copied().unwrap_or(0));
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn artifact_metadata_permissions_follow_workflow_replacement_and_fork_downgrades() {
        let bytes = b"permissions: {artifact-metadata: write}\njobs:\n  writer: {}\n  reader: {permissions: {artifact-metadata: read}}";
        let jobs = Permissions::jobs(bytes, false, false).unwrap();
        assert!(jobs["writer"].allows("artifact-metadata", true));
        assert!(jobs["reader"].allows("artifact-metadata", false));
        assert!(!jobs["reader"].allows("artifact-metadata", true));
        let jobs = Permissions::jobs(bytes, false, true).unwrap();
        assert!(!jobs["writer"].allows("artifact-metadata", true));
    }

    #[test]
    fn job_overrides_and_intersections_never_add_permissions() {
        let bytes = b"permissions: {contents: write, issues: write}\njobs:\n  first: {}\n  second: {permissions: {contents: read}}";
        let jobs = Permissions::jobs(bytes, false, false).unwrap();
        assert!(jobs["first"].allows("issues", true));
        assert!(!jobs["second"].allows("issues", false));
        let least = Permissions::intersection(jobs.into_values());
        assert!(least.allows("contents", false));
        assert!(!least.allows("contents", true));
        assert!(!least.allows("issues", false));
        for job in Permissions::jobs(bytes, true, true).unwrap().values() {
            assert!(!job.allows("contents", true));
            assert!(!job.allows("id-token", true));
        }
        assert!(
            Permissions::jobs(b"jobs: {test: {}}", false, false).unwrap()["test"]
                .allows("contents", false)
        );
        assert!(
            !Permissions::jobs(b"permissions: {}\njobs: {test: {}}", true, false).unwrap()["test"]
                .0
                .contains_key("contents")
        );
        assert!(
            Permissions::jobs(b"permissions: {invalid: write}\njobs: {}", false, false).is_err()
        );
    }
}
