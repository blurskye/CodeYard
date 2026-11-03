//! Complete Git SHA-1 syntax shared by native storage, adapters and browser views.
//! This validates syntax only, not existence or object type. Storage may require lowercase.
pub fn is_full_commit_id(value: impl AsRef<[u8]>) -> bool {
    let bytes = value.as_ref();
    bytes.len() == 40 && bytes.iter().all(u8::is_ascii_hexdigit)
}
