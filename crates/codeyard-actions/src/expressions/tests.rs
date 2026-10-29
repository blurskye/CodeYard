//! Cases from actions/runner's `src/Test/L0/Sdk/ExpressionParserL0.cs`, `Worker/Expressions/ConditionFunctionsL0.cs` and the expression documentation.
use super::*;
use serde_json::json;

fn contexts() -> Contexts {
    Contexts {
        values: BTreeMap::from([
            (
                "github".into(),
                Value::from_json(
                    &json!({"ref":"refs/heads/main", "event_name":"push", "event": {"commits":[{"id":"a"},{"missing":true},{"id":null},{"id":"b"}],"issue":{"labels":[{"name":"bug"},{"name":"help wanted"}]}}}),
                ),
            ),
            (
                "matrix".into(),
                Value::from_json(
                    &json!({"os":"ubuntu-latest", "numbers":[10,20,30], "nested":[[1,2],[3]], "object":{"Foo":{"id":1},"Bar":{"id":2}}}),
                ),
            ),
        ]),
    }
}
fn run(expression: &str) -> Value {
    evaluate(expression, &contexts(), Status::default()).expect(expression)
}
fn yes(expression: &str) {
    assert_eq!(run(expression), Value::Bool(true), "{expression}");
}
fn no(expression: &str) {
    assert_eq!(run(expression), Value::Bool(false), "{expression}");
}

#[test]
fn literals_and_numbers() {
    for (expression, value) in [
        ("null", Value::Null),
        ("true", Value::Bool(true)),
        ("false", Value::Bool(false)),
        ("'It''s open'", Value::String("It's open".into())),
    ] {
        assert_eq!(run(expression), value);
    }
    for (expression, value) in [
        ("0", 0.0),
        ("-0", 0.0),
        ("-12", -12.0),
        ("+1", 1.0),
        (".5", 0.5),
        ("1.", 1.0),
        ("-2.99e-2", -0.0299),
        ("0xff", 255.0),
        ("0xffffffff", -1.0),
        ("0o17", 15.0),
        ("01", 1.0),
    ] {
        assert_eq!(run(expression), Value::Number(value), "{expression}");
    }
    assert!(matches!(run("NaN"), Value::Number(n) if n.is_nan()));
    assert_eq!(run("Infinity"), Value::Number(f64::INFINITY));
    assert_eq!(run("-Infinity"), Value::Number(f64::NEG_INFINITY));
}

#[test]
fn truthiness_and_text() {
    for value in [
        Value::Null,
        Value::Bool(false),
        Value::Number(0.0),
        Value::Number(-0.0),
        Value::Number(f64::NAN),
        Value::String(String::new()),
    ] {
        assert!(!value.is_truthy());
    }
    for value in [
        Value::Array(vec![]),
        Value::Object(BTreeMap::new()),
        Value::Number(-1.0),
        Value::String("false".into()),
    ] {
        assert!(value.is_truthy());
    }
    for (number, expected) in [
        (1.0, "1"),
        (1.5, "1.5"),
        (-0.0, "0"),
        (1e15, "1E+15"),
        (1e-5, "1E-05"),
        (1e-4, "0.0001"),
        (1.2345678901234567, "1.23456789012346"),
    ] {
        assert_eq!(Value::Number(number).to_text(), expected);
    }
}

#[test]
fn json_values_round_trip() {
    let json = json!({"Name":"x", "values":[true, null, 1.5]});
    assert_eq!(Value::from_json(&json).to_json(), json);
    assert_eq!(Value::Number(f64::NAN).to_json(), serde_json::Value::Null);
    assert_eq!(Value::Null.to_text(), "");
    assert_eq!(Value::from_json(&json!([1, 2])).to_text(), "[1,2]");
}

#[test]
fn coercion_table() {
    for expression in [
        "null == false",
        "null == 0",
        "null == ''",
        "false == ''",
        "true == 1",
        "true == '1'",
        "'0' == 0",
        "'  ' == 0",
        "'0xff' == 255",
        "'0o10' == 8",
        "'1e2' == 100",
        "'MAIN' == 'main'",
        "'two' != 2",
        "NaN != NaN",
        "NaN != 0",
        "null == null",
    ] {
        yes(expression);
    }
    for expression in [
        "'false' == false",
        "'null' == null",
        "'two' == 2",
        "NaN == NaN",
        "NaN < 0",
        "NaN >= 0",
        "fromJSON('[]') == 0",
        "fromJSON('{}') == 0",
        "fromJSON('[]') == fromJSON('[]')",
        "fromJSON('{}') == fromJSON('{}')",
    ] {
        no(expression);
    }
}

#[test]
fn comparison_table() {
    for expression in [
        "'2' > '10'",
        "'2' < 10",
        "false < true",
        "null >= null",
        "null <= null",
        "'A' <= 'a'",
        "'B' > 'a'",
        "Infinity > 1",
        "-Infinity < 0",
        "matrix == matrix",
        "matrix.numbers == matrix.numbers",
        "fromJSON('{}') <= 1 == false",
    ] {
        yes(expression);
    }
    for expression in [
        "null > null",
        "null < null",
        "'no' >= 1",
        "'no' <= 1",
        "NaN <= NaN",
        "NaN >= NaN",
        "matrix < matrix",
    ] {
        no(expression);
    }
}

#[test]
fn precedence_and_short_circuit() {
    yes("!false && 1 < 2 == true || false");
    yes("!(false || false) && (true || false)");
    yes("true || fromJSON('bad')");
    no("false && fromJSON('bad')");
    assert_eq!(run("'' || 'fallback'"), Value::String("fallback".into()));
    assert_eq!(run("true && 12 || 42"), Value::Number(12.0));
    assert_eq!(run("true && 0 || 42"), Value::Number(42.0));
}

#[test]
fn property_and_index_lookup() {
    assert_eq!(run("GITHUB.EVENT_NAME"), Value::String("push".into()));
    assert_eq!(
        run("github['REF']"),
        Value::String("refs/heads/main".into())
    );
    assert_eq!(run("matrix.numbers[1]"), Value::Number(20.0));
    assert_eq!(run("matrix.numbers['1.9']"), Value::Number(20.0));
    assert_eq!(run("matrix.numbers[true]"), Value::Number(20.0));
    assert_eq!(run("matrix.numbers[null]"), Value::Number(10.0));
    for expression in [
        "github.missing",
        "github.missing.again",
        "matrix.numbers[-1]",
        "matrix.numbers[NaN]",
        "matrix.numbers[99]",
        "matrix.numbers.name",
        "github.ref[0]",
        "env.foo",
    ] {
        assert_eq!(run(expression), Value::Null, "{expression}");
    }
}

#[test]
fn object_filters() {
    assert_eq!(
        run("github.event.commits.*.id"),
        Value::from_json(&json!(["a", null, "b"]))
    );
    assert_eq!(run("matrix.object.*.id"), Value::from_json(&json!([2, 1])));
    assert_eq!(
        run("matrix.nested.*.*"),
        Value::from_json(&json!([1, 2, 3]))
    );
    assert_eq!(run("matrix.nested.*[0]"), Value::from_json(&json!([1, 3])));
    assert_eq!(run("github.missing.*.id"), Value::Array(vec![]));
    assert_eq!(
        run("matrix.numbers[*]"),
        Value::from_json(&json!([10, 20, 30]))
    );
    assert_eq!(run("matrix.numbers.*.missing"), Value::Array(vec![]));
}

#[test]
fn documented_search_functions() {
    yes("contains('Hello world', 'llo')");
    yes("CONTAINS('Hello world', 'HELLO')");
    yes("contains(github.event.issue.labels.*.name, 'bug')");
    yes("contains(fromJSON('[\"push\",\"pull_request\"]'), github.event_name)");
    yes("startsWith('Hello world', 'He')");
    yes("endsWith('Hello world', 'LD')");
    yes("contains(fromJSON('[0,1,2]'), true)");
    yes("contains(12345, 234)");
    no("contains(matrix, fromJSON('bad'))");
    no("contains(fromJSON('[]'), fromJSON('bad'))");
    no("startsWith(matrix, 'Object')");
    no("endsWith('text', matrix)");
}

#[test]
fn format_examples_and_escapes() {
    assert_eq!(
        run("format('Hello {0} {1} {2}', 'Mona', 'the', 'Octocat')"),
        Value::String("Hello Mona the Octocat".into())
    );
    assert_eq!(
        run("format('{{Hello {0}!}}', 'Mona')"),
        Value::String("{Hello Mona!}".into())
    );
    assert_eq!(
        run("format('{1}-{0}-{1}', 1.5, true)"),
        Value::String("true-1.5-true".into())
    );
    assert_eq!(
        run("format('unused', fromJSON('bad'))"),
        Value::String("unused".into())
    );
    assert_eq!(run("format('{0:}', 'x')"), Value::String("x".into()));
    for expression in [
        "format('{1}', 'only')",
        "format('{256}', 1)",
        "format('{0', 1)",
        "format('}')",
        "format('{}')",
        "format('{0:x}', 1)",
    ] {
        assert!(
            evaluate(expression, &contexts(), Status::default()).is_err(),
            "{expression}"
        );
    }
}

#[test]
fn join_examples() {
    assert_eq!(
        run("join(github.event.issue.labels.*.name, ', ')"),
        Value::String("bug, help wanted".into())
    );
    assert_eq!(
        run("join(matrix.numbers)"),
        Value::String("10,20,30".into())
    );
    assert_eq!(
        run("join('abc', fromJSON('bad'))"),
        Value::String("abc".into())
    );
    assert_eq!(
        run("join(fromJSON('[1]'), fromJSON('bad'))"),
        Value::String("1".into())
    );
    assert_eq!(
        run("join(fromJSON('[1,null,true]'), '-')"),
        Value::String("1--true".into())
    );
    assert_eq!(run("join(matrix)"), Value::String(String::new()));
    assert_eq!(run("join(fromJSON('[]'))"), Value::String(String::new()));
    assert_eq!(
        run("join(matrix.numbers, matrix)"),
        Value::String("10,20,30".into())
    );
}

#[test]
fn nested_joins_stop_at_the_memory_limit() {
    // Each inner join would build about 8 MB, far past the 1 MiB text limit.
    let zeros = vec!["0"; 1000].join(",");
    let inner = format!("join(fromJSON('[{zeros}]'), '{}')", "-".repeat(8000));
    let outer = format!("join(fromJSON('[{zeros}]'), {inner})");
    assert!(evaluate(&outer, &contexts(), Status::default()).is_err());
}

#[test]
fn json_functions_and_indentation() {
    assert_eq!(
        run("toJSON(fromJSON('{\"a\":[1,true,null],\"b\":{}}'))"),
        Value::String("{\n  \"a\": [\n    1,\n    true,\n    null\n  ],\n  \"b\": {}\n}".into())
    );
    assert_eq!(run("toJSON(1.0)"), Value::String("1".into()));
    assert_eq!(run("toJSON(null)"), Value::String("null".into()));
    assert_eq!(run("fromJSON('true')"), Value::Bool(true));
    assert_eq!(run("fromJSON('42')"), Value::Number(42.0));
    assert!(evaluate("fromJSON('bad')", &contexts(), Status::default()).is_err());
}

#[test]
fn conditions_and_status_functions() {
    for status in [
        Status::default(),
        Status {
            failed: true,
            cancelled: false,
        },
        Status {
            failed: false,
            cancelled: true,
        },
        Status {
            failed: true,
            cancelled: true,
        },
    ] {
        let context = contexts();
        assert_eq!(
            evaluate_condition("true", &context, status).unwrap(),
            !status.failed && !status.cancelled
        );
        assert_eq!(
            evaluate_condition("${{ true }}", &context, status).unwrap(),
            !status.failed && !status.cancelled
        );
        assert!(evaluate_condition("always()", &context, status).unwrap());
        assert_eq!(
            evaluate_condition("failure()", &context, status).unwrap(),
            status.failed
        );
        assert_eq!(
            evaluate_condition("cancelled()", &context, status).unwrap(),
            status.cancelled
        );
        assert_eq!(
            evaluate_condition("!cancelled()", &context, status).unwrap(),
            !status.cancelled
        );
        assert_eq!(
            evaluate_condition("contains('success()', 'success')", &context, status).unwrap(),
            !status.failed && !status.cancelled
        );
        assert!(!evaluate_condition("false && always()", &context, status).unwrap());
        assert!(evaluate_condition("true || failure()", &context, status).unwrap());
    }
    assert!(evaluate_condition(
        "foo",
        &contexts(),
        Status {
            failed: true,
            cancelled: false
        }
    )
    .is_err());
}

#[test]
fn interpolation_and_expression_boundaries() {
    let c = contexts();
    let s = Status::default();
    assert_eq!(
        interpolate("${{ 12 }}", &c, s).unwrap(),
        Value::Number(12.0)
    );
    assert_eq!(
        interpolate("${{ matrix.numbers }}", &c, s).unwrap(),
        Value::from_json(&json!([10, 20, 30]))
    );
    assert_eq!(
        interpolate("run ${{ matrix.os }}: ${{ 1.0 }}", &c, s).unwrap(),
        Value::String("run ubuntu-latest: 1".into())
    );
    assert_eq!(
        interpolate("${{ '' }}${{ 1 }}", &c, s).unwrap(),
        Value::String("1".into())
    );
    assert_eq!(
        interpolate(" ${{ true }} ", &c, s).unwrap(),
        Value::String(" true ".into())
    );
    assert_eq!(
        interpolate("${{ '}}' }}", &c, s).unwrap(),
        Value::String("}}".into())
    );
    assert_eq!(
        interpolate("${{ 'it''s }}' }}!", &c, s).unwrap(),
        Value::String("it's }}!".into())
    );
    assert!(interpolate("${{ github.ref", &c, s).is_err());
    assert_eq!(
        interpolate("no expressions", &c, s).unwrap(),
        Value::String("no expressions".into())
    );
}

#[test]
fn syntax_errors_and_parameter_counts() {
    assert_eq!(
        evaluate("foo", &contexts(), Status::default())
            .unwrap_err()
            .to_string(),
        "Unrecognized named-value: 'foo'. Located at position 1 within expression: foo"
    );
    assert!(evaluate("unknown()", &contexts(), Status::default())
        .unwrap_err()
        .to_string()
        .starts_with("Unrecognized function"));
    for expression in [
        "",
        "'unclosed",
        "\"double quotes\"",
        "True",
        "0x100000000",
        "0Xff",
        "0x+1",
        "0o+1",
        "-0xff",
        "1+2",
        "1.2.3",
        "1 2",
        "true false",
        "true &&",
        "(true",
        "true)",
        "github[]",
        "github.",
        "github.0",
        "'a'.x",
        "contains()",
        "contains(1)",
        "contains(1,2,3)",
        "always(1)",
        "join(1,2,3)",
        "format()",
        "contains(1,)",
    ] {
        assert!(
            evaluate(expression, &contexts(), Status::default()).is_err(),
            "{expression}"
        );
    }
}

#[test]
fn expression_limits() {
    let c = contexts();
    let s = Status::default();
    assert!(evaluate(&format!("'{}'", "a".repeat(20998)), &c, s).is_ok());
    assert!(evaluate(&format!("'{}'", "a".repeat(20999)), &c, s)
        .unwrap_err()
        .to_string()
        .contains("length 21000"));
    assert!(evaluate(&format!("{}true", "!".repeat(49)), &c, s).is_ok());
    assert!(evaluate(&format!("{}true", "!".repeat(50)), &c, s)
        .unwrap_err()
        .to_string()
        .contains("depth 50"));
    assert!(evaluate(&vec!["true"; 1000].join(" || "), &c, s).is_ok());
    assert!(evaluate(&"(".repeat(10000), &c, s).is_err());
    assert_eq!(
        evaluate(
            &format!("{}true{}", "(".repeat(10000), ")".repeat(10000)),
            &c,
            s
        )
        .unwrap(),
        Value::Bool(true)
    );
    assert_eq!(
        evaluate(
            &format!("{}true{}", "true || (".repeat(1000), ")".repeat(1000)),
            &c,
            s
        )
        .unwrap(),
        Value::Bool(true)
    );
}

#[test]
fn malformed_tokens_do_not_panic() {
    let tokens = [
        "(", ")", "[", "]", "!", "&&", ",", "true", "always()", "github", ".", "*", "'x'",
    ];
    let c = Contexts::default();
    for a in tokens {
        for b in tokens {
            for d in tokens {
                let _ = evaluate(&format!("{a} {b} {d}"), &c, Status::default());
            }
        }
    }
}

#[test]
fn hash_files_uses_the_workspace_context() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("a.txt"), "alpha").unwrap();
    let c = Contexts {
        values: BTreeMap::from([(
            "github".into(),
            Value::from_json(&json!({"workspace":workspace.path()})),
        )]),
    };
    assert_eq!(
        evaluate("hashFiles('**/*.txt')", &c, Status::default()).unwrap(),
        Value::String("aa86be763e41db7eaae266afc79ab46d02343c5d3b05da171d351afbd25c1525".into())
    );
    assert_eq!(
        evaluate("hashFiles('not-found')", &c, Status::default()).unwrap(),
        Value::String(String::new())
    );
    assert!(evaluate("hashFiles('*')", &Contexts::default(), Status::default()).is_err());
}
