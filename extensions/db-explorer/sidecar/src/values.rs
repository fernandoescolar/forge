//! Cell encoding: converts driver values into JSON cells.
//!
//! Rules: NULL -> null, booleans -> bool, integers -> number when |v| <= 2^53 (else string),
//! floats -> number (NaN/inf -> string), decimals -> string, temporal -> ISO-8601 string,
//! binary -> "0x" + hex (first 256 bytes, then "…"), anything else -> string or
//! "<unsupported: type>". Decoding never panics.

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, SecondsFormat};
use serde_json::{Number, Value};
use sqlx::mysql::{MySql, MySqlRow, MySqlValueRef};
use sqlx::postgres::{PgRow, PgValueRef, Postgres};
use sqlx::sqlite::{Sqlite, SqliteRow, SqliteValueRef};
use sqlx::{Column, Decode, Row, TypeInfo, ValueRef};
use std::fmt::Write;

const MAX_SAFE_INT: u64 = 1 << 53;
const MAX_BINARY: usize = 256;

pub fn int(v: i64) -> Value {
    if v.unsigned_abs() <= MAX_SAFE_INT { Value::from(v) } else { Value::String(v.to_string()) }
}

pub fn uint(v: u64) -> Value {
    if v <= MAX_SAFE_INT { Value::from(v) } else { Value::String(v.to_string()) }
}

pub fn float(v: f64) -> Value {
    match Number::from_f64(v) {
        Some(n) => Value::Number(n),
        None if v.is_nan() => Value::String("NaN".into()),
        None if v > 0.0 => Value::String("Infinity".into()),
        None => Value::String("-Infinity".into()),
    }
}

pub fn binary(b: &[u8]) -> Value {
    let mut s = String::with_capacity(2 + 2 * b.len().min(MAX_BINARY) + 3);
    s.push_str("0x");
    for byte in &b[..b.len().min(MAX_BINARY)] {
        let _ = write!(s, "{byte:02x}");
    }
    if b.len() > MAX_BINARY {
        s.push('…');
    }
    Value::String(s)
}

pub fn unsupported(type_name: &str) -> Value {
    Value::String(format!("<unsupported: {type_name}>"))
}

pub fn datetime(v: NaiveDateTime) -> Value {
    Value::String(v.format("%Y-%m-%dT%H:%M:%S%.f").to_string())
}

pub fn datetime_tz(v: DateTime<FixedOffset>) -> Value {
    Value::String(v.to_rfc3339_opts(SecondsFormat::AutoSi, true))
}

pub fn date(v: NaiveDate) -> Value {
    Value::String(v.format("%Y-%m-%d").to_string())
}

pub fn time(v: NaiveTime) -> Value {
    Value::String(v.format("%H:%M:%S%.f").to_string())
}

/// Parse an integer rendered as text (MySQL text protocol), keeping big values exact.
fn int_text(s: &str) -> Value {
    if let Ok(v) = s.parse::<i64>() {
        int(v)
    } else if let Ok(v) = s.parse::<u64>() {
        uint(v)
    } else {
        Value::String(s.to_string())
    }
}

fn float_text(s: &str) -> Value {
    s.parse::<f64>().map(float).unwrap_or_else(|_| Value::String(s.to_string()))
}

// ---------------------------------------------------------------- PostgreSQL

pub fn pg_row(row: &PgRow) -> Vec<Value> {
    (0..row.len())
        .map(|i| match row.try_get_raw(i) {
            Ok(v) => pg_value(v),
            Err(_) => unsupported(row.column(i).type_info().name()),
        })
        .collect()
}

fn pg_value(v: PgValueRef<'_>) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    let ty = v.type_info().name().to_string();
    macro_rules! dec {
        ($t:ty) => {
            <$t as Decode<Postgres>>::decode(v.clone())
        };
    }
    let typed = match ty.as_str() {
        "BOOL" => dec!(bool).map(Value::Bool),
        "INT2" => dec!(i16).map(|x| int(x.into())),
        "INT4" => dec!(i32).map(|x| int(x.into())),
        "INT8" => dec!(i64).map(int),
        "FLOAT4" => dec!(f32).map(|x| float(x.into())),
        "FLOAT8" => dec!(f64).map(float),
        "TIMESTAMP" => dec!(NaiveDateTime).map(datetime),
        "TIMESTAMPTZ" => dec!(DateTime<FixedOffset>).map(datetime_tz),
        "DATE" => dec!(NaiveDate).map(date),
        "TIME" => dec!(NaiveTime).map(time),
        "UUID" => dec!(uuid::Uuid).map(|u| Value::String(u.to_string())),
        "BYTEA" => dec!(Vec<u8>).map(|b| binary(&b)),
        // NUMERIC, MONEY, JSON, JSONB, INTERVAL, arrays, enums, ... use the text form.
        _ => Err("text".into()),
    };
    typed
        .or_else(|_| dec!(&str).map(|s| Value::String(s.to_string())))
        .unwrap_or_else(|_| unsupported(&ty))
}

// ---------------------------------------------------------------- MySQL / MariaDB

pub fn mysql_row(row: &MySqlRow) -> Vec<Value> {
    (0..row.len())
        .map(|i| match row.try_get_raw(i) {
            Ok(v) => mysql_value(v),
            Err(_) => unsupported(row.column(i).type_info().name()),
        })
        .collect()
}

/// Values arrive in the text protocol (raw_sql), so we classify by column type.
fn mysql_value(v: MySqlValueRef<'_>) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    let ty = v.type_info().name().to_string();
    let Ok(bytes) = <&[u8] as Decode<MySql>>::decode(v) else {
        return unsupported(&ty);
    };
    let is_binary = ty.contains("BLOB") || ty.contains("BINARY") || ty == "GEOMETRY";
    if ty == "BIT" {
        if bytes.len() <= 8 {
            return uint(bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)));
        }
        return binary(bytes);
    }
    let s = match std::str::from_utf8(bytes) {
        Ok(s) if !is_binary => s,
        _ => return binary(bytes),
    };
    match ty.as_str() {
        "BOOLEAN" => match s {
            "0" => Value::Bool(false),
            "1" => Value::Bool(true),
            _ => int_text(s),
        },
        "FLOAT" | "DOUBLE" => float_text(s),
        "DATETIME" | "TIMESTAMP" => Value::String(s.replacen(' ', "T", 1)),
        t if t.contains("INT") || t == "YEAR" => int_text(s),
        _ => Value::String(s.to_string()),
    }
}

// ---------------------------------------------------------------- SQLite

pub fn sqlite_row(row: &SqliteRow) -> Vec<Value> {
    (0..row.len())
        .map(|i| {
            let declared = row.column(i).type_info().name().to_string();
            match row.try_get_raw(i) {
                Ok(v) => sqlite_value(v, &declared),
                Err(_) => unsupported(&declared),
            }
        })
        .collect()
}

/// SQLite is dynamically typed: decode by the value's storage class.
fn sqlite_value(v: SqliteValueRef<'_>, declared: &str) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    let storage = v.type_info().name().to_string();
    let r = match storage.as_str() {
        "INTEGER" => <i64 as Decode<Sqlite>>::decode(v).map(|x| {
            if declared == "BOOLEAN" && (x == 0 || x == 1) { Value::Bool(x == 1) } else { int(x) }
        }),
        "REAL" => <f64 as Decode<Sqlite>>::decode(v).map(float),
        "BLOB" => <&[u8] as Decode<Sqlite>>::decode(v).map(binary),
        _ => <&[u8] as Decode<Sqlite>>::decode(v).map(|b| match std::str::from_utf8(b) {
            Ok(s) => Value::String(s.to_string()),
            Err(_) => binary(b),
        }),
    };
    r.unwrap_or_else(|_| unsupported(&storage))
}

// ---------------------------------------------------------------- SQL Server

pub fn mssql_value(cd: &tiberius::ColumnData<'_>) -> Value {
    use tiberius::{ColumnData as C, FromSql};
    fn conv<'a, T: FromSql<'a>>(cd: &'a C<'static>, f: impl Fn(T) -> Value) -> Value {
        match T::from_sql(cd) {
            Ok(Some(v)) => f(v),
            Ok(None) => Value::Null,
            Err(_) => unsupported("datetime"),
        }
    }
    // FromSql is implemented for `&'a ColumnData<'static>`; temporal variants own their data,
    // so re-wrap them as 'static values before converting.
    match cd {
        C::U8(Some(v)) => int((*v).into()),
        C::I16(Some(v)) => int((*v).into()),
        C::I32(Some(v)) => int((*v).into()),
        C::I64(Some(v)) => int(*v),
        C::F32(Some(v)) => float((*v).into()),
        C::F64(Some(v)) => float(*v),
        C::Bit(Some(v)) => Value::Bool(*v),
        C::String(Some(s)) => Value::String(s.to_string()),
        C::Guid(Some(u)) => Value::String(u.to_string()),
        C::Binary(Some(b)) => binary(b),
        C::Numeric(Some(n)) => Value::String(n.to_string()),
        C::Xml(Some(x)) => Value::String(x.to_string()),
        C::DateTime(Some(v)) => conv(&C::DateTime(Some(*v)), datetime),
        C::SmallDateTime(Some(v)) => conv(&C::SmallDateTime(Some(*v)), datetime),
        C::DateTime2(Some(v)) => conv(&C::DateTime2(Some(*v)), datetime),
        C::Date(Some(v)) => conv(&C::Date(Some(*v)), date),
        C::Time(Some(v)) => conv(&C::Time(Some(*v)), time),
        C::DateTimeOffset(Some(v)) => conv(&C::DateTimeOffset(Some(*v)), datetime_tz),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(int(42), Value::from(42));
        assert_eq!(int(1 << 53), Value::from(1i64 << 53));
        assert_eq!(int((1 << 53) + 1), Value::String("9007199254740993".into()));
        assert_eq!(int(i64::MIN), Value::String(i64::MIN.to_string()));
        assert_eq!(uint(u64::MAX), Value::String(u64::MAX.to_string()));
        assert_eq!(float(1.5), Value::from(1.5));
        assert_eq!(float(f64::NAN), Value::String("NaN".into()));
        assert_eq!(float(f64::NEG_INFINITY), Value::String("-Infinity".into()));
        assert_eq!(int_text("18446744073709551615"), Value::String("18446744073709551615".into()));
    }

    #[test]
    fn binary_truncates() {
        assert_eq!(binary(&[0xde, 0xad]), Value::String("0xdead".into()));
        let big = vec![0xffu8; 300];
        let s = binary(&big);
        let s = s.as_str().unwrap();
        assert!(s.ends_with('…'));
        assert_eq!(s.len(), 2 + 512 + '…'.len_utf8());
    }
}
