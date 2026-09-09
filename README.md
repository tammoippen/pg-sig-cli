# sqlsig

A small CLI that uses Windmill's own SQL signature parser
(`windmill-parser-sql`) to parse a SQL script and print the argument
signature Windmill would infer for it, as JSON.

## Layout

- `cli/` — the CLI binary (`sqlsig`)
- `vendor/windmill-parser`, `vendor/windmill-parser-sql` — vendored verbatim
  from https://github.com/windmill-labs/windmill at commit
  `8aa8b7ee6c23f859f169637c0bfd3f8d964509e9`
  (paths `backend/parsers/windmill-parser{,-sql}`). These crates are not
  published on crates.io, which is why they are vendored; the root
  `Cargo.toml` supplies the `[workspace.package]` / `[workspace.dependencies]`
  keys their manifests reference. The vendored code remains under Windmill's
  upstream license (AGPLv3 for the backend) — check the upstream repo before
  redistributing.

## Build

Requires Rust ≥ 1.85 (a transitive dependency uses edition 2024).

```
cargo build --release
# binary at target/release/sqlsig
```

## Usage

```
sqlsig [--lang pg|mysql|bigquery|snowflake|mssql|oracledb|duckdb] [-f FILE | SQL | -]
```

SQL comes from the first positional argument, `-f FILE`, or stdin.
Default dialect is `pg`.

```
$ sqlsig '
-- $1 email (varchar)
-- $2 max_rows (int) = 50
SELECT id FROM users
WHERE email = $1 AND age > $3::int AND note = $4
LIMIT $2;'
```

Output (abridged):

```json
{
  "lang": "pg",
  "signature": {
    "args": [
      { "name": "email",    "otyp": "varchar", "oidx": 1 },
      { "name": "max_rows", "otyp": "int",     "oidx": 2, "default": 50 },
      { "name": "$3",       "otyp": "int",     "oidx": 3 },
      { "name": "$4",       "otyp": "text",    "oidx": 4, "otyp_inferred": true }
    ]
  },
  "has_typed_annotations": true,
  "statement_count": 1,
  "placeholder_occurrences": [ { "param": 1, "byte_range": [...] }, ... ]
}
```

This demonstrates the parser's actual behavior:

- `$1`, `$2` typed by `-- $N name (type) = default` comment annotations
- `$3` typed by its inline `::int` cast
- `$4` has no type information anywhere, so it falls back to `text` and is
  flagged `otyp_inferred: true`
- a `$N` inside a string literal or comment produces no argument (the
  tokenizer is string/comment/dollar-quote aware)

Extra fields for `pg`: `has_typed_annotations` (any `-- $N name (type)`
annotation present), `placeholder_occurrences` (each real `$N` occurrence
with its byte range — what Windmill's executor uses to renumber sparse
placeholders), `db_resource` (`-- database <path>` header, if any), and
`statement_count` (top-level `;` splitting via `parse_sql_blocks`).
