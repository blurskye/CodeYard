//! The inputs a manual `workflow_dispatch` run takes: the fields a workflow declares, and the
//! checks a submitted set must pass before its run is queued.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// GitHub's limits: ten inputs per workflow and 25 options per choice.
pub const MAX_INPUTS: usize = 10;
pub const MAX_NAME_BYTES: usize = 64;
pub const MAX_OPTIONS: usize = 25;
/// Longest accepted value; a form value is capped at the same size.
pub const MAX_VALUE_BYTES: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Text,
    Boolean,
    Number,
    Choice,
    Environment,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DispatchInput {
    pub name: String,
    pub description: String,
    pub kind: InputKind,
    pub required: bool,
    /// Stored as text, the form every value takes in `event.inputs`.
    pub default: Option<String>,
    /// The allowed values of a `choice`; an `environment` input may list them too.
    pub options: Vec<String>,
}

/// Checks a declaration when its workflow is read, so a bad file is refused before it can
/// be run.
pub fn check_declaration(input: &DispatchInput) -> Result<(), &'static str> {
    if input.name.is_empty()
        || input.name.len() > MAX_NAME_BYTES
        || !input
            .name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        return Err("workflow input name is invalid");
    }
    if input.options.len() > MAX_OPTIONS {
        return Err("workflow input lists too many options");
    }
    if input.kind == InputKind::Choice && input.options.is_empty() {
        return Err("choice workflow input needs options");
    }
    if !input.options.is_empty()
        && !matches!(input.kind, InputKind::Choice | InputKind::Environment)
    {
        return Err("only choice and environment workflow inputs take options");
    }
    if let Some(default) = &input.default {
        check_value(input, default)?;
    }
    Ok(())
}

/// Checks the submitted values against the declared inputs and returns the complete set the
/// run gets: each submitted value, or the default for an input left out. An empty result means
/// the workflow declares no inputs and none were sent.
pub fn validate_inputs(
    declared: &[DispatchInput],
    submitted: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, &'static str> {
    if submitted
        .keys()
        .any(|name| !declared.iter().any(|input| &input.name == name))
    {
        return Err("workflow input is not declared by this workflow");
    }
    let mut values = BTreeMap::new();
    for input in declared {
        let submitted_value = submitted.get(&input.name);
        if input.required && submitted_value.is_some_and(String::is_empty) {
            return Err("a required workflow input is empty");
        }
        let given = submitted_value.filter(|value| !value.is_empty());
        match given.or(input.default.as_ref()) {
            Some(value) => {
                if input.required && value.is_empty() {
                    return Err("a required workflow input is empty");
                }
                check_value(input, value)?;
                values.insert(input.name.clone(), value.clone());
            }
            None if input.required => return Err("a required workflow input is missing"),
            None => {}
        }
    }
    Ok(values)
}

fn check_value(input: &DispatchInput, value: &str) -> Result<(), &'static str> {
    if value.len() > MAX_VALUE_BYTES || value.chars().any(char::is_control) {
        return Err("workflow input value is too long or contains control characters");
    }
    match input.kind {
        InputKind::Text => Ok(()),
        InputKind::Boolean if matches!(value, "true" | "false") => Ok(()),
        InputKind::Boolean => Err("boolean workflow input must be true or false"),
        InputKind::Number => match value.parse::<f64>() {
            Ok(number) if number.is_finite() => Ok(()),
            _ => Err("number workflow input must be a number"),
        },
        InputKind::Choice | InputKind::Environment
            if input.options.is_empty() || input.options.iter().any(|option| option == value) =>
        {
            Ok(())
        }
        InputKind::Choice | InputKind::Environment => {
            Err("workflow input is not one of its options")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(name: &str, kind: InputKind) -> DispatchInput {
        DispatchInput {
            name: name.into(),
            description: String::new(),
            kind,
            required: false,
            default: None,
            options: Vec::new(),
        }
    }

    fn submitted(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn defaults_fill_missing_inputs_and_submitted_values_win() {
        let mut version = input("version", InputKind::Text);
        version.default = Some("1.0".into());
        let mut debug = input("debug", InputKind::Boolean);
        debug.default = Some("false".into());
        let declared = [version, debug];
        assert_eq!(
            validate_inputs(&declared, &submitted(&[("debug", "true")])).unwrap(),
            submitted(&[("version", "1.0"), ("debug", "true")])
        );
        assert_eq!(
            validate_inputs(&declared, &submitted(&[("version", "")])).unwrap(),
            submitted(&[("version", "1.0"), ("debug", "false")])
        );
    }

    #[test]
    fn unknown_and_missing_required_inputs_are_refused() {
        let mut target = input("target", InputKind::Text);
        target.required = true;
        let declared = [target];
        assert!(validate_inputs(&declared, &submitted(&[("extra", "x")])).is_err());
        assert!(validate_inputs(&declared, &submitted(&[])).is_err());
        assert!(validate_inputs(&declared, &submitted(&[("target", "")])).is_err());
        assert!(validate_inputs(&declared, &submitted(&[("target", "prod")])).is_ok());
    }

    #[test]
    fn a_required_input_refuses_an_empty_value_even_with_a_default() {
        let mut env = input("env", InputKind::Text);
        env.required = true;
        env.default = Some("staging".into());
        let declared = [env];
        assert!(validate_inputs(&declared, &submitted(&[("env", "")])).is_err());
        // Left out, the default still applies.
        assert_eq!(
            validate_inputs(&declared, &submitted(&[])).unwrap(),
            submitted(&[("env", "staging")])
        );
    }

    #[test]
    fn required_inputs_refuse_empty_defaults() {
        let mut target = input("target", InputKind::Text);
        target.required = true;
        target.default = Some(String::new());
        assert!(check_declaration(&target).is_ok());
        assert!(validate_inputs(&[target.clone()], &BTreeMap::new()).is_err());
        assert!(validate_inputs(&[target], &submitted(&[("target", "prod")])).is_ok());
    }

    #[test]
    fn typed_inputs_accept_only_their_values() {
        let declared = [
            input("flag", InputKind::Boolean),
            input("count", InputKind::Number),
        ];
        assert!(validate_inputs(&declared, &submitted(&[("flag", "yes")])).is_err());
        assert!(validate_inputs(&declared, &submitted(&[("count", "nan")])).is_err());
        assert!(validate_inputs(&declared, &submitted(&[("count", "x1")])).is_err());
        assert!(
            validate_inputs(&declared, &submitted(&[("flag", "true"), ("count", "2.5")])).is_ok()
        );
    }

    #[test]
    fn choices_must_be_listed_options() {
        let mut env = input("env", InputKind::Choice);
        env.options = vec!["staging".into(), "production".into()];
        let declared = [env];
        assert!(validate_inputs(&declared, &submitted(&[("env", "staging")])).is_ok());
        assert!(validate_inputs(&declared, &submitted(&[("env", "dev")])).is_err());
        assert!(check_declaration(&DispatchInput {
            kind: InputKind::Choice,
            options: Vec::new(),
            ..input("env", InputKind::Choice)
        })
        .is_err());
    }

    #[test]
    fn declarations_reject_bad_names_and_defaults() {
        assert!(check_declaration(&input("bad:name", InputKind::Text)).is_err());
        assert!(check_declaration(&input("ok_name-1", InputKind::Text)).is_ok());
        let mut count = input("count", InputKind::Number);
        count.default = Some("many".into());
        assert!(check_declaration(&count).is_err());
    }
}
