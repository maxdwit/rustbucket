//! Database access: pool, migrations and the small admin operations used by the CLI.

use argon2::password_hash::PasswordHasher;
use std::str::FromStr;
use std::time::Duration;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

/// Why an account operation was refused (the web UI and the CLI map these to messages).
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum UserError {
    #[error("invalid uid: use 1-64 of letters, digits, space, _ . @ -, not starting with a dot")]
    InvalidUid,
    #[error("invalid email address")]
    InvalidEmail,
    #[error("display name must be 1-64 characters")]
    InvalidName,
    #[error("empty password")]
    EmptyPassword,
    #[error("no such user: {0}")]
    NoSuchUser(String),
    #[error("refusing: this is the last enabled administrator")]
    LastAdmin,
    #[error("already exists")]
    Exists,
}

/// The reason, if `e` is one of ours or a unique-constraint violation of the database.
pub fn user_error(e: &anyhow::Error) -> Option<UserError> {
    if let Some(u) = e.downcast_ref::<UserError>() {
        return Some(u.clone());
    }
    match e.downcast_ref::<sqlx::Error>() {
        Some(sqlx::Error::Database(db)) if db.is_unique_violation() => Some(UserError::Exists),
        _ => None,
    }
}

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Open (and create if missing) the SQLite database: WAL, foreign keys on, busy timeout.
/// SQLite has a single writer: start write transactions with `pool.begin_with("BEGIN IMMEDIATE")`
/// so a read-then-write transaction cannot fail with SQLITE_BUSY on lock upgrade.
pub async fn connect(url: &str) -> Result<SqlitePool, sqlx::Error> {
    let opts = SqliteConnectOptions::from_str(url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(10));
    SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(opts)
        .await
}

pub async fn migrate(pool: &SqlitePool) -> Result<(), sqlx::migrate::MigrateError> {
    MIGRATOR.run(pool).await
}

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    argon2::Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("password hashing failed: {e}"))
}

/// The uid is also the user's folder name below the data dir, so it must be a safe path component.
pub fn valid_uid(uid: &str) -> bool {
    !uid.is_empty()
        && uid.chars().count() <= 64
        && !uid.starts_with('.')
        && !uid.starts_with(' ')
        && !uid.ends_with(' ')
        && uid
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '_' | '.' | '@' | '-'))
}

pub async fn create_user(
    pool: &SqlitePool,
    uid: &str,
    display_name: &str,
    password: &str,
    is_admin: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(valid_uid(uid), UserError::InvalidUid);
    anyhow::ensure!(!password.is_empty(), UserError::EmptyPassword);
    let hash = hash_password(password)?;
    sqlx::query(
        "INSERT INTO users (uid, display_name, password_hash, is_admin) VALUES ($1, $2, $3, $4)",
    )
    .bind(uid)
    .bind(display_name)
    .bind(hash)
    .bind(is_admin)
    .execute(pool)
    .await?;
    Ok(())
}

/// A user as shown in lists and admin forms.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserRow {
    pub id: i64,
    pub uid: String,
    pub display_name: String,
    pub email: String,
    pub is_admin: bool,
    pub enabled: bool,
}

pub async fn list_users(pool: &SqlitePool) -> Result<Vec<UserRow>, sqlx::Error> {
    sqlx::query_as("SELECT id, uid, display_name, COALESCE(email, '') AS email, is_admin, enabled FROM users ORDER BY uid")
        .fetch_all(pool)
        .await
}

pub async fn get_user(pool: &SqlitePool, uid: &str) -> Result<Option<UserRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, uid, display_name, COALESCE(email, '') AS email, is_admin, enabled FROM users WHERE uid = $1",
    )
    .bind(uid)
    .fetch_optional(pool)
    .await
}

/// Changes to an existing user; `None` leaves a field as it is.
#[derive(Debug, Default, Clone)]
pub struct UserEdit {
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub password: Option<String>,
    pub is_admin: Option<bool>,
    pub enabled: Option<bool>,
    /// With a new password every browser session of the user is ended - except this one (its hash).
    pub keep_session: Option<String>,
}

pub fn valid_email(e: &str) -> bool {
    e.is_empty()
        || (e.len() <= 254
            && !e.contains(char::is_whitespace)
            && e.split_once('@').is_some_and(|(l, d)| {
                !l.is_empty() && d.contains('.') && !d.starts_with('.') && !d.ends_with('.')
            }))
}

/// Edit a user. The last enabled administrator can be neither demoted nor disabled, so the
/// instance can never end up without anyone able to run the admin pages.
pub async fn update_user(pool: &SqlitePool, uid: &str, edit: &UserEdit) -> anyhow::Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let cur: Option<(i64, bool, bool)> =
        sqlx::query_as("SELECT id, is_admin, enabled FROM users WHERE uid = $1")
            .bind(uid)
            .fetch_optional(&mut *tx)
            .await?;
    let (id, was_admin, was_enabled) = cur.ok_or_else(|| UserError::NoSuchUser(uid.to_string()))?;
    if let Some(e) = &edit.email {
        anyhow::ensure!(valid_email(e), UserError::InvalidEmail);
    }
    if let Some(n) = &edit.display_name {
        anyhow::ensure!(
            !n.trim().is_empty() && n.chars().count() <= 64,
            UserError::InvalidName
        );
    }
    let becomes_admin = edit.is_admin.unwrap_or(was_admin);
    let becomes_enabled = edit.enabled.unwrap_or(was_enabled);
    if was_admin && was_enabled && !(becomes_admin && becomes_enabled) {
        let others: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM users WHERE is_admin = 1 AND enabled = 1 AND id <> $1",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        anyhow::ensure!(others > 0, UserError::LastAdmin);
    }
    if let Some(n) = &edit.display_name {
        sqlx::query("UPDATE users SET display_name = $1 WHERE id = $2")
            .bind(n.trim())
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(e) = &edit.email {
        sqlx::query("UPDATE users SET email = $1 WHERE id = $2")
            .bind(e)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(p) = &edit.password {
        anyhow::ensure!(!p.is_empty(), UserError::EmptyPassword);
        let hash = hash_password(p)?;
        sqlx::query("UPDATE users SET password_hash = $1 WHERE id = $2")
            .bind(hash)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        // a new password ends every browser session (somebody may hold a stolen cookie), except the one that
        // is changing it; app passwords of devices stay, like in Nextcloud
        sqlx::query("DELETE FROM web_sessions WHERE user_id = $1 AND id_hash <> COALESCE($2, '')")
            .bind(id)
            .bind(&edit.keep_session)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("UPDATE users SET is_admin = $1, enabled = $2 WHERE id = $3")
        .bind(becomes_admin)
        .bind(becomes_enabled)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    if !becomes_enabled {
        // a disabled user loses every way in at once
        sqlx::query("DELETE FROM web_sessions WHERE user_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM app_passwords WHERE user_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn create_group(pool: &SqlitePool, gid: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!gid.is_empty(), "invalid gid");
    sqlx::query("INSERT INTO groups (gid, display_name) VALUES ($1, $1)")
        .bind(gid)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn add_member(pool: &SqlitePool, gid: &str, uid: &str) -> anyhow::Result<()> {
    let n = sqlx::query(
        "INSERT INTO group_members (group_id, user_id) \
         SELECT g.id, u.id FROM groups g, users u WHERE g.gid = $1 AND u.uid = $2 \
         ON CONFLICT DO NOTHING",
    )
    .bind(gid)
    .bind(uid)
    .execute(pool)
    .await?
    .rows_affected();
    if n == 0 {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM group_members m JOIN groups g ON g.id = m.group_id \
             JOIN users u ON u.id = m.user_id WHERE g.gid = $1 AND u.uid = $2)",
        )
        .bind(gid)
        .bind(uid)
        .fetch_one(pool)
        .await?;
        anyhow::ensure!(exists, "group or user not found");
    }
    Ok(())
}

/// Consistent online snapshot of the database into a new file (`VACUUM INTO`); safe while the
/// server is running. Back up the DB first and the blobs afterwards: blobs are written before
/// the DB row and deleted after it, so that order never leaves the backup with dangling rows.
pub async fn backup(pool: &SqlitePool, dest: &std::path::Path) -> anyhow::Result<()> {
    anyhow::ensure!(!dest.exists(), "{} already exists", dest.display());
    sqlx::query("VACUUM INTO $1")
        .bind(dest.to_string_lossy().into_owned())
        .execute(pool)
        .await?;
    Ok(())
}
