//! Documents as text, both ways: JSON plus the helpers of the MongoDB shell, so that types
//! survive an edit. `ObjectId("…")`, `ISODate("…")`, `NumberLong("…")`, `NumberInt(…)`,
//! `NumberDecimal("…")` and `UUID("…")` are read and written; anything else is written
//! (and read back) as canonical Extended JSON (`{"$timestamp": …}`, `{"$binary": …}`…).
//!
//! Numbers: a plain integer that fits in 32 bits is an Int32, a larger one an Int64, one
//! with a fraction or exponent a Double. So that an Int64 or a whole Double comes back as
//! the same type, they are written as `NumberLong("5")` and `5.0`.

use anyhow::{Context, Result, anyhow, bail};
use bson::spec::BinarySubtype;
use bson::{Bson, Document};
use std::fmt::Write as _;

/// `text` as BSON: JSON with the shell's helpers.
pub fn parse(text: &str) -> Result<Bson> {
    let json = to_extended_json(text)?;
    let value: serde_json::Value = serde_json::from_str(&json).map_err(|e| anyhow!("not valid JSON: {e}"))?;
    Bson::try_from(value).map_err(|e| anyhow!("{e}"))
}

/// `text` as a document (a filter, a sort, a projection, a document to save).
pub fn parse_document(text: &str, what: &str) -> Result<Document> {
    if text.trim().is_empty() {
        return Ok(Document::new());
    }
    match parse(text).with_context(|| format!("the {what}"))? {
        Bson::Document(doc) => Ok(doc),
        other => bail!("the {what} must be a JSON object ({{ … }}), not {}", kind(&other)),
    }
}

/// `text` as a list of documents (an aggregation pipeline).
pub fn parse_pipeline(text: &str) -> Result<Vec<Document>> {
    match parse(text).context("the pipeline")? {
        Bson::Array(stages) => stages
            .into_iter()
            .enumerate()
            .map(|(i, stage)| match stage {
                Bson::Document(doc) => Ok(doc),
                other => bail!("stage {} of the pipeline must be an object, not {}", i + 1, kind(&other)),
            })
            .collect(),
        Bson::Document(doc) => Ok(vec![doc]),
        other => bail!("the pipeline must be an array of stages ([{{ … }}, …]), not {}", kind(&other)),
    }
}

fn kind(value: &Bson) -> &'static str {
    match value {
        Bson::Array(_) => "an array",
        Bson::String(_) => "a string",
        Bson::Null => "null",
        Bson::Boolean(_) => "a boolean",
        _ => "a value",
    }
}

/// Rewrites the shell's helpers as Extended JSON, leaving strings alone.
fn to_extended_json(text: &str) -> Result<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '"' {
            let end = string_end(&chars, i)?;
            out.extend(&chars[i..end]);
            i = end;
            continue;
        }
        if c.is_ascii_alphabetic() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let mut word: String = chars[start..i].iter().collect();
            // `new Date(…)` / `new ISODate(…)`.
            if word == "new" {
                let mut j = i;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                let next_start = j;
                while j < chars.len() && chars[j].is_ascii_alphanumeric() {
                    j += 1;
                }
                let next: String = chars[next_start..j].iter().collect();
                if matches!(next.as_str(), "Date" | "ISODate") {
                    word = next;
                    i = j;
                }
            }
            if let Some(helper) = Helper::named(&word) {
                let (arg, end) = argument(&chars, i).with_context(|| format!("{word}(…)"))?;
                out.push_str(&helper.extended_json(&arg).with_context(|| format!("{word}({arg})"))?);
                i = end;
            } else {
                // true, false, null, or something JSON will reject with its own message.
                out.push_str(&word);
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    Ok(out)
}

/// Where the string starting at `start` (a `"`) ends, after its closing quote.
fn string_end(chars: &[char], start: usize) -> Result<usize> {
    let mut i = start + 1;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '"' => return Ok(i + 1),
            _ => i += 1,
        }
    }
    bail!("a string is not closed")
}

/// The argument of a helper call at `chars[at..]`: `( "text" )` or `( 123 )`; its text (a
/// string's content, unescaped) and where the call ends.
fn argument(chars: &[char], at: usize) -> Result<(String, usize)> {
    let skip = |mut i: usize| {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        i
    };
    let mut i = skip(at);
    if chars.get(i) != Some(&'(') {
        bail!("expected (");
    }
    i = skip(i + 1);
    let arg = if chars.get(i) == Some(&'"') {
        let end = string_end(chars, i)?;
        let literal: String = chars[i..end].iter().collect();
        i = end;
        serde_json::from_str::<String>(&literal)?
    } else {
        let start = i;
        while i < chars.len() && (chars[i].is_ascii_alphanumeric() || matches!(chars[i], '-' | '+' | '.')) {
            i += 1;
        }
        chars[start..i].iter().collect()
    };
    i = skip(i);
    if chars.get(i) != Some(&')') {
        bail!("expected )");
    }
    Ok((arg, i + 1))
}

enum Helper {
    ObjectId,
    Date,
    Long,
    Int,
    Decimal,
    Uuid,
}

impl Helper {
    fn named(word: &str) -> Option<Self> {
        Some(match word {
            "ObjectId" => Self::ObjectId,
            "ISODate" | "Date" => Self::Date,
            "NumberLong" => Self::Long,
            "NumberInt" => Self::Int,
            "NumberDecimal" => Self::Decimal,
            "UUID" => Self::Uuid,
            _ => return None,
        })
    }

    fn extended_json(&self, arg: &str) -> Result<String> {
        let quoted = |s: &str| serde_json::to_string(s).unwrap_or_default();
        Ok(match self {
            Self::ObjectId => {
                bson::oid::ObjectId::parse_str(arg).map_err(|_| anyhow!("an ObjectId is 24 hexadecimal digits"))?;
                format!("{{\"$oid\":{}}}", quoted(arg))
            }
            Self::Date => {
                // A date alone is midnight UTC.
                let date = if arg.len() == 10 { format!("{arg}T00:00:00Z") } else { arg.to_string() };
                bson::DateTime::parse_rfc3339_str(&date).map_err(|_| anyhow!("dates look like 2024-05-31 or 2024-05-31T14:30:00Z"))?;
                format!("{{\"$date\":{}}}", quoted(&date))
            }
            Self::Long => {
                arg.parse::<i64>().map_err(|_| anyhow!("not a whole number"))?;
                format!("{{\"$numberLong\":{}}}", quoted(arg))
            }
            Self::Int => {
                arg.parse::<i32>().map_err(|_| anyhow!("not a 32-bit whole number"))?;
                format!("{{\"$numberInt\":{}}}", quoted(arg))
            }
            Self::Decimal => format!("{{\"$numberDecimal\":{}}}", quoted(arg)),
            Self::Uuid => format!("{{\"$uuid\":{}}}", quoted(arg)),
        })
    }
}

/// `value` as text that [`parse`] reads back as the same value, indented by two spaces.
pub fn format(value: &Bson) -> String {
    let mut out = String::new();
    write(&mut out, value, 0, true);
    out
}

/// On one line, for summaries.
pub fn format_compact(value: &Bson) -> String {
    let mut out = String::new();
    write(&mut out, value, 0, false);
    out
}

fn write(out: &mut String, value: &Bson, depth: usize, pretty: bool) {
    let indent = |out: &mut String, depth: usize| {
        if pretty {
            out.push('\n');
            out.push_str(&"  ".repeat(depth));
        }
    };
    match value {
        Bson::Document(doc) if doc.is_empty() => out.push_str("{}"),
        Bson::Document(doc) => {
            out.push('{');
            for (n, (key, value)) in doc.iter().enumerate() {
                if n > 0 {
                    out.push(',');
                    if !pretty {
                        out.push(' ');
                    }
                }
                indent(out, depth + 1);
                out.push_str(&serde_json::to_string(key).unwrap_or_default());
                out.push_str(": ");
                write(out, value, depth + 1, pretty);
            }
            indent(out, depth);
            out.push('}');
        }
        Bson::Array(items) if items.is_empty() => out.push_str("[]"),
        Bson::Array(items) => {
            out.push('[');
            for (n, item) in items.iter().enumerate() {
                if n > 0 {
                    out.push(',');
                    if !pretty {
                        out.push(' ');
                    }
                }
                indent(out, depth + 1);
                write(out, item, depth + 1, pretty);
            }
            indent(out, depth);
            out.push(']');
        }
        Bson::String(s) => out.push_str(&serde_json::to_string(s).unwrap_or_default()),
        Bson::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
        Bson::Null => out.push_str("null"),
        Bson::Int32(n) => {
            let _ = write!(out, "{n}");
        }
        Bson::Int64(n) => {
            let _ = write!(out, "NumberLong(\"{n}\")");
        }
        Bson::Double(f) if f.is_finite() => {
            // Keep the fraction, so it reads back as a Double.
            let text = format!("{f:?}");
            out.push_str(&text);
        }
        Bson::ObjectId(id) => {
            let _ = write!(out, "ObjectId(\"{}\")", id.to_hex());
        }
        Bson::DateTime(date) => match date.try_to_rfc3339_string() {
            Ok(text) => {
                let _ = write!(out, "ISODate(\"{text}\")");
            }
            Err(_) => out.push_str(&canonical(value)),
        },
        Bson::Decimal128(d) => {
            let _ = write!(out, "NumberDecimal(\"{d}\")");
        }
        Bson::Binary(binary) if binary.subtype == BinarySubtype::Uuid && binary.bytes.len() == 16 => {
            let uuid = bson::Uuid::from_bytes(binary.bytes.clone().try_into().unwrap_or([0; 16]));
            let _ = write!(out, "UUID(\"{uuid}\")");
        }
        _ => out.push_str(&canonical(value)),
    }
}

/// Canonical Extended JSON, on one line.
fn canonical(value: &Bson) -> String {
    value.clone().into_canonical_extjson().to_string()
}

/// A short form of a top-level field for the documents table: strings, numbers and
/// booleans as themselves, ids and dates as text, objects and arrays by their size.
pub fn summary(value: &Bson) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Bson::String(s) => Value::String(s.chars().take(200).collect()),
        Bson::Int32(n) => Value::from(*n),
        Bson::Int64(n) if n.unsigned_abs() < (1 << 53) => Value::from(*n),
        Bson::Double(f) if f.is_finite() => Value::from(*f),
        Bson::Boolean(b) => Value::Bool(*b),
        Bson::Null => Value::Null,
        Bson::ObjectId(id) => Value::String(id.to_hex()),
        Bson::DateTime(date) => Value::String(date.try_to_rfc3339_string().unwrap_or_else(|_| date.to_string())),
        Bson::Document(doc) => Value::String(format!("{{ {} field{} }}", doc.len(), if doc.len() == 1 { "" } else { "s" })),
        Bson::Array(items) => Value::String(format!("[ {} item{} ]", items.len(), if items.len() == 1 { "" } else { "s" })),
        other => {
            let text = format_compact(other);
            Value::String(if text.chars().count() > 120 { format!("{}…", text.chars().take(120).collect::<String>()) } else { text })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    #[test]
    fn reads_json_with_the_shells_helpers() {
        let text = r#"{
            "_id": ObjectId("65f1c0ffee0000000000abcd"),
            "at": ISODate("2024-05-31T14:30:00Z"),
            "day": new Date("2024-05-31"),
            "big": NumberLong("9007199254740993"),
            "small": NumberInt(5),
            "price": NumberDecimal("19.99"),
            "uuid": UUID("123e4567-e89b-12d3-a456-426614174000"),
            "n": 5, "f": 5.0, "text": "ObjectId(\"not a call\")", "list": [1, true, null]
        }"#;
        let Bson::Document(doc) = parse(text).unwrap() else { panic!("a document") };
        assert_eq!(doc.get_object_id("_id").unwrap().to_hex(), "65f1c0ffee0000000000abcd");
        assert_eq!(doc.get_datetime("at").unwrap().try_to_rfc3339_string().unwrap(), "2024-05-31T14:30:00Z");
        assert_eq!(doc.get_datetime("day").unwrap().try_to_rfc3339_string().unwrap(), "2024-05-31T00:00:00Z");
        assert_eq!(doc.get_i64("big").unwrap(), 9007199254740993);
        assert_eq!(doc.get_i32("small").unwrap(), 5);
        assert_eq!(doc.get("price").unwrap().to_string(), "19.99");
        assert!(matches!(doc.get("uuid"), Some(Bson::Binary(b)) if b.subtype == BinarySubtype::Uuid));
        assert_eq!(doc.get("n"), Some(&Bson::Int32(5)));
        assert_eq!(doc.get("f"), Some(&Bson::Double(5.0)));
        assert_eq!(doc.get_str("text").unwrap(), "ObjectId(\"not a call\")", "helpers inside strings are text");
        // Field order is kept.
        assert_eq!(doc.keys().take(3).collect::<Vec<_>>(), ["_id", "at", "day"]);
    }

    #[test]
    fn round_trips_every_type() {
        let doc = doc! {
            "_id": bson::oid::ObjectId::parse_str("65f1c0ffee0000000000abcd").unwrap(),
            "name": "Ada \"the first\"",
            "n32": 7, "n64": 7_i64, "whole": 3.0, "pi": 3.14159,
            "when": bson::DateTime::from_millis(1_717_165_800_000),
            "price": "12.50".parse::<bson::Decimal128>().unwrap(),
            "uuid": bson::Binary { subtype: BinarySubtype::Uuid, bytes: vec![1; 16] },
            "ts": bson::Timestamp { time: 1, increment: 2 },
            "re": bson::Regex { pattern: "^a".into(), options: "i".into() },
            "nested": { "list": [1, "two", { "three": null }], "empty": {}, "none": [] },
        };
        let text = format(&Bson::Document(doc.clone()));
        assert!(text.contains("\"_id\": ObjectId(\"65f1c0ffee0000000000abcd\")"), "{text}");
        assert!(text.contains("\"n64\": NumberLong(\"7\")") && text.contains("\"whole\": 3.0") && text.contains("ISODate(\"2024-05-31T14:30:00Z\")"), "{text}");
        assert_eq!(parse(&text).unwrap(), Bson::Document(doc.clone()), "{text}");
        assert_eq!(parse(&format_compact(&Bson::Document(doc.clone()))).unwrap(), Bson::Document(doc));
    }

    #[test]
    fn says_what_is_wrong() {
        assert!(format!("{:#}", parse_document(r#"{"_id": ObjectId("xyz")}"#, "document").unwrap_err()).contains("24 hexadecimal"));
        assert!(format!("{:#}", parse_document("[1]", "filter").unwrap_err()).contains("must be a JSON object"));
        assert!(format!("{:#}", parse_document("{ name: 1 }", "filter").unwrap_err()).contains("not valid JSON"), "keys need quotes");
        assert_eq!(parse_document("  ", "filter").unwrap(), Document::new());
        assert_eq!(parse_pipeline(r#"[{"$match": {}}, {"$limit": 5}]"#).unwrap().len(), 2);
        assert!(format!("{:#}", parse_pipeline("[1]").unwrap_err()).contains("stage 1"));
    }

    #[test]
    fn summarizes_fields_for_the_table() {
        assert_eq!(summary(&Bson::Document(doc! { "a": 1, "b": 2 })), serde_json::json!("{ 2 fields }"));
        assert_eq!(summary(&Bson::Array(vec![Bson::Int32(1)])), serde_json::json!("[ 1 item ]"));
        assert_eq!(summary(&Bson::Int64(5)), serde_json::json!(5));
        assert_eq!(summary(&Bson::Int64(i64::MAX)), serde_json::json!("NumberLong(\"9223372036854775807\")"));
    }
}
