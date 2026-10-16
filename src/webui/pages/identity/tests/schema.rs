//! Original identity view contracts, grouped by responsibility.
use super::*;

#[test]
fn every_page_variant_roundtrips_safe_owned_json_without_secrets() {
    crate::webui::tests::in_scope(|| {
        for (page, _) in safe_pages() {
            let json = serde_json::to_string(&page).expect("safe identity page serializes");
            assert!(!json.contains("secret-"), "secret survived in {json}");
            for key in [
                "\"password\":",
                "\"password_hash\":",
                "\"current_password\":",
                "\"new_password\":",
                "\"confirm_password\":",
                "\"admin_secret\":",
            ] {
                assert!(!json.contains(key), "unexpected secret field {key}");
            }
            let decoded: Page = serde_json::from_str(&json).expect("identity page deserializes");
            assert_eq!(page, decoded);
            assert_eq!(html(render(page)), html(render(decoded)));
        }
    });
}

#[test]
fn recovery_schema_rejects_accidental_password_fields() {
    let mut value = serde_json::to_value(FormValues::default()).unwrap();
    value.as_object_mut().unwrap().insert(
        "password".into(),
        serde_json::json!("secret-password-marker"),
    );
    assert!(serde_json::from_value::<FormValues>(value).is_err());
}

#[test]
fn dispatch_covers_auth_settings_admin_logout_and_profile_with_escaped_recovery() {
    crate::webui::tests::in_scope(|| {
        for (page, label) in safe_pages() {
            let output = html(render(page));
            assert!(output.contains(label), "missing {label} in {output}");
            assert!(output.contains("csrf-marker"));
            assert!(!output.contains("<script>"));
            assert!(!output.contains("<b>Validation error</b>"));
            assert!(!output.contains("secret-"));
        }
    });
}
