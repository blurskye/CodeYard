/// How long a client has to send the bytes after reserving an upload.
pub const PUT_LIFETIME_SECONDS: i64 = 900;

/// Initial cleanup delay after a direct PUT expires. S3 PUT completion has no fixed
/// deadline, so S3 deletion records remain for periodic reconciliation after this delay.
pub const IN_FLIGHT_PUT_GRACE_SECONDS: i64 = 3600;

/// Deleted uploads and repositories are kept this long, so a database restored from a
/// backup taken within it still finds their bytes.
pub const RESTORE_WINDOW_SECONDS: i64 = 7 * 24 * 3600;
