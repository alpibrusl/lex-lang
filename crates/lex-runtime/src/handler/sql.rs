//! `sql` effect: driver-neutral parameters, SQLite / Postgres query runners, error records, and the LRU-bounded connection and cursor registries.

use super::*;

/// Build a `SqlError = { message, code, detail }` Lex record (#380).
/// `code` and `detail` are `None` by default; the driver-specific
/// converters below populate them with real values.
pub(super) fn sql_error(message: impl Into<String>, code: Option<String>, detail: Option<String>) -> Value {
    let some = |s: String| Value::Variant { name: "Some".into(), args: vec![Value::Str(s.into())] };
    let none = || Value::Variant { name: "None".into(), args: vec![] };
    let mut rec = indexmap::IndexMap::new();
    let msg: String = message.into();
    rec.insert("message".into(), Value::Str(msg.into()));
    rec.insert("code".into(), match code {
        Some(c) => some(c),
        None => none(),
    });
    rec.insert("detail".into(), match detail {
        Some(d) => some(d),
        None => none(),
    });
    Value::record_dynamic(rec)
}

/// Convert a rusqlite error into a `SqlError`. The `code` is the
/// symbolic extended-result-code name (`SQLITE_BUSY`,
/// `SQLITE_CONSTRAINT_UNIQUE`, …) when present — this is what
/// callers want for dialect-aware retry / conflict handling.
///
/// rusqlite has two main error shapes that carry a numeric code:
/// `SqliteFailure` (driver-side runtime errors — constraints, busy,
/// IO) and `SqlInputError` (statement-preparation failures —
/// syntax, unknown table). Both are unpacked the same way.
pub(super) fn sqlite_err_to_sql_error(e: rusqlite::Error, op: &str) -> Value {
    let message = format!("{op}: {e}");
    match &e {
        rusqlite::Error::SqliteFailure(ffi, detail_opt) => {
            sql_error(
                message,
                Some(sqlite_extended_code_name(ffi.extended_code)),
                detail_opt.clone(),
            )
        }
        rusqlite::Error::SqlInputError { error, msg, .. } => {
            sql_error(
                message,
                Some(sqlite_extended_code_name(error.extended_code)),
                Some(msg.clone()),
            )
        }
        _ => sql_error(message, None, None),
    }
}

/// Map a SQLite extended result code (numeric) to its symbolic name.
/// We only cover the codes a Lex caller is likely to dispatch on
/// (constraint kinds, busy/locked, read-only, IO); anything else
/// falls back to a generic `SQLITE_ERROR_<n>` stringification so the
/// numeric code is still recoverable.
pub(super) fn sqlite_extended_code_name(code: i32) -> String {
    use rusqlite::ffi::*;
    let s = match code {
        SQLITE_BUSY => "SQLITE_BUSY",
        SQLITE_LOCKED => "SQLITE_LOCKED",
        SQLITE_READONLY => "SQLITE_READONLY",
        SQLITE_IOERR => "SQLITE_IOERR",
        SQLITE_CORRUPT => "SQLITE_CORRUPT",
        SQLITE_NOTFOUND => "SQLITE_NOTFOUND",
        SQLITE_FULL => "SQLITE_FULL",
        SQLITE_CANTOPEN => "SQLITE_CANTOPEN",
        SQLITE_PROTOCOL => "SQLITE_PROTOCOL",
        SQLITE_SCHEMA => "SQLITE_SCHEMA",
        SQLITE_TOOBIG => "SQLITE_TOOBIG",
        SQLITE_CONSTRAINT => "SQLITE_CONSTRAINT",
        SQLITE_CONSTRAINT_CHECK => "SQLITE_CONSTRAINT_CHECK",
        SQLITE_CONSTRAINT_FOREIGNKEY => "SQLITE_CONSTRAINT_FOREIGNKEY",
        SQLITE_CONSTRAINT_NOTNULL => "SQLITE_CONSTRAINT_NOTNULL",
        SQLITE_CONSTRAINT_PRIMARYKEY => "SQLITE_CONSTRAINT_PRIMARYKEY",
        SQLITE_CONSTRAINT_TRIGGER => "SQLITE_CONSTRAINT_TRIGGER",
        SQLITE_CONSTRAINT_UNIQUE => "SQLITE_CONSTRAINT_UNIQUE",
        SQLITE_CONSTRAINT_VTAB => "SQLITE_CONSTRAINT_VTAB",
        SQLITE_CONSTRAINT_ROWID => "SQLITE_CONSTRAINT_ROWID",
        SQLITE_MISMATCH => "SQLITE_MISMATCH",
        SQLITE_RANGE => "SQLITE_RANGE",
        SQLITE_NOTADB => "SQLITE_NOTADB",
        SQLITE_AUTH => "SQLITE_AUTH",
        _ => return format!("SQLITE_ERROR_{code}"),
    };
    s.to_string()
}

/// Convert a postgres error into a `SqlError`. The `code` is the
/// 5-character SQLSTATE (`23505`, `40P01`, …); `detail` is the
/// driver's optional detail message when present.
pub(super) fn pg_err_to_sql_error(e: postgres::Error, op: &str) -> Value {
    let message = format!("{op}: {e}");
    let code = e.as_db_error().map(|db| db.code().code().to_string());
    let detail = e.as_db_error().and_then(|db| db.detail().map(|s| s.to_string()));
    sql_error(message, code, detail)
}

pub(super) fn expect_sql_handle(v: Option<&Value>) -> Result<u64, String> {
    match v {
        Some(Value::Int(n)) if *n >= 0 => Ok(*n as u64),
        Some(other) => Err(format!("expected Db handle (Int), got {other:?}")),
        None => Err("missing Db argument".into()),
    }
}

/// Convert a `List[SqlParam]` value to driver-neutral `SqlParamValue`s.
/// SqlParam = PStr(Str) | PInt(Int) | PFloat(Float) | PBool(Bool) | PNull
pub(super) fn expect_sql_params(v: Option<&Value>) -> Result<Vec<SqlParamValue>, String> {
    let items = match v {
        Some(Value::List(xs)) => xs,
        Some(other) => return Err(format!("expected List[SqlParam], got {other:?}")),
        None => return Err("missing params argument".into()),
    };
    items.iter().map(|item| {
        match item {
            Value::Variant { name, args } => match name.as_str() {
                "PStr"   => match args.first() {
                    Some(Value::Str(s)) => Ok(SqlParamValue::Text(s.to_string())),
                    _ => Err("PStr requires a Str argument".into()),
                },
                "PInt"   => match args.first() {
                    Some(Value::Int(n)) => Ok(SqlParamValue::Integer(*n)),
                    _ => Err("PInt requires an Int argument".into()),
                },
                "PFloat" => match args.first() {
                    Some(Value::Float(f)) => Ok(SqlParamValue::Real(*f)),
                    _ => Err("PFloat requires a Float argument".into()),
                },
                "PBool"  => match args.first() {
                    Some(Value::Bool(b)) => Ok(SqlParamValue::Bool(*b)),
                    _ => Err("PBool requires a Bool argument".into()),
                },
                "PNull"  => Ok(SqlParamValue::Null),
                other    => Err(format!("unknown SqlParam constructor `{other}`")),
            },
            // Backward-compat: bare strings are accepted as PStr.
            Value::Str(s) => Ok(SqlParamValue::Text(s.to_string())),
            other => Err(format!("expected SqlParam variant, got {other:?}")),
        }
    }).collect()
}

/// Convert `SqlParamValue`s to rusqlite-typed values for SQLite binding.
pub(super) fn sqlite_params(params: &[SqlParamValue]) -> Vec<rusqlite::types::Value> {
    params.iter().map(|p| match p {
        SqlParamValue::Text(s)    => rusqlite::types::Value::Text(s.clone()),
        SqlParamValue::Integer(n) => rusqlite::types::Value::Integer(*n),
        SqlParamValue::Real(f)    => rusqlite::types::Value::Real(*f),
        SqlParamValue::Bool(b)    => rusqlite::types::Value::Integer(*b as i64),
        SqlParamValue::Null       => rusqlite::types::Value::Null,
    }).collect()
}

/// Lex SQL is authored with SQLite-style `?` positional placeholders, but
/// Postgres requires `$1, $2, …`. Rewrite each `?` placeholder to the matching
/// `$n` so the same parameterized statement runs on both backends. Only `?`
/// outside single-quoted string literals are treated as placeholders (a `?`
/// inside a literal — or an escaped `''` — is left untouched).
pub(super) fn pg_rewrite_placeholders(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len() + 8);
    let mut n: u32 = 0;
    let mut in_str = false;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                out.push(c);
                if in_str {
                    // A doubled '' is an escaped quote: stay inside the literal.
                    if chars.peek() == Some(&'\'') {
                        out.push(chars.next().unwrap());
                    } else {
                        in_str = false;
                    }
                } else {
                    in_str = true;
                }
            }
            '?' if !in_str => {
                n += 1;
                out.push('$');
                out.push_str(&n.to_string());
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod pg_placeholder_tests {
    use super::pg_rewrite_placeholders;

    #[test]
    fn rewrites_positional_placeholders() {
        assert_eq!(
            pg_rewrite_placeholders(
                "INSERT INTO events(id, kind, parent, payload_json, ts_ms) VALUES (?, ?, ?, ?, ?) ON CONFLICT(id) DO NOTHING"
            ),
            "INSERT INTO events(id, kind, parent, payload_json, ts_ms) VALUES ($1, $2, $3, $4, $5) ON CONFLICT(id) DO NOTHING"
        );
        assert_eq!(
            pg_rewrite_placeholders("SELECT * FROM t WHERE a=? AND b=?"),
            "SELECT * FROM t WHERE a=$1 AND b=$2"
        );
    }

    #[test]
    fn leaves_question_marks_inside_string_literals() {
        assert_eq!(
            pg_rewrite_placeholders("INSERT INTO t VALUES (?, 'lit?', ?)"),
            "INSERT INTO t VALUES ($1, 'lit?', $2)"
        );
    }

    #[test]
    fn handles_escaped_quotes_in_literals() {
        assert_eq!(
            pg_rewrite_placeholders("UPDATE t SET note='it''s ok?' WHERE id=?"),
            "UPDATE t SET note='it''s ok?' WHERE id=$1"
        );
    }

    #[test]
    fn no_placeholders_is_unchanged() {
        assert_eq!(pg_rewrite_placeholders("SELECT 1"), "SELECT 1");
    }
}

/// Lex's `PFloat` params are always `f64`, but a placeholder's Postgres
/// parameter type is inferred from the column it binds to, and Lex SQL
/// schemas commonly use `REAL` (float4) rather than `DOUBLE PRECISION`
/// (float8). A plain `f64` only implements `ToSql` for float8, so binding
/// it against a float4 parameter fails to serialize. This wrapper accepts
/// either width and encodes to whichever one Postgres actually asked for.
#[derive(Debug)]
pub(super) struct PgFloatParam(f64);

impl postgres::types::ToSql for PgFloatParam {
    fn to_sql(
        &self,
        ty: &postgres::types::Type,
        out: &mut bytes::BytesMut,
    ) -> Result<postgres::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
        use bytes::BufMut;
        match *ty {
            postgres::types::Type::FLOAT4 => out.put_f32(self.0 as f32),
            _ => out.put_f64(self.0),
        }
        Ok(postgres::types::IsNull::No)
    }

    fn accepts(ty: &postgres::types::Type) -> bool {
        matches!(*ty, postgres::types::Type::FLOAT4 | postgres::types::Type::FLOAT8)
    }

    postgres::types::to_sql_checked!();
}

/// Box `SqlParamValue`s as `dyn ToSql + Sync` for Postgres binding.
pub(super) fn pg_param_refs(params: &[SqlParamValue]) -> Vec<Box<dyn postgres::types::ToSql + Sync>> {
    params.iter().map(|p| -> Box<dyn postgres::types::ToSql + Sync> {
        match p {
            SqlParamValue::Text(s)    => Box::new(s.clone()),
            SqlParamValue::Integer(n) => Box::new(*n),
            SqlParamValue::Real(f)    => Box::new(PgFloatParam(*f)),
            SqlParamValue::Bool(b)    => Box::new(*b),
            SqlParamValue::Null       => Box::new(Option::<String>::None),
        }
    }).collect()
}

#[cfg(test)]
mod pg_float_param_tests {
    use super::PgFloatParam;
    use bytes::{Buf, BytesMut};
    use postgres::types::{ToSql, Type};

    #[test]
    fn encodes_float4_as_4_bytes_matching_the_value() {
        let mut out = BytesMut::new();
        PgFloatParam(6.5).to_sql(&Type::FLOAT4, &mut out).unwrap();
        assert_eq!(out.len(), 4);
        assert_eq!(out.get_f32(), 6.5f32);
    }

    #[test]
    fn encodes_float8_as_8_bytes_matching_the_value() {
        let mut out = BytesMut::new();
        PgFloatParam(6.5).to_sql(&Type::FLOAT8, &mut out).unwrap();
        assert_eq!(out.len(), 8);
        assert_eq!(out.get_f64(), 6.5f64);
    }

    #[test]
    fn accepts_only_float4_and_float8() {
        assert!(PgFloatParam::accepts(&Type::FLOAT4));
        assert!(PgFloatParam::accepts(&Type::FLOAT8));
        assert!(!PgFloatParam::accepts(&Type::TEXT));
        assert!(!PgFloatParam::accepts(&Type::INT8));
    }
}

/// Run a statement on SQLite and pack rows into `Value::List(Value::Record(...))`.
pub(super) fn sql_run_query_sqlite(
    conn: &rusqlite::Connection,
    stmt_str: &str,
    params: &[SqlParamValue],
) -> Value {
    let mut stmt = match conn.prepare(stmt_str) {
        Ok(s)  => s,
        Err(e) => return err(sqlite_err_to_sql_error(e, "sql.query")),
    };
    let column_count = stmt.column_count();
    let column_names: Vec<String> = (0..column_count)
        .map(|i| stmt.column_name(i).unwrap_or("").to_string())
        .collect();
    let bound = sqlite_params(params);
    let bind: Vec<&dyn rusqlite::ToSql> = bound.iter()
        .map(|p| p as &dyn rusqlite::ToSql)
        .collect();
    let mut rows = match stmt.query(rusqlite::params_from_iter(bind.iter())) {
        Ok(r)  => r,
        Err(e) => return err(sqlite_err_to_sql_error(e, "sql.query")),
    };
    let mut out: Vec<Value> = Vec::new();
    loop {
        let row = match rows.next() {
            Ok(Some(r)) => r,
            Ok(None)    => break,
            Err(e)      => return err(sqlite_err_to_sql_error(e, "sql.query")),
        };
        let mut rec = indexmap::IndexMap::new();
        for (i, name) in column_names.iter().enumerate() {
            let cell = match row.get_ref(i) {
                Ok(c)  => sql_value_ref_to_lex(c),
                Err(e) => return err(sqlite_err_to_sql_error(e, &format!("sql.query: column {i}"))),
            };
            rec.insert(name.clone(), cell);
        }
        out.push(Value::record_dynamic(rec));
    }
    ok(Value::List(out.into()))
}

/// Run a statement on Postgres and pack rows into `Value::List(Value::Record(...))`.
pub(super) fn sql_run_query_pg(
    client: &mut postgres::Client,
    stmt_str: &str,
    params: &[SqlParamValue],
) -> Value {
    let pg = pg_param_refs(params);
    let refs: Vec<&(dyn postgres::types::ToSql + Sync)> =
        pg.iter().map(|b| b.as_ref()).collect();
    let stmt_pg = pg_rewrite_placeholders(stmt_str);
    let rows = match client.query(stmt_pg.as_str(), &refs) {
        Ok(r)  => r,
        Err(e) => return err(pg_err_to_sql_error(e, "sql.query")),
    };
    let out: std::collections::VecDeque<Value> = rows.iter().map(|row| {
        Value::record_dynamic(pg_row_to_lex_record(row))
    }).collect();
    ok(Value::List(out.into()))
}

/// Convert a Postgres row to a Lex record, mapping column types to Lex values.
pub(super) fn pg_row_to_lex_record(row: &postgres::Row) -> indexmap::IndexMap<String, Value> {
    use postgres::types::Type;
    let mut rec = indexmap::IndexMap::new();
    for (i, col) in row.columns().iter().enumerate() {
        let ty = col.type_();
        let val = if *ty == Type::INT2 || *ty == Type::INT4 || *ty == Type::INT8 {
            row.get::<_, Option<i64>>(i).map(Value::Int).unwrap_or(Value::Unit)
        } else if *ty == Type::FLOAT4 {
            row.get::<_, Option<f32>>(i).map(|f| Value::Float(f as f64)).unwrap_or(Value::Unit)
        } else if *ty == Type::FLOAT8 {
            row.get::<_, Option<f64>>(i).map(Value::Float).unwrap_or(Value::Unit)
        } else if *ty == Type::BOOL {
            row.get::<_, Option<bool>>(i).map(Value::Bool).unwrap_or(Value::Unit)
        } else if *ty == Type::BYTEA {
            row.get::<_, Option<Vec<u8>>>(i).map(Value::Bytes).unwrap_or(Value::Unit)
        } else {
            row.get::<_, Option<String>>(i).map(|s| Value::Str(s.into())).unwrap_or(Value::Unit)
        };
        rec.insert(col.name().to_string(), val);
    }
    rec
}

/// Extract a column value from a row record by name, returning `Option[X]`.
pub(super) fn sql_get_col<F>(args: &[Value], convert: F) -> Result<Value, String>
where
    F: Fn(&Value) -> Option<Value>,
{
    let row = args.first().ok_or("sql.get_*: missing row argument")?;
    let col = match args.get(1) {
        Some(Value::Str(s)) => s.as_str(),
        Some(other) => return Err(format!("sql.get_*: column name must be Str, got {other:?}")),
        None => return Err("sql.get_*: missing column name argument".into()),
    };
    let cell = match row {
        Value::Record { fields: rec, .. } => rec.get(col).cloned(),
        other => return Err(format!("sql.get_*: row must be a Record, got {other:?}")),
    };
    Ok(match cell.and_then(|v| convert(&v)) {
        Some(v) => Value::Variant { name: "Some".into(), args: vec![v] },
        None    => Value::Variant { name: "None".into(), args: vec![] },
    })
}

pub(super) fn sql_value_ref_to_lex(v: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null       => Value::Unit,
        ValueRef::Integer(n) => Value::Int(n),
        ValueRef::Real(f)    => Value::Float(f),
        ValueRef::Text(s)    => Value::Str(String::from_utf8_lossy(s).into_owned().into()),
        ValueRef::Blob(b)    => Value::Bytes(b.to_vec()),
    }
}

/// Process-wide registry of open `Db` handles. Same shape as the kv
/// and process registries: per-handle `Arc<Mutex<…>>` so dispatch
/// only briefly holds the global lock and ops on different
/// connections don't serialize. LRU-bounded at
/// [`MAX_SQL_HANDLES`] to avoid leaks from long-running programs
/// that open many short-lived databases.
pub(super) fn sql_registry() -> &'static Mutex<SqlRegistry> {
    static REGISTRY: OnceLock<Mutex<SqlRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(SqlRegistry::with_capacity(MAX_SQL_HANDLES)))
}

pub(super) const MAX_SQL_HANDLES: usize = 256;

// ── Streaming cursors (#379) ─────────────────────────────────────────
//
// `sql.query_iter[T]` opens a *server-side* cursor and returns an
// `Iter[T]` backed by a producer thread streaming rows through a
// bounded mpsc channel. The bytecode `iter.next` op dispatches on the
// `__IterCursor(handle)` variant tag and effect-calls
// `sql.cursor_next(handle)` to pull one row at a time.
//
// Producer-thread semantics: while the cursor is live, the producer
// holds the underlying SQL connection's `Arc<Mutex<SqlConn>>` lock.
// Other ops on the same Db handle block until the cursor is drained
// or evicted. This matches every server-side cursor protocol
// (sqlite's `sqlite3_step`, Postgres `DECLARE/FETCH`) — neither
// driver supports concurrent statements on a single connection.
//
// Channel capacity: 64 rows. Producer blocks at 64-row backlog,
// keeping resident memory bounded regardless of result-set size.
// Consumer disconnect (Receiver dropped) causes the next send to
// fail, the producer exits, drops the prepared statement, and
// releases the SqlConn lock — so closing a cursor is just "stop
// calling next and let the receiver go out of scope."

pub(super) const CURSOR_CHANNEL_CAPACITY: usize = 64;
pub(super) const MAX_CURSOR_HANDLES: usize = 256;

pub(super) type CursorReceiver = std::sync::mpsc::Receiver<Result<Value, String>>;

pub(crate) struct CursorRegistry {
    /// Each cursor's receiver lives behind its own Mutex so multiple
    /// `sql.cursor_next` calls on the same cursor serialize correctly.
    /// The outer `Arc` lets the global registry lock be released
    /// before blocking on `recv()`.
    pub(super) entries: indexmap::IndexMap<u64, Arc<Mutex<CursorReceiver>>>,
    pub(super) cap: usize,
}

impl CursorRegistry {
    pub(crate) fn with_capacity(cap: usize) -> Self {
        Self { entries: indexmap::IndexMap::new(), cap }
    }

    pub(crate) fn insert(&mut self, handle: u64, rx: CursorReceiver) {
        if self.entries.len() >= self.cap {
            self.entries.shift_remove_index(0);
        }
        self.entries.insert(handle, Arc::new(Mutex::new(rx)));
    }

    pub(crate) fn touch_get(&mut self, handle: u64) -> Option<Arc<Mutex<CursorReceiver>>> {
        let idx = self.entries.get_index_of(&handle)?;
        self.entries.move_index(idx, self.entries.len() - 1);
        self.entries.get(&handle).cloned()
    }

    pub(crate) fn remove(&mut self, handle: u64) {
        self.entries.shift_remove(&handle);
    }
}

pub(super) fn cursor_registry() -> &'static Mutex<CursorRegistry> {
    static REGISTRY: OnceLock<Mutex<CursorRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(CursorRegistry::with_capacity(MAX_CURSOR_HANDLES)))
}

pub(super) fn next_cursor_handle() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// SQLite cursor producer: locks the conn, prepares the statement,
/// walks rows, ships each to the consumer through `sender`. Exits on
/// row exhaustion, consumer disconnect, or first error. The lock is
/// released when the thread function returns (statement dropped first
/// to satisfy rusqlite's borrow).
pub(super) fn sqlite_cursor_producer(
    conn_arc: Arc<Mutex<SqlConn>>,
    stmt_str: String,
    params: Vec<SqlParamValue>,
    sender: std::sync::mpsc::SyncSender<Result<Value, String>>,
) {
    let mut conn_guard = match conn_arc.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let SqlConn::Sqlite(c) = &mut *conn_guard else {
        let _ = sender.send(Err("sqlite_cursor_producer called on non-sqlite conn".into()));
        return;
    };
    let mut stmt = match c.prepare(&stmt_str) {
        Ok(s) => s,
        Err(e) => { let _ = sender.send(Err(format!("prepare: {e}"))); return; }
    };
    let column_count = stmt.column_count();
    let column_names: Vec<String> = (0..column_count)
        .map(|i| stmt.column_name(i).unwrap_or("").to_string())
        .collect();
    let bound = sqlite_params(&params);
    let bind: Vec<&dyn rusqlite::ToSql> =
        bound.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
    let mut rows = match stmt.query(rusqlite::params_from_iter(bind.iter())) {
        Ok(r) => r,
        Err(e) => { let _ = sender.send(Err(format!("query: {e}"))); return; }
    };
    loop {
        match rows.next() {
            Ok(None) => break,
            Err(e) => {
                let _ = sender.send(Err(format!("row: {e}")));
                break;
            }
            Ok(Some(row)) => {
                let mut rec = indexmap::IndexMap::new();
                for (i, name) in column_names.iter().enumerate() {
                    let val = match row.get_ref(i) {
                        Ok(vr) => sql_value_ref_to_lex(vr),
                        Err(_) => Value::Unit,
                    };
                    rec.insert(name.clone(), val);
                }
                if sender.send(Ok(Value::record_dynamic(rec))).is_err() {
                    break;
                }
            }
        }
    }
}

/// Postgres cursor producer: opens a transaction + named cursor,
/// fetches rows in batches, ships each one through `sender`. Closes
/// the cursor and commits the transaction on exit.
pub(super) fn pg_cursor_producer(
    conn_arc: Arc<Mutex<SqlConn>>,
    stmt_str: String,
    params: Vec<SqlParamValue>,
    sender: std::sync::mpsc::SyncSender<Result<Value, String>>,
) {
    let mut conn_guard = match conn_arc.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let SqlConn::Postgres(c) = &mut *conn_guard else {
        let _ = sender.send(Err("pg_cursor_producer called on non-postgres conn".into()));
        return;
    };
    let pg = pg_param_refs(&params);
    let refs: Vec<&(dyn postgres::types::ToSql + Sync)> =
        pg.iter().map(|b| b.as_ref()).collect();
    let mut tx = match c.transaction() {
        Ok(t) => t,
        Err(e) => { let _ = sender.send(Err(format!("begin: {e}"))); return; }
    };
    // Use a uniquely-named cursor so concurrent producers on
    // distinct Db handles don't collide on the cursor namespace.
    let stmt_str = pg_rewrite_placeholders(&stmt_str);
    let cur_name = format!("__lex_cur_{}", next_cursor_handle());
    if let Err(e) = tx.execute(
        &format!("DECLARE \"{cur_name}\" NO SCROLL CURSOR FOR {stmt_str}"),
        &refs,
    ) {
        let _ = sender.send(Err(format!("declare: {e}")));
        return;
    }
    let fetch_sql = format!("FETCH 64 FROM \"{cur_name}\"");
    'outer: loop {
        let batch = match tx.query(&fetch_sql, &[]) {
            Ok(r) => r,
            Err(e) => { let _ = sender.send(Err(format!("fetch: {e}"))); break; }
        };
        if batch.is_empty() {
            break;
        }
        for row in batch.iter() {
            let rec = pg_row_to_lex_record(row);
            if sender.send(Ok(Value::record_dynamic(rec))).is_err() {
                break 'outer;
            }
        }
    }
    let _ = tx.execute(&format!("CLOSE \"{cur_name}\""), &[]);
    let _ = tx.commit();
}

/// Driver-neutral SQL parameter value shared between SQLite and Postgres paths.
#[derive(Debug, Clone)]
pub(super) enum SqlParamValue {
    Text(String),
    Integer(i64),
    Real(f64),
    Bool(bool),
    Null,
}

/// Abstraction over a SQLite connection or a Postgres client.
pub(crate) enum SqlConn {
    Sqlite(rusqlite::Connection),
    Postgres(postgres::Client),
}

pub(super) type SharedConn = Arc<Mutex<SqlConn>>;

pub(crate) struct SqlRegistry {
    pub(super) entries: indexmap::IndexMap<u64, SharedConn>,
    pub(super) cap: usize,
}

impl SqlRegistry {
    pub(crate) fn with_capacity(cap: usize) -> Self {
        Self { entries: indexmap::IndexMap::new(), cap }
    }

    pub(crate) fn insert(&mut self, handle: u64, conn: SqlConn) {
        if self.entries.len() >= self.cap {
            self.entries.shift_remove_index(0);
        }
        self.entries.insert(handle, Arc::new(Mutex::new(conn)));
    }

    /// Look up a handle, marking it MRU on hit. Returns a clone of
    /// the shared `Arc` so callers release the global registry
    /// lock before locking the per-handle mutex.
    pub(crate) fn touch_get(&mut self, handle: u64) -> Option<SharedConn> {
        let idx = self.entries.get_index_of(&handle)?;
        self.entries.move_index(idx, self.entries.len() - 1);
        self.entries.get(&handle).cloned()
    }

    pub(crate) fn remove(&mut self, handle: u64) {
        self.entries.shift_remove(&handle);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize { self.entries.len() }
}

pub(super) fn next_sql_handle() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

#[cfg(test)]
mod sql_registry_tests {
    use super::{SqlConn, SqlRegistry};

    fn fresh() -> SqlConn {
        SqlConn::Sqlite(rusqlite::Connection::open_in_memory().expect("open in-memory sqlite"))
    }

    #[test]
    fn insert_and_get_round_trip() {
        let mut r = SqlRegistry::with_capacity(4);
        r.insert(1, fresh());
        assert!(r.touch_get(1).is_some());
        assert!(r.touch_get(2).is_none());
    }

    #[test]
    fn cap_evicts_lru_on_overflow() {
        let mut r = SqlRegistry::with_capacity(2);
        r.insert(1, fresh());
        r.insert(2, fresh());
        let _ = r.touch_get(1);
        r.insert(3, fresh());
        assert!(r.touch_get(1).is_some(), "1 was MRU, should survive");
        assert!(r.touch_get(2).is_none(), "2 was LRU, should be evicted");
        assert!(r.touch_get(3).is_some(), "3 just inserted");
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn remove_drops_entry() {
        let mut r = SqlRegistry::with_capacity(4);
        r.insert(1, fresh());
        r.remove(1);
        assert!(r.touch_get(1).is_none());
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn many_inserts_stay_bounded_at_cap() {
        let cap = 8;
        let mut r = SqlRegistry::with_capacity(cap);
        for i in 0..(cap as u64 * 3) {
            r.insert(i, fresh());
            assert!(r.len() <= cap);
        }
        assert_eq!(r.len(), cap);
    }
}
