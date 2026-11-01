//! Ported from actions/runner's `src/Sdk/Expressions/EvaluationResult.cs` and `Sdk/{Operators,Functions}`.
use std::{cell::RefCell, cmp::Ordering, collections::BTreeMap, rc::Rc};

use super::parser::{Kind, Node, Op};
use super::{fold, json_text, lookup, parse_number, Contexts, Error, Status, Value};

type Item = Rc<Data>;

/// Largest text an expression may build. Join and format stop before exceeding it.
pub(super) const MAX_TEXT_BYTES: usize = 1_048_576;

pub(super) fn memory_limit() -> Error {
    Error::Evaluation(
        "The maximum allowed memory size was exceeded while evaluating the expression".into(),
    )
}

enum Data {
    Scalar(Value),
    Array(Vec<Item>, bool),
    Object(BTreeMap<String, Item>),
}

impl Data {
    fn from_value(value: &Value) -> Item {
        Rc::new(match value {
            Value::Array(items) => Self::Array(items.iter().map(Self::from_value).collect(), false),
            Value::Object(items) => Self::Object(
                items
                    .iter()
                    .map(|(k, v)| (k.clone(), Self::from_value(v)))
                    .collect(),
            ),
            value => Self::Scalar(value.clone()),
        })
    }
    fn value(&self) -> Value {
        match self {
            Self::Scalar(v) => v.clone(),
            Self::Array(items, _) => Value::Array(items.iter().map(|v| v.value()).collect()),
            Self::Object(items) => {
                Value::Object(items.iter().map(|(k, v)| (k.clone(), v.value())).collect())
            }
        }
    }
    fn truthy(&self) -> bool {
        match self {
            Self::Scalar(v) => v.is_truthy(),
            _ => true,
        }
    }
    fn text(&self) -> String {
        match self {
            Self::Scalar(v) => v.to_text(),
            Self::Array(..) => "Array".into(),
            Self::Object(_) => "Object".into(),
        }
    }
    fn number(&self) -> f64 {
        match self {
            Self::Scalar(Value::Null) => 0.0,
            Self::Scalar(Value::Bool(v)) => {
                if *v {
                    1.0
                } else {
                    0.0
                }
            }
            Self::Scalar(Value::Number(v)) => *v,
            Self::Scalar(Value::String(v)) => parse_number(v),
            _ => f64::NAN,
        }
    }
    fn primitive(&self) -> bool {
        matches!(self, Self::Scalar(_))
    }
}

fn scalar(value: Value) -> Item {
    Rc::new(Data::Scalar(value))
}
fn boolean(value: bool) -> Item {
    scalar(Value::Bool(value))
}
fn text(value: String) -> Item {
    scalar(Value::String(value))
}
fn null() -> Item {
    scalar(Value::Null)
}

fn equal(a: &Item, b: &Item) -> bool {
    match (a.as_ref(), b.as_ref()) {
        (Data::Scalar(Value::Null), Data::Scalar(Value::Null)) => true,
        (Data::Scalar(Value::String(a)), Data::Scalar(Value::String(b))) => fold(a) == fold(b),
        (Data::Scalar(_), Data::Scalar(_)) => a.number() == b.number(),
        (Data::Array(..), Data::Array(..)) | (Data::Object(_), Data::Object(_)) => Rc::ptr_eq(a, b),
        _ => false,
    }
}

fn compare(a: &Item, b: &Item) -> Option<Ordering> {
    match (a.as_ref(), b.as_ref()) {
        (Data::Scalar(Value::Null), Data::Scalar(Value::Null)) => Some(Ordering::Equal),
        (Data::Scalar(Value::String(a)), Data::Scalar(Value::String(b))) => {
            Some(fold(a).encode_utf16().cmp(fold(b).encode_utf16()))
        }
        (Data::Scalar(_), Data::Scalar(_)) => a.number().partial_cmp(&b.number()),
        _ => None,
    }
}

pub(super) fn evaluate(tree: &Node, contexts: &Contexts, status: Status) -> Result<Value, Error> {
    let evaluator = Evaluator {
        contexts: &contexts.values,
        cache: RefCell::new(BTreeMap::new()),
        status,
    };
    Ok(evaluator.eval(tree)?.value())
}

struct Evaluator<'a> {
    contexts: &'a BTreeMap<String, Value>,
    cache: RefCell<BTreeMap<String, Item>>,
    status: Status,
}

impl Evaluator<'_> {
    fn eval(&self, node: &Node) -> Result<Item, Error> {
        match &node.kind {
            Kind::Literal(v) => Ok(Data::from_value(v)),
            Kind::Wildcard => Ok(text("*".into())),
            Kind::Context(name) => Ok(self
                .cache
                .borrow_mut()
                .entry(name.clone())
                .or_insert_with(|| {
                    lookup(self.contexts, name)
                        .map(Data::from_value)
                        .unwrap_or_else(null)
                })
                .clone()),
            Kind::Not(child) => Ok(boolean(!self.eval(child)?.truthy())),
            Kind::Binary(op, a, b) => {
                let a = self.eval(a)?;
                let b = self.eval(b)?;
                Ok(boolean(match op {
                    Op::Eq => equal(&a, &b),
                    Op::Ne => !equal(&a, &b),
                    Op::Lt => compare(&a, &b) == Some(Ordering::Less),
                    Op::Gt => compare(&a, &b) == Some(Ordering::Greater),
                    Op::Le => matches!(compare(&a, &b), Some(Ordering::Less | Ordering::Equal)),
                    Op::Ge => matches!(compare(&a, &b), Some(Ordering::Greater | Ordering::Equal)),
                    _ => unreachable!("logical operators have their own nodes"),
                }))
            }
            Kind::Logical(op, nodes) => {
                let mut value = null();
                for node in nodes {
                    value = self.eval(node)?;
                    if value.truthy() == (*op == Op::Or) {
                        break;
                    }
                }
                Ok(value)
            }
            Kind::Index(left, index) => {
                let left = self.eval(left)?;
                if !matches!(left.as_ref(), Data::Array(..) | Data::Object(_)) {
                    return Ok(if index.is_none() {
                        Rc::new(Data::Array(Vec::new(), true))
                    } else {
                        null()
                    });
                }
                let index = index.as_ref().map(|node| self.eval(node)).transpose()?;
                Ok(apply_index(&left, index.as_ref()))
            }
            Kind::Call(name, args) => self.call(name, args),
        }
    }
    fn call(&self, name: &str, args: &[Node]) -> Result<Item, Error> {
        match name {
            "always" => Ok(boolean(true)),
            "success" => Ok(boolean(!self.status.failed && !self.status.cancelled)),
            "failure" => Ok(boolean(self.status.failed)),
            "cancelled" => Ok(boolean(self.status.cancelled)),
            "format" => self.format(args),
            "contains" | "startswith" | "endswith" => {
                let left = self.eval(&args[0])?;
                if left.primitive() {
                    let right = self.eval(&args[1])?;
                    if !right.primitive() {
                        return Ok(boolean(false));
                    }
                    let left = fold(&left.text());
                    let right = fold(&right.text());
                    Ok(boolean(match name {
                        "contains" => left.contains(&right),
                        "startswith" => left.starts_with(&right),
                        _ => left.ends_with(&right),
                    }))
                } else if let Data::Array(items, _) = left.as_ref() {
                    if name == "contains" && !items.is_empty() {
                        let right = self.eval(&args[1])?;
                        return Ok(boolean(items.iter().any(|item| equal(item, &right))));
                    }
                    Ok(boolean(false))
                } else {
                    Ok(boolean(false))
                }
            }
            "join" => {
                let value = self.eval(&args[0])?;
                if let Data::Array(items, _) = value.as_ref() {
                    let mut separator = ",".into();
                    if items.len() > 1 && args.len() > 1 {
                        let value = self.eval(&args[1])?;
                        if value.primitive() {
                            separator = value.text();
                        }
                    }
                    let parts: Vec<String> = items.iter().map(|v| v.text()).collect();
                    // Check the size before joining, so a nested join cannot allocate past the limit.
                    let length = parts
                        .len()
                        .saturating_sub(1)
                        .checked_mul(separator.len())
                        .and_then(|separators| {
                            parts
                                .iter()
                                .try_fold(separators, |total, part| total.checked_add(part.len()))
                        });
                    if length.is_none_or(|length| length > MAX_TEXT_BYTES) {
                        return Err(memory_limit());
                    }
                    Ok(text(parts.join(&separator)))
                } else {
                    Ok(text(if value.primitive() {
                        value.text()
                    } else {
                        String::new()
                    }))
                }
            }
            "tojson" => Ok(text(json_text(&self.eval(&args[0])?.value())?)),
            "fromjson" => {
                let input = self.eval(&args[0])?.text();
                let value = serde_json::from_str(&input).map_err(|error| {
                    Error::Evaluation(format!("Error parsing fromJSON: {error}"))
                })?;
                Ok(Data::from_value(&Value::from_json(&value)))
            }
            "hashfiles" => {
                let github = lookup(self.contexts, "github");
                let workspace = github
                    .and_then(|github| match github {
                        Value::Object(values) => lookup(values, "workspace"),
                        _ => None,
                    })
                    .ok_or_else(|| Error::Evaluation("hashFiles requires github.workspace".into()))?
                    .to_text();
                let patterns = args
                    .iter()
                    .map(|arg| self.eval(arg).map(|v| v.text()))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(text(super::hash_files::hash_files(&workspace, &patterns)?))
            }
            _ => unreachable!("the parser validates functions"),
        }
    }
    fn format(&self, args: &[Node]) -> Result<Item, Error> {
        let format = self.eval(&args[0])?.text();
        let invalid =
            || Error::Evaluation(format!("The following format string is invalid: {format}"));
        let chars: Vec<char> = format.chars().collect();
        let mut i = 0;
        let mut output = String::new();
        let mut cache: BTreeMap<usize, String> = BTreeMap::new();
        while i < chars.len() {
            match chars[i] {
                '{' | '}' if chars.get(i + 1) == Some(&chars[i]) => {
                    output.push(chars[i]);
                    i += 2;
                }
                '{' => {
                    i += 1;
                    let start = i;
                    while chars.get(i).is_some_and(char::is_ascii_digit) {
                        i += 1;
                    }
                    let index = chars[start..i]
                        .iter()
                        .collect::<String>()
                        .parse::<u8>()
                        .map_err(|_| invalid())? as usize;
                    let mut specifiers = String::new();
                    if chars.get(i) == Some(&':') {
                        i += 1;
                        loop {
                            match chars.get(i) {
                                None => return Err(invalid()),
                                Some('}') if chars.get(i + 1) == Some(&'}') => {
                                    specifiers.push('}');
                                    i += 2;
                                }
                                Some('}') => break,
                                Some(c) => {
                                    specifiers.push(*c);
                                    i += 1;
                                }
                            }
                        }
                    }
                    if chars.get(i) != Some(&'}') {
                        return Err(invalid());
                    }
                    i += 1;
                    let arg = args.get(index + 1).ok_or_else(|| Error::Evaluation(format!("The following format string references more arguments than were supplied: {format}")))?;
                    if !specifiers.is_empty() {
                        return Err(Error::Evaluation(format!(
                            "The format specifiers '{specifiers}' are not valid"
                        )));
                    }
                    if let std::collections::btree_map::Entry::Vacant(entry) = cache.entry(index) {
                        entry.insert(self.eval(arg)?.text());
                    }
                    output.push_str(&cache[&index]);
                }
                '}' => return Err(invalid()),
                c => {
                    output.push(c);
                    i += 1;
                }
            }
            if output.len() > MAX_TEXT_BYTES {
                return Err(memory_limit());
            }
        }
        Ok(text(output))
    }
}

fn select(left: &Item, index: &Item) -> Option<Item> {
    match left.as_ref() {
        Data::Object(items) if index.primitive() => lookup(items, &index.text()).cloned(),
        Data::Array(items, _) => {
            let number = index.number();
            if number.is_finite() && (0.0..=i32::MAX as f64).contains(&number) {
                items.get(number.floor() as usize).cloned()
            } else {
                None
            }
        }
        _ => None,
    }
}

fn children(left: &Item) -> Vec<Item> {
    match left.as_ref() {
        Data::Array(items, _) => items.clone(),
        Data::Object(items) => items.values().cloned().collect(),
        _ => Vec::new(),
    }
}

fn apply_index(left: &Item, index: Option<&Item>) -> Item {
    if let Data::Array(items, true) = left.as_ref() {
        let result = items
            .iter()
            .flat_map(|item| {
                if let Some(index) = index {
                    select(item, index).into_iter().collect()
                } else {
                    children(item)
                }
            })
            .collect();
        Rc::new(Data::Array(result, true))
    } else if let Some(index) = index {
        select(left, index).unwrap_or_else(null)
    } else {
        Rc::new(Data::Array(children(left), true))
    }
}
