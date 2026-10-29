use super::*;

fn caller(references: &[String]) -> Yaml {
    serde_yaml::from_str(&format!(
        "on: workflow_call\njobs:\n{}",
        references
            .iter()
            .enumerate()
            .map(|(index, name)| {
                format!("  job{index}: {{uses: './.github/workflows/{name}.yml'}}\n")
            })
            .collect::<String>()
    ))
    .unwrap()
}

#[test]
fn reusable_graph_limits_cycles_and_callable_events() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().canonicalize().unwrap();
    let workflows = directory.join(".github/workflows");
    std::fs::create_dir_all(&workflows).unwrap();
    let leaf = "on: workflow_call\njobs: {test: {runs-on: linux, steps: [{run: 'true'}]}}";
    for index in 0..21 {
        std::fs::write(workflows.join(format!("leaf{index}.yml")), leaf).unwrap();
    }
    let names: Vec<_> = (0..21).map(|index| format!("leaf{index}")).collect();
    assert_eq!(files(&caller(&names[..20]), &directory).unwrap().len(), 20);
    assert!(files(&caller(&names), &directory)
        .unwrap_err()
        .to_string()
        .contains("twenty"));
    for index in 0..3 {
        let next = if index == 2 {
            "leaf0".to_owned()
        } else {
            format!("level{}", index + 1)
        };
        std::fs::write(
            workflows.join(format!("level{index}.yml")),
            serde_yaml::to_string(&caller(&[next])).unwrap(),
        )
        .unwrap();
    }
    assert!(files(&caller(&["level1".into()]), &directory).is_ok());
    assert!(files(&caller(&["level0".into()]), &directory)
        .unwrap_err()
        .to_string()
        .contains("four"));
    std::fs::write(
        workflows.join("cycle.yml"),
        serde_yaml::to_string(&caller(&["cycle".into()])).unwrap(),
    )
    .unwrap();
    assert!(files(&caller(&["cycle".into()]), &directory)
        .unwrap_err()
        .to_string()
        .contains("cycle"));
    std::fs::write(
        workflows.join("ordinary.yml"),
        leaf.replace("workflow_call", "push"),
    )
    .unwrap();
    assert!(files(&caller(&["ordinary".into()]), &directory)
        .unwrap_err()
        .to_string()
        .contains("workflow_call"));
}

#[test]
fn omitted_inputs_have_typed_defaults() {
    let called = serde_yaml::from_str("on:\n  workflow_call:\n    inputs:\n      flag: {type: boolean}\n      count: {type: number}\n      message: {type: string, default: hello}\n").unwrap();
    let contexts = contexts(
        &Mapping::new(),
        &called,
        &Contexts::default(),
        &Masker::default(),
    )
    .unwrap();
    assert_eq!(
        expressions::evaluate("inputs.flag", &contexts, Status::default()).unwrap(),
        Value::Bool(false)
    );
    assert_eq!(
        expressions::evaluate("inputs.count", &contexts, Status::default()).unwrap(),
        Value::Number(0.0)
    );
    assert_eq!(
        expressions::evaluate("inputs.message", &contexts, Status::default()).unwrap(),
        Value::String("hello".into())
    );
}

#[test]
fn evaluated_called_secrets_are_masked() {
    let called =
        serde_yaml::from_str("on:\n  workflow_call:\n    secrets:\n      token: {}\n").unwrap();
    let call: Mapping =
        serde_yaml::from_str("secrets:\n  token: ${{ fromJSON(secrets.CREDENTIALS).token }}\n")
            .unwrap();
    let caller = Contexts {
        values: BTreeMap::from([(
            "secrets".into(),
            Value::Object(BTreeMap::from([(
                "CREDENTIALS".into(),
                Value::String(r#"{"token":"extracted-secret"}"#.into()),
            )])),
        )]),
    };
    let masker = Masker::default();
    let contexts = contexts(&call, &called, &caller, &masker).unwrap();
    assert_eq!(masker.mask("extracted-secret"), "***");
    assert_eq!(
        expressions::evaluate("secrets.token", &contexts, Status::default()).unwrap(),
        Value::String("extracted-secret".into())
    );
}
