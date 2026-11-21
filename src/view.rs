//! What a user sees of the file tree: nodes under their view path (shares appear as mounts) together
//! with the properties that depend on sharing (permission letters, owner, mount flags, share types).

use std::collections::HashMap;

use crate::paths;
use crate::propfind::NodeExtra;
use crate::shares::{Mount, Resolved, dav_letters, dir_etag, outgoing_by_node, resolve_in};
use crate::storage::{FsError, Node, Owner, Store};

pub struct Item {
    /// `path`/`name` are the viewer's, not the owner's.
    pub node: Node,
    pub extra: NodeExtra,
}

pub struct ViewCtx<'a> {
    store: &'a Store,
    me: &'a Owner,
    me_display: String,
    pub mounts: Vec<Mount>,
    outgoing: HashMap<i64, Vec<(i64, String, String)>>,
}

impl<'a> ViewCtx<'a> {
    pub async fn new(store: &'a Store, me: &'a Owner, me_display: &str) -> Result<Self, FsError> {
        Ok(Self {
            store,
            me,
            me_display: me_display.to_string(),
            mounts: store.mounts(me).await?,
            outgoing: outgoing_by_node(store, me.id).await?,
        })
    }

    pub fn resolve(&self, view: &str) -> Resolved {
        resolve_in(&self.mounts, self.me, view)
    }

    /// A node reached through `r` (the resolution of the node itself or of any ancestor within the
    /// same tree), translated into the viewer's tree.
    pub fn item(&self, r: &Resolved, mut node: Node) -> Item {
        let view = r.view_path(&node.path);
        let mut x = NodeExtra::default();
        if let Some(m) = &r.mount {
            let root = node.fileid == m.source.fileid;
            x.perms = Some(dav_letters(node.is_dir, r.perms, true, root));
            x.owner_uid = Some(m.owner.uid.clone());
            x.owner_display = Some(m.owner_display.clone());
            x.mount_root = root;
            x.mount_type = "shared";
            if root {
                x.note = Some(m.note.clone());
            }
        } else {
            if let Some(s) = self.outgoing.get(&node.fileid) {
                x.shares = s.clone();
            }
            if node.is_dir {
                x.etag = dir_etag(&node, &view, &self.mounts);
            }
        }
        node.name = view.rsplit('/').next().unwrap_or("").to_string();
        node.path = view;
        Item { node, extra: x }
    }

    /// What stock shows as share-type/sharee on a mount root that is requested directly: the viewer's
    /// own share (not the owner's other recipients).
    pub fn viewer_share(&self, m: &Mount) -> (i64, String, String) {
        if m.share_type == crate::shares::TYPE_USER {
            (m.share_type, self.me.uid.clone(), self.me_display.clone())
        } else {
            (m.share_type, m.share_with.clone(), m.share_with.clone())
        }
    }

    fn mount_resolution(m: &Mount) -> Resolved {
        Resolved {
            owner: m.owner.clone(),
            path: m.source.path.clone(),
            perms: m.perms,
            mount: Some(m.clone()),
            is_mount_root: true,
        }
    }

    /// A mount as an entry of the folder that contains it. Stock lists such an entry with "V" (the
    /// mount point itself can be moved) even when the share is read-only; asked directly, the mount
    /// root only has it with the update bit.
    pub fn mount_item(&self, m: &Mount) -> Item {
        let mut it = self.item(&Self::mount_resolution(m), m.source.clone());
        if let Some(p) = it.extra.perms.as_mut()
            && !p.contains('V')
        {
            *p = p.replacen("DN", "DNV", 1);
        }
        it
    }

    /// Direct children of `dir` (resolved by `r`): its own children plus mounts placed directly in it.
    pub async fn children(&self, r: &Resolved, dir: &Node) -> Result<Vec<Item>, FsError> {
        let mut out: Vec<Item> = self
            .store
            .children(dir)
            .await?
            .into_iter()
            .map(|n| self.item(r, n))
            .collect();
        if r.mount.is_none() {
            for m in &self.mounts {
                if paths::split(&m.target).0 == dir.path {
                    out.retain(|i| i.node.path != m.target); // a mount shadows an item of the same name
                    out.push(self.mount_item(m));
                }
            }
        }
        Ok(out)
    }

    /// Everything below `dir`, including the content of mounts that sit below it.
    pub async fn descendants(&self, r: &Resolved, dir: &Node) -> Result<Vec<Item>, FsError> {
        let mut out: Vec<Item> = self
            .store
            .descendants(dir)
            .await?
            .into_iter()
            .map(|n| self.item(r, n))
            .collect();
        if r.mount.is_none() {
            let prefix = if dir.path.is_empty() {
                String::new()
            } else {
                format!("{}/", dir.path)
            };
            for m in self.mounts.iter().filter(|m| m.target.starts_with(&prefix)) {
                let parent = paths::split(&m.target).0.to_string();
                // only mounts whose parent folder really exists in the viewer's tree are visible
                if !(parent == dir.path
                    || out.iter().any(|i| i.node.is_dir && i.node.path == parent))
                {
                    continue;
                }
                out.retain(|i| {
                    i.node.path != m.target && !i.node.path.starts_with(&format!("{}/", m.target))
                });
                let mr = Self::mount_resolution(m);
                out.push(self.item(&mr, m.source.clone()));
                if m.source.is_dir {
                    for n in self.store.descendants(&m.source).await? {
                        out.push(self.item(&mr, n));
                    }
                }
            }
        }
        Ok(out)
    }
}
