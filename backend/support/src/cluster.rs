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

/// The site's region policy. An organization gains or loses a presence region from its
/// members' authenticated requests that routers counted per region. Counts cover
/// `window_days` ending on a day; a region is added when its share meets `share_percent` of
/// at least `minimum_requests` on `held_days` days in a row, and removed after `remove_days`
/// days below `remove_percent`. A user account moves to another region by the `move_`
/// settings, which work the same way on the user's own requests; it then stays for
/// `move_cooldown_days`, and at most `move_daily_limit` accounts a day move from this cell
/// to one region. Settings missing from a stored policy take their defaults.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RegionPolicy {
    pub window_days: i32,
    pub share_percent: i32,
    pub minimum_requests: i64,
    pub held_days: i32,
    pub remove_percent: i32,
    pub remove_days: i32,
    pub dry_run: bool,
    pub paused: bool,
    pub move_window_days: i32,
    pub move_share_percent: i32,
    pub move_minimum_requests: i64,
    pub move_held_days: i32,
    pub move_cooldown_days: i32,
    pub move_daily_limit: i32,
    pub move_dry_run: bool,
    pub move_paused: bool,
}

impl Default for RegionPolicy {
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
            move_window_days: 7,
            move_share_percent: 70,
            move_minimum_requests: 1000,
            move_held_days: 3,
            move_cooldown_days: 30,
            move_daily_limit: 10,
            move_dry_run: true,
            move_paused: true,
        }
    }
}

impl RegionPolicy {
    /// The windows and the longest runs of days fit in the 60 days of kept counts.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(1..=30).contains(&self.window_days)
            || !(1..=100).contains(&self.share_percent)
            || !(1..=1_000_000_000).contains(&self.minimum_requests)
            || !(1..=14).contains(&self.held_days)
            || !(0..self.share_percent).contains(&self.remove_percent)
            || !(1..=30).contains(&self.remove_days)
            || !(1..=30).contains(&self.move_window_days)
            || !(51..=100).contains(&self.move_share_percent)
            || !(1..=1_000_000_000).contains(&self.move_minimum_requests)
            || !(1..=14).contains(&self.move_held_days)
            || !(1..=365).contains(&self.move_cooldown_days)
            || !(1..=1000).contains(&self.move_daily_limit)
        {
            return Err("Invalid region policy.");
        }
        Ok(())
    }
}
