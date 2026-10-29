use super::*;

#[tokio::test]
async fn code_scanning_security_configuration_errors_use_string_details() {
    for refusal in [records::Refusal::Duplicate, records::Refusal::Invalid] {
        let response = failure(anyhow::anyhow!(refusal)).into_response();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(!body["errors"].as_array().unwrap().is_empty());
        assert!(body["errors"]
            .as_array()
            .unwrap()
            .iter()
            .all(Value::is_string));
    }
}
