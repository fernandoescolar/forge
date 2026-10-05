//! `.http` files, as Visual Studio and VS Code's REST Client write them:
//!
//! ```http
//! @host = http://localhost:5000
//!
//! # @name login
//! POST {{host}}/login
//! Content-Type: application/json
//!
//! { "user": "ana" }
//!
//! ###
//! GET {{host}}/me
//! Authorization: Bearer {{login.response.body.$.token}}
//! ```
//!
//! Requests are separated by `###` lines. A request is its request line (`METHOD URL`,
//! or just a URL for GET), header lines up to a blank line, then the body. Lines starting
//! with `#` or `//` are comments; `# @name x` names the request so later ones can use its
//! response. `@name = value` lines define variables for the whole file.

use std::collections::HashMap;

const METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "TRACE", "CONNECT"];

#[derive(Clone, Debug, PartialEq)]
pub struct RequestSpec {
    /// From `# @name`.
    pub name: Option<String>,
    pub method: String,
    /// With `{{variables}}` still in it, as are headers and body.
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Body>,
    /// Zero-based row of the request line.
    pub row: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    Text(String),
    /// `< path`: the file's contents, relative to the `.http` file.
    File(String),
}

/// The file's `@variables`, in order (later ones may use earlier ones).
pub fn file_variables(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix('@')?;
            let (name, value) = rest.split_once('=')?;
            let name = name.trim();
            (!name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')).then(|| (name.to_string(), value.trim().to_string()))
        })
        .collect()
}

/// Every request in the file.
pub fn requests(text: &str) -> Vec<RequestSpec> {
    blocks(text).into_iter().filter_map(|(start, lines)| parse_block(start, &lines)).collect()
}

/// The request in the block that holds `row` (zero-based), if it has one.
pub fn request_at(text: &str, row: u32) -> Option<RequestSpec> {
    let blocks = blocks(text);
    let (start, lines) = blocks.iter().rev().find(|(start, _)| *start <= row)?;
    parse_block(*start, lines)
}

fn is_separator(line: &str) -> bool {
    line.trim_start().starts_with("###")
}

/// The file split at `###` lines: each block's first row and its lines.
fn blocks(text: &str) -> Vec<(u32, Vec<&str>)> {
    let mut blocks = vec![(0u32, Vec::new())];
    for (row, line) in text.lines().enumerate() {
        if is_separator(line) {
            blocks.push((row as u32 + 1, Vec::new()));
        } else {
            blocks.last_mut().unwrap().1.push(line);
        }
    }
    blocks
}

fn comment(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    trimmed.strip_prefix('#').or_else(|| trimmed.strip_prefix("//"))
}

fn parse_block(start: u32, lines: &[&str]) -> Option<RequestSpec> {
    let mut name = None;
    let mut index = 0;
    // Before the request line: blank lines, comments, metadata and variables.
    let (method, url, row) = loop {
        let line = lines.get(index)?;
        index += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('@') {
            continue;
        }
        if let Some(text) = comment(line) {
            if let Some(n) = text.trim().strip_prefix("@name") {
                name = Some(n.trim().trim_start_matches('=').trim().to_string()).filter(|n| !n.is_empty());
            }
            continue;
        }
        // `METHOD URL HTTP/1.1`; the URL may hold spaces inside `{{…}}`.
        let (first, rest) = trimmed.split_once(char::is_whitespace).unwrap_or((trimmed, ""));
        let (method, url) = if METHODS.contains(&first.to_ascii_uppercase().as_str()) {
            (first.to_ascii_uppercase(), rest.trim())
        } else {
            ("GET".to_string(), trimmed)
        };
        let url = match url.rsplit_once(char::is_whitespace) {
            Some((url, version)) if version.starts_with("HTTP/") => url.trim_end(),
            _ => url,
        };
        if url.is_empty() {
            return None;
        }
        let url = url.to_string();
        break (method, url, start + index as u32 - 1);
    };
    let mut url = url;
    // Query continuation lines (`?a=1`, `&b=2`), then headers up to a blank line.
    let mut headers = Vec::new();
    while let Some(line) = lines.get(index) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            index += 1;
            break;
        }
        index += 1;
        if comment(line).is_some() {
            continue;
        }
        if trimmed.starts_with('?') || trimmed.starts_with('&') {
            url.push_str(trimmed);
        } else if let Some((key, value)) = trimmed.split_once(':') {
            headers.push((key.trim().to_string(), value.trim().to_string()));
        }
    }
    let body_text = lines[index.min(lines.len())..].join("\n").trim_end().to_string();
    let body = if body_text.trim().is_empty() {
        None
    } else if let Some(path) = body_text.trim().strip_prefix("< ").filter(|p| !p.contains('\n')) {
        Some(Body::File(path.trim().to_string()))
    } else {
        Some(Body::Text(body_text))
    };
    Some(RequestSpec { name, method, url, headers, body, row })
}

/// What `{{…}}` can refer to besides the file's variables.
pub trait Resolver {
    /// `{{$guid}}`, `{{$timestamp}}`, `{{$processEnv NAME}}`, … (without the `$`).
    fn dynamic(&self, name: &str, args: &[&str]) -> Option<String>;
    /// `{{login.response.body.$.token}}`, `{{login.response.headers.Location}}`.
    fn response(&self, request: &str, path: &str) -> Option<String>;
}

/// Replaces every `{{…}}` in `text`. Variables are the environment's, then the file's
/// (which win, and may use each other). Returns the names it could not resolve.
pub fn substitute(text: &str, variables: &HashMap<String, String>, resolver: &dyn Resolver) -> Result<String, Vec<String>> {
    let mut missing = Vec::new();
    let out = substitute_depth(text, variables, resolver, &mut missing, 0);
    if missing.is_empty() { Ok(out) } else { Err(missing) }
}

fn substitute_depth(text: &str, variables: &HashMap<String, String>, resolver: &dyn Resolver, missing: &mut Vec<String>, depth: usize) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            out.push_str(&rest[open..]);
            return out;
        };
        let expr = after[..close].trim();
        let value = if let Some(dynamic) = expr.strip_prefix('$') {
            let mut parts = dynamic.split_whitespace();
            let name = parts.next().unwrap_or_default();
            resolver.dynamic(name, &parts.collect::<Vec<_>>())
        } else if let Some((request, path)) = expr.split_once(".response.") {
            resolver.response(request, path)
        } else {
            variables.get(expr).map(|v| if depth < 8 { substitute_depth(v, variables, resolver, missing, depth + 1) } else { v.clone() })
        };
        match value {
            Some(value) => out.push_str(&value),
            None => {
                if !missing.iter().any(|m| m == expr) {
                    missing.push(expr.to_string());
                }
            }
        }
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    out
}

/// Follows `$.a.b[0]` (JSONPath, the simple part) into `json`; strings come out unquoted.
pub fn json_path(json: &serde_json::Value, path: &str) -> Option<String> {
    let path = path.trim().strip_prefix('$')?;
    let mut value = json;
    for segment in path.split('.').filter(|s| !s.is_empty()) {
        let (key, indexes) = match segment.find('[') {
            Some(i) => (&segment[..i], &segment[i..]),
            None => (segment, ""),
        };
        if !key.is_empty() {
            value = value.get(key)?;
        }
        for index in indexes.split('[').filter(|s| !s.is_empty()) {
            value = value.get(index.trim_end_matches(']').parse::<usize>().ok()?)?;
        }
    }
    Some(match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "@host = http://localhost:5000
@api = {{host}}/api

# @name login
POST {{host}}/login
Content-Type: application/json

{
  \"user\": \"ana\"
}

###

// The profile
GET {{api}}/me
    ?expand=all
    &limit=5
Authorization: Bearer {{login.response.body.$.token}}

###
https://example.com HTTP/1.1
###
# nothing here
";

    #[test]
    fn splits_requests() {
        let all = requests(FILE);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].name.as_deref(), Some("login"));
        assert_eq!((all[0].method.as_str(), all[0].url.as_str(), all[0].row), ("POST", "{{host}}/login", 4));
        assert_eq!(all[0].headers, [("Content-Type".to_string(), "application/json".to_string())]);
        assert_eq!(all[0].body, Some(Body::Text("{\n  \"user\": \"ana\"\n}".into())));
        assert_eq!(all[1].url, "{{api}}/me?expand=all&limit=5");
        assert_eq!(all[1].body, None);
        assert_eq!((all[2].method.as_str(), all[2].url.as_str()), ("GET", "https://example.com"));
    }

    #[test]
    fn finds_the_request_at_the_cursor() {
        assert_eq!(request_at(FILE, 0).map(|r| r.row), Some(4), "variables before the first request belong to its block");
        assert_eq!(request_at(FILE, 8).map(|r| r.row), Some(4));
        assert_eq!(request_at(FILE, 12).map(|r| r.method), Some("GET".into()));
        assert_eq!(request_at(FILE, 23), None, "a block with no request");
        assert_eq!(file_variables(FILE), [("host".to_string(), "http://localhost:5000".to_string()), ("api".to_string(), "{{host}}/api".to_string())]);
    }

    #[test]
    fn reads_a_body_from_a_file() {
        let request = request_at("POST http://x\n\n< ./payload.json\n", 0).unwrap();
        assert_eq!(request.body, Some(Body::File("./payload.json".into())));
    }

    struct Fake;
    impl Resolver for Fake {
        fn dynamic(&self, name: &str, args: &[&str]) -> Option<String> {
            (name == "processEnv").then(|| format!("env:{}", args.join(",")))
        }
        fn response(&self, request: &str, path: &str) -> Option<String> {
            (request == "login").then(|| format!("{request}/{path}"))
        }
    }

    #[test]
    fn substitutes_variables() {
        let vars: HashMap<String, String> = file_variables(FILE).into_iter().collect();
        assert_eq!(substitute("{{api}}/x", &vars, &Fake).unwrap(), "http://localhost:5000/api/x");
        assert_eq!(substitute("{{ $processEnv HOME }}", &vars, &Fake).unwrap(), "env:HOME");
        assert_eq!(substitute("{{login.response.body.$.token}}", &vars, &Fake).unwrap(), "login/body.$.token");
        assert_eq!(substitute("{{nope}} {{other.response.body}} {{nope}}", &vars, &Fake), Err(vec!["nope".to_string(), "other.response.body".to_string()]));
    }

    #[test]
    fn follows_json_paths() {
        let json = serde_json::json!({ "token": "t1", "items": [{ "id": 7 }] });
        assert_eq!(json_path(&json, "$.token").as_deref(), Some("t1"));
        assert_eq!(json_path(&json, "$.items[0].id").as_deref(), Some("7"));
        assert_eq!(json_path(&json, "$.missing"), None);
    }
}
