use anyhow::Result;
#[derive(Clone, Debug)]
pub struct Configuration {
    pub group: String,
    pub cancel_in_progress: bool,
}

impl Configuration {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            codeyard_actions::workflow::valid_concurrency_group(&self.group),
            crate::ci_policy::CiAdmissionRefusal::Invalid("Invalid concurrency group")
        );
        Ok(())
    }
}
