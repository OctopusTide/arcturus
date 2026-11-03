//! The minimal SQLite binding and the store, against the real library the platform loads.

mod common;

use daemon::sqlite::{Connection, Value};
use daemon::store::Store;
use serde_json::json;

fn conn(dir: &tempfile::TempDir) -> Connection {
    Connection::open(&dir.path().join("t.db"), common::sqlite().as_ref()).unwrap()
}

#[test]
fn values_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let c = conn(&dir);
    assert!(c.library_version().starts_with("3."));
    c.execute_batch("CREATE TABLE t (i INTEGER, r REAL, s TEXT, n TEXT)").unwrap();
    let n = c.execute("INSERT INTO t VALUES (?, ?, ?, ?)", &[(-7i64).into(), 2.5.into(), "中文 ok".into(), Value::Null]);
    assert_eq!(n.unwrap(), 1);
    assert_eq!(c.last_insert_rowid(), 1);
    let row = c.query_row("SELECT i, r, s, n FROM t", &[]).unwrap().unwrap();
    assert_eq!(row.int(0).unwrap(), -7);
    assert_eq!(row.get(1), &Value::Real(2.5));
    assert_eq!(row.text(2).unwrap(), "中文 ok");
    assert_eq!(row.opt_text(3).unwrap(), None);
    assert!(c.query_row("SELECT i FROM t WHERE i = ?", &[1i64.into()]).unwrap().is_none());
}

#[test]
fn errors_carry_sqlite_messages_and_transactions_roll_back() {
    let dir = tempfile::tempdir().unwrap();
    let c = conn(&dir);
    let e = c.execute_batch("SELEC 1").unwrap_err();
    assert!(e.to_string().contains("syntax"), "{e}");
    c.execute_batch("CREATE TABLE t (x INTEGER)").unwrap();
    let r: Result<(), daemon::sqlite::Error> = c.transaction(|c| {
        c.execute("INSERT INTO t VALUES (1)", &[])?;
        c.execute("INSERT INTO nowhere VALUES (1)", &[])?;
        Ok(())
    });
    assert!(r.is_err());
    assert!(c.query("SELECT x FROM t", &[]).unwrap().is_empty());
}

#[test]
fn store_journal_keeps_the_first_original() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("s.db"), common::sqlite().as_ref()).unwrap();
    let a = store.journal_record("power", "lid", &json!("sleep"), Some(&json!("nothing")), None).unwrap();
    let b = store.journal_record("power", "lid", &json!("nothing"), Some(&json!("hibernate")), None).unwrap();
    assert_eq!(a.id, b.id);
    assert_eq!(b.original, json!("sleep"));
    assert_eq!(b.applied, Some(json!("hibernate")));
    assert!(store.journal_mark_restored("power", a.id, "module").unwrap());
    assert!(!store.journal_mark_restored("power", a.id, "module").unwrap());
    // A fresh change after restoring starts a new entry.
    let c = store.journal_record("power", "lid", &json!("sleep"), None, Some("again")).unwrap();
    assert_ne!(c.id, a.id);

    store.setting_set("power", "preset", &json!({"x": 1})).unwrap();
    assert_eq!(store.setting_get("power", "preset").unwrap(), Some(json!({"x": 1})));
    assert_eq!(store.setting_get("power", "other").unwrap(), None);
}
