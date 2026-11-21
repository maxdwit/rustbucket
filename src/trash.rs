//! Trash bin and file versions (storage side).
//!
//! * **Trash:** deleting moves the item to `<data>/<uid>/trash/<id>/item` (a rename on the same
//!   filesystem, so it is instant and atomic) and records it in `trash_items`. A file keeps its older
//!   versions with it (`<id>/versions/`). Restoring renames it back (new file id).
//! * **Versions:** overwriting a file hard-links the previous content to
//!   `<data>/<uid>/versions/<fileid>/<mtime>` before the new content replaces it. That costs no extra
//!   space until the contents diverge, needs no table (the directory is the index) and is atomic with the
//!   overwrite. The live file is listed as the newest version, like stock Nextcloud does.
//! * `expire` applies the retention settings and removes leftovers.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sqlx::SqliteConnection;

use crate::auth::unix_now;
use crate::paths;
use crate::storage::{
    FsError, Node, Owner, PutOpts, Store, ensure_root, get_node, insert_node, remove_subtree,
    touch_ancestors,
};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TrashItem {
    pub id: i64,
    pub owner_id: i64,
    pub name: String,
    pub orig_dir: String,
    pub deleted_at: i64,
    pub is_dir: bool,
    pub size: i64,
    pub mtime: i64,
    pub mimetype: String,
    pub orig_fileid: i64,
}

impl TrashItem {
    /// Name of the entry in the WebDAV trash collection: `<name>.d<deletion time>`.
    pub fn entry_name(&self) -> String {
        format!("{}.d{}", self.name, self.deleted_at)
    }

    /// Original path including the name, without a leading slash.
    pub fn original_location(&self) -> String {
        if self.orig_dir.is_empty() {
            self.name.clone()
        } else {
            format!("{}/{}", self.orig_dir, self.name)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionInfo {
    /// The version's name: the modification time of that content.
    pub ts: i64,
    pub size: i64,
    /// This is the live file.
    pub current: bool,
}

macro_rules! select_item {
    ($tail:literal) => {
        concat!(
            "SELECT id, owner_id, name, orig_dir, deleted_at, is_dir, size, mtime, mimetype, orig_fileid FROM trash_items ",
            $tail
        )
    };
}

/// Total size of the files below `p` (folders inside the trash are not indexed).
pub fn dir_size(p: &Path) -> i64 {
    let mut total = 0;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            match e.metadata() {
                Ok(m) if m.is_dir() => stack.push(e.path()),
                Ok(m) => total += m.len() as i64,
                Err(_) => {}
            }
        }
    }
    total
}

impl Store {
    pub fn trash_root(&self, o: &Owner) -> PathBuf {
        self.user_dir(&o.uid).join("trash")
    }

    pub fn versions_root(&self, o: &Owner) -> PathBuf {
        self.user_dir(&o.uid).join("versions")
    }

    fn versions_dir(&self, o: &Owner, fileid: i64) -> PathBuf {
        self.versions_root(o).join(fileid.to_string())
    }

    /// On-disk path of a trashed item (a file or a folder).
    pub fn trash_item_path(&self, o: &Owner, item: &TrashItem) -> PathBuf {
        self.trash_root(o).join(item.id.to_string()).join("item")
    }

    // ------------------------------------------------------------------ discarding (delete / overwrite)

    /// Take `node` out of the live tree inside the caller's transaction: move it to the trash
    /// (`keep`) or remove it for good. The caller adjusts ancestors' size/etag.
    pub(crate) async fn discard(
        &self,
        tx: &mut SqliteConnection,
        o: &Owner,
        node: &Node,
        keep: bool,
    ) -> Result<(), FsError> {
        let file_ids: Vec<i64> = if node.is_dir {
            let prefix = format!("{}/", node.path);
            sqlx::query_scalar(
                "SELECT fileid FROM file_cache WHERE owner_id = $1 AND is_dir = 0 AND substr(path, 1, $2) = $3",
            )
            .bind(o.id)
            .bind(prefix.chars().count() as i64)
            .bind(&prefix)
            .fetch_all(&mut *tx)
            .await?
        } else {
            vec![node.fileid]
        };
        let live = self.fs_path(o, &node.path);
        if keep {
            let (dir, name) = paths::split(&node.path);
            // the deletion time doubles as the entry's identity, so it must be unique per name
            let mut at = unix_now();
            while sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM trash_items WHERE owner_id = $1 AND name = $2 AND deleted_at = $3")
                .bind(o.id)
                .bind(name)
                .bind(at)
                .fetch_one(&mut *tx)
                .await?
                > 0
            {
                at += 1;
            }
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO trash_items (owner_id, name, orig_dir, deleted_at, is_dir, size, mtime, mimetype, orig_fileid) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING id",
            )
            .bind(o.id)
            .bind(name)
            .bind(dir)
            .bind(at)
            .bind(node.is_dir)
            .bind(node.size)
            .bind(node.mtime)
            .bind(&node.mimetype)
            .bind(node.fileid)
            .fetch_one(&mut *tx)
            .await?;
            let slot = self.trash_root(o).join(id.to_string());
            tokio::fs::create_dir_all(&slot).await?;
            // what an orphaned slot needs to become a trash entry again (crash/rollback between rename and commit)
            let meta = serde_json::json!({
                "name": name, "orig_dir": dir, "deleted_at": at, "is_dir": node.is_dir, "size": node.size,
                "mtime": node.mtime, "mimetype": node.mimetype, "orig_fileid": node.fileid,
            });
            tokio::fs::write(slot.join("meta.json"), meta.to_string()).await?;
            match tokio::fs::rename(&live, slot.join("item")).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // the file was already gone from disk: nothing to keep, drop the row again
                    let _ = tokio::fs::remove_dir(&slot).await;
                    sqlx::query("DELETE FROM trash_items WHERE id = $1")
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                }
                Err(e) => return Err(e.into()),
            }
            if !node.is_dir {
                let v = self.versions_dir(o, node.fileid);
                if tokio::fs::try_exists(&v).await.unwrap_or(false) {
                    let _ = tokio::fs::rename(&v, slot.join("versions")).await; // goes with it
                }
            } else {
                for id in &file_ids {
                    let _ = tokio::fs::remove_dir_all(self.versions_dir(o, *id)).await; // versions of files inside folders are not kept
                }
            }
        } else {
            crate::storage::remove_on_disk(&live, node.is_dir).await?;
            for id in &file_ids {
                let _ = tokio::fs::remove_dir_all(self.versions_dir(o, *id)).await;
            }
        }
        remove_subtree(&mut *tx, o.id, node).await?;
        Ok(())
    }

    /// DELETE that does not keep anything (used where the item moves elsewhere, e.g. across shares).
    pub async fn delete_permanent(&self, o: &Owner, path: &str) -> Result<(), FsError> {
        self.delete_inner(o, path, false).await
    }

    pub(crate) async fn delete_inner(
        &self,
        o: &Owner,
        path: &str,
        keep: bool,
    ) -> Result<(), FsError> {
        self.prep(o).await?;
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        ensure_root(&mut tx, o.id).await?;
        let node = get_node(&mut tx, o.id, path)
            .await?
            .ok_or(FsError::NotFound)?;
        if path.is_empty() {
            return Err(FsError::Forbidden("cannot delete the root"));
        }
        self.discard(&mut tx, o, &node, keep).await?; // disk first, then the index
        touch_ancestors(&mut tx, o.id, path, -node.size, unix_now()).await?;
        tx.commit().await?;
        Ok(())
    }

    // ------------------------------------------------------------------ trash

    pub async fn trash_list(&self, o: &Owner) -> Result<Vec<TrashItem>, FsError> {
        Ok(sqlx::query_as(select_item!(
            "WHERE owner_id = $1 ORDER BY deleted_at DESC, id DESC"
        ))
        .bind(o.id)
        .fetch_all(self.pool())
        .await?)
    }

    /// Look an entry up by its WebDAV name (`<name>.d<time>`).
    pub async fn trash_find(&self, o: &Owner, entry: &str) -> Result<Option<TrashItem>, FsError> {
        let Some(i) = entry.rfind(".d") else {
            return Ok(None);
        };
        let (name, ts) = (&entry[..i], &entry[i + 2..]);
        let Ok(ts) = ts.parse::<i64>() else {
            return Ok(None);
        };
        Ok(sqlx::query_as(select_item!(
            "WHERE owner_id = $1 AND name = $2 AND deleted_at = $3"
        ))
        .bind(o.id)
        .bind(name)
        .bind(ts)
        .fetch_optional(self.pool())
        .await?)
    }

    /// Contents of a trashed folder (or of a folder inside it): `(name, is_dir, size, mtime)`.
    pub async fn trash_children(
        &self,
        o: &Owner,
        item: &TrashItem,
        rel: &str,
    ) -> Result<Vec<(String, bool, i64, i64)>, FsError> {
        let dir = self.trash_item_path(o, item).join(rel);
        let mut out = Vec::new();
        let mut rd = tokio::fs::read_dir(&dir).await?;
        while let Some(e) = rd.next_entry().await? {
            let Ok(md) = e.metadata().await else { continue };
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs() as i64);
            out.push((
                e.file_name().to_string_lossy().into_owned(),
                md.is_dir(),
                if md.is_dir() {
                    dir_size(&e.path())
                } else {
                    md.len() as i64
                },
                mtime,
            ));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Put an item back where it was. Missing parent folder: top level. Name taken: "a (restored).txt".
    /// Returns the path it now has.
    pub async fn trash_restore(
        &self,
        o: &Owner,
        item: &TrashItem,
        to: Option<&str>,
    ) -> Result<String, FsError> {
        self.prep(o).await?;
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;
        ensure_root(&mut tx, o.id).await?;
        let src = self.trash_item_path(o, item);
        if !tokio::fs::try_exists(&src).await.unwrap_or(false) {
            return Err(FsError::NotFound);
        }
        let (dir, name) = match to {
            // an explicit target: its parent must exist and the name must be free
            Some(p) => {
                let (pp, n) = paths::split(p);
                let d = get_node(&mut tx, o.id, pp)
                    .await?
                    .ok_or(FsError::ParentNotFound)?;
                if !d.is_dir {
                    return Err(FsError::NotCollection);
                }
                if get_node(&mut tx, o.id, p).await?.is_some() {
                    return Err(FsError::PreconditionFailed);
                }
                (d, n.to_string())
            }
            None => {
                let dir = match get_node(&mut tx, o.id, &item.orig_dir).await? {
                    Some(p) if p.is_dir => p,
                    _ => get_node(&mut tx, o.id, "")
                        .await?
                        .ok_or(FsError::NotFound)?,
                };
                let path_of = |name: &str| {
                    if dir.path.is_empty() {
                        name.to_string()
                    } else {
                        format!("{}/{name}", dir.path)
                    }
                };
                let mut name = item.name.clone();
                if get_node(&mut tx, o.id, &path_of(&name)).await?.is_some() {
                    let mut n = 1;
                    loop {
                        let cand = restored_numbered(&item.name, item.is_dir, n);
                        if get_node(&mut tx, o.id, &path_of(&cand)).await?.is_none() {
                            name = cand;
                            break;
                        }
                        n += 1;
                    }
                }
                (dir, name)
            }
        };
        let path = if dir.path.is_empty() {
            name.clone()
        } else {
            format!("{}/{name}", dir.path)
        };
        tokio::fs::rename(&src, self.fs_path(o, &path)).await?;
        let new_id = insert_node(
            &mut tx,
            o.id,
            &dir,
            &name,
            &path,
            item.is_dir,
            if item.is_dir { 0 } else { item.size },
            item.mtime,
            &item.mimetype,
            None,
        )
        .await?;
        if !item.is_dir {
            let v = self
                .trash_root(o)
                .join(item.id.to_string())
                .join("versions");
            if tokio::fs::try_exists(&v).await.unwrap_or(false) {
                tokio::fs::create_dir_all(self.versions_root(o)).await?;
                let _ = tokio::fs::rename(&v, self.versions_dir(o, new_id)).await;
            }
            touch_ancestors(&mut tx, o.id, &path, item.size, unix_now()).await?;
        } else {
            // index just the restored subtree, in the same transaction (no full scan, nothing can fail afterwards)
            let total = self.index_tree(&mut tx, o, new_id, &path).await?;
            touch_ancestors(&mut tx, o.id, &path, total, unix_now()).await?;
        }
        sqlx::query("DELETE FROM trash_items WHERE id = $1")
            .bind(item.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        let _ = tokio::fs::remove_dir_all(self.trash_root(o).join(item.id.to_string())).await;
        Ok(path)
    }

    /// Insert index rows for everything below the (already inserted) folder `dir_id` at `path`, which
    /// now exists on disk. Returns the folder's total size and stores recursive folder sizes.
    async fn index_tree(
        &self,
        tx: &mut SqliteConnection,
        o: &Owner,
        dir_id: i64,
        path: &str,
    ) -> Result<i64, FsError> {
        let root = self.fs_path(o, path);
        let entries = tokio::task::spawn_blocking(move || crate::storage::walk(&root))
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))??;
        let mut ids: std::collections::HashMap<String, i64> =
            std::collections::HashMap::from([(String::new(), dir_id)]);
        let mut sizes: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        for e in &entries {
            let (parent, name) = paths::split(&e.path);
            let full = format!("{path}/{}", e.path);
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO file_cache (owner_id, parent_id, name, path, is_dir, size, mtime, etag, mimetype) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING fileid",
            )
            .bind(o.id)
            .bind(ids.get(parent).copied().unwrap_or(dir_id))
            .bind(name)
            .bind(&full)
            .bind(e.is_dir)
            .bind(if e.is_dir { 0 } else { e.size })
            .bind(e.mtime)
            .bind(crate::storage::new_etag())
            .bind(if e.is_dir { String::new() } else { crate::storage::mime_for(name) })
            .fetch_one(&mut *tx)
            .await?;
            ids.insert(e.path.clone(), id);
            if !e.is_dir {
                for a in paths::ancestors(&e.path) {
                    *sizes.entry(a).or_insert(0) += e.size;
                }
            }
        }
        for (rel, size) in &sizes {
            if let Some(id) = ids.get(rel) {
                sqlx::query("UPDATE file_cache SET size = $1 WHERE fileid = $2")
                    .bind(size)
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        Ok(sizes.get("").copied().unwrap_or(0))
    }

    /// Remove one trashed item for good.
    pub async fn trash_purge(&self, o: &Owner, item: &TrashItem) -> Result<(), FsError> {
        sqlx::query("DELETE FROM trash_items WHERE id = $1 AND owner_id = $2")
            .bind(item.id)
            .bind(o.id)
            .execute(self.pool())
            .await?;
        let _ = tokio::fs::remove_dir_all(self.trash_root(o).join(item.id.to_string())).await;
        Ok(())
    }

    pub async fn trash_empty(&self, o: &Owner) -> Result<usize, FsError> {
        let items = self.trash_list(o).await?;
        for i in &items {
            self.trash_purge(o, i).await?;
        }
        Ok(items.len())
    }

    // ------------------------------------------------------------------ versions

    /// Keep the content that is about to be overwritten as a version (hard link, no copy).
    pub(crate) async fn snapshot_version(&self, o: &Owner, old: &Node, live: &Path) {
        use std::os::unix::fs::MetadataExt;
        if old.size == 0 {
            return;
        }
        let dir = self.versions_dir(o, old.fileid);
        if tokio::fs::create_dir_all(&dir).await.is_err() {
            return;
        }
        let Ok(live_md) = tokio::fs::metadata(live).await else {
            return;
        };
        // one version per modification time, like stock Nextcloud: an overwrite that keeps the mtime of
        // content we already hold (or that is the very same file) adds nothing
        let target = dir.join(old.mtime.to_string());
        if let Ok(md) = tokio::fs::metadata(&target).await {
            let _ = (
                md.ino() == live_md.ino() && md.dev() == live_md.dev(),
                md.len(),
            );
            return;
        }
        if tokio::fs::hard_link(live, &target).await.is_err() {
            let _ = tokio::fs::copy(live, &target).await; // e.g. a filesystem without hard links
        }
        let (max, d) = (self.versions_max, dir);
        let _ = tokio::task::spawn_blocking(move || {
            let v = Self::read_versions(&d);
            for (ts, _) in v.iter().take(v.len().saturating_sub(max)) {
                let _ = std::fs::remove_file(d.join(ts.to_string()));
            }
        })
        .await;
    }

    /// Versions (names are mtimes) oldest first, from a versions directory.
    fn read_versions(dir: &Path) -> Vec<(i64, i64)> {
        let mut v: Vec<(i64, i64)> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let ts = e.file_name().to_str()?.parse::<i64>().ok()?;
                Some((ts, e.metadata().ok()?.len() as i64))
            })
            .collect();
        v.sort();
        v
    }

    /// All versions of a file, oldest first, the live file last.
    pub async fn versions_list(&self, o: &Owner, node: &Node) -> Result<Vec<VersionInfo>, FsError> {
        let dir = self.versions_dir(o, node.fileid);
        let old = tokio::task::spawn_blocking(move || Self::read_versions(&dir))
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let mut out: Vec<VersionInfo> = old
            .into_iter()
            .filter(|(ts, _)| *ts != node.mtime)
            .map(|(ts, size)| VersionInfo {
                ts,
                size,
                current: false,
            })
            .collect();
        out.push(VersionInfo {
            ts: node.mtime,
            size: node.size,
            current: true,
        });
        out.sort_by_key(|v| v.ts);
        Ok(out)
    }

    /// File holding the content of a version (the live file for the current one).
    pub fn version_file(&self, o: &Owner, node: &Node, ts: i64) -> PathBuf {
        if ts == node.mtime {
            self.fs_path(o, &node.path)
        } else {
            self.versions_dir(o, node.fileid).join(ts.to_string())
        }
    }

    /// Make an older version the live content again. The content it replaces becomes a version itself.
    pub async fn version_restore(&self, o: &Owner, node: &Node, ts: i64) -> Result<Node, FsError> {
        if ts == node.mtime {
            return Ok(node.clone());
        }
        let src = self.versions_dir(o, node.fileid).join(ts.to_string());
        let md = tokio::fs::metadata(&src)
            .await
            .map_err(|_| FsError::NotFound)?;
        let (tmp, file) = self.new_tmp(o).await?;
        drop(file);
        if let Err(e) = tokio::fs::copy(&src, &tmp).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e.into());
        }
        let opts = PutOpts {
            mtime: ts,
            mimetype: node.mimetype.clone(),
            checksum: None,
            if_match: None,
            if_none_match_star: false,
        };
        let (n, _) = self.put(o, &node.path, &tmp, md.len() as i64, opts).await?;
        Ok(n)
    }

    pub async fn version_delete(&self, o: &Owner, node: &Node, ts: i64) -> Result<(), FsError> {
        if ts == node.mtime {
            return Err(FsError::Forbidden("the current version cannot be deleted"));
        }
        tokio::fs::remove_file(self.versions_dir(o, node.fileid).join(ts.to_string()))
            .await
            .map_err(|_| FsError::NotFound)
    }

    // ------------------------------------------------------------------ expiry

    /// Apply the retention settings and remove leftovers. Returns `(trash items, versions)` removed.
    pub async fn expire(
        &self,
        trash_days: u32,
        version_days: u32,
    ) -> Result<(usize, usize), FsError> {
        let now = unix_now();
        let mut trashed = 0;
        let users: Vec<(i64, String)> = sqlx::query_as("SELECT id, uid FROM users")
            .fetch_all(self.pool())
            .await?;
        for (id, uid) in users {
            let o = Owner { id, uid };
            if trash_days > 0 {
                let cutoff = now - i64::from(trash_days) * 86_400;
                let old: Vec<TrashItem> =
                    sqlx::query_as(select_item!("WHERE owner_id = $1 AND deleted_at < $2"))
                        .bind(o.id)
                        .bind(cutoff)
                        .fetch_all(self.pool())
                        .await?;
                for i in &old {
                    self.trash_purge(&o, i).await?;
                }
                trashed += old.len();
            }
            // slots without a row (crash or rollback between the rename and the commit): the data is still
            // there. Adopt it again from its meta.json; without one, leave it alone - never delete it.
            if let Ok(mut rd) = tokio::fs::read_dir(self.trash_root(&o)).await {
                while let Ok(Some(e)) = rd.next_entry().await {
                    let Some(slot_id) = e.file_name().to_str().and_then(|s| s.parse::<i64>().ok())
                    else {
                        continue;
                    };
                    let known: i64 =
                        sqlx::query_scalar("SELECT COUNT(*) FROM trash_items WHERE id = $1")
                            .bind(slot_id)
                            .fetch_one(self.pool())
                            .await?;
                    let fresh = e
                        .metadata()
                        .await
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .is_some_and(|m| m > SystemTime::now() - Duration::from_secs(3600));
                    if known > 0
                        || fresh
                        || !tokio::fs::try_exists(e.path().join("item"))
                            .await
                            .unwrap_or(false)
                    {
                        continue;
                    }
                    match tokio::fs::read(e.path().join("meta.json"))
                        .await
                        .ok()
                        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                    {
                        Some(m) => {
                            let s = |k: &str| m[k].as_str().unwrap_or("").to_string();
                            let n = |k: &str| m[k].as_i64().unwrap_or(0);
                            let mut at = n("deleted_at");
                            while sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM trash_items WHERE owner_id = $1 AND name = $2 AND deleted_at = $3").bind(o.id).bind(s("name")).bind(at).fetch_one(self.pool()).await? > 0 {
                                at += 1;
                            }
                            sqlx::query("INSERT INTO trash_items (id, owner_id, name, orig_dir, deleted_at, is_dir, size, mtime, mimetype, orig_fileid) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)")
                                .bind(slot_id).bind(o.id).bind(s("name")).bind(s("orig_dir")).bind(at)
                                .bind(m["is_dir"].as_bool().unwrap_or(false)).bind(n("size")).bind(n("mtime")).bind(s("mimetype")).bind(n("orig_fileid"))
                                .execute(self.pool())
                                .await?;
                            tracing::warn!(user = %o.uid, slot = slot_id, "adopted an orphaned trash slot");
                        }
                        None => {
                            tracing::warn!(user = %o.uid, slot = slot_id, "orphaned trash slot without meta.json left untouched")
                        }
                    }
                }
            }
        }
        let versions = self.expire_versions(version_days).await?;
        Ok((trashed, versions))
    }

    async fn expire_versions(&self, days: u32) -> Result<usize, FsError> {
        let users: Vec<(i64, String)> = sqlx::query_as("SELECT id, uid FROM users")
            .fetch_all(self.pool())
            .await?;
        let cutoff = SystemTime::now() - Duration::from_secs(u64::from(days) * 86_400);
        let mut removed = 0;
        for (id, uid) in users {
            let o = Owner { id, uid };
            let Ok(mut rd) = tokio::fs::read_dir(self.versions_root(&o)).await else {
                continue;
            };
            while let Ok(Some(dir)) = rd.next_entry().await {
                let Some(fileid) = dir.file_name().to_str().and_then(|s| s.parse::<i64>().ok())
                else {
                    continue;
                };
                // versions of a file that no longer exists (deleted for good, removed outside) go too
                let alive: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM file_cache WHERE fileid = $1")
                        .bind(fileid)
                        .fetch_one(self.pool())
                        .await?;
                if alive == 0 {
                    removed += Self::read_versions(&dir.path()).len();
                    let _ = tokio::fs::remove_dir_all(dir.path()).await;
                    continue;
                }
                if days > 0 {
                    // ctime = when the version was created (the hard link), not the old content's mtime
                    use std::os::unix::fs::MetadataExt;
                    for (ts, _) in Self::read_versions(&dir.path()) {
                        let p = dir.path().join(ts.to_string());
                        let made = std::fs::metadata(&p).map(|m| {
                            SystemTime::UNIX_EPOCH + Duration::from_secs(m.ctime().max(0) as u64)
                        });
                        if made.is_ok_and(|t| t < cutoff) && std::fs::remove_file(&p).is_ok() {
                            removed += 1;
                        }
                    }
                }
                let _ = tokio::fs::remove_dir(dir.path()).await; // only succeeds when empty
            }
        }
        Ok(removed)
    }
}

fn restored_numbered(name: &str, is_dir: bool, n: usize) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if !is_dir && i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    if n == 1 {
        format!("{stem} (restored){ext}")
    } else {
        format!("{stem} (restored {n}){ext}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restored_names() {
        assert_eq!(restored_numbered("dup.txt", false, 1), "dup (restored).txt");
        assert_eq!(
            restored_numbered("dup.txt", false, 3),
            "dup (restored 3).txt"
        );
        assert_eq!(restored_numbered("folder", true, 1), "folder (restored)");
        assert_eq!(restored_numbered(".hidden", false, 1), ".hidden (restored)");
    }

    #[test]
    fn entry_names_and_locations() {
        let i = TrashItem {
            id: 1,
            owner_id: 1,
            name: "b.txt".into(),
            orig_dir: "x/y".into(),
            deleted_at: 1700000000,
            is_dir: false,
            size: 1,
            mtime: 2,
            mimetype: String::new(),
            orig_fileid: 3,
        };
        assert_eq!(
            (i.entry_name().as_str(), i.original_location().as_str()),
            ("b.txt.d1700000000", "x/y/b.txt")
        );
    }
}
