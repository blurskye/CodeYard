use super::*;
#[test]
fn visitor_fingerprints_are_keyed_typed_and_rotatable() {
    let first = "ab".repeat(32);
    let second = "cd".repeat(32);
    let account = Identity::account(1).unwrap();
    let key = Identity::deploy_key(1).unwrap();
    let client = Identity::client("192.0.2.1", "Browser").unwrap();
    let hash = account.fingerprint(&first).unwrap();
    assert_eq!(hash.len(), 64);
    assert_eq!(hash, account.fingerprint(&first).unwrap());
    assert_ne!(hash, account.fingerprint(&second).unwrap());
    assert_ne!(hash, key.fingerprint(&first).unwrap());
    assert_ne!(hash, client.fingerprint(&first).unwrap());
    assert_eq!(
        client.fingerprint(&first).unwrap(),
        Identity::client("192.0.2.1", "Browser")
            .unwrap()
            .fingerprint(&first)
            .unwrap()
    );
    assert_ne!(
        client.fingerprint(&first).unwrap(),
        Identity::client("192.0.2.2", "Browser")
            .unwrap()
            .fingerprint(&first)
            .unwrap()
    );
    assert!(Identity::account(0).is_none());
    assert!(Identity::deploy_key(-1).is_none());
    assert!(Identity::client("claimed-not-an-address", "Browser").is_none());
    assert!(account.fingerprint("invalid-key").is_err());
}
#[test]
fn content_observations_strip_referral_secrets_and_bound_titles() {
    let origin = "https://code.example";
    let metric = Metric::view(
        "/alice/proj/issues?page=1&secret=hidden",
        "Title\0\n<script>",
        Some("https://u:p@outside.example/private/path?secret=hidden"),
        origin,
    )
    .unwrap();
    assert_eq!(metric.path, "/alice/proj/issues");
    assert_eq!(metric.title, "Title<script>");
    assert_eq!(metric.referrer, "outside.example");
    assert_eq!(metric.path_hash.len(), 64);
    for source in [
        "https://code.example/alice/proj",
        "https://192.0.2.1/private",
        "https://[2001:db8::1]/private",
        "javascript:alert(1)",
        "not a URL",
    ] {
        assert!(
            Metric::view("/alice/proj", "Page", Some(source), origin)
                .unwrap()
                .referrer
                .is_empty(),
            "{source}"
        );
    }
    assert!(Metric::view("//outside.example", "Page", None, origin).is_err());
    assert!(Metric::view("/a\0b", "Page", None, origin).is_err());
    assert!(Metric::view(&format!("/{}", "a".repeat(16384)), "Page", None, origin).is_err());
    assert_eq!(
        Metric::view("/alice/proj", &"é".repeat(1000), None, origin)
            .unwrap()
            .title
            .chars()
            .count(),
        512
    );
}
#[test]
fn retained_window_includes_today_and_fourteen_completed_utc_days() {
    let window = Window::at(NaiveDate::from_ymd_opt(2026, 10, 12).unwrap());
    assert_eq!(window.today, "2026-10-12");
    assert_eq!(window.yesterday, "2026-10-11");
    assert_eq!(window.first, "2026-09-28");
}
