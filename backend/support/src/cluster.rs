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

/// When an organization gains or loses a presence region, from its members' authenticated
/// requests that routers counted per region. Counts cover `window_days` ending on a day; a
/// region is added when its share meets `share_percent` of at least `minimum_requests` on
/// `held_days` days in a row, and removed after `remove_days` days below `remove_percent`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresencePolicy {
    pub window_days: i32,
    pub share_percent: i32,
    pub minimum_requests: i64,
    pub held_days: i32,
    pub remove_percent: i32,
    pub remove_days: i32,
    pub dry_run: bool,
    pub paused: bool,
}

impl Default for PresencePolicy {
    /// Starts paused and in dry-run mode, as the scaling policies do.
    fn default() -> Self {
        Self {
            window_days: 7,
            share_percent: 15,
            minimum_requests: 500,
            held_days: 3,
            remove_percent: 5,
            remove_days: 14,
            dry_run: true,
            paused: true,
        }
    }
}

impl PresencePolicy {
    /// The window and the longest run of days fit in the 60 days of kept counts.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(1..=30).contains(&self.window_days)
            || !(1..=100).contains(&self.share_percent)
            || !(1..=1_000_000_000).contains(&self.minimum_requests)
            || !(1..=14).contains(&self.held_days)
            || !(0..self.share_percent).contains(&self.remove_percent)
            || !(1..=30).contains(&self.remove_days)
        {
            return Err("Invalid presence policy.");
        }
        Ok(())
    }
}
