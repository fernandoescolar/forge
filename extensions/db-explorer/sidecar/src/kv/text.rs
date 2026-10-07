//! Redis values as text, both ways, the way `redis-cli` shows them: command lines split into
//! arguments (with quotes and escapes), replies formatted (`1) "a"`, `(integer) 5`, `(nil)`),
//! and binary strings escaped (`\xNN`) so an edit writes the same bytes back.

use anyhow::{Result, bail};
use redis::Value;
use std::fmt::Write as _;

/// `bytes` as text: as they are when they are UTF-8 (`false`), else with `\xNN` for bytes that
/// aren't and `\\` for backslashes (`true`); [`from_display`] reverses the second form.
pub fn display(bytes: &[u8]) -> (String, bool) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_string(), false),
        Err(_) => (escape(bytes), true),
    }
}

/// Every byte that isn't printable ASCII as `\xNN`, and `\` as `\\`.
pub fn escape(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            0x20..=0x7e => out.push(b as char),
            _ => {
                let _ = write!(out, "\\x{b:02x}");
            }
        }
    }
    out
}

/// The bytes of `text`, read as escaped (see [`display`]) when `escaped`, else as UTF-8.
pub fn from_display(text: &str, escaped: bool) -> Result<Vec<u8>> {
    if !escaped {
        return Ok(text.as_bytes().to_vec());
    }
    let mut out = Vec::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            match bytes.get(i + 1) {
                Some(b'\\') => {
                    out.push(b'\\');
                    i += 2;
                }
                Some(b'x') => {
                    let hex = text.get(i + 2..i + 4).filter(|h| h.len() == 2 && h.chars().all(|c| c.is_ascii_hexdigit()));
                    let Some(hex) = hex else { bail!("\\x needs two hexadecimal digits") };
                    out.push(u8::from_str_radix(hex, 16)?);
                    i += 4;
                }
                _ => bail!("a backslash starts \\\\ or \\xNN"),
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(out)
}

/// A command line split into its arguments, as `redis-cli` reads it: separated by spaces,
/// `"double"` quotes with escapes (`\n`, `\t`, `\"`, `\\`, `\xNN`), `'single'` quotes as written.
pub fn split(line: &str) -> Result<Vec<Vec<u8>>> {
    let mut args = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let Some(&first) = chars.peek() else { break };
        let mut arg: Vec<u8> = Vec::new();
        match first {
            '"' => {
                chars.next();
                loop {
                    match chars.next() {
                        None => bail!("a \" is not closed"),
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => arg.push(b'\n'),
                            Some('r') => arg.push(b'\r'),
                            Some('t') => arg.push(b'\t'),
                            Some('a') => arg.push(7),
                            Some('b') => arg.push(8),
                            Some('x') => {
                                let hex: String = (0..2).filter_map(|_| chars.next()).collect();
                                let Ok(byte) = u8::from_str_radix(&hex, 16) else { bail!("\\x needs two hexadecimal digits") };
                                arg.push(byte);
                            }
                            Some(c) => arg.extend(c.to_string().as_bytes()),
                            None => bail!("a \" is not closed"),
                        },
                        Some(c) => arg.extend(c.to_string().as_bytes()),
                    }
                }
            }
            '\'' => {
                chars.next();
                loop {
                    match chars.next() {
                        None => bail!("a ' is not closed"),
                        Some('\'') => break,
                        Some('\\') if chars.peek() == Some(&'\'') => {
                            chars.next();
                            arg.push(b'\'');
                        }
                        Some(c) => arg.extend(c.to_string().as_bytes()),
                    }
                }
            }
            _ => {
                while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                    arg.extend(c.to_string().as_bytes());
                }
            }
        }
        // A closing quote must end the argument.
        if matches!(first, '"' | '\'') && chars.peek().is_some_and(|c| !c.is_whitespace()) {
            bail!("a closing quote must be followed by a space");
        }
        args.push(arg);
    }
    Ok(args)
}

/// A reply as `redis-cli` prints it.
pub fn format_reply(value: &Value) -> String {
    let mut out = String::new();
    write_reply(&mut out, value, 0);
    out
}

fn quoted(bytes: &[u8]) -> String {
    let mut out = String::from("\"");
    match std::str::from_utf8(bytes) {
        Ok(text) => {
            for c in text.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if (c as u32) < 0x20 => {
                        let _ = write!(out, "\\x{:02x}", c as u32);
                    }
                    c => out.push(c),
                }
            }
        }
        Err(_) => out.push_str(&escape(bytes).replace('"', "\\\"")),
    }
    out.push('"');
    out
}

fn write_reply(out: &mut String, value: &Value, indent: usize) {
    let items = |out: &mut String, items: &[Value], indent: usize| {
        if items.is_empty() {
            out.push_str("(empty array)");
            return;
        }
        let width = items.len().to_string().len();
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                out.push('\n');
                out.push_str(&" ".repeat(indent));
            }
            let label = format!("{:>width$}) ", i + 1);
            out.push_str(&label);
            write_reply(out, item, indent + label.len());
        }
    };
    match value {
        Value::Nil => out.push_str("(nil)"),
        Value::Int(n) => {
            let _ = write!(out, "(integer) {n}");
        }
        Value::BulkString(bytes) => out.push_str(&quoted(bytes)),
        Value::SimpleString(s) => out.push_str(s),
        Value::Okay => out.push_str("OK"),
        Value::Double(f) => {
            let _ = write!(out, "(double) {f}");
        }
        Value::Boolean(b) => {
            let _ = write!(out, "({b})");
        }
        Value::VerbatimString { text, .. } => out.push_str(text),
        Value::BigNumber(n) => {
            let _ = write!(out, "(big number) {n:?}");
        }
        Value::Array(v) | Value::Set(v) => items(out, v, indent),
        Value::Map(pairs) => {
            let flat: Vec<Value> = pairs.iter().flat_map(|(k, v)| [k.clone(), v.clone()]).collect();
            items(out, &flat, indent);
        }
        Value::Attribute { data, .. } => write_reply(out, data, indent),
        Value::Push { data, .. } => items(out, data, indent),
        Value::ServerError(e) => {
            let _ = write!(out, "(error) {}", format!("{} {}", e.code(), e.details().unwrap_or_default()).trim());
        }
        _ => {
            let _ = write!(out, "{value:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_command_lines_like_redis_cli() {
        let args = |line: &str| split(line).unwrap().into_iter().map(|a| String::from_utf8_lossy(&a).into_owned()).collect::<Vec<_>>();
        assert_eq!(args("  SET  user:1   Ada "), ["SET", "user:1", "Ada"]);
        assert_eq!(args(r#"SET greeting "hello world\n" 'it\'s'"#), ["SET", "greeting", "hello world\n", "it's"]);
        assert_eq!(args(r#"SET k "a \"q\" \x41""#), ["SET", "k", "a \"q\" A"]);
        assert_eq!(args(r"SET k 'raw \n'"), ["SET", "k", r"raw \n"]);
        assert_eq!(split(r#"SET k "\xff""#).unwrap()[2], vec![0xff]);
        assert!(split(r#"SET k "open"#).is_err());
        assert!(split(r#"SET k "a"b"#).is_err());
        assert!(split("").unwrap().is_empty());
    }

    #[test]
    fn binary_strings_round_trip() {
        assert_eq!(display(b"caf\xc3\xa9"), ("café".to_string(), false));
        let bytes = b"\x00\xffa\\b";
        let (text, escaped) = display(bytes);
        assert_eq!((text.as_str(), escaped), (r"\x00\xffa\\b", true));
        assert_eq!(from_display(&text, true).unwrap(), bytes);
        assert_eq!(from_display(r"a\\b", false).unwrap(), br"a\\b", "plain text is taken as written");
        assert!(from_display(r"\xz1", true).is_err());
    }

    #[test]
    fn formats_replies_like_redis_cli() {
        assert_eq!(format_reply(&Value::Okay), "OK");
        assert_eq!(format_reply(&Value::Nil), "(nil)");
        assert_eq!(format_reply(&Value::Int(3)), "(integer) 3");
        assert_eq!(format_reply(&Value::BulkString(b"a \"b\"\n".to_vec())), r#""a \"b\"\n""#);
        assert_eq!(format_reply(&Value::Array(vec![])), "(empty array)");
        let nested = Value::Array(vec![Value::BulkString(b"one".to_vec()), Value::Array(vec![Value::Int(1), Value::Nil])]);
        assert_eq!(format_reply(&nested), "1) \"one\"\n2) 1) (integer) 1\n   2) (nil)");
        let many = Value::Array((0..10).map(|i| Value::Int(i)).collect());
        assert!(format_reply(&many).starts_with(" 1) (integer) 0\n 2)"), "numbers aligned");
    }
}
