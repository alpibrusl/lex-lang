//! #930 phase 2b-1: a package head carries its committed `lex.lock` as
//! content-addressed store state, so the write-time gate can resolve the
//! head's pinned dependencies.

use lex_store::Store;

fn fresh() -> (Store, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    (Store::open(tmp.path()).unwrap(), tmp)
}

const LOCK: &str = "version = 1\n\n[[package]]\nname = \"lex-nt\"\nregistry = \"https://vcs.lexlang.org\"\nconstraint = \"^0.1\"\nversion = \"0.1.3\"\nhead_op = \"op_abc123\"\n";

#[test]
fn committed_lock_round_trips_keyed_by_head() {
    let (store, _tmp) = fresh();
    store.set_committed_lock("op_head1", LOCK).expect("set");
    assert_eq!(store.committed_lock("op_head1").expect("get").as_deref(), Some(LOCK));
}

#[test]
fn absent_lock_is_none_not_error() {
    let (store, _tmp) = fresh();
    assert_eq!(store.committed_lock("op_never_locked").expect("get"), None);
}

#[test]
fn locks_are_per_head() {
    let (store, _tmp) = fresh();
    store.set_committed_lock("op_head1", LOCK).expect("set 1");
    let other = "version = 1\n";
    store.set_committed_lock("op_head2", other).expect("set 2");
    assert_eq!(store.committed_lock("op_head1").unwrap().as_deref(), Some(LOCK));
    assert_eq!(store.committed_lock("op_head2").unwrap().as_deref(), Some(other));
}

#[test]
fn set_is_idempotent() {
    let (store, _tmp) = fresh();
    store.set_committed_lock("op_head1", LOCK).expect("set");
    store.set_committed_lock("op_head1", LOCK).expect("set again");
    assert_eq!(store.committed_lock("op_head1").unwrap().as_deref(), Some(LOCK));
}
