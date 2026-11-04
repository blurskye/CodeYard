use std::collections::HashMap;
/// Current native repository metadata, including typed effective property values/defaults.
/// Callers must scope the repository before constructing this read projection.
pub struct RepositoryContext<'a> {
    pub id: i64,
    pub name: &'a str,
    pub default_branch: &'a str,
    pub is_private: bool,
    pub is_archived: bool,
    pub is_fork: bool,
    pub properties: &'a HashMap<String, Vec<String>>,
}
