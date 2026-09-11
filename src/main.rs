// sqlsig — Copyright (c) 2026 Tammo Ippen, Marcel Konrad
//
// This program is free software: you can redistribute it and/or modify it
// under the terms of the GNU Affero General Public License, version 3, as
// published by the Free Software Foundation. It is distributed WITHOUT ANY
// WARRANTY; see the LICENSE file for the full text, and NOTICE for the
// third-party components it links (Windmill's AGPLv3 parser crates).

//! sqlsig — parse a SQL script with windmill-parser-sql and print the
//! argument signature Windmill would infer for it, as JSON.
//!
//! Usage:
//!   sqlsig [--lang <dialect>] "<sql>"        parse SQL given as an argument
//!   sqlsig [--lang <dialect>] -f query.sql   parse SQL read from a file
//!   echo "<sql>" | sqlsig [--lang <dialect>] parse SQL read from stdin
//!
//! Dialects: pg (default), mysql, bigquery, snowflake, mssql, oracledb, duckdb
//!
//! `--validate` additionally checks the SQL for patterns that are known to
//! fail inside Windmill even though they run fine against a plain Postgres
//! client (currently `pg` dialect only). Pass `--conn <CONNINFO>` (or set
//! `DATABASE_URL`) to also PREPARE the (renumbered) statements against a
//! real database — read-only, since PREPARE never executes anything.

use std::io::Read;

use anyhow::{bail, Context, Result};
use serde_json::json;
use windmill_parser::MainArgSignature;
use windmill_parser_sql::{
    parse_bigquery_sig, parse_db_resource, parse_duckdb_sig, parse_mssql_sig, parse_mysql_sig,
    parse_oracledb_sig, parse_pg_statement_arg_positions, parse_pgsql_sig_with_typed_schema,
    parse_snowflake_sig, parse_sql_blocks,
};

mod validate;

const USAGE: &str = "usage: sqlsig [--lang pg|mysql|bigquery|snowflake|mssql|oracledb|duckdb] [--validate [--conn CONNINFO]] [-f FILE | SQL | -]
       reads from stdin when no SQL argument (or '-') is given
       --validate checks for SQL that fails in Windmill despite running fine against plain Postgres (pg dialect only)
       --conn CONNINFO also PREPAREs the statements against a real DB (read-only); falls back to $DATABASE_URL";

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut lang = "pg".to_string();
    let mut sql: Option<String> = None;
    let mut from_file: Option<String> = None;
    let mut do_validate = false;
    let mut conn_str: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            "--lang" | "-l" => {
                lang = args.next().context("--lang requires a value")?;
            }
            "-f" | "--file" => {
                from_file = Some(args.next().context("-f requires a path")?);
            }
            "--validate" => do_validate = true,
            "--conn" => {
                conn_str = Some(args.next().context("--conn requires a value")?);
            }
            "-" => sql = Some(read_stdin()?),
            _ if sql.is_none() && from_file.is_none() => sql = Some(a),
            _ => bail!("unexpected argument: {a}\n{USAGE}"),
        }
    }

    let code = match (sql, from_file) {
        (Some(_), Some(_)) => bail!("give either SQL or -f FILE, not both\n{USAGE}"),
        (Some(s), None) => s,
        (None, Some(p)) => {
            std::fs::read_to_string(&p).with_context(|| format!("reading {p}"))?
        }
        (None, None) => read_stdin()?,
    };

    // Dialect-specific signature. For pg we use the *_with_typed_schema
    // variant so we can also report whether any arg was explicitly typed via
    // a `-- $1 name (type)` annotation.
    let (sig, typed_schema): (MainArgSignature, Option<bool>) = match lang.as_str() {
        "pg" | "postgres" | "postgresql" | "pgsql" => {
            let (sig, typed) = parse_pgsql_sig_with_typed_schema(&code)?;
            (sig, Some(typed))
        }
        "mysql" => (parse_mysql_sig(&code)?, None),
        "bigquery" => (parse_bigquery_sig(&code)?, None),
        "snowflake" => (parse_snowflake_sig(&code)?, None),
        "mssql" => (parse_mssql_sig(&code)?, None),
        "oracledb" | "oracle" => (parse_oracledb_sig(&code)?, None),
        "duckdb" => (parse_duckdb_sig(&code)?, None),
        other => bail!("unknown dialect: {other}\n{USAGE}"),
    };

    let mut out = json!({
        "lang": lang,
        "signature": sig,
        // `-- database <resource>` header, if present
        "db_resource": parse_db_resource(&code),
        // statements after splitting on top-level `;`
        "statement_count": parse_sql_blocks(&code, lang.starts_with("pg")).len(),
    });

    if let Some(typed) = typed_schema {
        let o = out.as_object_mut().unwrap();
        // true iff at least one `-- $N name (type)` annotation was found
        o.insert("has_typed_annotations".into(), json!(typed));
        // every `$N` placeholder occurrence the tokenizer sees as a real
        // parameter (i.e. not inside a string/comment/dollar-quoted block),
        // with its byte range in the source
        let occurrences: Vec<_> = parse_pg_statement_arg_positions(&code)
            .into_iter()
            .map(|(idx, r)| json!({"param": idx, "byte_range": [r.start, r.end]}))
            .collect();
        o.insert("placeholder_occurrences".into(), json!(occurrences));
    }

    let mut has_error = false;
    if do_validate {
        if !lang.starts_with("pg") && lang != "postgres" && lang != "postgresql" {
            bail!("--validate is currently only supported for --lang pg");
        }

        let mut issues = validate::static_checks(&code, &sig);

        let conn_str = conn_str.or_else(|| std::env::var("DATABASE_URL").ok());
        let live_checked = match &conn_str {
            Some(conn_str) => {
                let rt = tokio::runtime::Runtime::new().context("starting async runtime")?;
                match rt.block_on(validate::validate_live(&code, &sig, conn_str)) {
                    Ok(live_issues) => issues.extend(live_issues),
                    Err(e) => issues.push(validate::Issue::connection_error(e)),
                }
                true
            }
            None => false,
        };

        has_error = issues.iter().any(|i| matches!(i.severity, validate::Severity::Error));
        let o = out.as_object_mut().unwrap();
        o.insert(
            "validation".into(),
            json!({
                "live_checked": live_checked,
                "issues": issues.iter().map(|i| i.to_json()).collect::<Vec<_>>(),
            }),
        );
    }

    println!("{}", serde_json::to_string_pretty(&out)?);
    if has_error {
        std::process::exit(1);
    }
    Ok(())
}

fn read_stdin() -> Result<String> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("reading stdin")?;
    Ok(buf)
}
