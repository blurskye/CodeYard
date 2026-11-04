/// The streamed file upload route. The request-body gate skips its PUT, which reads the
/// body as it arrives.
pub const CONTENT_ROUTE: &str = "/api/files/:file_id/content";

pub mod download;
