//! Dialect-aware SQL generation: identifier quoting, paging and change statements.

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Postgres,
    MySql,
    Sqlite,
    MsSql,
}

pub fn quote_ident(d: Dialect, ident: &str) -> String {
    match d {
        Dialect::Postgres | Dialect::Sqlite => format!("\"{}\"", ident.replace('"', "\"\"")),
        Dialect::MySql => format!("`{}`", ident.replace('`', "``")),
        Dialect::MsSql => format!("[{}]", ident.replace(']', "]]")),
    }
}

pub fn qualified(d: Dialect, schema: Option<&str>, table: &str) -> String {
    match schema {
        Some(s) if !s.is_empty() => format!("{}.{}", quote_ident(d, s), quote_ident(d, table)),
        _ => quote_ident(d, table),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrderBy {
    pub column: String,
    #[serde(default)]
    pub desc: bool,
}

fn where_clause(where_: Option<&str>) -> String {
    match where_.map(str::trim) {
        Some(w) if !w.is_empty() => format!(" WHERE ({w})"),
        _ => String::new(),
    }
}

pub fn select_page(
    d: Dialect,
    schema: Option<&str>,
    table: &str,
    where_: Option<&str>,
    order: &[OrderBy],
    offset: u64,
    limit: u64,
) -> String {
    let mut sql = format!("SELECT * FROM {}{}", qualified(d, schema, table), where_clause(where_));
    let order_sql = order
        .iter()
        .map(|o| format!("{}{}", quote_ident(d, &o.column), if o.desc { " DESC" } else { " ASC" }))
        .collect::<Vec<_>>()
        .join(", ");
    if !order_sql.is_empty() {
        sql.push_str(" ORDER BY ");
        sql.push_str(&order_sql);
    }
    match d {
        Dialect::MsSql => {
            if order_sql.is_empty() {
                sql.push_str(" ORDER BY (SELECT NULL)");
            }
            sql.push_str(&format!(" OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY"));
        }
        _ => sql.push_str(&format!(" LIMIT {limit} OFFSET {offset}")),
    }
    sql
}

pub fn count(d: Dialect, schema: Option<&str>, table: &str, where_: Option<&str>) -> String {
    let func = if d == Dialect::MsSql { "COUNT_BIG(*)" } else { "COUNT(*)" };
    format!("SELECT {func} FROM {}{}", qualified(d, schema, table), where_clause(where_))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Change {
    Update { key: Map<String, Value>, values: Map<String, Value> },
    Delete { key: Map<String, Value> },
    Insert { values: Map<String, Value> },
}

impl Change {
    pub fn kind(&self) -> &'static str {
        match self {
            Change::Update { .. } => "update",
            Change::Delete { .. } => "delete",
            Change::Insert { .. } => "insert",
        }
    }
}

/// A parameterized statement; `expect_one` means it must affect exactly one row.
#[derive(Debug, Clone, PartialEq)]
pub struct Stmt {
    pub sql: String,
    pub params: Vec<Value>,
    pub expect_one: bool,
}

/// Strip type modifiers ("character varying(20)" -> "character varying") so a CAST never
/// silently truncates a value that the column itself would reject.
fn base_type(ty: &str) -> String {
    let mut out = String::with_capacity(ty.len());
    let mut depth = 0;
    for c in ty.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.trim().to_string()
}

struct Builder<'a> {
    d: Dialect,
    types: &'a HashMap<String, String>,
    params: Vec<Value>,
}

impl Builder<'_> {
    fn param(&mut self, column: &str, v: &Value) -> Result<String> {
        self.params.push(v.clone());
        let n = self.params.len();
        Ok(match self.d {
            Dialect::Postgres => {
                let Some(ty) = self.types.get(column) else { bail!("unknown column \"{column}\"") };
                format!("CAST(${n} AS {})", base_type(ty))
            }
            Dialect::MySql | Dialect::Sqlite => "?".to_string(),
            Dialect::MsSql => format!("@P{n}"),
        })
    }

    fn predicate(&mut self, key: &Map<String, Value>) -> Result<String> {
        if key.is_empty() {
            bail!("the change has no key columns");
        }
        let mut parts = Vec::with_capacity(key.len());
        for (col, v) in key {
            let q = quote_ident(self.d, col);
            if v.is_null() {
                parts.push(format!("{q} IS NULL"));
            } else {
                parts.push(format!("{q} = {}", self.param(col, v)?));
            }
        }
        Ok(parts.join(" AND "))
    }
}

/// Build the statement for one change. `types` maps column name -> declared type (required for
/// PostgreSQL, where every parameter is bound as text and cast to the column type).
pub fn build_change(
    d: Dialect,
    schema: Option<&str>,
    table: &str,
    change: &Change,
    types: &HashMap<String, String>,
) -> Result<Stmt> {
    let t = qualified(d, schema, table);
    let mut b = Builder { d, types, params: Vec::new() };
    let (sql, expect_one) = match change {
        Change::Update { key, values } => {
            if values.is_empty() {
                bail!("the update has no values");
            }
            let mut sets = Vec::with_capacity(values.len());
            for (col, v) in values {
                sets.push(format!("{} = {}", quote_ident(d, col), b.param(col, v)?));
            }
            let pred = b.predicate(key)?;
            (format!("UPDATE {t} SET {} WHERE {pred}", sets.join(", ")), true)
        }
        Change::Delete { key } => {
            let pred = b.predicate(key)?;
            (format!("DELETE FROM {t} WHERE {pred}"), true)
        }
        Change::Insert { values } if values.is_empty() => match d {
            Dialect::MySql => (format!("INSERT INTO {t} () VALUES ()"), false),
            _ => (format!("INSERT INTO {t} DEFAULT VALUES"), false),
        },
        Change::Insert { values } => {
            let mut cols = Vec::with_capacity(values.len());
            let mut ps = Vec::with_capacity(values.len());
            for (col, v) in values {
                cols.push(quote_ident(d, col));
                ps.push(b.param(col, v)?);
            }
            (format!("INSERT INTO {t} ({}) VALUES ({})", cols.join(", "), ps.join(", ")), false)
        }
    };
    Ok(Stmt { sql, params: b.params, expect_one })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use Dialect::*;

    #[test]
    fn quoting() {
        assert_eq!(quote_ident(Postgres, "a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_ident(Sqlite, "plain"), "\"plain\"");
        assert_eq!(quote_ident(MySql, "a`b"), "`a``b`");
        assert_eq!(quote_ident(MsSql, "a]b[c"), "[a]]b[c]");
        assert_eq!(qualified(Postgres, Some("s"), "t"), "\"s\".\"t\"");
        assert_eq!(qualified(MsSql, None, "t"), "[t]");
        assert_eq!(qualified(MySql, Some(""), "t"), "`t`");
    }

    #[test]
    fn paging() {
        let ob = [OrderBy { column: "id".into(), desc: true }, OrderBy { column: "n".into(), desc: false }];
        assert_eq!(
            select_page(Postgres, Some("public"), "t", Some(" x > 1 "), &ob, 10, 5),
            "SELECT * FROM \"public\".\"t\" WHERE (x > 1) ORDER BY \"id\" DESC, \"n\" ASC LIMIT 5 OFFSET 10"
        );
        assert_eq!(select_page(MySql, None, "t", None, &[], 0, 100), "SELECT * FROM `t` LIMIT 100 OFFSET 0");
        assert_eq!(select_page(Sqlite, None, "t", Some("  "), &[], 0, 1), "SELECT * FROM \"t\" LIMIT 1 OFFSET 0");
        assert_eq!(
            select_page(MsSql, Some("dbo"), "t", None, &[], 20, 10),
            "SELECT * FROM [dbo].[t] ORDER BY (SELECT NULL) OFFSET 20 ROWS FETCH NEXT 10 ROWS ONLY"
        );
        assert_eq!(
            select_page(MsSql, None, "t", Some("a=1"), &ob[..1], 0, 10),
            "SELECT * FROM [t] WHERE (a=1) ORDER BY [id] DESC OFFSET 0 ROWS FETCH NEXT 10 ROWS ONLY"
        );
        assert_eq!(count(MsSql, None, "t", Some("a=1")), "SELECT COUNT_BIG(*) FROM [t] WHERE (a=1)");
        assert_eq!(count(MySql, Some("db"), "t", None), "SELECT COUNT(*) FROM `db`.`t`");
    }

    fn change(v: Value) -> Change {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn changes_per_dialect() {
        let types: HashMap<String, String> =
            [("id", "integer"), ("name", "character varying(20)"), ("tags", "character varying(5)[]")]
                .into_iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect();
        let up = change(json!({"kind":"update","key":{"id":"7"},"values":{"name":"x","tags":null}}));
        let s = build_change(Postgres, Some("public"), "t", &up, &types).unwrap();
        assert_eq!(
            s.sql,
            "UPDATE \"public\".\"t\" SET \"name\" = CAST($1 AS character varying), \"tags\" = CAST($2 AS character varying[]) WHERE \"id\" = CAST($3 AS integer)"
        );
        assert_eq!(s.params, vec![json!("x"), Value::Null, json!("7")]);
        assert!(s.expect_one);

        let s = build_change(MsSql, None, "t", &up, &types).unwrap();
        assert_eq!(s.sql, "UPDATE [t] SET [name] = @P1, [tags] = @P2 WHERE [id] = @P3");

        let del = change(json!({"kind":"delete","key":{"id":1,"name":null}}));
        let s = build_change(MySql, None, "t", &del, &types).unwrap();
        assert_eq!(s.sql, "DELETE FROM `t` WHERE `id` = ? AND `name` IS NULL");
        assert_eq!(s.params, vec![json!(1)]);

        let ins = change(json!({"kind":"insert","values":{"id":1,"name":"a"}}));
        let s = build_change(Sqlite, None, "t", &ins, &types).unwrap();
        assert_eq!(s.sql, "INSERT INTO \"t\" (\"id\", \"name\") VALUES (?, ?)");
        assert!(!s.expect_one);

        let empty = change(json!({"kind":"insert","values":{}}));
        assert_eq!(build_change(MySql, None, "t", &empty, &types).unwrap().sql, "INSERT INTO `t` () VALUES ()");
        assert_eq!(build_change(MsSql, None, "t", &empty, &types).unwrap().sql, "INSERT INTO [t] DEFAULT VALUES");

        let bad = change(json!({"kind":"update","key":{"nope":1},"values":{"id":2}}));
        assert!(build_change(Postgres, None, "t", &bad, &types).is_err());
        let nokey = change(json!({"kind":"delete","key":{}}));
        assert!(build_change(Sqlite, None, "t", &nokey, &types).is_err());
    }
}
