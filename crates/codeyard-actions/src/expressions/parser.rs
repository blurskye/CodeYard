//! Ported from actions/runner's `src/Sdk/Expressions/ExpressionParser.cs` and `Tokens/LexicalAnalyzer.cs`.
use super::{lookup, parse_number, Contexts, Error, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Op {
    Not,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
}

pub(super) struct Node {
    pub kind: Kind,
    depth: usize,
}

pub(super) enum Kind {
    Literal(Value),
    Wildcard,
    Context(String),
    Not(Box<Node>),
    Binary(Op, Box<Node>, Box<Node>),
    Logical(Op, Vec<Node>),
    Call(String, Vec<Node>),
    Index(Box<Node>, Option<Box<Node>>),
}

impl Node {
    pub fn has_status_function(&self) -> bool {
        match &self.kind {
            Kind::Call(name, args) => {
                matches!(
                    name.as_str(),
                    "success" | "failure" | "cancelled" | "always"
                ) || args.iter().any(Self::has_status_function)
            }
            Kind::Not(node) => node.has_status_function(),
            Kind::Binary(_, a, b) => a.has_status_function() || b.has_status_function(),
            Kind::Logical(_, nodes) => nodes.iter().any(Self::has_status_function),
            Kind::Index(a, b) => {
                a.has_status_function() || b.as_ref().is_some_and(|b| b.has_status_function())
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum TokenKind {
    Word(String),
    Number(f64),
    Text(String),
    Op(Op),
    Left,
    Right,
    Bracket,
    EndBracket,
    Dot,
    Star,
    Comma,
    End,
}

#[derive(Clone, Debug)]
struct Token {
    kind: TokenKind,
    start: usize,
    end: usize,
}

fn syntax(expression: &str, token: &Token, description: &str) -> Error {
    if token.kind == TokenKind::End {
        return Error::Syntax("Unexpected end of expression".into());
    }
    Error::Syntax(format!(
        "{description}: '{}'. Located at position {} within expression: {expression}",
        &expression[token.start..token.end],
        expression[..token.start].encode_utf16().count() + 1
    ))
}

fn boundary(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '(' | ')' | '[' | ']' | ',' | '.' | '!' | '>' | '<' | '=' | '&' | '|'
        )
}

fn lex(expression: &str) -> Result<Vec<Token>, Error> {
    let mut tokens: Vec<Token> = Vec::new();
    let mut i = 0;
    while i < expression.len() {
        let c = expression[i..].chars().next().expect("character boundary");
        if c.is_whitespace() {
            i += c.len_utf8();
            continue;
        }
        let start = i;
        i += c.len_utf8();
        let operand = tokens.last().is_none_or(|t| {
            matches!(
                t.kind,
                TokenKind::Left | TokenKind::Bracket | TokenKind::Comma | TokenKind::Op(_)
            )
        });
        let kind = match c {
            '(' => TokenKind::Left,
            ')' => TokenKind::Right,
            '[' => TokenKind::Bracket,
            ']' => TokenKind::EndBracket,
            ',' => TokenKind::Comma,
            '*' => TokenKind::Star,
            '.' if !operand => TokenKind::Dot,
            '\'' => {
                let mut text = String::new();
                let mut closed = false;
                while i < expression.len() {
                    let c = expression[i..].chars().next().expect("character boundary");
                    i += c.len_utf8();
                    if c == '\'' {
                        if expression.as_bytes().get(i) == Some(&b'\'') {
                            i += 1;
                        } else {
                            closed = true;
                            break;
                        }
                    }
                    text.push(c);
                }
                if !closed {
                    return Err(syntax(
                        expression,
                        &Token {
                            kind: TokenKind::Text(text),
                            start,
                            end: i,
                        },
                        "Unexpected symbol",
                    ));
                }
                TokenKind::Text(text)
            }
            '!' | '<' | '>' | '=' | '&' | '|' => {
                let next = expression.as_bytes().get(i).copied();
                let op = match (c, next) {
                    ('!', Some(b'=')) => Op::Ne,
                    ('<', Some(b'=')) => Op::Le,
                    ('>', Some(b'=')) => Op::Ge,
                    ('=', Some(b'=')) => Op::Eq,
                    ('&', Some(b'&')) => Op::And,
                    ('|', Some(b'|')) => Op::Or,
                    ('!', _) => Op::Not,
                    ('<', _) => Op::Lt,
                    ('>', _) => Op::Gt,
                    _ => {
                        return Err(syntax(
                            expression,
                            &Token {
                                kind: TokenKind::EndBracket,
                                start,
                                end: i,
                            },
                            "Unexpected symbol",
                        ))
                    }
                };
                if matches!(op, Op::Ne | Op::Le | Op::Ge | Op::Eq | Op::And | Op::Or) {
                    i += 1;
                }
                TokenKind::Op(op)
            }
            _ => {
                let number = c.is_ascii_digit() || matches!(c, '-' | '+' | '.');
                while i < expression.len() {
                    let next = expression[i..].chars().next().expect("character boundary");
                    if boundary(next) && !(number && next == '.') {
                        break;
                    }
                    i += next.len_utf8();
                }
                let raw = &expression[start..i];
                if number {
                    let value = parse_number(raw);
                    if value.is_nan() {
                        return Err(syntax(
                            expression,
                            &Token {
                                kind: TokenKind::Number(value),
                                start,
                                end: i,
                            },
                            "Unexpected symbol",
                        ));
                    }
                    TokenKind::Number(value)
                } else {
                    if !raw.chars().enumerate().all(|(i, c)| {
                        c.is_ascii_alphabetic()
                            || c == '_'
                            || (i > 0 && (c.is_ascii_digit() || c == '-'))
                    }) {
                        return Err(syntax(
                            expression,
                            &Token {
                                kind: TokenKind::Word(raw.into()),
                                start,
                                end: i,
                            },
                            "Unexpected symbol",
                        ));
                    }
                    TokenKind::Word(raw.into())
                }
            }
        };
        tokens.push(Token {
            kind,
            start,
            end: i,
        });
    }
    tokens.push(Token {
        kind: TokenKind::End,
        start: i,
        end: i,
    });
    Ok(tokens)
}

pub(super) fn parse(expression: &str, contexts: &Contexts) -> Result<Node, Error> {
    if expression.encode_utf16().count() > 21000 {
        return Err(Error::Syntax("Exceeded max expression length 21000".into()));
    }
    Parser {
        expression,
        contexts,
        tokens: lex(expression)?,
        index: 0,
        operands: Vec::new(),
        operators: Vec::new(),
    }
    .parse()
}

// Grouping does not count towards the runner's tree-depth limit. Explicit stacks
// also let us reject long malformed expressions without exhausting the Rust stack.
enum Pending {
    Operator(Op),
    Group(usize),
    Index(usize),
    Call {
        name: String,
        token: Token,
        base: usize,
        separators: usize,
    },
}

struct Parser<'a> {
    expression: &'a str,
    contexts: &'a Contexts,
    tokens: Vec<Token>,
    index: usize,
    operands: Vec<Node>,
    operators: Vec<Pending>,
}

fn node(kind: Kind, depth: usize) -> Result<Node, Error> {
    if depth > 50 {
        Err(Error::Syntax("Exceeded max expression depth 50".into()))
    } else {
        Ok(Node { kind, depth })
    }
}

fn precedence(op: Op) -> u8 {
    match op {
        Op::Or => 1,
        Op::And => 2,
        Op::Eq | Op::Ne => 3,
        Op::Lt | Op::Le | Op::Gt | Op::Ge => 4,
        Op::Not => 5,
    }
}

fn function_limits(name: &str) -> Option<(usize, usize)> {
    Some(match name {
        "contains" | "startswith" | "endswith" => (2, 2),
        "format" | "hashfiles" => (1, 255),
        "join" => (1, 2),
        "tojson" | "fromjson" => (1, 1),
        "always" | "success" | "failure" | "cancelled" => (0, 0),
        _ => return None,
    })
}

impl Parser<'_> {
    fn current(&self) -> &Token {
        &self.tokens[self.index]
    }
    fn error(&self) -> Error {
        syntax(self.expression, self.current(), "Unexpected symbol")
    }
    fn advance(&mut self) -> Token {
        let token = self.current().clone();
        if token.kind != TokenKind::End {
            self.index += 1;
        }
        token
    }
    fn pop(&mut self) -> Result<Node, Error> {
        self.operands.pop().ok_or_else(|| self.error())
    }
    fn flush(&mut self) -> Result<(), Error> {
        while matches!(self.operators.last(), Some(Pending::Operator(_))) {
            self.reduce()?;
        }
        Ok(())
    }
    fn reduce(&mut self) -> Result<(), Error> {
        let Some(Pending::Operator(op)) = self.operators.pop() else {
            return Err(self.error());
        };
        let right = self.pop()?;
        let result = if op == Op::Not {
            let depth = right.depth + 1;
            node(Kind::Not(Box::new(right)), depth)?
        } else {
            let left = self.pop()?;
            if matches!(op, Op::And | Op::Or) {
                let (mut children, mut depth) = match left.kind {
                    Kind::Logical(previous, nested) if previous == op => (nested, left.depth - 1),
                    kind => (vec![Node { kind, depth: left.depth }], left.depth),
                };
                match right.kind {
                    Kind::Logical(previous, nested) if previous == op => {
                        depth = depth.max(right.depth - 1);
                        children.extend(nested);
                    }
                    kind => {
                        depth = depth.max(right.depth);
                        children.push(Node { kind, depth: right.depth });
                    }
                }
                let depth = depth + 1;
                node(Kind::Logical(op, children), depth)?
            } else {
                let depth = left.depth.max(right.depth) + 1;
                node(Kind::Binary(op, Box::new(left), Box::new(right)), depth)?
            }
        };
        self.operands.push(result);
        Ok(())
    }
    fn index(&mut self, index: Node) -> Result<(), Error> {
        let left = self.pop()?;
        let depth = left.depth.max(index.depth) + 1;
        let index = if matches!(index.kind, Kind::Wildcard) {
            None
        } else {
            Some(Box::new(index))
        };
        self.operands
            .push(node(Kind::Index(Box::new(left), index), depth)?);
        Ok(())
    }
    fn parse(mut self) -> Result<Node, Error> {
        let mut expecting = true;
        let mut indexable = false;
        while self.current().kind != TokenKind::End {
            let token = self.advance();
            match token.kind.clone() {
                TokenKind::Op(Op::Not) if expecting => {
                    self.operators.push(Pending::Operator(Op::Not))
                }
                TokenKind::Left if expecting => {
                    self.operators.push(Pending::Group(self.operands.len()))
                }
                TokenKind::Word(name) if expecting => {
                    let literal = match name.as_str() {
                        "null" => Some(Value::Null),
                        "true" => Some(Value::Bool(true)),
                        "false" => Some(Value::Bool(false)),
                        "NaN" => Some(Value::Number(f64::NAN)),
                        "Infinity" => Some(Value::Number(f64::INFINITY)),
                        _ => None,
                    };
                    if let Some(value) = literal {
                        self.operands.push(node(Kind::Literal(value), 1)?);
                        expecting = false;
                        indexable = false;
                    } else {
                        let name = name.to_ascii_lowercase();
                        if self.current().kind == TokenKind::Left {
                            if function_limits(&name).is_none() {
                                return Err(syntax(
                                    self.expression,
                                    &token,
                                    "Unrecognized function",
                                ));
                            }
                            self.advance();
                            self.operators.push(Pending::Call {
                                name,
                                token,
                                base: self.operands.len(),
                                separators: 0,
                            });
                        } else {
                            if lookup(&self.contexts.values, &name).is_none()
                                && !matches!(
                                    name.as_str(),
                                    "github"
                                        | "env"
                                        | "vars"
                                        | "secrets"
                                        | "inputs"
                                        | "matrix"
                                        | "strategy"
                                        | "needs"
                                        | "steps"
                                        | "job"
                                        | "jobs"
                                        | "runner"
                                )
                            {
                                return Err(syntax(
                                    self.expression,
                                    &token,
                                    "Unrecognized named-value",
                                ));
                            }
                            self.operands.push(node(Kind::Context(name), 1)?);
                            expecting = false;
                            indexable = true;
                        }
                    }
                }
                TokenKind::Number(value) if expecting => {
                    self.operands
                        .push(node(Kind::Literal(Value::Number(value)), 1)?);
                    expecting = false;
                    indexable = false;
                }
                TokenKind::Text(value) if expecting => {
                    self.operands
                        .push(node(Kind::Literal(Value::String(value)), 1)?);
                    expecting = false;
                    indexable = false;
                }
                TokenKind::Star
                    if expecting
                        && self.index >= 2
                        && self.tokens[self.index - 2].kind == TokenKind::Bracket =>
                {
                    self.operands.push(node(Kind::Wildcard, 1)?);
                    expecting = false;
                    indexable = true;
                }
                TokenKind::Bracket if !expecting && indexable => {
                    self.operators.push(Pending::Index(self.operands.len() - 1));
                    expecting = true;
                }
                TokenKind::Dot if !expecting && indexable => {
                    let property = self.advance();
                    let kind = match property.kind.clone() {
                        TokenKind::Star => Kind::Wildcard,
                        TokenKind::Word(name) => Kind::Literal(Value::String(name)),
                        _ => return Err(syntax(self.expression, &property, "Unexpected symbol")),
                    };
                    self.index(node(kind, 1)?)?;
                }
                TokenKind::Op(op) if !expecting && op != Op::Not => {
                    while let Some(Pending::Operator(previous)) = self.operators.last() {
                        if precedence(*previous) < precedence(op) {
                            break;
                        }
                        self.reduce()?;
                    }
                    self.operators.push(Pending::Operator(op));
                    expecting = true;
                }
                TokenKind::Comma if !expecting => {
                    self.flush()?;
                    match self.operators.last_mut() {
                        Some(Pending::Call {
                            base, separators, ..
                        }) if self.operands.len() == *base + *separators + 1 => *separators += 1,
                        _ => return Err(syntax(self.expression, &token, "Unexpected symbol")),
                    }
                    expecting = true;
                }
                TokenKind::Right => {
                    if !expecting {
                        self.flush()?;
                    }
                    match self.operators.pop() {
                        Some(Pending::Group(base))
                            if !expecting && self.operands.len() == base + 1 => {}
                        Some(Pending::Call {
                            name,
                            token: function,
                            base,
                            separators,
                        }) => {
                            let empty = expecting && separators == 0 && self.operands.len() == base;
                            if !empty && (expecting || self.operands.len() != base + separators + 1)
                            {
                                return Err(syntax(self.expression, &token, "Unexpected symbol"));
                            }
                            let args = self.operands.split_off(base);
                            let (min, max) = function_limits(&name).expect("validated function");
                            if args.len() < min {
                                return Err(syntax(
                                    self.expression,
                                    &function,
                                    "Too few parameters supplied",
                                ));
                            }
                            if args.len() > max {
                                return Err(syntax(
                                    self.expression,
                                    &function,
                                    "Too many parameters supplied",
                                ));
                            }
                            let depth = args.iter().map(|n| n.depth).max().unwrap_or(0) + 1;
                            self.operands.push(node(Kind::Call(name, args), depth)?);
                        }
                        _ => return Err(syntax(self.expression, &token, "Unexpected symbol")),
                    }
                    expecting = false;
                    indexable = true;
                }
                TokenKind::EndBracket if !expecting => {
                    self.flush()?;
                    let Some(Pending::Index(base)) = self.operators.pop() else {
                        return Err(syntax(self.expression, &token, "Unexpected symbol"));
                    };
                    if self.operands.len() != base + 2 {
                        return Err(syntax(self.expression, &token, "Unexpected symbol"));
                    }
                    let index = self.pop()?;
                    self.index(index)?;
                    indexable = true;
                }
                _ => return Err(syntax(self.expression, &token, "Unexpected symbol")),
            }
        }
        if expecting {
            return Err(self.error());
        }
        self.flush()?;
        if !self.operators.is_empty() || self.operands.len() != 1 {
            return Err(self.error());
        }
        self.pop()
    }
}
