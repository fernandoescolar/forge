//! The subset of MSBuild `Condition` syntax seen in real projects: `==`, `!=`, `<`, `>`,
//! `<=`, `>=`, `And`, `Or`, `!`, parentheses, `Exists()`, `HasTrailingSlash()` and bare
//! `true`/`false`. Properties are expanded before parsing. Anything it cannot read counts
//! as true, so unknown syntax shows more of a project rather than less.

use std::path::Path;

use super::Properties;

#[derive(Clone, Debug, PartialEq)]
enum Token {
    LParen,
    RParen,
    And,
    Or,
    Not,
    Op(&'static str),
    Func(String),
    Str(String),
}

pub fn evaluate(condition: &str, properties: &Properties, base_dir: &Path) -> bool {
    if condition.trim().is_empty() {
        return true;
    }
    let expanded = properties.expand(condition);
    let Some(tokens) = tokenize(&expanded) else { return true };
    let mut parser = Parser { tokens, pos: 0, base_dir };
    match parser.or() {
        Some(value) if parser.pos == parser.tokens.len() => value,
        _ => true,
    }
}

fn tokenize(input: &str) -> Option<Vec<Token>> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            c if c.is_whitespace() => i += 1,
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            '\'' => {
                let end = chars[i + 1..].iter().position(|&c| c == '\'')? + i + 1;
                tokens.push(Token::Str(chars[i + 1..end].iter().collect()));
                i = end + 1;
            }
            '=' if next == Some('=') => {
                tokens.push(Token::Op("=="));
                i += 2;
            }
            '!' if next == Some('=') => {
                tokens.push(Token::Op("!="));
                i += 2;
            }
            '<' | '>' => {
                let op = match (c, next) {
                    ('<', Some('=')) => "<=",
                    ('>', Some('=')) => ">=",
                    ('<', _) => "<",
                    _ => ">",
                };
                tokens.push(Token::Op(op));
                i += op.len();
            }
            '!' => {
                tokens.push(Token::Not);
                i += 1;
            }
            _ => {
                let start = i;
                while i < chars.len() && !chars[i].is_whitespace() && !"()!='<>".contains(chars[i]) {
                    i += 1;
                }
                if i == start {
                    return None;
                }
                let word: String = chars[start..i].iter().collect();
                let lower = word.to_lowercase();
                tokens.push(match lower.as_str() {
                    "and" => Token::And,
                    "or" => Token::Or,
                    "exists" | "hastrailingslash" => Token::Func(lower),
                    _ => Token::Str(word),
                });
            }
        }
    }
    Some(tokens)
}

struct Parser<'a> {
    tokens: Vec<Token>,
    pos: usize,
    base_dir: &'a Path,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        token
    }

    fn or(&mut self) -> Option<bool> {
        let mut left = self.and()?;
        while self.peek() == Some(&Token::Or) {
            self.pos += 1;
            let right = self.and()?;
            left = left || right;
        }
        Some(left)
    }

    fn and(&mut self) -> Option<bool> {
        let mut left = self.unary()?;
        while self.peek() == Some(&Token::And) {
            self.pos += 1;
            let right = self.unary()?;
            left = left && right;
        }
        Some(left)
    }

    fn unary(&mut self) -> Option<bool> {
        if self.peek() == Some(&Token::Not) {
            self.pos += 1;
            return Some(!self.unary()?);
        }
        self.primary()
    }

    fn primary(&mut self) -> Option<bool> {
        match self.next()? {
            Token::LParen => {
                let value = self.or()?;
                (self.next()? == Token::RParen).then_some(value)
            }
            Token::Func(name) => {
                if self.next()? != Token::LParen {
                    return None;
                }
                let Token::Str(arg) = self.next()? else { return None };
                if self.next()? != Token::RParen {
                    return None;
                }
                Some(match name.as_str() {
                    "exists" => !arg.trim().is_empty() && self.base_dir.join(crate::paths::from_msbuild(&arg)).exists(),
                    _ => arg.ends_with('/') || arg.ends_with('\\'),
                })
            }
            Token::Str(left) => {
                if let Some(Token::Op(op)) = self.peek().cloned() {
                    self.pos += 1;
                    let Token::Str(right) = self.next()? else { return None };
                    return Some(compare(&left, op, &right));
                }
                match left.to_lowercase().as_str() {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

fn compare(left: &str, op: &str, right: &str) -> bool {
    match op {
        "==" => left.eq_ignore_ascii_case(right),
        "!=" => !left.eq_ignore_ascii_case(right),
        _ => {
            let (Some(l), Some(r)) = (parse_number(left), parse_number(right)) else { return false };
            match op {
                "<" => l < r,
                ">" => l > r,
                "<=" => l <= r,
                _ => l >= r,
            }
        }
    }
}

/// Numbers and versions compare part by part.
fn parse_number(value: &str) -> Option<Vec<u64>> {
    let value = value.trim();
    let value = value.strip_prefix("0x").map(|hex| u64::from_str_radix(hex, 16).ok().map(|n| n.to_string())).unwrap_or(Some(value.to_string()))?;
    value.split('.').map(|part| part.parse().ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(pairs: &[(&str, &str)]) -> Properties {
        let mut p = Properties::default();
        for (k, v) in pairs {
            p.set(k, v);
        }
        p
    }

    #[test]
    fn evaluates_common_conditions() {
        let p = props(&[("Configuration", "Debug"), ("TargetFramework", "net8.0"), ("LangVersion", "12")]);
        let dir = Path::new("/");
        assert!(evaluate("'$(Configuration)' == 'debug'", &p, dir));
        assert!(!evaluate("'$(Configuration)|$(Platform)' == 'Release|AnyCPU'", &p, dir));
        assert!(evaluate("'$(Undefined)' == ''", &p, dir));
        assert!(evaluate("'$(TargetFramework)' == 'net8.0' And !('$(Configuration)' != 'Debug')", &p, dir));
        assert!(evaluate("'$(TargetFramework)' == 'net48' Or '$(Configuration)' == 'Debug'", &p, dir));
        assert!(evaluate("$(LangVersion) >= 10", &p, dir));
        assert!(!evaluate("Exists('does-not-exist.props')", &p, dir));
        assert!(evaluate("HasTrailingSlash('a/')", &p, dir));
        assert!(!evaluate("false", &p, dir));
        // Unknown syntax fails open.
        assert!(evaluate("$(TargetFramework.StartsWith('net4'))", &p, dir));
    }
}
