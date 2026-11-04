//! Public, credential-free scaling policy shared by settings forms and the controller.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScalingPolicy {
    pub role: String,
    pub minimum: i32,
    pub maximum: i32,
    pub step: i32,
    pub cooldown_seconds: i64,
    pub trigger_threshold: i64,
    pub trigger_seconds: i64,
    pub idle_seconds: i64,
    pub dry_run: bool,
    pub paused: bool,
}

impl ScalingPolicy {
    pub fn defaults(role: &str) -> Self {
        Self {
            role: role.into(),
            minimum: 0,
            maximum: 10,
            step: 1,
            cooldown_seconds: 300,
            trigger_threshold: if role == "storage" { 20 } else { 120 },
            trigger_seconds: if role == "storage" { 600 } else { 0 },
            idle_seconds: 900,
            dry_run: true,
            paused: true,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if !matches!(self.role.as_str(), "web" | "ssh" | "storage" | "runner")
            || self.minimum < 0
            || self.maximum < self.minimum
            || self.maximum > 1000
            || self.step < 1
            || self.step > 1000
            || !(60..=86400).contains(&self.cooldown_seconds)
            || !(0..=86400).contains(&self.trigger_seconds)
            || !(60..=86400).contains(&self.idle_seconds)
            || !(1..=3600).contains(&self.trigger_threshold)
            || (self.role == "storage" && self.trigger_threshold > 80)
        {
            return Err("Invalid scaling policy.");
        }
        Ok(())
    }
}
