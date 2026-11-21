//! Sharing core (Phase 6): permission bits, per-user mounts, path resolution and the values the
//! DAV layer derives from them (permission letters, combined directory ETags).
//!
//! A recipient sees every share as a *mount* somewhere in their own tree (`share_targets.target`).
//! Everything below a mount lives in the owner's tree: `resolve` maps a path in the viewer's tree to
//! `(owner, owner-relative path, permission bits)` and all file operations then run against the
//! owner's tree, with the owner's etag propagation. Nothing is copied or fanned out to recipients.
//! Group shares are evaluated at request time from `group_members`, so membership changes need no hooks.

use std::collections::{HashMap, HashSet};

use sha2::{Digest, Sha256};

use crate::storage::{FsError, Node, Owner, Store};

pub const READ: u32 = 1;
pub const UPDATE: u32 = 2;
pub const CREATE: u32 = 4;
pub const DELETE: u32 = 8;
pub const SHARE: u32 = 16;
pub const ALL: u32 = 31;

pub const TYPE_USER: i64 = 0;
pub const TYPE_GROUP: i64 = 1;

/// One share as the viewer sees it: where it is mounted and what they may do there.
#[derive(Debug, Clone)]
pub struct Mount {
    pub share_id: i64,
    pub share_type: i64,
    /// Recipient the share was made to: the viewer's uid, or the group's id.
    pub share_with: String,
    pub owner: Owner,
    pub owner_display: String,
    /// The shared node in the owner's tree (current path: follows renames/moves by the owner).
    pub source: Node,
    /// Path in the viewer's tree, no leading slash.
    pub target: String,
    pub perms: u32,
    pub note: String,
    pub stime: i64,
}

/// A path in the viewer's tree resolved to the tree that really holds it.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub owner: Owner,
    /// Path inside `owner`'s tree.
    pub path: String,
    /// What the viewer may do here (`ALL` in their own tree).
    pub perms: u32,
    pub mount: Option<Mount>,
    /// The path is exactly the mount point.
    pub is_mount_root: bool,
}

impl Resolved {
    pub fn own(user: &Owner, path: &str) -> Self {
        Self {
            owner: user.clone(),
            path: path.to_string(),
            perms: ALL,
            mount: None,
            is_mount_root: false,
        }
    }

    pub fn is_shared(&self) -> bool {
        self.mount.is_some()
    }

    /// Path in the viewer's tree for a node path in the resolved owner's tree.
    pub fn view_path(&self, owner_path: &str) -> String {
        match &self.mount {
            None => owner_path.to_string(),
            Some(m) => view_path_of(m, owner_path),
        }
    }

    pub fn can(&self, bit: u32) -> bool {
        self.perms & bit != 0
    }
}

/// Viewer-tree path of `owner_path`, which must lie in the mount's source subtree.
pub fn view_path_of(m: &Mount, owner_path: &str) -> String {
    let rest = owner_path
        .strip_prefix(m.source.path.as_str())
        .unwrap_or("")
        .trim_start_matches('/');
    if rest.is_empty() {
        m.target.clone()
    } else {
        format!("{}/{rest}", m.target)
    }
}

/// Pure resolution against an already loaded mount list.
pub fn resolve_in(mounts: &[Mount], user: &Owner, view: &str) -> Resolved {
    let best = mounts
        .iter()
        .filter(|m| {
            view == m.target
                || view
                    .strip_prefix(m.target.as_str())
                    .is_some_and(|r| r.starts_with('/'))
        })
        .max_by_key(|m| m.target.len());
    let Some(m) = best else {
        return Resolved::own(user, view);
    };
    let rest = view[m.target.len()..].trim_start_matches('/');
    let path = match (m.source.path.is_empty(), rest.is_empty()) {
        (_, true) => m.source.path.clone(),
        (true, false) => rest.to_string(),
        (false, false) => format!("{}/{rest}", m.source.path),
    };
    Resolved {
        owner: m.owner.clone(),
        path,
        perms: m.perms,
        mount: Some(m.clone()),
        is_mount_root: rest.is_empty(),
    }
}

/// Permission letters of `oc:permissions` (and `X-NC-Permissions`), as recorded from stock
/// Nextcloud 35 for every combination of share bits.
pub fn dav_letters(is_dir: bool, perms: u32, shared: bool, mount_root: bool) -> String {
    if !shared {
        return if is_dir { "RGDNVCK" } else { "RGDNVW" }.into();
    }
    let (upd, cre, del, shr) = (
        perms & UPDATE != 0,
        perms & CREATE != 0,
        perms & DELETE != 0,
        perms & SHARE != 0,
    );
    let mut s = String::from("S");
    if shr {
        s.push('R');
    }
    s.push('G');
    if mount_root {
        // the mount itself can always be unshared (D) and renamed (N); moving needs update
        s.push_str("DN");
        if upd || !is_dir {
            s.push('V');
        }
        if is_dir {
            if cre {
                s.push_str("CK");
            }
        } else if upd {
            s.push('W');
        }
        return s;
    }
    if del {
        s.push('D');
    }
    if upd || (cre && del) {
        s.push('N');
    }
    if upd {
        s.push('V');
    }
    if is_dir {
        if cre {
            s.push_str("CK");
        }
    } else if upd {
        s.push('W');
    }
    s
}

#[derive(sqlx::FromRow)]
struct MountRow {
    share_id: i64,
    share_type: i64,
    share_with: String,
    permissions: i64,
    stime: i64,
    note: String,
    file_source: i64,
    owner_id: i64,
    owner_uid: String,
    owner_display: String,
    target: Option<String>,
    hidden: i64,
}

/// "proj" -> "proj (2)", "a.txt" -> "a (2).txt"
pub fn numbered(name: &str, n: usize, is_dir: bool) -> String {
    if n <= 1 {
        return name.to_string();
    }
    match name.rfind('.') {
        Some(i) if !is_dir && i > 0 => format!("{} ({n}){}", &name[..i], &name[i..]),
        _ => format!("{name} ({n})"),
    }
}

impl Store {
    /// Names already used at the top of the user's tree (own items and other mounts).
    async fn root_names(&self, user_id: i64) -> Result<HashSet<String>, FsError> {
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT c.name FROM file_cache c JOIN file_cache r ON r.fileid = c.parent_id \
             WHERE c.owner_id = $1 AND r.owner_id = $1 AND r.path = ''",
        )
        .bind(user_id)
        .fetch_all(self.pool())
        .await?;
        let targets: Vec<String> = sqlx::query_scalar(
            "SELECT target FROM share_targets WHERE user_id = $1 AND instr(target, '/') = 0",
        )
        .bind(user_id)
        .fetch_all(self.pool())
        .await?;
        Ok(names.into_iter().chain(targets).collect())
    }

    /// A free name at the user's root for a newly appearing share (stock: "proj (2)").
    pub async fn default_target(&self, user_id: i64, source: &Node) -> Result<String, FsError> {
        let taken = self.root_names(user_id).await?;
        Ok(free_name(&taken, &source.name, source.is_dir))
    }

    /// All shares that apply to `user`, with their mount point. Missing targets (group shares seen
    /// for the first time) are created here.
    pub async fn mounts(&self, user: &Owner) -> Result<Vec<Mount>, FsError> {
        let rows: Vec<MountRow> = sqlx::query_as(
            "SELECT s.id AS share_id, s.share_type, s.share_with, s.permissions, s.stime, s.note, s.file_source, \
                    u.id AS owner_id, u.uid AS owner_uid, u.display_name AS owner_display, \
                    t.target AS target, COALESCE(t.hidden, 0) AS hidden \
             FROM shares s JOIN users u ON u.id = s.owner_id \
             LEFT JOIN share_targets t ON t.share_id = s.id AND t.user_id = $1 \
             WHERE (s.share_type = 0 AND s.share_with = $2) \
                OR (s.share_type = 1 AND s.share_with IN \
                    (SELECT g.gid FROM groups g JOIN group_members m ON m.group_id = g.id WHERE m.user_id = $1)) \
             ORDER BY s.id",
        )
        .bind(user.id)
        .bind(&user.uid)
        .fetch_all(self.pool())
        .await?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let mut taken: Option<HashSet<String>> = None;
        let mut out: Vec<Mount> = Vec::new();
        for r in rows {
            if r.hidden != 0 {
                continue;
            }
            let Some(source) = self.node_by_id(r.file_source).await? else {
                continue;
            };
            let target = match r.target {
                Some(t) => t,
                None => {
                    if taken.is_none() {
                        taken = Some(self.root_names(user.id).await?);
                    }
                    let set = taken.as_mut().expect("just set");
                    let t = free_name(set, &source.name, source.is_dir);
                    set.insert(t.clone());
                    sqlx::query(
                        "INSERT OR IGNORE INTO share_targets (share_id, user_id, target) VALUES ($1, $2, $3)",
                    )
                    .bind(r.share_id)
                    .bind(user.id)
                    .bind(&t)
                    .execute(self.pool())
                    .await?;
                    t
                }
            };
            let perms = r.permissions as u32 & ALL;
            // the same item shared directly and through a group is one mount with the union of bits
            if let Some(existing) = out.iter_mut().find(|m| m.source.fileid == source.fileid) {
                existing.perms |= perms;
                continue;
            }
            out.push(Mount {
                share_id: r.share_id,
                share_type: r.share_type,
                share_with: r.share_with,
                owner: Owner {
                    id: r.owner_id,
                    uid: r.owner_uid,
                },
                owner_display: r.owner_display,
                source,
                target,
                perms,
                note: r.note,
                stime: r.stime,
            });
        }
        Ok(out)
    }

    /// A node by id if the user may see it: their own, or inside one of their shares. Also returns
    /// the owner of the tree that holds the bytes.
    pub async fn visible_by_id(
        &self,
        user: &Owner,
        id: i64,
    ) -> Result<Option<(Node, Owner)>, FsError> {
        let Some(node) = self.node_by_id(id).await? else {
            return Ok(None);
        };
        if node.owner_id == user.id {
            return Ok(Some((node, user.clone())));
        }
        for m in self.mounts(user).await? {
            let inside = node.owner_id == m.owner.id
                && (node.path == m.source.path
                    || node
                        .path
                        .strip_prefix(m.source.path.as_str())
                        .is_some_and(|r| r.starts_with('/')));
            if inside {
                return Ok(Some((node, m.owner)));
            }
        }
        Ok(None)
    }

    /// Resolve a path of the viewer's tree (see module docs).
    pub async fn resolve(&self, user: &Owner, view: &str) -> Result<Resolved, FsError> {
        let mounts = self.mounts(user).await?;
        Ok(resolve_in(&mounts, user, view))
    }
}

fn free_name(taken: &HashSet<String>, name: &str, is_dir: bool) -> String {
    (1..)
        .map(|n| numbered(name, n, is_dir))
        .find(|c| !taken.contains(c))
        .expect("unbounded range")
}

/// Combined ETag of a directory in the viewer's tree that has mounts somewhere below it: it changes
/// when the directory itself changes, when a share appears/disappears/moves, or when anything
/// inside a mounted share changes. `view` is the directory's path in the viewer's tree.
pub fn dir_etag(node: &Node, view: &str, mounts: &[Mount]) -> Option<String> {
    let prefix = if view.is_empty() {
        String::new()
    } else {
        format!("{view}/")
    };
    let below: Vec<&Mount> = mounts
        .iter()
        .filter(|m| m.target.starts_with(&prefix))
        .collect();
    if below.is_empty() {
        return None;
    }
    let mut h = Sha256::new();
    h.update(node.etag.as_bytes());
    for m in below {
        h.update(format!("|{}:{}:{}", m.share_id, m.target, m.source.etag));
    }
    Some(
        h.finalize()[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

/// Shares a user has made, by shared node: type, recipient and recipient display name.
pub async fn outgoing_by_node(
    store: &Store,
    owner_id: i64,
) -> Result<HashMap<i64, Vec<(i64, String, String)>>, FsError> {
    let rows: Vec<(i64, i64, String, Option<String>)> = sqlx::query_as(
        "SELECT s.file_source, s.share_type, s.share_with, u.display_name \
         FROM shares s LEFT JOIN users u ON s.share_type = 0 AND u.uid = s.share_with \
         WHERE s.owner_id = $1 ORDER BY s.id",
    )
    .bind(owner_id)
    .fetch_all(store.pool())
    .await?;
    let mut m: HashMap<i64, Vec<(i64, String, String)>> = HashMap::new();
    for (node, ty, with, display) in rows {
        let display = display.unwrap_or_else(|| with.clone());
        m.entry(node).or_default().push((ty, with, display));
    }
    Ok(m)
}

impl Store {
    /// The recipient removes a share from their own view. User shares are deleted (stock does the
    /// same); group shares are hidden for this user only. Never touches the owner's files.
    pub async fn unshare_for(&self, user: &Owner, source_fileid: i64) -> Result<(), FsError> {
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT s.id, s.share_type FROM shares s WHERE s.file_source = $1 AND \
             ((s.share_type = 0 AND s.share_with = $2) OR (s.share_type = 1 AND s.share_with IN \
              (SELECT g.gid FROM groups g JOIN group_members m ON m.group_id = g.id WHERE m.user_id = $3)))",
        )
        .bind(source_fileid)
        .bind(&user.uid)
        .bind(user.id)
        .fetch_all(self.pool())
        .await?;
        for (id, ty) in rows {
            if ty == TYPE_USER {
                sqlx::query("DELETE FROM shares WHERE id = $1")
                    .bind(id)
                    .execute(self.pool())
                    .await?;
            } else {
                sqlx::query(
                    "INSERT INTO share_targets (share_id, user_id, target, hidden) VALUES ($1, $2, '', 1) \
                     ON CONFLICT (share_id, user_id) DO UPDATE SET hidden = 1",
                )
                .bind(id)
                .bind(user.id)
                .execute(self.pool())
                .await?;
            }
        }
        Ok(())
    }

    /// Rename/move a mount point inside the recipient's own tree.
    pub async fn set_target(
        &self,
        share_id: i64,
        user_id: i64,
        target: &str,
    ) -> Result<(), FsError> {
        sqlx::query(
            "INSERT INTO share_targets (share_id, user_id, target) VALUES ($1, $2, $3) \
             ON CONFLICT (share_id, user_id) DO UPDATE SET target = excluded.target, hidden = 0",
        )
        .bind(share_id)
        .bind(user_id)
        .bind(target)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// COPY between two different trees (own <-> share, share <-> share): files are copied one by one
    /// through the normal `mkcol`/`put`, so every invariant (etags, sizes, index) holds on the
    /// destination side. A MOVE across trees is this followed by deleting the source.
    pub async fn copy_across(
        &self,
        so: &Owner,
        src: &str,
        d: &Owner,
        dst: &str,
        overwrite: bool,
    ) -> Result<(Node, bool), FsError> {
        let node = self.get(so, src).await?.ok_or(FsError::NotFound)?;
        let (dst_parent, _) = crate::paths::split(dst);
        let parent = self
            .get(d, dst_parent)
            .await?
            .ok_or(FsError::ParentNotFound)?;
        if !parent.is_dir {
            return Err(FsError::NotCollection);
        }
        let created = match self.get(d, dst).await? {
            Some(_) if !overwrite => return Err(FsError::PreconditionFailed),
            Some(_) => {
                self.delete(d, dst).await?;
                false
            }
            None => true,
        };
        if node.is_dir {
            self.mkcol(d, dst).await?;
            for n in self.descendants(&node).await? {
                let rel = n.path[node.path.len()..].trim_start_matches('/');
                let target = if dst.is_empty() {
                    rel.to_string()
                } else {
                    format!("{dst}/{rel}")
                };
                if n.is_dir {
                    self.mkcol(d, &target).await?;
                } else {
                    self.copy_file_across(so, &n, d, &target).await?;
                }
            }
        } else {
            self.copy_file_across(so, &node, d, dst).await?;
        }
        let out = self.get(d, dst).await?.ok_or(FsError::NotFound)?;
        Ok((out, created))
    }

    async fn copy_file_across(
        &self,
        so: &Owner,
        n: &Node,
        d: &Owner,
        dst: &str,
    ) -> Result<(), FsError> {
        let (tmp, file) = self.new_tmp(d).await?;
        drop(file);
        if let Err(e) = tokio::fs::copy(self.fs_path(so, &n.path), &tmp).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e.into());
        }
        let opts = crate::storage::PutOpts {
            mtime: n.mtime,
            mimetype: n.mimetype.clone(),
            checksum: n.checksum.clone(),
            if_match: None,
            if_none_match_star: false,
        };
        self.put(d, dst, &tmp, n.size, opts).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(path: &str, is_dir: bool) -> Node {
        Node {
            fileid: 1,
            owner_id: 1,
            parent_id: None,
            name: path.rsplit('/').next().unwrap().to_string(),
            path: path.to_string(),
            is_dir,
            size: 0,
            mtime: 0,
            etag: "e1".into(),
            mimetype: String::new(),
            checksum: None,
        }
    }

    fn mount(target: &str, source: &str, perms: u32) -> Mount {
        Mount {
            share_id: 7,
            share_type: 0,
            share_with: "bob".into(),
            owner: Owner {
                id: 1,
                uid: "alice".into(),
            },
            owner_display: "Alice".into(),
            source: node(source, true),
            target: target.into(),
            perms,
            note: String::new(),
            stime: 0,
        }
    }

    #[test]
    fn letters_match_the_stock_recording() {
        // (is_dir, perms, mount_root) -> letters, recorded from Nextcloud 35.0.3
        let cases = [
            (true, 1, true, "SGDN"),
            (true, 3, true, "SGDNV"),
            (true, 5, true, "SGDNCK"),
            (true, 9, true, "SGDN"),
            (true, 13, true, "SGDNCK"),
            (true, 15, true, "SGDNVCK"),
            (true, 17, true, "SRGDN"),
            (true, 31, true, "SRGDNVCK"),
            (false, 1, false, "SG"), // file below a folder share
            (false, 3, false, "SGNVW"),
            (false, 5, false, "SG"),
            (false, 9, false, "SGD"),
            (false, 13, false, "SGDN"),
            (false, 15, false, "SGDNVW"),
            (false, 31, false, "SRGDNVW"),
            (true, 1, false, "SG"), // folder below a folder share
            (true, 3, false, "SGNV"),
            (true, 5, false, "SGCK"),
            (true, 13, false, "SGDNCK"),
            (true, 15, false, "SGDNVCK"),
            (false, 1, true, "SGDNV"), // single-file share, read only
        ];
        for (dir, p, root, want) in cases {
            assert_eq!(
                dav_letters(dir, p, true, root),
                want,
                "dir={dir} perms={p} root={root}"
            );
        }
        assert_eq!(dav_letters(true, ALL, false, false), "RGDNVCK");
        assert_eq!(dav_letters(false, ALL, false, false), "RGDNVW");
    }

    #[test]
    fn resolves_to_the_owner_tree_and_back() {
        let me = Owner {
            id: 2,
            uid: "bob".into(),
        };
        let ms = vec![mount("proj (2)", "work/proj", 15), mount("Docs/x", "x", 1)];
        let r = resolve_in(&ms, &me, "proj (2)/sub/a.txt");
        assert_eq!(
            (
                r.owner.uid.as_str(),
                r.path.as_str(),
                r.perms,
                r.is_mount_root
            ),
            ("alice", "work/proj/sub/a.txt", 15, false)
        );
        assert_eq!(r.view_path("work/proj/sub/a.txt"), "proj (2)/sub/a.txt");
        let r = resolve_in(&ms, &me, "proj (2)");
        assert!(r.is_mount_root && r.path == "work/proj");
        // similar names are not the mount; a mount deeper in the tree resolves too
        assert!(!resolve_in(&ms, &me, "proj (2)x/a").is_shared());
        assert!(resolve_in(&ms, &me, "Docs/x/y").is_shared());
        assert!(!resolve_in(&ms, &me, "Docs").is_shared());
        assert_eq!(resolve_in(&ms, &me, "own/file").owner.uid, "bob");
    }

    #[test]
    fn numbering_and_combined_etag() {
        assert_eq!(numbered("proj", 2, true), "proj (2)");
        assert_eq!(numbered("a.txt", 3, false), "a (3).txt");
        assert_eq!(numbered(".hidden", 2, false), ".hidden (2)");
        let taken: HashSet<String> = ["proj".into(), "proj (2)".into()].into();
        assert_eq!(free_name(&taken, "proj", true), "proj (3)");

        let root = node("", true);
        let ms = vec![mount("proj", "proj", 15), mount("Docs/x", "x", 1)];
        let a = dir_etag(&root, "", &ms).unwrap();
        // changes when a mounted share changes ...
        let mut ms2 = ms.clone();
        ms2[0].source.etag = "e2".into();
        assert_ne!(dir_etag(&root, "", &ms2).unwrap(), a);
        // ... or disappears; a directory without mounts below keeps its own etag
        assert_ne!(dir_etag(&root, "", &ms[..1]).unwrap(), a);
        assert!(dir_etag(&node("Other", true), "Other", &ms).is_none());
        assert!(dir_etag(&node("Docs", true), "Docs", &ms).is_some());
    }
}
