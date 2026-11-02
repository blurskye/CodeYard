use super::*;
use crate::backend::db::dependabot_alerts;
fn row(version: &str, state: &str) -> dependabot_alerts::Alert {
    dependabot_alerts::Alert {
        repo_id: 1, number: 1, ghsa_id: "GHSA-2345-6789-cfgh".into(),
        manifest: "package-lock.json".into(), ecosystem: "npm".into(), package: "foo".into(),
        version: version.into(), scope: "runtime".into(), state: state.into(),
        advisory: serde_json::json!({"summary":"An advisory","description":"Description","severity":"high"}).to_string(),
        vulnerability: serde_json::json!({"first_patched_version":"1.2.4"}).to_string(),
        created_at: Utc::now(), updated_at: Utc::now(), fixed_at: None,
        dismissed_at: None, dismissed_by: None, dismissed_reason: None, dismissed_comment: None,
    }
}
#[test]
fn shared_alert_state_and_version_govern_proposal_receipts() {
    let mut snapshot = project(vec![row("1.2.3", "open")], None, "{}").unwrap();
    let key = snapshot.alerts.keys().next().unwrap().clone();
    snapshot.alerts.get_mut(&key).unwrap().update = Some(Update {
        branch: "codeyard/security/test".into(),
        base_sha: "a".repeat(40),
        commit_sha: "b".repeat(40),
        old_version: "1.2.3".into(),
        fixed_version: "1.2.4".into(),
        requested_by: 1,
        pull_request_id: Some(1),
    });
    snapshot.manual(&key, "1.2.3", "Review manually").unwrap();
    let raw = receipts(&snapshot).unwrap();
    assert!(!raw.contains("summary"));
    assert!(!raw.contains("packages"));
    assert!(!raw.contains("dismissed"));
    let dismissed = project(vec![row("1.2.3", "dismissed")], None, &raw).unwrap();
    assert_eq!(dismissed.alerts[&key].state, AlertState::Dismissed);
    assert!(dismissed.alerts[&key].update.is_some());
    let changed = project(vec![row("1.2.2", "open")], None, &raw).unwrap();
    assert!(changed.alerts[&key].update.is_none());
    assert!(changed.alerts[&key].manual_reason.is_none());
    let fixed = project(vec![row("1.2.3", "fixed")], None, &raw).unwrap();
    assert_eq!(fixed.alerts[&key].state, AlertState::Resolved);
    assert!(fixed.alerts[&key].update.is_some());
}
#[test]
fn changed_advisories_clear_manual_reasons_but_not_published_proposals() {
    let mut snapshot = project(vec![row("1.2.3", "open")], None, "{}").unwrap();
    let key = snapshot.alerts.keys().next().unwrap().clone();
    assert!(snapshot.manual(&key, "1.2.2", "Stale review").is_err());
    snapshot.manual(&key, "1.2.3", "No verified patch").unwrap();
    let raw = receipts(&snapshot).unwrap();
    let mut updated = row("1.2.3", "open");
    updated.vulnerability = serde_json::json!({"first_patched_version":"1.2.5"}).to_string();
    let snapshot = project(vec![updated], None, &raw).unwrap();
    assert!(snapshot.alerts[&key].manual_reason.is_none());
    assert_eq!(snapshot.alerts[&key].finding.fixed_versions, vec!["1.2.5"]);
}
