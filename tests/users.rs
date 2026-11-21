//! Account editing: the rules the CLI and the admin UI share.

use rustbucket::db::{self, UserEdit};

async fn pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let dir = tempfile::tempdir().unwrap();
    let pool = db::connect(&format!("sqlite://{}", dir.path().join("t.db").display()))
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    (dir, pool)
}

#[tokio::test]
async fn edit_fields_and_validate() {
    let (_d, p) = pool().await;
    db::create_user(&p, "carol", "Carol", "old-pw", false)
        .await
        .unwrap();
    db::update_user(
        &p,
        "carol",
        &UserEdit {
            display_name: Some("Carol Example".into()),
            email: Some("carol@example.org".into()),
            password: Some("new-pw".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let u = db::get_user(&p, "carol").await.unwrap().unwrap();
    assert_eq!(
        (
            u.display_name.as_str(),
            u.email.as_str(),
            u.is_admin,
            u.enabled
        ),
        ("Carol Example", "carol@example.org", false, true)
    );
    // the new password works, the old one does not
    assert!(
        rustbucket::auth::verify_user_password(&p, "carol", "new-pw")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        rustbucket::auth::verify_user_password(&p, "carol", "old-pw")
            .await
            .unwrap()
            .is_none()
    );
    // clearing the email is allowed, nonsense is not; unknown users and empty passwords are refused
    db::update_user(
        &p,
        "carol",
        &UserEdit {
            email: Some(String::new()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    for bad in [
        "no-at-sign",
        "a@b",
        "a b@c.example.org",
        "@c.example.org",
        "a@.example.org",
    ] {
        assert!(
            db::update_user(
                &p,
                "carol",
                &UserEdit {
                    email: Some(bad.into()),
                    ..Default::default()
                }
            )
            .await
            .is_err(),
            "{bad}"
        );
    }
    assert!(
        db::update_user(&p, "nobody", &UserEdit::default())
            .await
            .is_err()
    );
    assert!(
        db::update_user(
            &p,
            "carol",
            &UserEdit {
                password: Some(String::new()),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
    assert!(
        db::update_user(
            &p,
            "carol",
            &UserEdit {
                display_name: Some("  ".into()),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn the_last_enabled_admin_is_protected() {
    let (_d, p) = pool().await;
    db::create_user(&p, "root", "Root", "pw", true)
        .await
        .unwrap();
    db::create_user(&p, "other", "Other", "pw", false)
        .await
        .unwrap();
    for e in [
        UserEdit {
            is_admin: Some(false),
            ..Default::default()
        },
        UserEdit {
            enabled: Some(false),
            ..Default::default()
        },
    ] {
        let err = db::update_user(&p, "root", &e)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("last enabled administrator"), "{err}");
    }
    // a second admin makes it possible; then the first of them is the protected one
    db::update_user(
        &p,
        "other",
        &UserEdit {
            is_admin: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    db::update_user(
        &p,
        "root",
        &UserEdit {
            is_admin: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        db::update_user(
            &p,
            "other",
            &UserEdit {
                enabled: Some(false),
                ..Default::default()
            }
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn disabling_removes_every_way_in() {
    let (_d, p) = pool().await;
    db::create_user(&p, "root", "Root", "pw", true)
        .await
        .unwrap();
    db::create_user(&p, "kid", "Kid", "pw", false)
        .await
        .unwrap();
    let id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE uid = 'kid'")
        .fetch_one(&p)
        .await
        .unwrap();
    let token = rustbucket::auth::create_app_password(&p, id, "phone")
        .await
        .unwrap();
    sqlx::query("INSERT INTO web_sessions (id_hash, user_id, created_at, expires_at) VALUES ('h', $1, 0, 9999999999)").bind(id).execute(&p).await.unwrap();
    db::update_user(
        &p,
        "kid",
        &UserEdit {
            enabled: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let left: i64 = sqlx::query_scalar("SELECT (SELECT COUNT(*) FROM web_sessions WHERE user_id = $1) + (SELECT COUNT(*) FROM app_passwords WHERE user_id = $1)").bind(id).fetch_one(&p).await.unwrap();
    assert_eq!(left, 0);
    assert!(
        rustbucket::auth::verify_user_password(&p, "kid", "pw")
            .await
            .unwrap()
            .is_none()
    );
    let _ = token;
    // enabling again restores the password login (sessions/app passwords are gone for good)
    db::update_user(
        &p,
        "kid",
        &UserEdit {
            enabled: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        rustbucket::auth::verify_user_password(&p, "kid", "pw")
            .await
            .unwrap()
            .is_some()
    );
}
