use serde_json::{json, Value};
pub fn release_notes(notes: &crate::repositories::releases::notes::ReleaseNotes) -> Value {
    json!({"name":notes.name,"body":notes.body})
}

