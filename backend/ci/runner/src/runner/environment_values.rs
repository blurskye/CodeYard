//! The core binds job IDs to environment scopes; engines never infer scope from global values.
use super::{Contexts, Masker, Value};
use crate::values;
use anyhow::{ensure, Result};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::Path,
};

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EnvironmentValues {
    jobs: BTreeMap<String, String>,
    environments: BTreeMap<String, Values>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Values {
    secrets: BTreeMap<String, String>,
    variables: BTreeMap<String, String>,
}
fn valid(values: &BTreeMap<String, String>, budget: usize) -> bool {
    let mut names = BTreeSet::new();
    values.len() <= values::MAX_SCOPE_VALUES
        && values.iter().all(|(name, value)| {
            crate::values::validate(name, value).is_ok() && names.insert(name.to_ascii_uppercase())
        })
        && values
            .iter()
            .map(|(name, value)| name.len() + value.len())
            .sum::<usize>()
            <= budget
}
impl EnvironmentValues {
    pub(super) fn load(path: Option<&Path>, masker: &Masker) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let invalid = || anyhow::anyhow!("Invalid environment values file");
        let file = std::fs::File::open(path).map_err(|_| invalid())?;
        let mut bytes = Vec::new();
        file.take(values::MAX_ENVIRONMENT_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| invalid())?;
        ensure!(
            bytes.len() <= values::MAX_ENVIRONMENT_FILE_BYTES,
            "Environment values file is too large"
        );
        let values: Self = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        ensure!(
            values.jobs.len() <= 1024 && values.environments.len() <= 1024,
            "Too many environment value bindings"
        );
        for (job, scope) in &values.jobs {
            ensure!(
                // Called workflows' jobs are keyed by their run-wide ID, such as `call/deploy`.
                !job.is_empty() && job.len() <= 1024 && values.environments.contains_key(scope),
                "Invalid environment value binding"
            );
        }
        for (name, scope) in &values.environments {
            ensure!(
                !name.is_empty()
                    && name.len() <= 2048
                    && valid(&scope.secrets, values::MAX_SECRET_BYTES)
                    && valid(&scope.variables, values::MAX_VARIABLE_BYTES),
                "Invalid environment value scope"
            );
            for secret in scope.secrets.values() {
                masker.add(secret);
            }
        }
        Ok(values)
    }
    pub(super) fn apply(&self, job: &str, contexts: &mut Contexts) {
        let Some(scope) = self
            .jobs
            .get(job)
            .and_then(|scope| self.environments.get(scope))
        else {
            return;
        };
        for (name, values) in [("secrets", &scope.secrets), ("vars", &scope.variables)] {
            let target = contexts
                .values
                .entry(name.to_owned())
                .or_insert_with(|| Value::Object(BTreeMap::new()));
            if let Value::Object(target) = target {
                for (key, value) in values {
                    target.retain(|name, _| !name.eq_ignore_ascii_case(key));
                    target.insert(key.clone(), Value::String(value.clone()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    fn load(value: serde_json::Value, masker: &Masker) -> Result<EnvironmentValues> {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(value.to_string().as_bytes()).unwrap();
        EnvironmentValues::load(Some(file.path()), masker)
    }
    #[test]
    fn environment_values_are_job_scoped_and_mask_shadowed_secrets() {
        let masker = Masker::default();
        masker.add("repository-secret");
        let values=load(serde_json::json!({"jobs":{"deploy":"i\u{307}"},"environments":{"i\u{307}":{"secrets":{"TOKEN":"environment-secret","ONLY_HERE":"scoped-secret"},"variables":{"COLOR":"visible-variable"}}}}),&masker).unwrap();
        let base = Contexts {
            values: BTreeMap::from([
                (
                    "secrets".into(),
                    Value::Object(BTreeMap::from([(
                        "token".into(),
                        Value::String("repository-secret".into()),
                    )])),
                ),
                ("vars".into(), Value::Object(BTreeMap::new())),
            ]),
        };
        let mut deploy = base.clone();
        values.apply("deploy", &mut deploy);
        assert_eq!(
            deploy.values["vars"],
            super::super::object(&BTreeMap::from([(
                "COLOR".into(),
                "visible-variable".into()
            )]))
        );
        let mut ordinary = base.clone();
        values.apply("ordinary", &mut ordinary);
        assert_eq!(ordinary.values, base.values);
        // A called workflow's job named deploy has its own identity, not the root's binding.
        let mut nested = base.clone();
        values.apply("call/deploy", &mut nested);
        assert_eq!(nested.values, base.values);
        assert_eq!(
            deploy.values["secrets"],
            super::super::object(&BTreeMap::from([
                ("TOKEN".into(), "environment-secret".into()),
                ("ONLY_HERE".into(), "scoped-secret".into())
            ]))
        );
        assert_eq!(
            masker.mask("repository-secret environment-secret visible-variable"),
            "*** *** visible-variable"
        );
    }
    #[test]
    fn environment_values_reject_invalid_files_without_echoing_values() {
        let marker = "private-environment-marker";
        let result = load(
            serde_json::json!({"jobs":{},"environments":{"prod":{"secrets":marker,"variables":{}}}}),
            &Masker::default(),
        );
        let Err(error) = result else {
            panic!("invalid environment file was accepted")
        };
        assert!(!format!("{error:#}").contains(marker));
        assert!(load(
            serde_json::json!({"jobs":{"deploy":"missing"},"environments":{}}),
            &Masker::default()
        )
        .is_err());
        assert!(load(serde_json::json!({"jobs":{"deploy":"prod"},"environments":{"prod":{"secrets":{"GITHUB_TOKEN":"forged"},"variables":{}}}}),&Masker::default()).is_err());
    }
}
