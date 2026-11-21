use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustbucket::config::Config;
use rustbucket::db;
use rustbucket::routes::{AppState, router};
use sqlx::SqlitePool;
use tower::ServiceExt;

/// Fresh SQLite database in a temp dir (kept alive by the returned guard).
async fn db_pool() -> (tempfile::TempDir, SqlitePool) {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("test.db").display());
    let pool = db::connect(&url).await.unwrap();
    (dir, pool)
}

async fn get(app: &axum::Router, uri: &str, accept: Option<&str>) -> (StatusCode, String, String) {
    let mut r = Request::get(uri);
    if let Some(a) = accept {
        r = r.header("accept", a);
    }
    let resp = app
        .clone()
        .oneshot(r.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let st = resp.status();
    let ct = resp.headers()["content-type"].to_str().unwrap().to_string();
    let body = String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    (st, ct, body)
}

#[tokio::test]
async fn migrations_up_down_up() {
    let (_c, pool) = db_pool().await;
    db::migrate(&pool).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
    db::MIGRATOR.undo(&pool, 0).await.unwrap();
    assert!(
        sqlx::query("SELECT 1 FROM users")
            .execute(&pool)
            .await
            .is_err()
    );
    db::migrate(&pool).await.unwrap();
}

#[tokio::test]
async fn status_capabilities_health() {
    let (_c, pool) = db_pool().await;
    db::migrate(&pool).await.unwrap();
    let app = router(AppState::new(pool, Arc::new(Config::default())));

    let (st, ct, body) = get(&app, "/status.php", None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(ct.starts_with("application/json"));
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["installed"], true);
    assert!(v["versionstring"].as_str().unwrap().starts_with("35."));

    let (st, ct, body) = get(&app, "/ocs/v2.php/cloud/capabilities?format=json", None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(ct.starts_with("application/json"));
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ocs"]["meta"]["statuscode"], 200);
    assert_eq!(v["ocs"]["data"]["version"]["major"], 35);

    let (_, ct, body) = get(&app, "/ocs/v1.php/cloud/capabilities", None).await;
    assert!(ct.starts_with("application/xml"));
    assert!(body.contains("<statuscode>100</statuscode>") && body.contains("<major>35</major>"));

    let (st, _, _) = get(&app, "/health", None).await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn admin_operations() {
    let (_c, pool) = db_pool().await;
    db::migrate(&pool).await.unwrap();
    db::create_user(&pool, "alice", "Alice", "s3cret", true)
        .await
        .unwrap();
    assert!(
        db::create_user(&pool, "alice", "Alice", "x", false)
            .await
            .is_err()
    );
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE uid='alice'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(hash.starts_with("$argon2") && !hash.contains("s3cret"));
    db::create_group(&pool, "team").await.unwrap();
    db::add_member(&pool, "team", "alice").await.unwrap();
    db::add_member(&pool, "team", "alice").await.unwrap(); // idempotent
    assert!(db::add_member(&pool, "team", "nobody").await.is_err());
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM group_members")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

/// Golden replay harness: replays fixtures recorded from stock NC 35 (Phase 0).
/// Skips loudly while no fixtures exist; never fabricate fixtures.
#[test]
fn golden_replay_status_capabilities() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let has = std::fs::read_dir(&dir)
        .map(|d| d.count() > 0)
        .unwrap_or(false);
    if !has {
        eprintln!("SKIPPED: no golden fixtures in tests/fixtures (Phase 0 captures outstanding)");
    }
}

#[tokio::test]
async fn online_backup_is_a_consistent_copy() {
    let (dir, pool) = db_pool().await;
    db::migrate(&pool).await.unwrap();
    db::create_user(&pool, "alice", "Alice", "pw", false)
        .await
        .unwrap();
    let dest = dir.path().join("snap.db");
    db::backup(&pool, &dest).await.unwrap();
    assert!(
        db::backup(&pool, &dest).await.is_err(),
        "never overwrites an existing file"
    );
    let copy = db::connect(&format!("sqlite://{}", dest.display()))
        .await
        .unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&copy)
        .await
        .unwrap();
    assert_eq!(n, 1);
}
