//! Minimal SQLite binding that calls the C API through function pointers.
//!
//! Where the SQLite library comes from is the platform's business: the platform layer
//! implements [`SqliteLibrary`] (on Windows it loads the built-in `winsqlite3.dll` from
//! System32, so no C compiler is needed). Only what the store needs is bound: open, exec,
//! prepared statements with integer/real/text/null parameters, and row reading.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::Arc;

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_INTEGER: c_int = 1;
const SQLITE_FLOAT: c_int = 2;
const SQLITE_NULL: c_int = 5;
const SQLITE_OPEN_READWRITE: c_int = 0x0000_0002;
const SQLITE_OPEN_CREATE: c_int = 0x0000_0004;
const SQLITE_OPEN_FULLMUTEX: c_int = 0x0001_0000;
/// `SQLITE_TRANSIENT`: SQLite copies bound text before the bind call returns.
const SQLITE_TRANSIENT: isize = -1;
const BUSY_TIMEOUT_MS: c_int = 5000;

type Db = c_void;
type Stmt = c_void;

/// A loaded SQLite C library, provided by the platform layer.
pub trait SqliteLibrary: Send + Sync {
    /// Address of the exported C function `name`, or null when it is missing. The library
    /// must stay loaded for the life of the process.
    fn symbol(&self, name: &CStr) -> *const c_void;

    /// Short description for status output, e.g. `"winsqlite3.dll (System32)"`.
    fn describe(&self) -> String;
}

macro_rules! sqlite_api {
    ($( fn $name:ident ( $($arg:ty),* ) -> $ret:ty; )*) => {
        /// Function table resolved from the library. `extern "system"` is the C convention on
        /// every target except 32-bit Windows, where it matches winsqlite3's WINAPI.
        #[allow(non_snake_case)]
        struct Api { $( $name: unsafe extern "system" fn($($arg),*) -> $ret, )* }

        impl Api {
            fn load(lib: &dyn SqliteLibrary) -> std::result::Result<Api, String> {
                Ok(Api { $( $name: {
                    let name = CStr::from_bytes_with_nul(concat!(stringify!($name), "\0").as_bytes())
                        .expect("literal is NUL-terminated");
                    let sym = lib.symbol(name);
                    if sym.is_null() {
                        return Err(format!("{} has no {}", lib.describe(), stringify!($name)));
                    }
                    // SAFETY: the symbol is the SQLite C function of exactly this signature
                    // (sqlite3.h), and the library stays loaded (see SqliteLibrary::symbol).
                    unsafe {
                        std::mem::transmute::<*const c_void, unsafe extern "system" fn($($arg),*) -> $ret>(sym)
                    }
                }, )* })
            }
        }
    };
}

sqlite_api! {
    fn sqlite3_libversion() -> *const c_char;
    fn sqlite3_threadsafe() -> c_int;
    fn sqlite3_open_v2(*const c_char, *mut *mut Db, c_int, *const c_char) -> c_int;
    fn sqlite3_close_v2(*mut Db) -> c_int;
    fn sqlite3_errmsg(*mut Db) -> *const c_char;
    fn sqlite3_errstr(c_int) -> *const c_char;
    fn sqlite3_extended_errcode(*mut Db) -> c_int;
    fn sqlite3_busy_timeout(*mut Db, c_int) -> c_int;
    fn sqlite3_exec(*mut Db, *const c_char, *mut c_void, *mut c_void, *mut *mut c_char) -> c_int;
    fn sqlite3_free(*mut c_void) -> ();
    fn sqlite3_prepare_v2(*mut Db, *const c_char, c_int, *mut *mut Stmt, *mut *const c_char) -> c_int;
    fn sqlite3_step(*mut Stmt) -> c_int;
    fn sqlite3_finalize(*mut Stmt) -> c_int;
    fn sqlite3_bind_parameter_count(*mut Stmt) -> c_int;
    fn sqlite3_bind_null(*mut Stmt, c_int) -> c_int;
    fn sqlite3_bind_int64(*mut Stmt, c_int, i64) -> c_int;
    fn sqlite3_bind_double(*mut Stmt, c_int, f64) -> c_int;
    fn sqlite3_bind_text(*mut Stmt, c_int, *const c_char, c_int, isize) -> c_int;
    fn sqlite3_column_count(*mut Stmt) -> c_int;
    fn sqlite3_column_type(*mut Stmt, c_int) -> c_int;
    fn sqlite3_column_int64(*mut Stmt, c_int) -> i64;
    fn sqlite3_column_double(*mut Stmt, c_int) -> f64;
    fn sqlite3_column_text(*mut Stmt, c_int) -> *const u8;
    fn sqlite3_column_bytes(*mut Stmt, c_int) -> c_int;
    fn sqlite3_changes(*mut Db) -> c_int;
    fn sqlite3_last_insert_rowid(*mut Db) -> i64;
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}
impl From<u32> for Value {
    fn from(v: u32) -> Self {
        Value::Int(i64::from(v))
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Int(i64::from(v))
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Real(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_owned())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map_or(Value::Null, Into::into)
    }
}

#[derive(Debug, Clone)]
pub struct Error {
    /// SQLite (extended) result code, or -1 for errors raised by this wrapper.
    pub code: i32,
    pub message: String,
}

impl Error {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Error { code, message: message.into() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sqlite error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// One result row.
#[derive(Debug, Clone, PartialEq)]
pub struct Row(pub Vec<Value>);

impl Row {
    pub fn get(&self, i: usize) -> &Value {
        self.0.get(i).unwrap_or(&Value::Null)
    }

    pub fn opt_int(&self, i: usize) -> Result<Option<i64>> {
        match self.get(i) {
            Value::Null => Ok(None),
            Value::Int(v) => Ok(Some(*v)),
            other => Err(Error::new(-1, format!("column {i}: expected integer, got {other:?}"))),
        }
    }

    pub fn int(&self, i: usize) -> Result<i64> {
        self.opt_int(i)?.ok_or_else(|| Error::new(-1, format!("column {i}: unexpected NULL")))
    }

    pub fn opt_text(&self, i: usize) -> Result<Option<String>> {
        match self.get(i) {
            Value::Null => Ok(None),
            Value::Text(v) => Ok(Some(v.clone())),
            other => Err(Error::new(-1, format!("column {i}: expected text, got {other:?}"))),
        }
    }

    pub fn text(&self, i: usize) -> Result<String> {
        self.opt_text(i)?.ok_or_else(|| Error::new(-1, format!("column {i}: unexpected NULL")))
    }
}

pub struct Connection {
    db: *mut Db,
    api: Arc<Api>,
}

// SAFETY: the connection is opened with SQLITE_OPEN_FULLMUTEX (serialized mode), in which
// SQLite allows use from any thread; `open` refuses a non-threadsafe library build.
unsafe impl Send for Connection {}

impl Connection {
    /// Opens (creating if needed) the database file at `path`.
    pub fn open(path: &Path, lib: &dyn SqliteLibrary) -> Result<Connection> {
        let api = Arc::new(Api::load(lib).map_err(|m| Error::new(-1, m))?);
        // SAFETY: no arguments.
        if unsafe { (api.sqlite3_threadsafe)() } == 0 {
            return Err(Error::new(-1, format!("{} is built without thread safety", lib.describe())));
        }
        let path = path
            .to_str()
            .ok_or_else(|| Error::new(-1, format!("database path is not valid Unicode: {}", path.display())))?;
        let c_path = CString::new(path).map_err(|_| Error::new(-1, "database path contains NUL"))?;
        let mut db = null_mut();
        let flags = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_OPEN_FULLMUTEX;
        // SAFETY: valid NUL-terminated path and out-pointer.
        let rc = unsafe { (api.sqlite3_open_v2)(c_path.as_ptr(), &mut db, flags, null()) };
        // Wrap first so the handle is closed even when opening failed.
        let conn = Connection { db, api };
        if rc != SQLITE_OK {
            return Err(conn.error(rc));
        }
        // SAFETY: `db` is an open connection.
        unsafe { (conn.api.sqlite3_busy_timeout)(db, BUSY_TIMEOUT_MS) };
        Ok(conn)
    }

    /// Version string of the loaded SQLite library.
    pub fn library_version(&self) -> String {
        // SAFETY: returns a static NUL-terminated string.
        unsafe { CStr::from_ptr((self.api.sqlite3_libversion)()) }.to_string_lossy().into_owned()
    }

    fn error(&self, rc: c_int) -> Error {
        // SAFETY: errmsg/errstr return NUL-terminated strings owned by SQLite.
        unsafe {
            if self.db.is_null() {
                let msg = CStr::from_ptr((self.api.sqlite3_errstr)(rc)).to_string_lossy().into_owned();
                Error::new(rc, msg)
            } else {
                let msg = CStr::from_ptr((self.api.sqlite3_errmsg)(self.db)).to_string_lossy().into_owned();
                Error::new((self.api.sqlite3_extended_errcode)(self.db), msg)
            }
        }
    }

    /// Runs one or more `;`-separated statements without parameters.
    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        let c_sql = CString::new(sql).map_err(|_| Error::new(-1, "SQL contains NUL"))?;
        let mut errmsg: *mut c_char = null_mut();
        // SAFETY: valid connection, NUL-terminated SQL, no callback.
        let rc = unsafe { (self.api.sqlite3_exec)(self.db, c_sql.as_ptr(), null_mut(), null_mut(), &mut errmsg) };
        if rc == SQLITE_OK {
            return Ok(());
        }
        if errmsg.is_null() {
            return Err(self.error(rc));
        }
        // SAFETY: SQLite allocated `errmsg`; it is copied, then freed once.
        let msg = unsafe { CStr::from_ptr(errmsg) }.to_string_lossy().into_owned();
        unsafe { (self.api.sqlite3_free)(errmsg.cast()) };
        Err(Error::new(rc, msg))
    }

    /// Runs one statement and returns the number of changed rows.
    pub fn execute(&self, sql: &str, params: &[Value]) -> Result<usize> {
        let stmt = self.prepare(sql, params)?;
        while stmt.step()? {}
        // SAFETY: valid connection.
        Ok(unsafe { (self.api.sqlite3_changes)(self.db) }.max(0) as usize)
    }

    /// Runs one query and collects every row.
    pub fn query(&self, sql: &str, params: &[Value]) -> Result<Vec<Row>> {
        let stmt = self.prepare(sql, params)?;
        let mut rows = Vec::new();
        while stmt.step()? {
            rows.push(stmt.row());
        }
        Ok(rows)
    }

    /// Runs one query and returns its first row, if any.
    pub fn query_row(&self, sql: &str, params: &[Value]) -> Result<Option<Row>> {
        let stmt = self.prepare(sql, params)?;
        Ok(if stmt.step()? { Some(stmt.row()) } else { None })
    }

    pub fn last_insert_rowid(&self) -> i64 {
        // SAFETY: valid connection.
        unsafe { (self.api.sqlite3_last_insert_rowid)(self.db) }
    }

    /// Runs `f` inside `BEGIN IMMEDIATE` … `COMMIT`; rolls back when `f` fails.
    pub fn transaction<T, E: From<Error>>(
        &self,
        f: impl FnOnce(&Connection) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        self.execute_batch("BEGIN IMMEDIATE")?;
        match f(self) {
            Ok(v) => {
                if let Err(e) = self.execute_batch("COMMIT") {
                    let _ = self.execute_batch("ROLLBACK");
                    return Err(e.into());
                }
                Ok(v)
            }
            Err(e) => {
                let _ = self.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    fn prepare(&self, sql: &str, params: &[Value]) -> Result<Statement<'_>> {
        let len = c_int::try_from(sql.len()).map_err(|_| Error::new(-1, "SQL too long"))?;
        let mut raw = null_mut();
        // SAFETY: `sql` is valid for `len` bytes; SQLite does not need a terminator then.
        let rc = unsafe { (self.api.sqlite3_prepare_v2)(self.db, sql.as_ptr().cast(), len, &mut raw, null_mut()) };
        if rc != SQLITE_OK {
            return Err(self.error(rc));
        }
        if raw.is_null() {
            return Err(Error::new(-1, "SQL contains no statement"));
        }
        let stmt = Statement { conn: self, raw };
        stmt.bind(params)?;
        Ok(stmt)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if !self.db.is_null() {
            // SAFETY: closed exactly once; statements never outlive the connection.
            unsafe { (self.api.sqlite3_close_v2)(self.db) };
        }
    }
}

struct Statement<'c> {
    conn: &'c Connection,
    raw: *mut Stmt,
}

impl Statement<'_> {
    fn bind(&self, params: &[Value]) -> Result<()> {
        let api = &self.conn.api;
        // SAFETY: valid statement.
        let expected = unsafe { (api.sqlite3_bind_parameter_count)(self.raw) } as usize;
        if expected != params.len() {
            return Err(Error::new(-1, format!("statement takes {expected} parameters, got {}", params.len())));
        }
        for (i, p) in params.iter().enumerate() {
            let idx = (i + 1) as c_int;
            // SAFETY: valid statement and 1-based index within range; text is copied
            // (SQLITE_TRANSIENT) before the call returns.
            let rc = unsafe {
                match p {
                    Value::Null => (api.sqlite3_bind_null)(self.raw, idx),
                    Value::Int(v) => (api.sqlite3_bind_int64)(self.raw, idx, *v),
                    Value::Real(v) => (api.sqlite3_bind_double)(self.raw, idx, *v),
                    Value::Text(s) => {
                        let n = c_int::try_from(s.len()).map_err(|_| Error::new(-1, "text parameter too long"))?;
                        (api.sqlite3_bind_text)(self.raw, idx, s.as_ptr().cast(), n, SQLITE_TRANSIENT)
                    }
                }
            };
            if rc != SQLITE_OK {
                return Err(self.conn.error(rc));
            }
        }
        Ok(())
    }

    /// Advances the statement; `true` means a row is available.
    fn step(&self) -> Result<bool> {
        // SAFETY: valid statement.
        match unsafe { (self.conn.api.sqlite3_step)(self.raw) } {
            SQLITE_ROW => Ok(true),
            SQLITE_DONE => Ok(false),
            rc => Err(self.conn.error(rc)),
        }
    }

    fn row(&self) -> Row {
        let api = &self.conn.api;
        // SAFETY: called only right after `step` returned a row; text is read before its
        // byte count, as sqlite3.h recommends, and copied out immediately.
        unsafe {
            let n = (api.sqlite3_column_count)(self.raw);
            Row(
                (0..n)
                    .map(|i| match (api.sqlite3_column_type)(self.raw, i) {
                        SQLITE_NULL => Value::Null,
                        SQLITE_INTEGER => Value::Int((api.sqlite3_column_int64)(self.raw, i)),
                        SQLITE_FLOAT => Value::Real((api.sqlite3_column_double)(self.raw, i)),
                        _ => {
                            let p = (api.sqlite3_column_text)(self.raw, i);
                            let len = (api.sqlite3_column_bytes)(self.raw, i).max(0) as usize;
                            if p.is_null() {
                                Value::Text(String::new())
                            } else {
                                Value::Text(String::from_utf8_lossy(std::slice::from_raw_parts(p, len)).into_owned())
                            }
                        }
                    })
                    .collect(),
            )
        }
    }
}

impl Drop for Statement<'_> {
    fn drop(&mut self) {
        // SAFETY: finalized exactly once.
        unsafe { (self.conn.api.sqlite3_finalize)(self.raw) };
    }
}
