use chrono::DateTime;
use chrono::Utc;
use codeyard_actions::values::MAX_VARIABLE_BYTES;
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct Variable {
    pub name: String,
    pub value: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum VariableError {
    #[error("Variable database unavailable")]
    Database(#[from] sqlx::Error),
    #[error("Variable authority unavailable")]
    Authority(#[source] anyhow::Error),
    #[error("Variable access denied")]
    Denied,
    #[error("Variable not found")]
    NotFound,
    #[error("Variable name already exists")]
    Exists,
    #[error("Variable name is invalid or reserved")]
    InvalidName,
    #[error("Variable value exceeds 48 KiB or contains NUL")]
    InvalidValue,
    #[error("Repository variables exceed the count or size limit")]
    Limit,
}

pub fn validate(name: &str, value: &str) -> Result<(), VariableError> {
    use codeyard_actions::values::InvalidValue;
    codeyard_actions::values::validate(name, value).map_err(|error| match error {
        InvalidValue::Name | InvalidValue::ReservedName => VariableError::InvalidName,
        InvalidValue::Nul | InvalidValue::TooLarge => VariableError::InvalidValue,
    })
}

pub fn validate_all(variables: &[(String, String)]) -> Result<(), VariableError> {
    validate_set(variables, 500)
}

/// Organization and repository variables still share a 256 KiB workflow allowance.
pub fn validate_workflow_variables(variables: &[(String, String)]) -> Result<(), VariableError> {
    validate_set(variables, 1500)
}

fn validate_set(variables: &[(String, String)], limit: usize) -> Result<(), VariableError> {
    if variables.len() > limit {
        return Err(VariableError::Limit);
    }
    let mut bytes = 0;
    for (name, value) in variables {
        validate(name, value)?;
        bytes += name.len() + value.len();
    }
    if bytes > MAX_VARIABLE_BYTES {
        return Err(VariableError::Limit);
    }
    Ok(())
}

pub(crate) fn ensure_room(
    count: i64,
    bytes: i64,
    name: &str,
    value: &str,
) -> Result<(), VariableError> {
    if count >= 500 || bytes + (name.len() + value.len()) as i64 > MAX_VARIABLE_BYTES as i64 {
        return Err(VariableError::Limit);
    }
    Ok(())
}

native_operation!(crate::postgres::variables::list,
    crate::sqlite::variables::list,
    fn list(pool: &DbPool, actor: i64, credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>, repo: i64) -> Result<Vec<Variable>, VariableError>);

native_operation!(crate::postgres::variables::save,
    crate::sqlite::variables::save,
    fn save(pool: &DbPool, actor: i64, credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>, repo: i64, previous: Option<&str>, name: &str, value: Option<&str>) -> Result<(), VariableError>);

native_operation!(crate::postgres::variables::delete,
    crate::sqlite::variables::delete,
    fn delete(pool: &DbPool, actor: i64, credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>, repo: i64, name: &str) -> Result<(), VariableError>);

native_operation!(crate::postgres::variables::for_run,
    crate::sqlite::variables::for_run,
    fn for_run(pool: &DbPool, repo: i64) -> Result<Vec<(String, String)>, VariableError>);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_have_name_value_count_and_byte_limits() {
        for name in ["", "0NAME", "BAD-NAME", "GITHUB_TOKEN"] {
            assert!(matches!(
                validate(name, ""),
                Err(VariableError::InvalidName)
            ));
        }
        for value in ["x\0y".into(), "x".repeat(49153)] {
            assert!(matches!(
                validate("NAME", &value),
                Err(VariableError::InvalidValue)
            ));
        }
        let set = |count: usize, size: usize| {
            (0..count)
                .map(|i| (format!("NAME_{i}"), "x".repeat(size)))
                .collect::<Vec<_>>()
        };
        validate_all(&set(500, 0)).unwrap();
        assert!(matches!(
            validate_all(&set(501, 0)),
            Err(VariableError::Limit)
        ));
        validate_all(&set(5, 49152)).unwrap();
        assert!(matches!(
            validate_all(&set(6, 49152)),
            Err(VariableError::Limit)
        ));
        ensure_room(499, 0, "NAME", "").unwrap();
        assert!(matches!(
            ensure_room(500, 0, "NAME", ""),
            Err(VariableError::Limit)
        ));
        assert!(matches!(
            ensure_room(0, 262144, "NAME", ""),
            Err(VariableError::Limit)
        ));
    }
}
