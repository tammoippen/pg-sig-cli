// sqlsig — Copyright (c) 2026 Tammo Ippen, Marcel Konrad
//
// This program is free software: you can redistribute it and/or modify it
// under the terms of the GNU Affero General Public License, version 3, as
// published by the Free Software Foundation. It is distributed WITHOUT ANY
// WARRANTY; see the LICENSE file for the full text, and NOTICE for the
// third-party components it links (Windmill's AGPLv3 parser crates).

//! `--validate`: catch SQL that runs fine against a plain Postgres client
//! but fails inside Windmill, because Windmill's pg executor doesn't just
//! forward the query verbatim. It:
//!
//!  - renumbers sparse positional placeholders (`$5, $50` -> `$1, $2`) per
//!    statement (see `parse_pg_statement_arg_positions` in
//!    `windmill-parser-sql`),
//!  - only ever recognizes bare `$N` placeholders — a `:name` style bind
//!    param is not special-cased for the `pg` dialect and is sent to
//!    Postgres as literal SQL,
//!  - and, when every arg's declared/inferred type is one it knows about,
//!    sends the query as an *unnamed prepared statement* with those types
//!    baked in (`query_typed_raw`) rather than letting Postgres infer
//!    parameter types from context. An un-annotated, uncast `$N` falls back
//!    to `text`, which Postgres will happily bind — right up until the
//!    query body needs an implicit `text -> uuid`/`jsonb`/enum/... cast that
//!    doesn't exist, at which point `PREPARE` (and therefore the job) fails
//!    with something like `operator does not exist: uuid = text`.
//!
//! This module reproduces both: cheap static checks that need no DB
//! connection, and — given a connection string — the exact per-statement
//! `PREPARE` Windmill would issue, so mismatches surface without ever
//! executing (and therefore without ever mutating) anything.

use std::collections::HashMap;

use anyhow::{Context, Result};
use regex::Regex;
use windmill_parser::{Arg, MainArgSignature};
use windmill_parser_sql::{parse_pg_statement_arg_positions, parse_sql_blocks};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    fn as_str(&self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Issue {
    pub severity: Severity,
    /// Where the issue was found: `"static"` (no DB needed) or `"live"`
    /// (found while PREPAREing against a real connection).
    pub source: &'static str,
    /// 1-based index into the statements `parse_sql_blocks` split the code
    /// into, if the issue is tied to one.
    pub statement: Option<usize>,
    pub message: String,
}

impl Issue {
    fn new(severity: Severity, source: &'static str, statement: Option<usize>, message: impl Into<String>) -> Self {
        Issue { severity, source, statement, message: message.into() }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "severity": self.severity.as_str(),
            "source": self.source,
            "statement": self.statement,
            "message": self.message,
        })
    }

    pub fn connection_error(e: anyhow::Error) -> Self {
        Issue::new(Severity::Error, "live", None, format!("{e:#}"))
    }
}

lazy_static::lazy_static! {
    // A `-- $N name (type)` / `-- $N name` declaration line, mirroring
    // windmill-parser-sql's own `RE_ARG_PGSQL` closely enough to check
    // consistency (it doesn't need to be byte-for-byte identical, since
    // these checks are advisory, not a re-implementation of the parser).
    static ref RE_ARG_PGSQL: Regex =
        Regex::new(r#"(?m)^-- \$(\d+) (\w+)(?: \(([A-Za-z0-9_\[\]]+)\))?(?: ?= ?(.+))? *(?:\r|\n|$)"#).unwrap();

    // A `:name` occurrence that is *not* part of a `::type` cast and does
    // not look like an array slice (`arr[1:2]`, digits right after the
    // colon). This is a plain regex over the raw source — unlike the
    // parser's tokenizer it isn't string-/comment-/dollar-quote aware, so
    // it can false-positive (e.g. inside a string literal); it's a
    // heuristic warning, not a hard error.
    static ref RE_NAMED_PARAM: Regex = Regex::new(r#"(^|[^:]):([A-Za-z_]\w*)"#).unwrap();

    // `$N::type []` — an inline cast whose array brackets are separated
    // from the type name by whitespace. Real Postgres tokenizes `type` and
    // `[]` independently, so `::int []` and `::int[]` are equivalent there.
    // windmill-parser-sql's inline-cast regex
    // (`\$(\d+)(?:::(\w+(?:\[\])?))?`) requires the `[]` to *directly*
    // abut the type name, though: with the space present it captures only
    // the bare scalar type name and silently drops the array marker — and
    // crucially does *not* set `otyp_inferred`, so check 4 below (which
    // only looks at that flag) can't catch it. This is the exact failure
    // mode reported from an sqlfluff auto-format pass that inserted a space
    // before `[]`, silently downgrading e.g. `vector[]`/`int[]` params to a
    // scalar type and then failing at PREPARE time with something like
    // `cannot cast type integer to integer[]`.
    static ref RE_CAST_ARRAY_SPACE: Regex = Regex::new(r#"\$(\d+)::[A-Za-z_]\w*\s+\[\s*\]"#).unwrap();

    // Same failure mode inside a `-- $N name (type [])` declaration
    // comment: `RE_ARG_PGSQL`'s type group (`[A-Za-z0-9_\[\]]+`) excludes
    // whitespace, so a space before `[]` here doesn't just drop the array
    // marker — it makes the *whole* declaration line fail to match, and the
    // arg silently reverts to an unnamed, untyped `$N` (still caught by
    // check 4, but flagged here too for a message that points at the real
    // root cause).
    static ref RE_ANNOTATION_ARRAY_SPACE: Regex =
        Regex::new(r#"(?m)^-- \$(\d+) \w+ \([A-Za-z0-9_]+\s+\[\s*\]\)"#).unwrap();
}

/// Checks that need no database connection: named-parameter usage the `pg`
/// dialect won't recognize, and inconsistencies between `-- $N` declaration
/// comments and how `$N` is actually used in the SQL body.
pub fn static_checks(code: &str, sig: &MainArgSignature) -> Vec<Issue> {
    let mut issues = Vec::new();

    // 1. `:name`-style bind params. Windmill's `pg` dialect only recognizes
    //    `$N`; a `:name` placeholder is passed through to Postgres as-is and
    //    will raise a syntax error there — a query that a driver supporting
    //    named params (e.g. sqlalchemy, sqlx named-args) would run fine.
    let mut seen_named: Vec<String> = Vec::new();
    for cap in RE_NAMED_PARAM.captures_iter(code) {
        let name = cap.get(2).unwrap().as_str().to_string();
        if !seen_named.contains(&name) {
            seen_named.push(name);
        }
    }
    if !seen_named.is_empty() {
        issues.push(Issue::new(
            Severity::Warning,
            "static",
            None,
            format!(
                "gefundene(r) benannte(r) Parameter {} — Windmill's pg-Dialekt kennt nur positionelle `$N`-Platzhalter; `:name` wird unver\u{e4}ndert an Postgres weitergereicht und f\u{fc}hrt dort typischerweise zu einem Syntaxfehler.",
                seen_named.iter().map(|n| format!(":{n}")).collect::<Vec<_>>().join(", ")
            ),
        ));
    }

    // 2. Declaration comments (`-- $N name (type)`) referencing an index
    //    that no `$N` placeholder actually uses in the SQL body — almost
    //    always a typo (off-by-one, wrong number after copy/paste).
    let used_indices: std::collections::HashSet<i32> =
        parse_pg_statement_arg_positions(code).into_iter().map(|(i, _)| i).collect();
    let mut declared_indices: HashMap<i32, usize> = HashMap::new();
    for cap in RE_ARG_PGSQL.captures_iter(code) {
        if let Some(idx) = cap.get(1).and_then(|m| m.as_str().parse::<i32>().ok()) {
            *declared_indices.entry(idx).or_insert(0) += 1;
            if !used_indices.contains(&idx) {
                issues.push(Issue::new(
                    Severity::Warning,
                    "static",
                    None,
                    format!(
                        "`-- ${idx} ...`-Annotation vorhanden, aber `${idx}` wird nirgends im SQL-Text verwendet — vermutlich ein Tippfehler im Index."
                    ),
                ));
            }
        }
    }

    // 3. The same index declared more than once. windmill-parser-sql keeps
    //    *both* entries (it doesn't dedupe), so the resulting arg signature
    //    ends up with two args sharing the same `oidx` — Windmill will only
    //    ever bind one of them, silently.
    for (idx, count) in declared_indices.iter() {
        if *count > 1 {
            issues.push(Issue::new(
                Severity::Error,
                "static",
                None,
                format!(
                    "`$@idx@` mehrfach ({count}x) per `-- $@idx@ ...`-Kommentar annotiert — Windmill legt dabei mehrere widerspr\u{fc}chliche Arg-Eintr\u{e4}ge f\u{fc}r denselben Index an."
                )
                .replace("@idx@", &idx.to_string()),
            ));
        }
    }

    // 4. `$N` used in the query without any type info anywhere (no
    //    declaration, no inline `::type` cast). Windmill defaults these to
    //    `text` (`otyp_inferred: true`) and — since `text` *is* a type it
    //    knows how to bind — sends it as an explicit `text` parameter. That
    //    fails at PREPARE time against any column/operator that doesn't
    //    accept an implicit `text` argument (uuid, jsonb, enums, arrays, …).
    for arg in &sig.args {
        if arg.otyp_inferred {
            let idx = arg.oidx.map(|i| i.to_string()).unwrap_or_else(|| "?".to_string());
            issues.push(Issue::new(
                Severity::Warning,
                "static",
                None,
                format!(
                    "`${idx}` hat keine explizite Typangabe (weder `-- ${idx} name (type)` noch `${idx}::type`) und f\u{e4}llt auf `text` zur\u{fc}ck. Das schl\u{e4}gt fehl, sobald der Zielkontext keinen impliziten `text`-Cast erlaubt (z.\u{a0}B. uuid, jsonb, enum, Array-Spalten). Am besten mit einer `-- ${idx} name (type)`-Annotation oder einem `::type`-Cast fixieren."
                ),
            ));
        }
    }

    // 5. `$N::type []` / `-- $N name (type [])` — array brackets separated
    //    from the type name by whitespace. Valid, semantically identical SQL
    //    against real Postgres, but windmill-parser-sql's regexes require
    //    the `[]` to directly abut the type name, so the array marker is
    //    silently lost (inline cast) or the whole declaration goes
    //    unrecognized (comment annotation) — exactly the bug an sqlfluff
    //    auto-format pass introduced by inserting a space before `[]`.
    for cap in RE_CAST_ARRAY_SPACE.captures_iter(code) {
        let idx = cap.get(1).unwrap().as_str();
        issues.push(Issue::new(
            Severity::Error,
            "static",
            None,
            format!(
                "`${idx}::type []` mit Leerzeichen vor `[]` gefunden — gegen echtes Postgres funktioniert das identisch zu `${idx}::type[]`, aber Windmill's Parser erkennt das Array-Suffix dann nicht mehr und bindet `${idx}` als Skalar statt als Array. Leerzeichen vor den Klammern entfernen (`${idx}::type[]`)."
            ),
        ));
    }
    for cap in RE_ANNOTATION_ARRAY_SPACE.captures_iter(code) {
        let idx = cap.get(1).unwrap().as_str();
        issues.push(Issue::new(
            Severity::Error,
            "static",
            None,
            format!(
                "`-- ${idx} name (type [])`-Annotation mit Leerzeichen vor `[]` gefunden — dadurch erkennt Windmill's Parser die gesamte Deklarationszeile nicht mehr; `${idx}` f\u{e4}llt komplett auf einen unbenannten, ungetypten Parameter zur\u{fc}ck. Leerzeichen vor den Klammern entfernen (`(type[])`)."
            ),
        ));
    }

    issues
}

/// Reproduces Windmill's per-statement dispatch (renumber sparse `$N`, then
/// either `PREPARE` with explicit types or a plain `PREPARE` for Postgres to
/// infer) against a real connection, without ever executing anything —
/// `PREPARE` alone never runs the statement, so this is read-only even for
/// `INSERT`/`UPDATE`/`DELETE`/DDL.
pub async fn validate_live(code: &str, sig: &MainArgSignature, conn_str: &str) -> Result<Vec<Issue>> {
    let (client, connection) = tokio_postgres::connect(conn_str, tokio_postgres::NoTls)
        .await
        .with_context(|| "Verbindung zur Postgres-Datenbank fehlgeschlagen")?;

    let conn_task = tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("warnung: Verbindung zur Datenbank beendet: {e}");
        }
    });

    let args_by_idx: HashMap<i32, &Arg> = sig.args.iter().filter_map(|a| a.oidx.map(|i| (i, a))).collect();

    let mut issues = Vec::new();
    let blocks = parse_sql_blocks(code, true);
    for (stmt_no, block) in blocks.iter().enumerate() {
        let statement_index = stmt_no + 1;
        let positions = parse_pg_statement_arg_positions(block);
        let mut unique_indices: Vec<i32> = positions.iter().map(|(i, _)| *i).collect();
        unique_indices.sort_unstable();
        unique_indices.dedup();

        // Renumber sparse placeholders (`$5, $50` -> `$1, $2`), back to
        // front by byte position, exactly like `pg_executor.rs` does.
        let renumber: HashMap<i32, usize> =
            unique_indices.iter().enumerate().map(|(i, oidx)| (*oidx, i + 1)).collect();
        let mut query = (*block).to_string();
        let mut ordered_positions = positions.clone();
        ordered_positions.sort_by_key(|(_, range)| std::cmp::Reverse(range.start));
        for (oidx, range) in ordered_positions {
            if let Some(new_i) = renumber.get(&oidx) {
                if oidx as usize != *new_i {
                    query.replace_range(range, &new_i.to_string());
                }
            }
        }

        // Resolve a Postgres type per (renumbered) position, in the same
        // order `$1, $2, ...` appear after renumbering.
        let mut param_types = Vec::with_capacity(unique_indices.len());
        let mut all_resolved = true;
        for oidx in &unique_indices {
            let otyp = args_by_idx.get(oidx).and_then(|a| a.otyp.as_deref()).unwrap_or("text");
            match otyp_to_pg_type(otyp) {
                Some(t) => param_types.push(t),
                None => {
                    all_resolved = false;
                    break;
                }
            }
        }

        let prepare_result = if all_resolved {
            client.prepare_typed(&query, &param_types).await
        } else {
            client.prepare(&query).await
        };

        if let Err(e) = prepare_result {
            let e = anyhow::Error::new(e);
            issues.push(Issue::new(
                Severity::Error,
                "live",
                Some(statement_index),
                format!("PREPARE fehlgeschlagen: {e:#}"),
            ));
        }
    }

    drop(client);
    let _ = conn_task.await;

    Ok(issues)
}

/// Mirrors `otyp_to_pg_type` in windmill-worker's `pg_executor.rs`: the
/// mapping Windmill uses to decide whether an arg's declared/cast/inferred
/// type name is one it can bind as an explicit prepared-statement parameter
/// type. Anything not in this table makes Windmill fall back to a plain
/// `PREPARE` (Postgres infers the type from context, so it doesn't hit the
/// `text`-mismatch failure mode this tool is trying to catch either).
fn otyp_to_pg_type(otyp: &str) -> Option<tokio_postgres::types::Type> {
    use tokio_postgres::types::Type;

    let base = otyp.trim_end_matches("[]");
    let is_array = otyp.ends_with("[]");

    let (scalar, array) = match base {
        "bool" | "boolean" => (Type::BOOL, Type::BOOL_ARRAY),
        "char" | "character" => (Type::CHAR, Type::CHAR_ARRAY),
        "smallint" | "smallserial" | "int2" | "serial2" => (Type::INT2, Type::INT2_ARRAY),
        "int" | "integer" | "int4" | "serial" => (Type::INT4, Type::INT4_ARRAY),
        "bigint" | "bigserial" | "int8" | "serial8" => (Type::INT8, Type::INT8_ARRAY),
        "real" | "float4" => (Type::FLOAT4, Type::FLOAT4_ARRAY),
        "double" | "double precision" | "float8" => (Type::FLOAT8, Type::FLOAT8_ARRAY),
        "numeric" | "decimal" => (Type::NUMERIC, Type::NUMERIC_ARRAY),
        "text" => (Type::TEXT, Type::TEXT_ARRAY),
        "varchar" | "character varying" => (Type::VARCHAR, Type::VARCHAR_ARRAY),
        "uuid" => (Type::UUID, Type::UUID_ARRAY),
        "date" => (Type::DATE, Type::DATE_ARRAY),
        "time" => (Type::TIME, Type::TIME_ARRAY),
        "timetz" => (Type::TIMETZ, Type::TIMETZ_ARRAY),
        "timestamp" => (Type::TIMESTAMP, Type::TIMESTAMP_ARRAY),
        "timestamptz" => (Type::TIMESTAMPTZ, Type::TIMESTAMPTZ_ARRAY),
        "json" => (Type::JSON, Type::JSON_ARRAY),
        "jsonb" => (Type::JSONB, Type::JSONB_ARRAY),
        "bytea" => (Type::BYTEA, Type::BYTEA_ARRAY),
        "oid" => (Type::OID, Type::OID_ARRAY),
        _ => return None,
    };

    Some(if is_array { array } else { scalar })
}
