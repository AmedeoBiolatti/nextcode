//! A step's `if`: a small expression language, as GitHub Actions' `if`
//! without `${{ }}`. Values are strings (`'prod'`, `"prod"`), numbers,
//! `true`/`false`/`null`, references (`inputs.<id>`,
//! `steps.<id>.status`, `steps.<id>.exit_code`) and the functions
//! `success()` (every dependency ready or skipped), `failure()` (one
//! failed) and `always()`; operators `!`, `==`, `!=`, `<`, `<=`, `>`, `>=`,
//! `&&`, `||` and parentheses. `false`, `0`, `''` and `null` are false;
//! everything else is true.

use std::fmt;

/// A value an expression computes.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
}

impl Value {
    pub fn truthy(&self) -> bool {
        match self {
            Self::Null => false,
            Self::Bool(flag) => *flag,
            Self::Number(number) => *number != 0.0,
            Self::String(text) => !text.is_empty(),
        }
    }

    fn number(&self) -> Option<f64> {
        match self {
            Self::Number(number) => Some(*number),
            Self::String(text) => text.trim().parse().ok(),
            Self::Bool(flag) => Some(f64::from(u8::from(*flag))),
            Self::Null => None,
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("null"),
            Self::Bool(flag) => write!(f, "{flag}"),
            Self::Number(number) if number.fract() == 0.0 && number.abs() < 1e15 => {
                write!(f, "{}", *number as i64)
            }
            Self::Number(number) => write!(f, "{number}"),
            Self::String(text) => f.write_str(text),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Value),
    /// A dotted reference and where it starts in the expression's text.
    Ref(Vec<String>, usize),
    /// `success()`, `failure()`, `always()`.
    Call(String, usize),
    Not(Box<Expr>),
    Binary(Op, Box<Expr>, Box<Expr>),
}

/// A syntax error and where it is in the expression's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub message: String,
    pub offset: usize,
}

pub const FUNCTIONS: &[&str] = &["success", "failure", "always"];

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Value(Value),
    Word(String),
    Op(&'static str),
    Open,
    Close,
}

fn tokens(text: &str) -> Result<Vec<(Token, usize)>, Error> {
    let mut found = Vec::new();
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (at, c) = chars[i];
        let next = chars.get(i + 1).map(|&(_, c)| c);
        let (token, width) = match c {
            c if c.is_whitespace() => {
                i += 1;
                continue;
            }
            '(' => (Token::Open, 1),
            ')' => (Token::Close, 1),
            '!' if next == Some('=') => (Token::Op("!="), 2),
            '!' => (Token::Op("!"), 1),
            '=' if next == Some('=') => (Token::Op("=="), 2),
            '<' if next == Some('=') => (Token::Op("<="), 2),
            '>' if next == Some('=') => (Token::Op(">="), 2),
            '<' => (Token::Op("<"), 1),
            '>' => (Token::Op(">"), 1),
            '&' if next == Some('&') => (Token::Op("&&"), 2),
            '|' if next == Some('|') => (Token::Op("||"), 2),
            '\'' | '"' => {
                let mut value = String::new();
                let mut j = i + 1;
                loop {
                    let Some(&(_, inner)) = chars.get(j) else {
                        return Err(Error {
                            message: "unclosed string".into(),
                            offset: at,
                        });
                    };
                    j += 1;
                    if inner == c {
                        // `''` inside single quotes is a quote, as in YAML.
                        if c == '\'' && chars.get(j).map(|&(_, c)| c) == Some('\'') {
                            value.push('\'');
                            j += 1;
                            continue;
                        }
                        break;
                    }
                    value.push(inner);
                }
                found.push((Token::Value(Value::String(value)), at));
                i = j;
                continue;
            }
            c if c.is_ascii_digit() || (c == '-' && next.is_some_and(|n| n.is_ascii_digit())) => {
                let mut j = i + 1;
                while chars
                    .get(j)
                    .is_some_and(|&(_, c)| c.is_ascii_digit() || c == '.')
                {
                    j += 1;
                }
                let end = chars.get(j).map_or(text.len(), |&(at, _)| at);
                let number = text[at..end].parse().map_err(|_| Error {
                    message: format!("`{}` is not a number", &text[at..end]),
                    offset: at,
                })?;
                found.push((Token::Value(Value::Number(number)), at));
                i = j;
                continue;
            }
            c if c.is_alphabetic() || c == '_' => {
                let mut j = i + 1;
                while chars
                    .get(j)
                    .is_some_and(|&(_, c)| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
                {
                    j += 1;
                }
                let end = chars.get(j).map_or(text.len(), |&(at, _)| at);
                let word = &text[at..end];
                let token = match word {
                    "true" => Token::Value(Value::Bool(true)),
                    "false" => Token::Value(Value::Bool(false)),
                    "null" => Token::Value(Value::Null),
                    _ => Token::Word(word.to_owned()),
                };
                found.push((token, at));
                i = j;
                continue;
            }
            other => {
                let hint = match other {
                    '=' => " (compare with `==`)",
                    '&' => " (and is `&&`)",
                    '|' => " (or is `||`)",
                    '$' => " (write references without `${…}`: `inputs.env`)",
                    _ => "",
                };
                return Err(Error {
                    message: format!("unexpected `{other}`{hint}"),
                    offset: at,
                });
            }
        };
        found.push((token, at));
        i += width;
    }
    Ok(found)
}

struct Parser {
    tokens: Vec<(Token, usize)>,
    at: usize,
    end: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at).map(|(token, _)| token)
    }

    fn offset(&self) -> usize {
        self.tokens.get(self.at).map_or(self.end, |&(_, at)| at)
    }

    fn eat_op(&mut self, op: &str) -> bool {
        let found = matches!(self.peek(), Some(Token::Op(found)) if *found == op);
        if found {
            self.at += 1;
        }
        found
    }

    fn or(&mut self) -> Result<Expr, Error> {
        let mut left = self.and()?;
        while self.eat_op("||") {
            left = Expr::Binary(Op::Or, Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, Error> {
        let mut left = self.compare()?;
        while self.eat_op("&&") {
            left = Expr::Binary(Op::And, Box::new(left), Box::new(self.compare()?));
        }
        Ok(left)
    }

    fn compare(&mut self) -> Result<Expr, Error> {
        let left = self.unary()?;
        for (text, op) in [
            ("==", Op::Eq),
            ("!=", Op::Ne),
            ("<=", Op::Le),
            (">=", Op::Ge),
            ("<", Op::Lt),
            (">", Op::Gt),
        ] {
            if self.eat_op(text) {
                return Ok(Expr::Binary(op, Box::new(left), Box::new(self.unary()?)));
            }
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, Error> {
        if self.eat_op("!") {
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        let offset = self.offset();
        let Some((token, at)) = self.tokens.get(self.at).cloned() else {
            return Err(Error {
                message: "the expression ends too early".into(),
                offset,
            });
        };
        self.at += 1;
        match token {
            Token::Value(value) => Ok(Expr::Literal(value)),
            Token::Open => {
                let inner = self.or()?;
                if self.peek() != Some(&Token::Close) {
                    return Err(Error {
                        message: "missing `)`".into(),
                        offset: self.offset(),
                    });
                }
                self.at += 1;
                Ok(inner)
            }
            Token::Word(word) if self.peek() == Some(&Token::Open) => {
                self.at += 1;
                if self.peek() != Some(&Token::Close) {
                    return Err(Error {
                        message: format!("`{word}()` takes no arguments"),
                        offset: self.offset(),
                    });
                }
                self.at += 1;
                if !FUNCTIONS.contains(&word.as_str()) {
                    return Err(Error {
                        message: format!(
                            "unknown function `{word}()` (functions: success(), failure(), always())"
                        ),
                        offset: at,
                    });
                }
                Ok(Expr::Call(word, at))
            }
            Token::Word(word) => Ok(Expr::Ref(word.split('.').map(str::to_owned).collect(), at)),
            Token::Close => Err(Error {
                message: "unexpected `)`".into(),
                offset: at,
            }),
            Token::Op(op) => Err(Error {
                message: format!("unexpected `{op}`"),
                offset: at,
            }),
        }
    }
}

/// Parses an `if` expression.
pub fn parse(text: &str) -> Result<Expr, Error> {
    let tokens = tokens(text)?;
    if tokens.is_empty() {
        return Err(Error {
            message: "an empty `if`".into(),
            offset: 0,
        });
    }
    let mut parser = Parser {
        tokens,
        at: 0,
        end: text.len(),
    };
    let expr = parser.or()?;
    if parser.at < parser.tokens.len() {
        return Err(Error {
            message: "unexpected text after the expression (join conditions with `&&` or `||`)"
                .into(),
            offset: parser.offset(),
        });
    }
    Ok(expr)
}

impl Expr {
    /// Every reference with where it is.
    pub fn refs(&self) -> Vec<(&[String], usize)> {
        let mut found = Vec::new();
        fn visit<'a>(expr: &'a Expr, found: &mut Vec<(&'a [String], usize)>) {
            match expr {
                Expr::Ref(path, at) => found.push((path, *at)),
                Expr::Not(inner) => visit(inner, found),
                Expr::Binary(_, left, right) => {
                    visit(left, found);
                    visit(right, found);
                }
                Expr::Literal(_) | Expr::Call(..) => {}
            }
        }
        visit(self, &mut found);
        found
    }

    /// The value with references and functions answered by `lookup` and
    /// `call` (an unknown reference is `null`).
    pub fn eval(&self, lookup: &dyn Fn(&[String]) -> Value, call: &dyn Fn(&str) -> bool) -> Value {
        match self {
            Expr::Literal(value) => value.clone(),
            Expr::Ref(path, _) => lookup(path),
            Expr::Call(name, _) => Value::Bool(call(name)),
            Expr::Not(inner) => Value::Bool(!inner.eval(lookup, call).truthy()),
            Expr::Binary(Op::And, left, right) => {
                let left = left.eval(lookup, call);
                if left.truthy() {
                    right.eval(lookup, call)
                } else {
                    left
                }
            }
            Expr::Binary(Op::Or, left, right) => {
                let left = left.eval(lookup, call);
                if left.truthy() {
                    left
                } else {
                    right.eval(lookup, call)
                }
            }
            Expr::Binary(op, left, right) => {
                let (left, right) = (left.eval(lookup, call), right.eval(lookup, call));
                let equal = match (&left, &right) {
                    (Value::Null, Value::Null) => true,
                    (Value::Null, _) | (_, Value::Null) => false,
                    (Value::Number(_), _) | (_, Value::Number(_)) => {
                        left.number().is_some() && left.number() == right.number()
                    }
                    _ => left.to_string() == right.to_string(),
                };
                let ordered = || left.number().zip(right.number());
                Value::Bool(match op {
                    Op::Eq => equal,
                    Op::Ne => !equal,
                    Op::Lt => ordered().is_some_and(|(a, b)| a < b),
                    Op::Le => ordered().is_some_and(|(a, b)| a <= b),
                    Op::Gt => ordered().is_some_and(|(a, b)| a > b),
                    Op::Ge => ordered().is_some_and(|(a, b)| a >= b),
                    Op::And | Op::Or => unreachable!("handled above"),
                })
            }
        }
    }

    /// Whether it calls `name()`.
    pub fn calls(&self, name: &str) -> bool {
        match self {
            Expr::Call(called, _) => called == name,
            Expr::Not(inner) => inner.calls(name),
            Expr::Binary(_, left, right) => left.calls(name) || right.calls(name),
            Expr::Literal(_) | Expr::Ref(..) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(text: &str) -> Value {
        let lookup = |path: &[String]| match path.join(".").as_str() {
            "inputs.env" => Value::String("prod".into()),
            "inputs.workers" => Value::Number(8.0),
            "inputs.clean" => Value::Bool(false),
            "steps.db.status" => Value::String("failed".into()),
            "steps.db.exit_code" => Value::Number(3.0),
            _ => Value::Null,
        };
        let call = |name: &str| name != "success";
        parse(text).unwrap().eval(&lookup, &call)
    }

    #[test]
    fn expressions_evaluate_like_github_actions_if() {
        assert_eq!(eval("inputs.env == 'prod'"), Value::Bool(true));
        assert_eq!(eval("inputs.env != \"prod\""), Value::Bool(false));
        assert_eq!(
            eval("inputs.workers >= 4 && !inputs.clean"),
            Value::Bool(true)
        );
        assert_eq!(
            eval("steps.db.status == 'failed' || false"),
            Value::Bool(true)
        );
        assert_eq!(
            eval("steps.db.exit_code == '3'"),
            Value::Bool(true),
            "numbers compare as numbers"
        );
        assert_eq!(
            eval("(inputs.clean || inputs.workers > 10) && always()"),
            Value::Bool(false)
        );
        assert_eq!(eval("failure() && !success()"), Value::Bool(true));
        assert_eq!(eval("inputs.nope == null"), Value::Bool(true));
        assert_eq!(eval("'it''s' == \"it's\""), Value::Bool(true));
        assert!(eval("inputs.env").truthy());
        assert!(!eval("''").truthy() && !eval("0").truthy() && !eval("null").truthy());
        assert_eq!(eval("inputs.workers < -1"), Value::Bool(false));
    }

    #[test]
    fn errors_say_what_and_where() {
        let error = |text: &str| parse(text).unwrap_err();
        assert_eq!(
            error("inputs.env = 'prod'").message,
            "unexpected `=` (compare with `==`)"
        );
        assert_eq!(error("inputs.env = 'prod'").offset, 11);
        assert!(error("'open").message.contains("unclosed string"));
        assert!(error("(a == b").message.contains("missing `)`"));
        assert!(
            error("done()")
                .message
                .contains("unknown function `done()`")
        );
        assert!(error("a b").message.contains("join conditions"));
        assert!(
            error("${inputs.env} == 'x'")
                .message
                .contains("without `${…}`")
        );
        assert!(error("").message.contains("empty"));
        assert!(error("a ==").message.contains("ends too early"));
    }

    #[test]
    fn references_and_calls_are_listed() {
        let expr = parse("inputs.env == 'x' && (steps.db.status != 'ready' || failure())").unwrap();
        let refs: Vec<(String, usize)> = expr
            .refs()
            .into_iter()
            .map(|(path, at)| (path.join("."), at))
            .collect();
        assert_eq!(
            refs,
            [("inputs.env".into(), 0), ("steps.db.status".into(), 22)]
        );
        assert!(expr.calls("failure") && !expr.calls("always"));
    }
}
