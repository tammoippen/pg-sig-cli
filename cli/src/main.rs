//! sqlsig — parse a SQL script with windmill-parser-sql and print the
//! argument signature Windmill would infer for it, as JSON.
//!
//! Usage:
//!   sqlsig [--lang <dialect>] "<sql>"        parse SQL given as an argument
//!   sqlsig [--lang <dialect>] -f query.sql   parse SQL read from a file
//!   echo "<sql>" | sqlsig [--lang <dialect>] parse SQL read from stdin
//!
//! Dialects: pg (default), mysql, bigquery, snowflake, mssql, oracledb, duckdb

use std::io::Read;

use anyhow::{bail, Context, Result};
use serde_json::json;
use windmill_parser::MainArgSignature;
use windmill_parser_sql::{
    parse_bigquery_sig, parse_db_resource, parse_duckdb_sig, parse_mssql_sig, parse_mysql_sig,
    parse_oracledb_sig, parse_pg_statement_arg_positions, parse_pgsql_sig_with_typed_schema,
    parse_snowflake_sig, parse_sql_blocks,
};

const USAGE: &str = "usage: sqlsig [--lang pg|mysql|bigquery|snowflake|mssql|oracledb|duckdb] [-f FILE | SQL | -]
       reads from stdin when no SQL argument (or '-') is given";

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

    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn read_stdin() -> Result<String> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("reading stdin")?;
    Ok(buf)
}
