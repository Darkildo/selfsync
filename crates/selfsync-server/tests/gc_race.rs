//! Гонка сборки мусора с записью: tombstone, который ожил и снова удалился между
//! построением плана и его выполнением (или между выборкой и стиранием в sweeper'е),
//! не стирается раньше окна хранения, а `purged_seq` не отстаёт от стёртого.

use selfsync_core::hash::Hash;
use selfsync_core::path::{VaultPath, canonical_encode};
use selfsync_proto::v1 as pb;
use selfsync_server::blobs::BlobStore;
use selfsync_server::db::vault::{
    DAY_MS, OpsCtx, apply_ops, deleted, history, migrate_vault, sweep, vault_state,
};
use selfsync_server::gc::{self, GcOptions};

fn p(s: &str) -> pb::Path {
    VaultPath::parse(s).unwrap().to_proto()
}

fn put(path: &str, base: u64, h: u8) -> pb::Op {
    pb::Op {
        kind: Some(pb::op::Kind::Put(pb::Put {
            path: Some(p(path)),
            base_rev: base,
            hash: vec![h; 32],
            size: 10,
            mtime: 1,
        })),
    }
}

fn del(path: &str, base: u64) -> pb::Op {
    pb::Op {
        kind: Some(pb::op::Kind::Delete(pb::Delete {
            path: Some(p(path)),
            base_rev: base,
        })),
    }
}

fn run(c: &mut rusqlite::Connection, now: i64, ops: Vec<pb::Op>) {
    let all = |_: &Hash| true;
    let ctx = OpsCtx {
        device: 1,
        now,
        max_blob_size: 1 << 20,
        blob_exists: &all,
    };
    for r in apply_ops(c, &ctx, &ops).unwrap().results {
        assert!(
            matches!(r.result, Some(pb::op_result::Result::Applied(_))),
            "{r:?}"
        );
    }
}

/// `note.md`: создан (seq 1) и удалён (seq 2) в момент 1000; `now` — когда окно
/// хранения (30 дней) этого tombstone'а уже истекло.
fn setup(dir: &std::path::Path) -> (rusqlite::Connection, BlobStore, i64) {
    let store = BlobStore::new(dir);
    let mut c = selfsync_server::db::open(&dir.join("meta.db"), Default::default()).unwrap();
    migrate_vault(&mut c).unwrap();
    run(&mut c, 1000, vec![put("note.md", 0, 1)]);
    run(&mut c, 1000, vec![del("note.md", 1)]);
    (c, store, 1000 + 31 * DAY_MS)
}

/// Восстановить и снова удалить: свежий tombstone (seq 4) с `deleted_at = now`.
fn restore_and_delete(c: &mut rusqlite::Connection, now: i64) {
    run(c, now, vec![put("note.md", 2, 1)]);
    run(c, now, vec![del("note.md", 3)]);
}

#[test]
fn stale_gc_plan_keeps_fresh_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, store, now) = setup(dir.path());
    let key = canonical_encode(&p("note.md"));
    let plan = gc::plan(&c, &store, GcOptions::default(), now).unwrap();
    assert_eq!(plan.tombstones, vec![(key.clone(), 2)]);

    restore_and_delete(&mut c, now);
    let r = gc::execute(&mut c, &store, &plan).unwrap();

    assert_eq!(r.tombstones, 0, "свежий tombstone не по плану");
    assert_eq!(history(&c, &key).unwrap().len(), 4, "история цела");
    assert_eq!(deleted(&c, now).unwrap().len(), 1, "файл в корзине");
    assert_eq!(vault_state(&c).unwrap().purged_seq, 0);
}

#[test]
fn gc_plan_purges_unchanged_tombstone() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, store, now) = setup(dir.path());
    let key = canonical_encode(&p("note.md"));
    let plan = gc::plan(&c, &store, GcOptions::default(), now).unwrap();
    let r = gc::execute(&mut c, &store, &plan).unwrap();
    assert_eq!(r.tombstones, 1);
    assert!(history(&c, &key).unwrap().is_empty());
    assert_eq!(vault_state(&c).unwrap().purged_seq, 2);
}

#[test]
fn sweep_keeps_fresh_tombstone_after_restore() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, _store, now) = setup(dir.path());
    let key = canonical_encode(&p("note.md"));
    restore_and_delete(&mut c, now);
    let (n, _) = sweep(&mut c, now).unwrap();
    assert_eq!(n, 0);
    assert_eq!(history(&c, &key).unwrap().len(), 4);
    assert_eq!(vault_state(&c).unwrap().purged_seq, 0);
    // и стирает его, когда истечёт уже его окно
    let (n, _) = sweep(&mut c, now + 31 * DAY_MS).unwrap();
    assert_eq!(n, 1);
    assert_eq!(vault_state(&c).unwrap().purged_seq, 4);
}
