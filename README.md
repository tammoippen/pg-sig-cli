# sqlsig

A small CLI that uses Windmill's own SQL signature parser
(`windmill-parser-sql`) to parse a SQL script and print the argument
signature Windmill would infer for it, as JSON.

## Layout

A single crate: `Cargo.toml` and `src/main.rs`, plus `LICENSE` and `NOTICE`.

`windmill-parser` and `windmill-parser-sql` come straight from
https://github.com/windmill-labs/windmill as pinned git dependencies
(rev `8aa8b7ee6c23f859f169637c0bfd3f8d964509e9`, paths
`backend/parsers/windmill-parser{,-sql}`). They are not published on
crates.io, so a git dependency is how they are consumed; Cargo resolves them
out of windmill's own `backend/` workspace, so none of their manifest keys
need mirroring here. They remain under Windmill's upstream license (AGPLv3
for the backend) — check the upstream repo before redistributing.

To move to a newer upstream commit, change the two `rev =` values in
`Cargo.toml` and run `cargo update`.

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

## Validating SQL for Windmill (`--validate`)

Windmill's pg executor doesn't just forward your SQL verbatim, so some
scripts run fine against a plain Postgres client but fail inside Windmill.
`--validate` (currently `pg`-dialect only) catches the common causes:

- **`:name`-style bind params** — Windmill's `pg` dialect only recognizes
  positional `$N` placeholders; a `:name` param is passed through as literal
  SQL and Postgres rejects it.
- **Stale/duplicate `-- $N` annotations** — a declaration referencing an
  index no `$N` placeholder actually uses (typo), or the same index declared
  twice (Windmill keeps both, conflicting, arg entries).
- **Un-annotated, uncast `$N`** — falls back to `text`
  (`otyp_inferred: true`). Windmill sends it as an explicit `text` parameter,
  which fails at `PREPARE` time against anything that doesn't accept an
  implicit `text` cast (`uuid`, `jsonb`, enums, arrays, …) — even though the
  same query works fine when a driver lets Postgres infer the parameter type
  from context.

```
sqlsig --validate "SELECT * FROM users WHERE id = \$1"
```

Pass `--conn <CONNINFO>` (libpq keyword/value string or a `postgres://` URL;
falls back to `$DATABASE_URL`) to additionally reproduce Windmill's exact
per-statement dispatch against a real database: placeholders are renumbered
the same way, then `PREPARE`d with the same declared/inferred types
Windmill would bind. This is **read-only** — `PREPARE` alone never executes
the statement, so it's safe even for `INSERT`/`UPDATE`/`DELETE`/DDL.

```
sqlsig --validate --conn "postgres://user:pass@host/db" -f query.sql
```

Output gets an extra `validation` field:

```json
{
  "validation": {
    "live_checked": true,
    "issues": [
      {
        "severity": "error",
        "source": "live",
        "statement": 1,
        "message": "PREPARE fehlgeschlagen: db error: ERROR: operator does not exist: uuid = text\n..."
      }
    ]
  }
}
```

The process exits non-zero if any `error`-severity issue was found, so
`--validate` is usable as a CI check. Only `NoTls` connections are currently
supported (no TLS-required hosts yet).

## License

`sqlsig` is licensed under the **GNU Affero General Public License v3.0 only**
(`AGPL-3.0-only`) — see [LICENSE](LICENSE).

This is not a free choice: `sqlsig` links `windmill-parser` and
`windmill-parser-sql`, which live under `backend/` in the Windmill repository
and are AGPLv3 (Copyright (c) 2022 Windmill Labs, Inc). The AGPL's copyleft
therefore extends to this program as a whole. Note in particular AGPL §13: if
you run a modified version of this code to offer a network service, you must
offer its source to the users of that service.

See [NOTICE](NOTICE) for third-party attribution.

