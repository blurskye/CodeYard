use codeyard::backend::repositories::secret_scanning::{
    patterns::Create, scan, scan_with_patterns, MAX_BLOB_BYTES,
};

#[test]
fn secret_scanning_rules_and_locations() {
    let pat = format!("ghp_{}", "2H8kL9mN4pQ7rS3tV6wX5yZ0aB1cD8eF9gJ2");
    let text = format!("# config\nπ = '{pat}'\nAWS = AKIAIOSFODNN7EXAMPLE\n");
    let found = scan("config.env", "commit", text.as_bytes()).unwrap();
    let alert = found.iter().find(|alert| alert.secret == pat).unwrap();
    assert_eq!(alert.secret_type, "github_personal_access_token");
    assert_eq!(
        (
            alert.start_line,
            alert.start_column,
            alert.end_line,
            alert.end_column
        ),
        // GitHub counts columns in bytes ("interpreted as 8BIT ASCII"), so π takes two.
        (2, 7, 2, 46)
    );
    assert!(!found
        .iter()
        .any(|alert| alert.rule_id == "aws-access-token"));
    assert!(scan("node_modules/config.env", "commit", text.as_bytes())
        .unwrap()
        .is_empty());
    assert!(scan("image.png", "commit", text.as_bytes())
        .unwrap()
        .is_empty());
    assert!(scan("binary", "commit", &[0, 1, 2]).unwrap().is_empty());
    assert!(scan("large", "commit", &vec![b'a'; MAX_BLOB_BYTES + 1])
        .unwrap()
        .is_empty());
    let pypi = format!(
        "pypi-AgEIcHlwaS5vcmc{}",
        "3b17a095fdc84e62091adc64b3f5a7e891c246df938b5a017eafc647320bd952"
    );
    assert!(scan("keys", "commit", pypi.as_bytes())
        .unwrap()
        .iter()
        .any(|secret| secret.secret == pypi && secret.secret_type == "pypi_upload_token"));
    let aws = "AKIAZ7VJQ6HPLXWMN2R4";
    let found = scan("keys", "commit", aws.as_bytes()).unwrap();
    assert!(found
        .iter()
        .any(|alert| alert.secret == aws && alert.secret_type == "aws_access_key_id"));
}

#[test]
fn secret_scanning_own_tokens_and_entropy() {
    for (prefix, kind) in [
        ("cy_pat_", "codeyard_personal_access_token"),
        ("cy_fgpat_", "codeyard_fine_grained_personal_access_token"),
        ("ghs_", "codeyard_actions_token"),
    ] {
        let payload = if prefix == "ghs_" {
            "AUf2Wt_DtOxg3y0mK9sHbR-Zq6pVxN8cLe1jE4aY5f-"
        } else { "3b17a095fdc84e62091adc64b3f5a7e891c246df938b5a017eafc647320bd952" };
        let value = format!("{prefix}{payload}");
        let found = scan("tokens.txt", "commit", value.as_bytes()).unwrap();
        assert!(found
            .iter()
            .any(|alert| alert.secret == value && alert.secret_type == kind));
        let unicode = format!("π{value}π");
        assert!(scan("tokens.txt", "commit", unicode.as_bytes())
            .unwrap()
            .iter()
            .any(|alert| alert.secret == value
                && alert.secret_type == kind
                && alert.start_column == 3));
    }
    let found = scan(
        "config.env",
        "commit",
        b"api_key = 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'\n",
    )
    .unwrap();
    assert!(found.is_empty());
}

#[test]
fn secret_scanning_custom_patterns_share_delimiters() {
    let mut pattern = serde_json::from_value::<Create>(
        serde_json::json!({"name":"Custom","pattern":"fixed_[A-Z0-9]{16}"}),
    )
    .unwrap()
    .into_pattern(17);
    pattern.state = "published".into();
    let found = scan_with_patterns(
        "keys.txt",
        "commit",
        b"fixed_Z7VJQ6HPLXWMN2R4 fixed_7VJQ6HPLXWMN2R4Z",
        &[pattern],
        false,
    )
    .unwrap();
    assert_eq!(found.len(), 2);
    assert_eq!(
        found
            .iter()
            .map(|secret| secret.start_column)
            .collect::<Vec<_>>(),
        vec![1, 24]
    );
}

#[test]
fn secret_scanning_re2_character_classes() {
    let mut pattern = serde_json::from_value::<Create>(
        serde_json::json!({"name":"RE2 classes","pattern":r"(?:\p{L}|\x{1F510})_\w{16}"}),
    )
    .unwrap()
    .into_pattern(18);
    pattern.state = "published".into();
    let found = scan_with_patterns(
        "keys.txt",
        "commit",
        "π_Z7VJQ6HPLXWMN2R4".as_bytes(),
        std::slice::from_ref(&pattern),
        false,
    )
    .unwrap();
    assert_eq!(found.len(), 1);
    // Columns count bytes, as GitHub documents, so the two-byte π ends the match at 19.
    assert_eq!((found[0].start_column, found[0].end_column), (1, 19));
    assert_eq!(
        scan_with_patterns(
            "keys.txt",
            "commit",
            "🔐_Z7VJQ6HPLXWMN2R4".as_bytes(),
            std::slice::from_ref(&pattern),
            false
        )
        .unwrap()
        .len(),
        1
    );
    assert!(scan_with_patterns(
        "keys.txt",
        "commit",
        "π_Z7VJQ6HPLXWMN2Rπ".as_bytes(),
        &[pattern],
        false
    )
    .unwrap()
    .is_empty());
}

#[test]
fn secret_scanning_detection_budgets_fail_closed() {
    let text = (0..1001)
        .map(|id| format!("cy_pat_{id:064x}\n"))
        .collect::<String>();
    let Err(error) = scan("keys.txt", "commit", text.as_bytes()) else {
        panic!("unbounded credential matches must fail")
    };
    assert!(error.to_string().contains("budget"));
    let mut pattern = serde_json::from_value::<Create>(
        serde_json::json!({"name":"Wide","pattern":".","start_delimiter":"","end_delimiter":""}),
    )
    .unwrap()
    .into_pattern(1);
    pattern.state = "published".into();
    assert!(
        scan_with_patterns("keys.txt", "commit", &vec![b'x'; 2000], &[pattern], false).is_err()
    );
}
