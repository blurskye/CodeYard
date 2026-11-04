use std::time::Duration;
/// The route template, shared with the form-body middleware that exempts it.
pub const ROUTE: &str = "/:owner/:repo/upload/*ref_path";

/// How long an upload may take to arrive. Large files over a slow connection need minutes.
pub const READ_TIMEOUT: Duration = Duration::from_secs(15 * 60);
