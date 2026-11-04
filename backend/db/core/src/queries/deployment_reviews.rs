use anyhow::Result;
pub(crate) fn execution_state(status: &str) -> Result<&'static str> {
    Ok(match status {
        "running" => "in_progress",
        "success" => "success",
        "failed" => "failure",
        "cancelled" => "error",
        _ => anyhow::bail!(crate::queries::action_settings::Refusal::Invalid),
    })
}
