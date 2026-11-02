use super::*;
fn page(values: HashMap<String, String>) -> Page {
    let mut page = sample_page(values, vec![]);
    page.tab = "properties".into();
    page.properties = Some(Box::new(PropertyPanel {
        definitions: vec![PropertySummary {
            name: "environment".into(),
            value_type: "multi_select".into(),
            description: "Metadata".into(),
            required: true,
        }],
        selected: Some(PropertyDefinition {
            name: "environment".into(),
            value_type: "multi_select".into(),
            description: "<script>description</script>".into(),
            required: true,
            require_explicit_values: false,
            has_default: true,
            default_value: "first".into(),
            allowed_values: "first\nsecond<&".into(),
            values_editable_by: "org_and_repo_actors".into(),
        }),
    }));
    page
}
#[test]
fn organization_property_textareas_and_policy_choices_work_in_ssr() {
    let rendered = html(page(HashMap::new()));
    let choices = rendered
        .split("id=\"property-choices\"")
        .nth(1)
        .unwrap()
        .split("</textarea>")
        .next()
        .unwrap()
        .split_once('>')
        .unwrap()
        .1;
    assert!(choices.contains("first\nsecond"));
    assert!(!choices.contains("second<&"));
    assert!(!rendered.contains("<script>description</script>"));
    assert!(selected(
        selects(&rendered, "value_type")[0],
        "multi_select"
    ));
    assert!(selected(
        selects(&rendered, "values_editable_by")[0],
        "org_and_repo_actors"
    ));
    assert!(rendered.contains("name=\"confirm\""));
    let confirm = rendered
        .split("name=\"confirm\"")
        .nth(1)
        .unwrap()
        .split('>')
        .next()
        .unwrap();
    assert!(!confirm.contains("value=\"environment\""));
}
#[test]
fn refused_property_edits_preserve_typed_drafts_and_unchecked_flags() {
    let rendered = html(page(draft(&[
        ("action", "save-property"),
        ("property_name", "environment"),
        ("value_type", "string"),
        ("description", "refused <b>draft</b>"),
        ("allowed_values", "new\nchoices"),
        ("default_value", "draft-default"),
        ("use_default", "true"),
        ("values_editable_by", "org_actors"),
    ])));
    assert!(selected(selects(&rendered, "value_type")[0], "string"));
    assert!(selected(
        selects(&rendered, "values_editable_by")[0],
        "org_actors"
    ));
    let required = rendered
        .split("name=\"required\"")
        .nth(1)
        .unwrap()
        .split('>')
        .next()
        .unwrap();
    assert!(!required.contains("checked"));
    let description = rendered
        .split("id=\"property-description\"")
        .nth(1)
        .unwrap()
        .split("</textarea>")
        .next()
        .unwrap()
        .split_once('>')
        .unwrap()
        .1;
    assert!(description.contains("refused "));
    assert!(!description.contains("<b>draft</b>"));
    assert!(rendered.contains("draft-default"));
}
#[test]
fn ordinary_members_and_suspended_owners_have_read_only_property_details() {
    for suspended in [false, true] {
        let mut model = page(HashMap::new());
        if suspended {
            model.selected.as_mut().unwrap().suspended = true;
        } else {
            model.is_owner = false;
            model.can_manage = false;
        }
        let rendered = html(model);
        assert!(rendered.contains("Allowed choices"));
        assert!(rendered.contains("Default value"));
        assert!(!rendered.contains("value=\"save-property\""));
        assert!(!rendered.contains("value=\"set-property-value\""));
        assert!(!rendered.contains("value=\"delete-property\""));
    }
}
