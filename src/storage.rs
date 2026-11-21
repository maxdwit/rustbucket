//! File storage with real names on disk: `<data_dir>/<uid>/files/<path>` (like Nextcloud).
//!
//! The filesystem holds the content and the names; the `file_cache` table is an index that adds
//! what a filesystem cannot give clients: stable numeric fileids, ETags that propagate up the
//! tree, recursive sizes. Every mutation runs in one `BEGIN IMMEDIATE` transaction that does the
//! filesystem change *and* the index update, so mutations and `scan` are mutually exclusive.
//!
//! Crash behaviour: a crash between the filesystem change and the commit leaves the index stale.
//! `scan` repairs that from the disk. Bytes are never lost, but fileid continuity can be: e.g. a
//! rename that was not committed shows up as "deleted + new file" (one extra re-sync).
//!
//! Safety rails against wiping the index (which clients see as server-side deletions):
//! `<data_dir>/.rustbucket-data` marker, per-user `files/` must exist once the user has rows,
//! `scan` refuses to remove (almost) everything without `--force`, and there is no automatic scan.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use sqlx::{SqliteConnection, SqlitePool};
use tokio::fs::File;
use unicode_normalization::UnicodeNormalization;

use crate::auth::{random_token, unix_now};
use crate::paths;

pub const DATA_MARKER: &str = ".rustbucket-data";

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Node {
    pub fileid: i64,
    pub owner_id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: i64,
    pub mtime: i64,
    pub etag: String,
    pub mimetype: String,
    pub checksum: Option<String>,
}

/// `SELECT <all node columns> FROM file_cache <tail>` as a compile-time literal.
macro_rules! select_node {
    ($tail:literal) => {
        concat!(
            "SELECT fileid, owner_id, parent_id, name, path, is_dir, size, mtime, etag, mimetype, checksum FROM file_cache ",
            $tail
        )
    };
}

/// The user a request acts for; `uid` is the folder name below the data dir.
#[derive(Debug, Clone)]
pub struct Owner {
    pub id: i64,
    pub uid: String,
}

#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("not found")]
    NotFound,
    #[error("parent not found")]
    ParentNotFound,
    #[error("parent is not a collection")]
    NotCollection,
    #[error("already exists")]
    Exists,
    #[error("is a collection")]
    IsCollection,
    #[error("precondition failed")]
    PreconditionFailed,
    #[error("forbidden: {0}")]
    Forbidden(&'static str),
    #[error("data folder problem: {0}")]
    DataFolder(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub(crate) fn new_etag() -> String {
    random_token(32).to_lowercase()
}

pub struct PutOpts {
    pub mtime: i64,
    pub mimetype: String,
    pub checksum: Option<String>,
    pub if_match: Option<String>,
    pub if_none_match_star: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
    root: PathBuf,
    /// Most old versions kept per file.
    pub(crate) versions_max: usize,
}

/// Number of characters (SQLite `substr` counts characters, not bytes).
fn chars(s: &str) -> i64 {
    s.chars().count() as i64
}

pub(crate) fn set_mtime(path: &Path, mtime: i64) -> std::io::Result<()> {
    let f = std::fs::OpenOptions::new().read(true).open(path)?;
    f.set_modified(UNIX_EPOCH + Duration::from_secs(mtime.max(0) as u64))
}

pub(crate) fn mime_for(name: &str) -> String {
    mime_guess::from_path(name)
        .first_or_octet_stream()
        .essence_str()
        .to_string()
}

impl Store {
    pub fn new(pool: SqlitePool, data_dir: PathBuf) -> Self {
        Self {
            pool,
            root: data_dir,
            versions_max: 50,
        }
    }

    pub fn with_versions_max(mut self, n: usize) -> Self {
        self.versions_max = n;
        self
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// A node by id regardless of its owner (sharing resolves ids across trees).
    pub async fn node_by_id(&self, fileid: i64) -> Result<Option<Node>, FsError> {
        Ok(sqlx::query_as(select_node!("WHERE fileid = $1"))
            .bind(fileid)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// The user's own folder below the data dir (`files/`, `tmp/` and `uploads/` live in it).
    pub fn user_dir(&self, uid: &str) -> PathBuf {
        self.root.join(uid)
    }

    pub fn files_root(&self, o: &Owner) -> PathBuf {
        self.root.join(&o.uid).join("files")
    }

    /// Absolute filesystem path of a (normalised) tree path.
    pub fn fs_path(&self, o: &Owner, path: &str) -> PathBuf {
        let r = self.files_root(o);
        if path.is_empty() { r } else { r.join(path) }
    }

    /// Fresh temp file inside the user's own folder, i.e. on the same filesystem as the target,
    /// so the final `rename` is atomic.
    pub async fn new_tmp(&self, o: &Owner) -> Result<(PathBuf, File), FsError> {
        let dir = self.root.join(&o.uid).join("tmp");
        tokio::fs::create_dir_all(&dir).await?;
        let p = dir.join(random_token(24));
        let f = File::create(&p).await?;
        Ok((p, f))
    }

    /// Startup/scan check: the data dir carries a marker. A missing marker next to existing
    /// index rows means the data dir is wrong or unmounted: refuse instead of serving "empty".
    pub async fn check_data_dir(&self) -> Result<(), FsError> {
        let marker = self.root.join(DATA_MARKER);
        if tokio::fs::try_exists(&marker).await? {
            return Ok(());
        }
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM file_cache WHERE path <> ''")
            .fetch_one(&self.pool)
            .await?;
        if rows > 0 {
            return Err(FsError::DataFolder(format!(
                "{} is missing but the index has {rows} entries - wrong or unmounted data dir? \
                 (create the marker file by hand only if you are sure)",
                marker.display()
            )));
        }
        tokio::fs::create_dir_all(&self.root).await?;
        tokio::fs::write(&marker, b"rustbucket data directory\n").await?;
        Ok(())
    }

    /// The user's `files/` folder must exist; it may only be created for a user without rows.
    pub(crate) async fn prep(&self, o: &Owner) -> Result<(), FsError> {
        let root = self.files_root(o);
        if tokio::fs::try_exists(&root).await? {
            return Ok(());
        }
        let rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM file_cache WHERE owner_id = $1 AND path <> ''",
        )
        .bind(o.id)
        .fetch_one(&self.pool)
        .await?;
        if rows > 0 {
            return Err(FsError::DataFolder(format!(
                "{} is missing but the index has {rows} entries",
                root.display()
            )));
        }
        tokio::fs::create_dir_all(&root).await?;
        Ok(())
    }

    pub async fn instance_id(&self) -> Result<String, FsError> {
        let fresh = format!("oc{}", random_token(10).to_lowercase());
        sqlx::query("INSERT OR IGNORE INTO settings (key, value) VALUES ('instanceid', $1)")
            .bind(fresh)
            .execute(&self.pool)
            .await?;
        Ok(
            sqlx::query_scalar("SELECT value FROM settings WHERE key = 'instanceid'")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    // ---- reads -----------------------------------------------------------------------

    pub async fn get(&self, o: &Owner, path: &str) -> Result<Option<Node>, FsError> {
        self.prep(o).await?;
        let mut c = self.pool.acquire().await?;
        ensure_root(&mut c, o.id).await?;
        Ok(get_node(&mut c, o.id, path).await?)
    }

    /// Look a file up by its (per-user) file id.
    pub async fn get_by_id(&self, o: &Owner, fileid: i64) -> Result<Option<Node>, FsError> {
        self.prep(o).await?;
        Ok(
            sqlx::query_as(select_node!("WHERE owner_id = $1 AND fileid = $2"))
                .bind(o.id)
                .bind(fileid)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// Free bytes on the filesystem that holds the data dir.
    pub fn free_bytes(&self) -> u64 {
        rustix::fs::statvfs(&self.root).map_or(0, |s| s.f_bavail * s.f_frsize)
    }

    /// Where derived data (previews) lives: dot-prefixed, so it can never be a user's folder.
    pub fn appdata_dir(&self) -> PathBuf {
        self.root.join(".appdata")
    }

    pub async fn children(&self, dir: &Node) -> Result<Vec<Node>, FsError> {
        Ok(
            sqlx::query_as(select_node!("WHERE parent_id = $1 ORDER BY name"))
                .bind(dir.fileid)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    pub async fn descendants(&self, dir: &Node) -> Result<Vec<Node>, FsError> {
        let prefix = if dir.path.is_empty() {
            String::new()
        } else {
            format!("{}/", dir.path)
        };
        Ok(sqlx::query_as(select_node!(
            "WHERE owner_id = $1 AND path <> '' AND substr(path, 1, $2) = $3 ORDER BY path"
        ))
        .bind(dir.owner_id)
        .bind(chars(&prefix))
        .bind(prefix)
        .fetch_all(&self.pool)
        .await?)
    }

    // ---- writes ----------------------------------------------------------------------

    pub async fn mkcol(&self, o: &Owner, path: &str) -> Result<Node, FsError> {
        self.prep(o).await?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure_root(&mut tx, o.id).await?;
        if path.is_empty() || get_node(&mut tx, o.id, path).await?.is_some() {
            return Err(FsError::Exists);
        }
        let (parent_path, name) = paths::split(path);
        let parent = get_node(&mut tx, o.id, parent_path)
            .await?
            .ok_or(FsError::ParentNotFound)?;
        if !parent.is_dir {
            return Err(FsError::NotCollection);
        }
        match tokio::fs::create_dir(self.fs_path(o, path)).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(FsError::Exists),
            Err(e) => return Err(e.into()),
        }
        let now = unix_now();
        insert_node(&mut tx, o.id, &parent, name, path, true, 0, now, "", None).await?;
        touch_ancestors(&mut tx, o.id, path, 0, now).await?;
        let node = get_node(&mut tx, o.id, path).await?.expect("just inserted");
        tx.commit().await?;
        Ok(node)
    }

    /// Replace/create a file from a finished temp file (which is consumed). `(node, created)`.
    pub async fn put(
        &self,
        o: &Owner,
        path: &str,
        tmp: &Path,
        size: i64,
        opts: PutOpts,
    ) -> Result<(Node, bool), FsError> {
        let res = self.put_inner(o, path, tmp, size, &opts).await;
        if res.is_err() {
            let _ = tokio::fs::remove_file(tmp).await; // still ours if we failed before the rename
        }
        res
    }

    async fn put_inner(
        &self,
        o: &Owner,
        path: &str,
        tmp: &Path,
        size: i64,
        op: &PutOpts,
    ) -> Result<(Node, bool), FsError> {
        self.prep(o).await?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure_root(&mut tx, o.id).await?;
        if path.is_empty() {
            return Err(FsError::IsCollection);
        }
        let (parent_path, name) = paths::split(path);
        let parent = get_node(&mut tx, o.id, parent_path)
            .await?
            .ok_or(FsError::ParentNotFound)?;
        if !parent.is_dir {
            return Err(FsError::NotCollection);
        }
        let existing = get_node(&mut tx, o.id, path).await?;
        if let Some(e) = &existing {
            if e.is_dir {
                return Err(FsError::IsCollection);
            }
            if op.if_none_match_star {
                return Err(FsError::PreconditionFailed);
            }
        }
        if let Some(m) = &op.if_match {
            let want = m.trim().trim_matches('"');
            if existing.as_ref().map(|e| e.etag.as_str()) != Some(want) && m.trim() != "*" {
                return Err(FsError::PreconditionFailed);
            }
        }
        // the content that is about to be replaced stays available as a version
        if let Some(e) = &existing {
            self.snapshot_version(o, e, &self.fs_path(o, path)).await;
        }
        // disk mtime == index mtime, so `scan` does not see a change
        set_mtime(tmp, op.mtime)?;
        tokio::fs::rename(tmp, self.fs_path(o, path)).await?; // atomic replace
        let now = unix_now();
        let (created, delta) = match existing {
            Some(e) => {
                sqlx::query(
                    "UPDATE file_cache SET size = $1, mtime = $2, etag = $3, mimetype = $4, checksum = $5 WHERE fileid = $6",
                )
                .bind(size)
                .bind(op.mtime)
                .bind(new_etag())
                .bind(&op.mimetype)
                .bind(&op.checksum)
                .bind(e.fileid)
                .execute(&mut *tx)
                .await?;
                (false, size - e.size)
            }
            None => {
                insert_node(
                    &mut tx,
                    o.id,
                    &parent,
                    name,
                    path,
                    false,
                    size,
                    op.mtime,
                    &op.mimetype,
                    op.checksum.as_deref(),
                )
                .await?;
                (true, size)
            }
        };
        touch_ancestors(&mut tx, o.id, path, delta, now).await?;
        let node = get_node(&mut tx, o.id, path).await?.expect("just written");
        tx.commit().await?;
        Ok((node, created))
    }

    /// DELETE: the item goes to the trash bin (see `trash`); `delete_permanent` keeps nothing.
    pub async fn delete(&self, o: &Owner, path: &str) -> Result<(), FsError> {
        self.delete_inner(o, path, true).await
    }

    /// MOVE. Keeps fileid and etag of the moved node. Returns `(node, created)`.
    pub async fn rename(
        &self,
        o: &Owner,
        src: &str,
        dst: &str,
        overwrite: bool,
    ) -> Result<(Node, bool), FsError> {
        self.prep(o).await?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure_root(&mut tx, o.id).await?;
        let node = get_node(&mut tx, o.id, src)
            .await?
            .ok_or(FsError::NotFound)?;
        check_move_copy(src, dst)?;
        let (dst_parent_path, dst_name) = paths::split(dst);
        let dst_parent = get_node(&mut tx, o.id, dst_parent_path)
            .await?
            .ok_or(FsError::ParentNotFound)?;
        if !dst_parent.is_dir {
            return Err(FsError::NotCollection);
        }
        let created = match get_node(&mut tx, o.id, dst).await? {
            Some(existing) => {
                if !overwrite {
                    return Err(FsError::PreconditionFailed);
                }
                self.discard(&mut tx, o, &existing, true).await?;
                touch_ancestors(&mut tx, o.id, dst, -existing.size, unix_now()).await?;
                false
            }
            None => true,
        };
        let dst_parent = get_node(&mut tx, o.id, dst_parent_path)
            .await?
            .ok_or(FsError::ParentNotFound)?;
        tokio::fs::rename(self.fs_path(o, src), self.fs_path(o, dst)).await?;
        sqlx::query("UPDATE file_cache SET name = $1, path = $2, parent_id = $3 WHERE fileid = $4")
            .bind(dst_name)
            .bind(dst)
            .bind(dst_parent.fileid)
            .bind(node.fileid)
            .execute(&mut *tx)
            .await?;
        if node.is_dir {
            let old_prefix = format!("{src}/");
            sqlx::query(
                "UPDATE file_cache SET path = $1 || substr(path, $2) WHERE owner_id = $3 AND substr(path, 1, $4) = $5",
            )
            .bind(format!("{dst}/"))
            .bind(chars(&old_prefix) + 1)
            .bind(o.id)
            .bind(chars(&old_prefix))
            .bind(&old_prefix)
            .execute(&mut *tx)
            .await?;
        }
        let now = unix_now();
        touch_ancestors(&mut tx, o.id, src, -node.size, now).await?;
        touch_ancestors(&mut tx, o.id, dst, node.size, now).await?;
        let moved = get_node(&mut tx, o.id, dst).await?.expect("moved");
        tx.commit().await?;
        Ok((moved, created))
    }

    /// COPY: real copies (files can be edited in place by other tools), new fileids.
    pub async fn copy(
        &self,
        o: &Owner,
        src: &str,
        dst: &str,
        overwrite: bool,
    ) -> Result<(Node, bool), FsError> {
        self.prep(o).await?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure_root(&mut tx, o.id).await?;
        let node = get_node(&mut tx, o.id, src)
            .await?
            .ok_or(FsError::NotFound)?;
        check_move_copy(src, dst)?;
        let (dst_parent_path, dst_name) = paths::split(dst);
        let dst_parent = get_node(&mut tx, o.id, dst_parent_path)
            .await?
            .ok_or(FsError::ParentNotFound)?;
        if !dst_parent.is_dir {
            return Err(FsError::NotCollection);
        }
        let created = match get_node(&mut tx, o.id, dst).await? {
            Some(existing) => {
                if !overwrite {
                    return Err(FsError::PreconditionFailed);
                }
                self.discard(&mut tx, o, &existing, true).await?;
                touch_ancestors(&mut tx, o.id, dst, -existing.size, unix_now()).await?;
                false
            }
            None => true,
        };
        let dst_parent = get_node(&mut tx, o.id, dst_parent_path)
            .await?
            .ok_or(FsError::ParentNotFound)?;
        let mut all = vec![node.clone()];
        if node.is_dir {
            let prefix = format!("{src}/");
            let mut kids: Vec<Node> = sqlx::query_as(select_node!(
                "WHERE owner_id = $1 AND substr(path, 1, $2) = $3 ORDER BY path"
            ))
            .bind(o.id)
            .bind(chars(&prefix))
            .bind(&prefix)
            .fetch_all(&mut *tx)
            .await?;
            all.append(&mut kids);
        }
        // 1. copy on disk (parents first); on failure remove the partial destination
        let result: Result<(), FsError> = async {
            for n in &all {
                let rel = if n.fileid == node.fileid {
                    dst.to_string()
                } else {
                    format!("{dst}/{}", &n.path[src.len() + 1..])
                };
                let target = self.fs_path(o, &rel);
                if n.is_dir {
                    tokio::fs::create_dir(&target).await?;
                } else {
                    tokio::fs::copy(self.fs_path(o, &n.path), &target).await?;
                    set_mtime(&target, n.mtime)?;
                }
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            let _ = remove_on_disk(&self.fs_path(o, dst), node.is_dir).await;
            return Err(e);
        }
        // 2. index
        let now = unix_now();
        let mut idmap: HashMap<i64, i64> = HashMap::new();
        for n in &all {
            let (parent_fileid, name, new_path) = if n.fileid == node.fileid {
                (dst_parent.fileid, dst_name.to_string(), dst.to_string())
            } else {
                (
                    idmap[&n.parent_id.expect("child has parent")],
                    n.name.clone(),
                    format!("{dst}/{}", &n.path[src.len() + 1..]),
                )
            };
            let new_id = sqlx::query_scalar::<_, i64>(
                "INSERT INTO file_cache (owner_id, parent_id, name, path, is_dir, size, mtime, etag, mimetype, checksum) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING fileid",
            )
            .bind(o.id)
            .bind(parent_fileid)
            .bind(name)
            .bind(new_path)
            .bind(n.is_dir)
            .bind(n.size)
            .bind(if n.fileid == node.fileid && n.is_dir { now } else { n.mtime })
            .bind(new_etag())
            .bind(&n.mimetype)
            .bind(&n.checksum)
            .fetch_one(&mut *tx)
            .await?;
            idmap.insert(n.fileid, new_id);
        }
        touch_ancestors(&mut tx, o.id, dst, node.size, now).await?;
        let copied = get_node(&mut tx, o.id, dst).await?.expect("copied");
        tx.commit().await?;
        Ok((copied, created))
    }

    // ---- reconcile -------------------------------------------------------------------

    /// Bring the index in line with the disk (like `occ files:scan`): add new files/folders,
    /// refresh changed ones (size or mtime, never directory mtimes), drop rows whose file is
    /// gone, recompute directory sizes and give every touched ancestor a new ETag.
    ///
    /// Takes the write lock *before* walking the disk, so no request can change the tree in
    /// between. Refuses to remove (almost) everything unless `force`; `dry_run` rolls back.
    pub async fn scan(&self, o: &Owner, dry_run: bool, force: bool) -> Result<ScanReport, FsError> {
        self.check_data_dir().await?;
        self.prep(o).await?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        ensure_root(&mut tx, o.id).await?;
        let root = self.files_root(o);
        let disk = tokio::task::spawn_blocking(move || walk(&root))
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))??;
        let db: Vec<Node> = sqlx::query_as(select_node!("WHERE owner_id = $1 ORDER BY path"))
            .bind(o.id)
            .fetch_all(&mut *tx)
            .await?;
        let mut by_path: HashMap<String, Node> =
            db.into_iter().map(|n| (n.path.clone(), n)).collect();
        let db_rows = by_path.len().saturating_sub(1); // without the root
        let mut rep = ScanReport::default();
        let mut changed: HashSet<String> = HashSet::new(); // paths whose ancestors need a new etag

        // rows whose file is gone (or changed type)
        let on_disk: HashMap<&str, &DiskEntry> =
            disk.iter().map(|d| (d.path.as_str(), d)).collect();
        let gone: Vec<String> = by_path
            .values()
            .filter(|n| !n.path.is_empty())
            .filter(|n| {
                on_disk
                    .get(n.path.as_str())
                    .is_none_or(|d| d.is_dir != n.is_dir)
            })
            .map(|n| n.path.clone())
            .collect();
        let would_remove = gone.len();
        if !force && db_rows > 0 && (would_remove == db_rows || would_remove * 2 > db_rows) {
            return Err(FsError::DataFolder(format!(
                "scan would remove {would_remove} of {db_rows} index entries - refusing (is the data folder mounted correctly?). Use --force to override"
            )));
        }
        for p in &gone {
            if let Some(n) = by_path.remove(p) {
                sqlx::query("DELETE FROM file_cache WHERE fileid = $1")
                    .bind(n.fileid)
                    .execute(&mut *tx)
                    .await?;
                rep.removed += 1;
                changed.insert(p.clone());
            }
        }
        // children of removed dirs were cascade-deleted; forget them too
        let alive: HashSet<i64> =
            sqlx::query_scalar("SELECT fileid FROM file_cache WHERE owner_id = $1")
                .bind(o.id)
                .fetch_all(&mut *tx)
                .await?
                .into_iter()
                .collect();
        by_path.retain(|_, n| alive.contains(&n.fileid));

        // new and changed entries, parents first (disk is sorted by path)
        let now = unix_now();
        let mut ids: HashMap<String, i64> =
            by_path.iter().map(|(p, n)| (p.clone(), n.fileid)).collect();
        for d in &disk {
            match by_path.get(&d.path) {
                None => {
                    let (parent_path, name) = paths::split(&d.path);
                    let parent_id = ids[parent_path];
                    let id: i64 = sqlx::query_scalar(
                        "INSERT INTO file_cache (owner_id, parent_id, name, path, is_dir, size, mtime, etag, mimetype) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING fileid",
                    )
                    .bind(o.id)
                    .bind(parent_id)
                    .bind(name)
                    .bind(&d.path)
                    .bind(d.is_dir)
                    .bind(if d.is_dir { 0 } else { d.size })
                    .bind(d.mtime)
                    .bind(new_etag())
                    .bind(if d.is_dir { String::new() } else { mime_for(name) })
                    .fetch_one(&mut *tx)
                    .await?;
                    ids.insert(d.path.clone(), id);
                    rep.added += 1;
                    changed.insert(d.path.clone());
                }
                Some(n) if !d.is_dir && (n.size != d.size || n.mtime != d.mtime) => {
                    sqlx::query("UPDATE file_cache SET size = $1, mtime = $2, etag = $3, checksum = NULL WHERE fileid = $4")
                        .bind(d.size)
                        .bind(d.mtime)
                        .bind(new_etag())
                        .bind(n.fileid)
                        .execute(&mut *tx)
                        .await?;
                    rep.updated += 1;
                    changed.insert(d.path.clone());
                }
                Some(_) => {}
            }
        }

        // directory sizes (recursive) from the disk; a changed size also changes the etag
        let mut dir_sizes: BTreeMap<String, i64> = BTreeMap::new();
        dir_sizes.insert(String::new(), 0);
        for d in &disk {
            if d.is_dir {
                dir_sizes.entry(d.path.clone()).or_insert(0);
            } else {
                for a in paths::ancestors(&d.path) {
                    *dir_sizes.entry(a).or_insert(0) += d.size;
                }
            }
        }
        let mut etag_dirs: HashSet<String> = HashSet::new();
        for p in &changed {
            etag_dirs.extend(paths::ancestors(p));
        }
        for (p, size) in &dir_sizes {
            let current: Option<i64> =
                sqlx::query_scalar("SELECT size FROM file_cache WHERE owner_id = $1 AND path = $2")
                    .bind(o.id)
                    .bind(p)
                    .fetch_optional(&mut *tx)
                    .await?;
            if current != Some(*size) {
                sqlx::query("UPDATE file_cache SET size = $1 WHERE owner_id = $2 AND path = $3")
                    .bind(size)
                    .bind(o.id)
                    .bind(p)
                    .execute(&mut *tx)
                    .await?;
                etag_dirs.insert(p.clone());
            }
        }
        for p in &etag_dirs {
            sqlx::query("UPDATE file_cache SET etag = $1, mtime = $2 WHERE owner_id = $3 AND path = $4 AND is_dir = 1")
                .bind(new_etag())
                .bind(now)
                .bind(o.id)
                .bind(p)
                .execute(&mut *tx)
                .await?;
        }
        if dry_run {
            tx.rollback().await?;
        } else {
            tx.commit().await?;
        }
        Ok(rep)
    }
}

pub(crate) struct DiskEntry {
    pub(crate) path: String,
    pub(crate) is_dir: bool,
    pub(crate) size: i64,
    pub(crate) mtime: i64,
}

/// Walk `files/` (symlinks and names that are not NFC / not UTF-8 are skipped with a warning).
pub(crate) fn walk(root: &Path) -> std::io::Result<Vec<DiskEntry>> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                tracing::warn!(path = %entry.path().display(), "scan: skipping non-UTF-8 name");
                continue;
            };
            if name.nfc().collect::<String>() != name || name == "." || name == ".." {
                tracing::warn!(path = %entry.path().display(), "scan: skipping name that is not NFC-normalised");
                continue;
            }
            let ft = entry.file_type()?;
            if ft.is_symlink() || !(ft.is_dir() || ft.is_file()) {
                tracing::warn!(path = %entry.path().display(), "scan: skipping symlink/special file");
                continue;
            }
            let meta = entry.metadata()?;
            let path = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            if ft.is_dir() {
                stack.push((entry.path(), path.clone()));
            }
            out.push(DiskEntry {
                path,
                is_dir: ft.is_dir(),
                size: if ft.is_dir() { 0 } else { meta.len() as i64 },
                mtime,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

pub(crate) async fn remove_on_disk(p: &Path, is_dir: bool) -> Result<(), FsError> {
    let r = if is_dir {
        tokio::fs::remove_dir_all(p).await
    } else {
        tokio::fs::remove_file(p).await
    };
    match r {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()), // already gone: fine, the index follows
        Err(e) => Err(e.into()),
    }
}

fn check_move_copy(src: &str, dst: &str) -> Result<(), FsError> {
    if src.is_empty() || dst.is_empty() {
        return Err(FsError::Forbidden("cannot move or copy the root"));
    }
    if src == dst {
        return Err(FsError::Forbidden("source and destination are the same"));
    }
    if dst.starts_with(&format!("{src}/")) || src.starts_with(&format!("{dst}/")) {
        return Err(FsError::Forbidden("source and destination overlap"));
    }
    Ok(())
}

pub(crate) async fn ensure_root(c: &mut SqliteConnection, owner: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR IGNORE INTO file_cache (owner_id, parent_id, name, path, is_dir, size, mtime, etag) \
         VALUES ($1, NULL, '', '', 1, 0, $2, $3)",
    )
    .bind(owner)
    .bind(unix_now())
    .bind(new_etag())
    .execute(c)
    .await?;
    Ok(())
}

pub(crate) async fn get_node(
    c: &mut SqliteConnection,
    owner: i64,
    path: &str,
) -> Result<Option<Node>, sqlx::Error> {
    sqlx::query_as(select_node!("WHERE owner_id = $1 AND path = $2"))
        .bind(owner)
        .bind(path)
        .fetch_optional(c)
        .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn insert_node(
    c: &mut SqliteConnection,
    owner: i64,
    parent: &Node,
    name: &str,
    path: &str,
    is_dir: bool,
    size: i64,
    mtime: i64,
    mimetype: &str,
    checksum: Option<&str>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO file_cache (owner_id, parent_id, name, path, is_dir, size, mtime, etag, mimetype, checksum) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING fileid",
    )
    .bind(owner)
    .bind(parent.fileid)
    .bind(name)
    .bind(path)
    .bind(is_dir)
    .bind(size)
    .bind(mtime)
    .bind(new_etag())
    .bind(mimetype)
    .bind(checksum)
    .fetch_one(c)
    .await
}

/// Give every ancestor of `path` a new ETag and mtime, and adjust its recursive size.
pub(crate) async fn touch_ancestors(
    c: &mut SqliteConnection,
    owner: i64,
    path: &str,
    delta: i64,
    now: i64,
) -> Result<(), sqlx::Error> {
    for a in paths::ancestors(path) {
        sqlx::query("UPDATE file_cache SET etag = $1, mtime = $2, size = size + $3 WHERE owner_id = $4 AND path = $5")
            .bind(new_etag())
            .bind(now)
            .bind(delta)
            .bind(owner)
            .bind(a)
            .execute(&mut *c)
            .await?;
    }
    Ok(())
}

/// Delete the index rows of a node and its descendants.
pub(crate) async fn remove_subtree(
    c: &mut SqliteConnection,
    owner: i64,
    node: &Node,
) -> Result<(), sqlx::Error> {
    let prefix = format!("{}/", node.path);
    sqlx::query(
        "DELETE FROM file_cache WHERE owner_id = $1 AND (path = $2 OR substr(path, 1, $3) = $4)",
    )
    .bind(owner)
    .bind(&node.path)
    .bind(chars(&prefix))
    .bind(&prefix)
    .execute(c)
    .await?;
    Ok(())
}
