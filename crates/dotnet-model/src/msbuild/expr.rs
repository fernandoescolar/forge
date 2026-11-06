//! MSBuild `$(…)` expressions: plain properties and the property functions seen in real
//! projects, such as `$(MSBuildProjectName.Replace('.Tests', ''))` or
//! `$([System.String]::Copy('$(Name)').ToLower())`. An expression it cannot read stays in
//! the text, with the expressions inside it expanded, so callers can tell it apart.

use super::Properties;

pub(super) fn expand(properties: &Properties, text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("$(") {
        out.push_str(&rest[..start]);
        let body_start = start + 2;
        let Some(end) = closing_paren(rest, body_start) else {
            out.push_str("$(");
            rest = &rest[body_start..];
            continue;
        };
        let body = &rest[body_start..end];
        match evaluate(properties, body) {
            Some(value) => out.push_str(&value),
            None => {
                out.push_str("$(");
                out.push_str(&expand(properties, body));
                out.push(')');
            }
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// The `)` closing a parenthesis opened just before `from`. Quotes hide parentheses, except
/// that a `$(…)` inside a quoted argument is skipped whole, as it may hold quotes of its own.
fn closing_paren(text: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0;
    let mut quote = None;
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == b'$' && bytes.get(i + 1) == Some(&b'(') {
            i = closing_paren(text, i + 2)? + 1;
            continue;
        }
        match (quote, bytes[i]) {
            (Some(q), b) if b == q => quote = None,
            (Some(_), _) => {}
            (None, b @ (b'\'' | b'"' | b'`')) => quote = Some(b),
            (None, b'(') => depth += 1,
            (None, b')') if depth == 0 => return Some(i),
            (None, b')') => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    None
}

/// Splits an argument list at its top-level commas.
fn split_args(text: &str) -> Vec<&str> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let bytes = text.as_bytes();
    let mut args = Vec::new();
    let mut quote = None;
    let mut depth = 0;
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && bytes.get(i + 1) == Some(&b'(') {
            match closing_paren(text, i + 2) {
                Some(end) => {
                    i = end + 1;
                    continue;
                }
                None => break,
            }
        }
        match (quote, bytes[i]) {
            (Some(q), b) if b == q => quote = None,
            (Some(_), _) => {}
            (None, b @ (b'\'' | b'"' | b'`')) => quote = Some(b),
            (None, b'(') => depth += 1,
            (None, b')') => depth -= 1,
            (None, b',') if depth == 0 => {
                args.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    args.push(&text[start..]);
    args
}

fn argument(properties: &Properties, arg: &str) -> String {
    let arg = arg.trim();
    let unquoted = ['\'', '"', '`'].iter().find_map(|&q| arg.strip_prefix(q)?.strip_suffix(q));
    expand(properties, unquoted.unwrap_or(arg))
}

/// Takes an optional `(…)` from the start of `text`, returning its arguments and what follows.
fn call_args<'a>(properties: &Properties, text: &'a str) -> Option<(Vec<String>, &'a str)> {
    let Some(open) = text.trim_start().strip_prefix('(') else {
        return Some((Vec::new(), text));
    };
    let end = closing_paren(open, 0)?;
    let args = split_args(&open[..end]).into_iter().map(|arg| argument(properties, arg)).collect();
    Some((args, &open[end + 1..]))
}

fn evaluate(properties: &Properties, body: &str) -> Option<String> {
    let body = body.trim();
    let (mut value, mut rest) = if body.starts_with('[') {
        static_call(properties, body)?
    } else {
        let end = body.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')).unwrap_or(body.len());
        let name = &body[..end];
        if !name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
            return None;
        }
        // Calling a function on an undefined property works on the empty string.
        (properties.get(name).unwrap_or("").to_string(), &body[end..])
    };
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return Some(value);
        }
        let member = rest.strip_prefix('.')?;
        let end = member.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(member.len());
        let (args, after) = call_args(properties, &member[end..])?;
        value = call(&value, &member[..end], &args)?;
        rest = after;
    }
}

fn static_call<'a>(properties: &Properties, body: &'a str) -> Option<(String, &'a str)> {
    let close = body.find(']')?;
    let member = body[close + 1..].strip_prefix("::")?;
    let end = member.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(member.len());
    let (args, rest) = call_args(properties, &member[end..])?;
    if !body[1..close].trim().eq_ignore_ascii_case("System.String") {
        return None;
    }
    let value = match (member[..end].to_ascii_lowercase().as_str(), args.as_slice()) {
        ("copy", [text]) => text.clone(),
        ("isnullorempty", [text]) => dotnet_bool(text.is_empty()),
        ("isnullorwhitespace", [text]) => dotnet_bool(text.trim().is_empty()),
        _ => return None,
    };
    Some((value, rest))
}

fn dotnet_bool(value: bool) -> String {
    if value { "True" } else { "False" }.to_string()
}

fn char_index(text: &str, byte_index: Option<usize>) -> String {
    byte_index.map_or(-1, |i| text[..i].chars().count() as i64).to_string()
}

/// `System.String` members, which MSBuild binds ignoring case.
fn call(value: &str, method: &str, args: &[String]) -> Option<String> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let trim_set = |set: &str, c: char| if set.is_empty() { c.is_whitespace() } else { set.contains(c) };
    Some(match (method.to_ascii_lowercase().as_str(), args.as_slice()) {
        ("replace", [from, to]) if !from.is_empty() => value.replace(from, to),
        ("tolower" | "tolowerinvariant", []) => value.to_lowercase(),
        ("toupper" | "toupperinvariant", []) => value.to_uppercase(),
        ("tostring", []) => value.to_string(),
        ("trim", []) => value.trim().to_string(),
        ("trimstart", []) => value.trim_start().to_string(),
        ("trimend", []) => value.trim_end().to_string(),
        ("trim", [set]) => value.trim_matches(|c| trim_set(set, c)).to_string(),
        ("trimstart", [set]) => value.trim_start_matches(|c| trim_set(set, c)).to_string(),
        ("trimend", [set]) => value.trim_end_matches(|c| trim_set(set, c)).to_string(),
        ("substring", [start]) => {
            let start: usize = start.parse().ok()?;
            let length = value.chars().count();
            if start > length {
                return None;
            }
            value.chars().skip(start).collect()
        }
        ("substring", [start, count]) => {
            let (start, count): (usize, usize) = (start.parse().ok()?, count.parse().ok()?);
            if start.checked_add(count)? > value.chars().count() {
                return None;
            }
            value.chars().skip(start).take(count).collect()
        }
        ("length", []) => value.chars().count().to_string(),
        ("indexof", [text]) => char_index(value, value.find(text)),
        ("lastindexof", [text]) => char_index(value, value.rfind(text)),
        ("contains", [text]) => dotnet_bool(value.contains(text)),
        ("startswith", [text]) => dotnet_bool(value.starts_with(text)),
        ("endswith", [text]) => dotnet_bool(value.ends_with(text)),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props() -> Properties {
        let mut props = Properties::default();
        props.set("Name", "Shop.Orders.Tests");
        props.set("Suffix", ".Tests");
        props
    }

    #[test]
    fn plain_properties() {
        let p = props();
        assert_eq!(p.expand("a $(Name) b"), "a Shop.Orders.Tests b");
        assert_eq!(p.expand("$(name)"), "Shop.Orders.Tests");
        assert_eq!(p.expand("[$(Undefined)]"), "[]");
        assert_eq!(p.expand("no properties"), "no properties");
    }

    #[test]
    fn chained_functions() {
        let p = props();
        assert_eq!(p.expand("Acme.$(Name.Replace('.Tests', '').Replace('Shop.', ''))"), "Acme.Orders");
        assert_eq!(p.expand("$(Name.ToLower())"), "shop.orders.tests");
        assert_eq!(p.expand("$(Name.substring(5, 6))"), "Orders");
        assert_eq!(p.expand("$(Name.Substring(5).TrimEnd('.Tes'))"), "Orders.Test");
        assert_eq!(p.expand("$(Name.Length)"), "17");
        assert_eq!(p.expand("$(Name.IndexOf('.'))|$(Name.LastIndexOf('.'))|$(Name.IndexOf('x'))"), "4|11|-1");
        assert_eq!(p.expand("$(Name.EndsWith('.Tests'))"), "True");
        assert_eq!(p.expand("$(Undefined.Replace('a', 'b'))"), "");
    }

    #[test]
    fn arguments_may_hold_expressions_and_delimiters() {
        let p = props();
        assert_eq!(p.expand("$(Name.Replace($(Suffix), ''))"), "Shop.Orders");
        assert_eq!(p.expand("$(Name.Replace('$(Suffix)', ''))"), "Shop.Orders");
        assert_eq!(p.expand("$(Name.Replace('$(Suffix.Replace('.', ''))', 'x'))"), "Shop.Orders.x");
        assert_eq!(p.expand("$(Name.Replace('.', ',)'))"), "Shop,)Orders,)Tests");
    }

    #[test]
    fn string_static_functions() {
        let p = props();
        assert_eq!(p.expand("$([System.String]::Copy('$(Name)').Replace('$(Suffix)', ''))"), "Shop.Orders");
        assert_eq!(p.expand("$([System.String]::IsNullOrEmpty('$(Undefined)'))"), "True");
    }

    #[test]
    fn what_it_cannot_read_stays_visible() {
        let p = props();
        assert_eq!(p.expand("$(Name.Split('.'))"), "$(Name.Split('.'))");
        assert_eq!(p.expand("$(Name.Replace('', 'x'))"), "$(Name.Replace('', 'x'))");
        assert_eq!(p.expand("$(Name.Substring(99))"), "$(Name.Substring(99))");
        assert_eq!(p.expand("$([MSBuild]::NormalizePath('$(Suffix)', 'a'))"), "$([MSBuild]::NormalizePath('.Tests', 'a'))");
        assert_eq!(p.expand("$(Name $(Suffix)"), "$(Name .Tests");
    }
}
