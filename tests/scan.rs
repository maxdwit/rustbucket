//! `scan` reconciles the index with the disk: crash windows, imports, guards.

use std::path::PathBuf;
use std::sync::Arc;

use rustbucket::config::Config;
use rustbucket::db;
use rustbucket::routes::AppState;
use rustbucket::storage::{FsError, Owner, PutOpts, ScanReport, Store};

struct Env {
    _dir: tempfile::TempDir,
    store: Store,
    owner: Owner,
    data: PathBuf,
}

async fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let pool = db::connect(&format!("sqlite://{}", dir.path().join("t.db").display()))
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    db::create_user(&pool, "alice", "Alice", "pw", false)
        .await
        .unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE uid='alice'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let cfg = Config {
        data_dir: data.clone(),
        ..Config::default()
    };
    let store = AppState::new(pool, Arc::new(cfg)).store;
    store.check_data_dir().await.unwrap();
    Env {
        _dir: dir,
        store,
        owner: Owner {
            id,
            uid: "alice".into(),
        },
        data,
    }
}

async fn put(e: &Env, path: &str, body: &[u8]) {
    let (tmp, _f) = e.store.new_tmp(&e.owner).await.unwrap();
    std::fs::write(&tmp, body).unwrap();
    let opts = PutOpts {
        mtime: 1_700_000_000,
        mimetype: "text/plain".into(),
        checksum: None,
        if_match: None,
        if_none_match_star: false,
    };
    e.store
        .put(&e.owner, path, &tmp, body.len() as i64, opts)
        .await
        .unwrap();
}

async fn root(e: &Env) -> (String, i64) {
    let n = e.store.get(&e.owner, "").await.unwrap().unwrap();
    (n.etag, n.size)
}

fn files(e: &Env) -> PathBuf {
    e.data.join("alice/files")
}

#[tokio::test]
async fn clean_tree_scans_as_no_change() {
    let e = env().await;
    e.store.mkcol(&e.owner, "d").await.unwrap();
    put(&e, "d/a.txt", b"abc").await;
    put(&e, "b.txt", b"12").await;
    let before = root(&e).await;
    let r = e.store.scan(&e.owner, true, false).await.unwrap();
    assert_eq!(
        r,
        ScanReport::default(),
        "everything the server wrote is consistent with the disk"
    );
    assert_eq!(root(&e).await, before, "dry run changes nothing");
}

#[tokio::test]
async fn import_modify_and_remove_out_of_band() {
    let e = env().await;
    put(&e, "keep.txt", b"keep").await;
    put(&e, "edit.txt", b"old").await;
    put(&e, "del.txt", b"bye").await;
    let (etag0, size0) = root(&e).await;
    let edit_id = e
        .store
        .get(&e.owner, "edit.txt")
        .await
        .unwrap()
        .unwrap()
        .fileid;
    // out-of-band changes: new folder + file, in-place edit, delete
    std::fs::create_dir_all(files(&e).join("imported/sub")).unwrap();
    std::fs::write(files(&e).join("imported/sub/new.md"), b"hello import").unwrap();
    std::fs::write(files(&e).join("edit.txt"), b"much longer content").unwrap();
    std::fs::remove_file(files(&e).join("del.txt")).unwrap();

    let dry = e.store.scan(&e.owner, true, false).await.unwrap();
    assert_eq!(
        dry,
        ScanReport {
            added: 3,
            updated: 1,
            removed: 1
        }
    ); // imported, imported/sub, new.md
    assert_eq!(root(&e).await, (etag0.clone(), size0), "dry run is a no-op");

    let r = e.store.scan(&e.owner, false, false).await.unwrap();
    assert_eq!(
        r,
        ScanReport {
            added: 3,
            updated: 1,
            removed: 1
        }
    );
    let (etag1, size1) = root(&e).await;
    assert_ne!(etag1, etag0, "root etag changed");
    assert_eq!(size1, 4 + 19 + 12); // keep + edit + new.md
    assert_eq!(
        e.store
            .get(&e.owner, "edit.txt")
            .await
            .unwrap()
            .unwrap()
            .fileid,
        edit_id,
        "fileid survives an in-place edit"
    );
    assert!(e.store.get(&e.owner, "del.txt").await.unwrap().is_none());
    let sub = e
        .store
        .get(&e.owner, "imported/sub")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sub.size, 12);
    assert_eq!(
        e.store
            .get(&e.owner, "imported/sub/new.md")
            .await
            .unwrap()
            .unwrap()
            .mimetype,
        "text/markdown"
    );
    // idempotent
    assert_eq!(
        e.store.scan(&e.owner, false, false).await.unwrap(),
        ScanReport::default()
    );
}

#[tokio::test]
async fn crash_window_rename_becomes_delete_plus_add_without_data_loss() {
    let e = env().await;
    put(&e, "a.txt", b"data").await;
    put(&e, "other.txt", b"o").await;
    put(&e, "more.txt", b"m").await;
    // simulate: rename on disk happened, index commit did not
    std::fs::rename(files(&e).join("a.txt"), files(&e).join("b.txt")).unwrap();
    let r = e.store.scan(&e.owner, false, false).await.unwrap();
    assert_eq!(
        r,
        ScanReport {
            added: 1,
            updated: 0,
            removed: 1
        }
    );
    assert_eq!(std::fs::read(files(&e).join("b.txt")).unwrap(), b"data");
    assert!(e.store.get(&e.owner, "b.txt").await.unwrap().is_some());
}

#[tokio::test]
async fn scan_refuses_to_wipe_the_index() {
    let e = env().await;
    put(&e, "a.txt", b"a").await;
    put(&e, "b.txt", b"b").await;
    put(&e, "c.txt", b"c").await;
    // e.g. wrong/empty mount: everything "disappears"
    std::fs::remove_dir_all(files(&e)).unwrap();
    std::fs::create_dir_all(files(&e)).unwrap();
    let err = e.store.scan(&e.owner, false, false).await.unwrap_err();
    assert!(matches!(err, FsError::DataFolder(_)), "{err:?}");
    assert!(
        e.store.get(&e.owner, "a.txt").await.unwrap().is_some(),
        "index untouched after refusal"
    );
    // explicit override
    let r = e.store.scan(&e.owner, false, true).await.unwrap();
    assert_eq!(r.removed, 3);
}

#[tokio::test]
async fn missing_files_folder_or_marker_is_an_error_not_an_empty_server() {
    let e = env().await;
    put(&e, "a.txt", b"a").await;
    // user folder vanished (unmounted disk): requests fail instead of serving an empty tree
    std::fs::remove_dir_all(e.data.join("alice")).unwrap();
    assert!(matches!(
        e.store.get(&e.owner, "a.txt").await,
        Err(FsError::DataFolder(_))
    ));
    assert!(matches!(
        e.store.scan(&e.owner, false, true).await,
        Err(FsError::DataFolder(_))
    ));
    // marker gone while the index has rows: startup check refuses
    std::fs::remove_file(e.data.join(".rustbucket-data")).unwrap();
    assert!(matches!(
        e.store.check_data_dir().await,
        Err(FsError::DataFolder(_))
    ));
}

#[tokio::test]
async fn scan_skips_symlinks_and_non_nfc_names() {
    let e = env().await;
    put(&e, "real.txt", b"r").await;
    std::os::unix::fs::symlink("/etc/passwd", files(&e).join("link")).unwrap();
    std::fs::write(files(&e).join("a\u{0308}.txt"), b"nfd").unwrap(); // decomposed ä
    let r = e.store.scan(&e.owner, false, false).await.unwrap();
    assert_eq!(
        r,
        ScanReport::default(),
        "neither the symlink nor the NFD name was indexed"
    );
}

#[test]
fn uid_must_be_a_safe_path_component() {
    for bad in [
        "",
        ".",
        "..",
        ".hidden",
        "a/b",
        "a\\b",
        " lead",
        "trail ",
        "x\0y",
        &"u".repeat(65),
    ] {
        assert!(!db::valid_uid(bad), "{bad:?}");
    }
    for good in [
        "alice",
        "Alice Example",
        "a.b-c_d",
        "user@example.com",
        "müller",
    ] {
        assert!(db::valid_uid(good), "{good:?}");
    }
}
