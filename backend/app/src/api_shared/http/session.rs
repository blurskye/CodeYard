use axum::http::HeaderMap;
/// Set by `middleware::stamp_client_address` on every request; a value sent by the client is
/// dropped there.
pub const CLIENT_HEADER: &str = "x-codeyard-client";

/// The address the server saw for this request, used to limit failed sign-ins. `None` when the
/// request did not come in over a socket.
pub fn client_address(headers: &HeaderMap) -> Option<&str> {
    headers.get(CLIENT_HEADER)?.to_str().ok()
}
