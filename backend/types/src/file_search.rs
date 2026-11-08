//! One bounded path-query rule for owning-store scans and hydrated file finding.

/// Lowercase, deduplicated whitespace-separated terms, limited to 256 input characters.
pub fn query_terms(query: &str) -> Vec<String> {
    let query: String = query.chars().take(256).collect();
    let mut terms: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
    terms.sort_unstable();
    terms.dedup();
    terms
}

/// Every term must appear in the path, ignoring case. No terms match every path.
pub fn matches_terms(path: &str, terms: &[String]) -> bool {
    if terms.is_empty() {
        return true;
    }
    let path = path.to_lowercase();
    terms.iter().all(|term| path.contains(term))
}
