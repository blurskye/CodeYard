use codeyard_database::queries::secrets::SecretsError;
pub fn validate_repository_access(add: &[i64], remove: &[i64]) -> Result<(), SecretsError> {
    if add.len() > 1000
        || remove.len() > 1000
        || add.iter().chain(remove).any(|id| *id <= 0)
        || add.iter().any(|id| remove.contains(id))
    {
        return Err(SecretsError::InvalidRepositorySelection);
    }
    Ok(())
}
