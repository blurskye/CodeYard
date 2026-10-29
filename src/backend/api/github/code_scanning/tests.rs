use super::*;

#[test]
fn code_scanning_queries_parse_upload_ids_and_numeric_pagination() {
    for uri in [
        "/?sarif_id=00000000-0000-4000-8000-000000000001",
        "/?sarif_id=00000000-0000-4000-8000-000000000001&page=2&per_page=3&pr=12",
    ] {
        let uri = uri.parse().unwrap();
        let Query(filters) = Query::<Filters>::try_from_uri(&uri).unwrap();
        assert_eq!(
            filters.sarif_id.as_deref(),
            Some("00000000-0000-4000-8000-000000000001")
        );
        let window = filters.pagination().window().unwrap();
        assert_eq!(
            window,
            if filters.pr.is_some() {
                (3, 3)
            } else {
                (0, 30)
            }
        );
        if filters.pr.is_some() {
            assert_eq!(filters.analysis_filters().unwrap().pull_request, Some(12));
        }
    }
    let uri="/?page=3&per_page=7&ref=refs%2Fheads%2Ftopic&state=open&severity=high&sort=updated&direction=asc".parse().unwrap();
    let Query(filters) = Query::<Filters>::try_from_uri(&uri).unwrap();
    assert_eq!(filters.pagination().window().unwrap(), (14, 7));
    assert_eq!(filters.ref_name.as_deref(), Some("refs/heads/topic"));
    assert_eq!(filters.alert_filters().unwrap().severity, Some("high"));
    assert!(Query::<Filters>::try_from_uri(&"/?page=bad".parse().unwrap()).is_err());
}
