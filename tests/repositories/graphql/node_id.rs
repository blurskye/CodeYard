//! Independent encoders for asserting the public REST/GraphQL ID contract.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
pub(super) fn encode(kind: &str, id: i64) -> String {
    format!("{kind}_{}", URL_SAFE_NO_PAD.encode(id.to_be_bytes()))
}
pub(super) fn git(kind: &str, key: &str) -> String {
    format!("{kind}_{}", URL_SAFE_NO_PAD.encode(key))
}
pub(super) fn reference(repo: i64, name: &str) -> String {
    format!(
        "REF_{}",
        URL_SAFE_NO_PAD.encode([repo.to_be_bytes().as_slice(), name.as_bytes()].concat())
    )
}

/// Root-bearing numeric IDs use MessagePack [0, owner/repository ID, row ID].
pub(super) fn scoped(kind: &str, root: i64, id: i64) -> String {
    let bytes = [
        vec![0x93, 0, 0xcf],
        root.to_be_bytes().to_vec(),
        vec![0xcf],
        id.to_be_bytes().to_vec(),
    ]
    .concat();
    format!("{kind}_{}", URL_SAFE_NO_PAD.encode(bytes))
}
