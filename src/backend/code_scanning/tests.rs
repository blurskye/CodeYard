use super::*;
use std::io::Write;

fn upload(sarif: Value) -> Upload {
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(sarif.to_string().as_bytes()).unwrap();
    Upload {
        sequence: 1,
        id: uuid::Uuid::new_v4().to_string(),
        repo_id: 1,
        actor_id: Some(1),
        ref_name: "refs/heads/main".into(),
        commit_sha: "a".repeat(40),
        sarif: STANDARD.encode(gzip.finish().unwrap()),
        checkout_uri: Some("file:///workspace/".into()),
        processing_status: "pending".into(),
        claim_id: None,
        error: None,
        created_at: chrono::Utc::now(),
    }
}
fn document() -> Value {
    json!({"version":"2.1.0","runs":[{"tool":{"driver":{"name":"Trivy","semanticVersion":"0.57.0","rules":[{"id":"CVE-2024-example","shortDescription":{"text":"Vulnerable dependency"},"properties":{"security-severity":"9.8"}}]}},"automationDetails":{"id":"trivy/fs/"},"originalUriBaseIds":{"ROOTPATH":{"uri":"file:///workspace/"}},"artifacts":[{"location":{"uri":"src/a%20b.rs","uriBaseId":"ROOTPATH"}}],"results":[{"ruleIndex":0,"message":{"text":"Update dependency"},"partialFingerprints":{"primaryLocationLineHash":"stable-line"},"locations":[{"physicalLocation":{"artifactLocation":{"index":0},"region":{"startLine":3,"endLine":4}}}]}]}]})
}
#[test]
fn open_scanner_metadata_locations_and_fingerprints() {
    let mut source = document();
    let first = parse(&upload(source.clone())).unwrap();
    let mut no_trailing_slash = upload(source.clone());
    no_trailing_slash.checkout_uri = Some("file:///workspace".into());
    assert_eq!(
        parse(&no_trailing_slash).unwrap()[0].findings[0].fingerprint,
        first[0].findings[0].fingerprint
    );
    assert_eq!(first[0].category, "trivy/fs");
    assert_eq!(first[0].results_count, 1);
    let location: Value = serde_json::from_str(&first[0].findings[0].location).unwrap();
    assert_eq!(location["path"], "src/a b.rs");
    assert_eq!(location["end_line"], 4);
    let rule: Value = serde_json::from_str(&first[0].findings[0].rule).unwrap();
    assert_eq!(rule["security_severity_level"], "critical");
    let tool: Value = serde_json::from_str(&first[0].tool).unwrap();
    assert_eq!(tool["version"], "0.57.0");
    source["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["region"] =
        json!({"startLine":30,"endLine":31});
    let moved = parse(&upload(source)).unwrap();
    assert_eq!(
        first[0].findings[0].fingerprint,
        moved[0].findings[0].fingerprint
    );
}
#[test]
fn persisted_metadata_and_artifact_paths_are_bounded() {
    let mut tool = document();
    tool["runs"][0]["tool"]["driver"]["name"] = json!("x".repeat(1025));
    let mut category = document();
    category["runs"][0]["automationDetails"]["id"] = json!(format!("{}/", "x".repeat(1025)));
    let mut path = document();
    path["runs"][0]["artifacts"][0]["location"]["uri"] = json!("x".repeat(4097));
    let mut message = document();
    message["runs"][0]["results"][0]["message"]["text"] = json!("x".repeat(1024 * 1024 + 1));
    let mut rule = document();
    rule["runs"][0]["tool"]["driver"]["rules"][0]["help"]["text"] =
        json!("x".repeat(1024 * 1024 + 1));
    let mut tags = document();
    tags["runs"][0]["tool"]["driver"]["rules"][0]["properties"]["tags"] = json!(["security", 42]);
    let mut base = document();
    base["runs"][0]["originalUriBaseIds"]["ROOTPATH"]["uri"] = json!("x".repeat(16 * 1024 + 1));
    let mut base_id = document();
    base_id["runs"][0]["artifacts"][0]["location"]["uriBaseId"] = json!("x".repeat(1025));
    for source in [tool, category, path, message, rule, tags, base, base_id] {
        assert!(parse(&upload(source)).is_err());
    }
    let mut checkout = upload(document());
    checkout.checkout_uri = Some("x".repeat(16 * 1024 + 1));
    assert!(parse(&checkout).is_err());
}

#[test]
fn rule_id_lookup_handles_maximum_rule_and_result_counts() {
    let mut source = document();
    source["runs"][0]["tool"]["driver"]["rules"] = json!((0..25_000)
        .map(|index| json!({"id":format!("rule-{index}")}))
        .collect::<Vec<_>>());
    let mut result = source["runs"][0]["results"][0].clone();
    result.as_object_mut().unwrap().remove("ruleIndex");
    result["ruleId"] = json!("rule-24999");
    source["runs"][0]["results"] = json!(vec![result; 25_000]);
    let parsed = parse(&upload(source)).unwrap();
    assert_eq!(parsed[0].results_count, 25_000);
    assert_eq!(parsed[0].rules_count, 25_000);
    assert_eq!(parsed[0].findings.len(), 1);
    let rule: Value = serde_json::from_str(&parsed[0].findings[0].rule).unwrap();
    assert_eq!(rule["id"], "rule-24999");
}

#[test]
fn invalid_security_scores_do_not_fabricate_severities() {
    for score in [
        "NaN".into(),
        "inf".into(),
        "-1".into(),
        "11".into(),
        "9".repeat(1025),
    ] {
        let mut source = document();
        source["runs"][0]["tool"]["driver"]["rules"][0]["properties"]["security-severity"] =
            json!(score);
        let parsed = parse(&upload(source)).unwrap();
        let rule: Value = serde_json::from_str(&parsed[0].findings[0].rule).unwrap();
        assert!(rule["security_severity_level"].is_null());
    }
}

#[test]
fn failed_or_externalized_reports_are_not_clean_analyses() {
    let mut failed = document();
    failed["runs"][0]["results"] = json!([]);
    failed["runs"][0]["invocations"] = json!([{"executionSuccessful":false}]);
    let mut external = document();
    external["runs"][0]["results"] = json!([]);
    external["runs"][0]["externalPropertyFileReferences"] =
        json!({"results":[{"location":{"uri":"results.sarif-external-properties"}}]});
    for source in [failed, external] {
        assert!(parse(&upload(source)).is_err());
    }
}

#[test]
fn malformed_unbounded_and_outside_checkout_results_are_refused() {
    for version in ["2.0.0", "3.0.0", ""] {
        let mut source = document();
        source["version"] = json!(version);
        assert!(parse(&upload(source)).is_err());
    }
    for path in [
        "../outside.rs",
        "%2e%2e/outside.rs",
        "file:///elsewhere/source.rs",
        "file:///workspace-evil/source.rs",
        "src/%00.rs",
        "src/%2e%2e/../../outside",
    ] {
        let mut source = document();
        source["runs"][0]["artifacts"][0]["location"] = json!({"uri":path});
        let mut input = upload(source);
        assert!(parse(&input).is_err(), "{path}");
        input.checkout_uri = Some("file:///workspace".into());
        assert!(parse(&input).is_err(), "{path}");
    }
    let mut source = document();
    source["runs"] = json!(vec![source["runs"][0].clone(); 21]);
    assert!(parse(&upload(source)).is_err());
    let mut source = document();
    source["runs"][0]["results"] = json!(vec![source["runs"][0]["results"][0].clone(); 25_001]);
    assert!(parse(&upload(source)).is_err());
    let mut source = upload(document());
    source.sarif = STANDARD.encode(b"not gzip");
    assert!(parse(&source).is_err());
}
#[test]
fn duplicate_categories_are_not_silently_overwritten() {
    let mut source = document();
    source["runs"] = json!([source["runs"][0], source["runs"][0]]);
    assert!(parse(&upload(source.clone())).is_err());
    source["runs"][1]["automationDetails"]["id"] = json!("another/category");
    assert_eq!(parse(&upload(source)).unwrap().len(), 2);
}

#[test]
fn documented_limits_and_rule_extensions() {
    let mut source = document();
    let extension = source["runs"][0]["tool"]["driver"].clone();
    source["runs"][0]["tool"]["driver"]["rules"] = json!([]);
    source["runs"][0]["tool"]["extensions"] = json!([extension]);
    source["runs"][0]["results"][0]["ruleIndex"] = Value::Null;
    source["runs"][0]["results"][0]["rule"] = json!({"index":0,"toolComponent":{"index":0}});
    assert_eq!(parse(&upload(source.clone())).unwrap()[0].rules_count, 1);
    source["runs"][0]["tool"]["extensions"] = json!(vec![extension; 101]);
    assert!(parse(&upload(source)).is_err());
    let mut source = document();
    let location = source["runs"][0]["results"][0]["locations"][0].clone();
    source["runs"][0]["results"][0]["locations"] = json!(vec![location.clone(); 1000]);
    assert!(parse(&upload(source.clone())).is_ok());
    source["runs"][0]["results"][0]["locations"]
        .as_array_mut()
        .unwrap()
        .push(location);
    assert!(parse(&upload(source)).is_err());
    let mut source = document();
    source["runs"][0]["tool"]["driver"]["rules"][0]["properties"]["tags"] =
        json!((0..20).map(|i| format!("tag-{i}")).collect::<Vec<_>>());
    let parsed = parse(&upload(source.clone())).unwrap();
    let rule: Value = serde_json::from_str(&parsed[0].findings[0].rule).unwrap();
    assert_eq!(rule["tags"].as_array().unwrap().len(), 10);
    source["runs"][0]["tool"]["driver"]["rules"][0]["properties"]["tags"]
        .as_array_mut()
        .unwrap()
        .push(json!("extra"));
    assert!(parse(&upload(source)).is_err());
    let mut source = document();
    source["runs"][0]["results"][0]["codeFlows"] = json!([{"threadFlows":[{"locations":vec![json!({});5001]}]},{"threadFlows":[{"locations":vec![json!({});5000]}]}]);
    assert!(parse(&upload(source)).is_err());
}

#[test]
fn only_the_highest_priority_5000_results_are_published() {
    let mut source = document();
    let result = source["runs"][0]["results"][0].clone();
    source["runs"][0]["tool"]["driver"]["rules"] = json!([{"id":"low","defaultConfiguration":{"level":"note"}},{"id":"high","properties":{"security-severity":"9.8"}}]);
    source["runs"][0]["results"] = json!((0..5001)
        .map(|index| {
            let mut result = result.clone();
            result["ruleIndex"] = json!(usize::from(index == 5000));
            result["partialFingerprints"]["primaryLocationLineHash"] = json!(index.to_string());
            result
        })
        .collect::<Vec<_>>());
    let parsed = parse(&upload(source)).unwrap();
    assert_eq!(parsed[0].results_count, 5001);
    assert_eq!(parsed[0].findings.len(), 5000);
    assert_eq!(parsed[0].findings[0].source_index, 5000);
}

#[test]
fn gzip_expansion_is_bounded_before_json_is_parsed() {
    let mut source = upload(document());
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let chunk = vec![b' '; 1024 * 1024];
    for _ in 0..101 {
        gzip.write_all(&chunk).unwrap();
    }
    source.sarif = STANDARD.encode(gzip.finish().unwrap());
    assert!(parse(&source).unwrap_err().to_string().contains("100 MiB"));
    source.sarif = STANDARD.encode(vec![0; MAX_COMPRESSED + 1]);
    assert!(parse(&source).unwrap_err().to_string().contains("10 MiB"));
}
