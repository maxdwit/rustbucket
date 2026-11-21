//! PROPFIND request parsing (namespace aware) and multistatus rendering.

use std::time::{Duration, UNIX_EPOCH};

use quick_xml::NsReader;
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;

use crate::ocs::escape;
use crate::paths;
use crate::storage::Node;

pub const NS_DAV: &str = "DAV:";
pub const NS_OC: &str = "http://owncloud.org/ns";
pub const NS_NC: &str = "http://nextcloud.org/ns";

#[derive(Debug, PartialEq, Eq)]
pub enum PropRequest {
    /// Empty body or `<allprop/>`.
    AllProp,
    PropName,
    Props(Vec<(String, String)>), // (namespace uri, local name)
}

/// Parse a PROPFIND body. Namespaces are matched by URI, never by prefix.
#[derive(Debug, PartialEq, Eq)]
pub struct ParseError;

type PropList = Vec<(String, String)>;

pub fn parse_request(body: &[u8]) -> Result<PropRequest, ParseError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(PropRequest::AllProp);
    }
    let mut r = NsReader::from_reader(body);
    r.config_mut().trim_text(true);
    let mut in_prop = false;
    let mut depth = 0usize; // nesting below <prop>
    let mut props = Vec::new();
    let mut saw_prop = false;
    loop {
        let (ns, ev) = r.read_resolved_event().map_err(|_| ParseError)?;
        let uri = match &ns {
            ResolveResult::Bound(n) => n.as_ref().to_string(),
            _ => String::new(),
        };
        match ev {
            Event::Start(e) => {
                let local = e.local_name().as_ref().to_string();
                if in_prop {
                    if depth == 0 {
                        props.push((uri, local));
                    }
                    depth += 1;
                } else if uri == NS_DAV && local == "prop" {
                    in_prop = true;
                    saw_prop = true;
                    depth = 0;
                }
            }
            Event::Empty(e) => {
                let local = e.local_name().as_ref().to_string();
                if in_prop {
                    if depth == 0 {
                        props.push((uri, local));
                    }
                } else if uri == NS_DAV && local == "allprop" {
                    return Ok(PropRequest::AllProp);
                } else if uri == NS_DAV && local == "propname" {
                    return Ok(PropRequest::PropName);
                } else if uri == NS_DAV && local == "prop" {
                    saw_prop = true;
                }
            }
            Event::End(_) if in_prop => {
                if depth == 0 {
                    in_prop = false;
                } else {
                    depth -= 1;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if saw_prop {
        Ok(PropRequest::Props(props))
    } else {
        Ok(PropRequest::AllProp)
    }
}

enum Val {
    Text(String),
    Empty,
    Raw(String),
}

/// Per-node values that depend on who looks at it: sharing state, effective permissions and owner.
/// `Default` is a plain node in the viewer's own tree.
#[derive(Debug, Clone, Default)]
pub struct NodeExtra {
    /// `oc:permissions` letters; `None` = own item (full rights).
    pub perms: Option<String>,
    /// Owner when the node lives in somebody else's tree.
    pub owner_uid: Option<String>,
    pub owner_display: Option<String>,
    pub mount_root: bool,
    pub mount_type: &'static str,
    /// `(share type, recipient id, recipient display name)` of shares the owner made of this node.
    pub shares: Vec<(i64, String, String)>,
    /// Combined etag of a directory that has mounts below it.
    pub etag: Option<String>,
    pub note: Option<String>,
}

pub struct Ctx<'a> {
    /// href prefix, e.g. `/remote.php/dav/files/alice`
    pub prefix: &'a str,
    pub instance_id: &'a str,
    pub owner_uid: &'a str,
    pub owner_display: &'a str,
}

pub fn permissions(n: &Node) -> &'static str {
    if n.is_dir { "RGDNVCK" } else { "RGDNVW" }
}

pub fn file_id(ctx_instance: &str, fileid: i64) -> String {
    format!("{fileid:08}{ctx_instance}")
}

pub fn http_date(ts: i64) -> String {
    httpdate::fmt_http_date(UNIX_EPOCH + Duration::from_secs(ts.max(0) as u64))
}

fn value(n: &Node, ctx: &Ctx, x: &NodeExtra, ns: &str, name: &str) -> Option<Val> {
    use Val::*;
    Some(match (ns, name) {
        (NS_DAV, "getlastmodified") => Text(http_date(n.mtime)),
        (NS_DAV, "getcontentlength") if !n.is_dir => Text(n.size.to_string()),
        (NS_DAV, "getcontenttype") if !n.is_dir => Text(n.mimetype.clone()),
        (NS_DAV, "resourcetype") => {
            if n.is_dir {
                Raw("<d:collection/>".into())
            } else {
                Empty
            }
        }
        (NS_DAV, "getetag") => Text(format!("\"{}\"", x.etag.as_deref().unwrap_or(&n.etag))),
        (NS_DAV, "quota-used-bytes") if n.is_dir => Text(n.size.to_string()),
        (NS_DAV, "quota-available-bytes") if n.is_dir => Text("-3".into()),
        (NS_OC, "id") => Text(file_id(ctx.instance_id, n.fileid)),
        (NS_OC, "fileid") => Text(n.fileid.to_string()),
        (NS_OC, "size") => Text(n.size.to_string()),
        (NS_OC, "permissions") => Text(x.perms.clone().unwrap_or_else(|| permissions(n).into())),
        (NS_OC, "downloadURL") if !n.is_dir => Empty,
        (NS_OC, "data-fingerprint") => Empty,
        (NS_OC, "share-types") if x.shares.is_empty() => Empty,
        (NS_OC, "share-types") => {
            let mut seen = Vec::new();
            for (t, _, _) in &x.shares {
                if !seen.contains(t) {
                    seen.push(*t);
                }
            }
            Raw(seen
                .iter()
                .map(|t| format!("<oc:share-type>{t}</oc:share-type>"))
                .collect())
        }
        (NS_OC, "owner-id") => Text(x.owner_uid.clone().unwrap_or_else(|| ctx.owner_uid.into())),
        (NS_OC, "owner-display-name") => Text(
            x.owner_display
                .clone()
                .unwrap_or_else(|| ctx.owner_display.into()),
        ),
        (NS_OC, "comments-unread") => Text("0".into()),
        (NS_OC, "favorite") => Text("0".into()),
        (NS_OC, "checksums") => {
            let c = n.checksum.as_ref()?;
            let mut s = String::new();
            escape(c, &mut s);
            return Some(Val::Text(format!("\u{0}{s}"))); // marker: wrap in <oc:checksum>
        }
        (NS_NC, "has-preview") => Text(crate::preview::previewable(&n.mimetype).to_string()),
        (NS_NC, "mount-type") if x.mount_type.is_empty() => Empty,
        (NS_NC, "mount-type") => Text(x.mount_type.into()),
        (NS_NC, "share-attributes") => Text("[]".into()),
        (NS_NC, "sharees") if x.shares.is_empty() => Empty,
        (NS_NC, "sharees") => {
            let mut s = String::new();
            for (t, id, display) in &x.shares {
                s.push_str("<nc:sharee><nc:id>");
                escape(id, &mut s);
                s.push_str("</nc:id><nc:display-name>");
                escape(display, &mut s);
                s.push_str(&format!(
                    "</nc:display-name><nc:type>{t}</nc:type></nc:sharee>"
                ));
            }
            Raw(s)
        }
        (NS_NC, "note") if x.mount_root => Text(x.note.clone().unwrap_or_default()),
        (NS_NC, "is-mount-root") => Text(x.mount_root.to_string()),
        (NS_NC, "creation_time") => Text("0".into()),
        (NS_NC, "system-tags") => Empty,
        _ => return None,
    })
}

const ALLPROP: &[(&str, &str)] = &[
    (NS_DAV, "getlastmodified"),
    (NS_DAV, "getcontentlength"),
    (NS_DAV, "getcontenttype"),
    (NS_DAV, "resourcetype"),
    (NS_DAV, "getetag"),
    (NS_DAV, "quota-used-bytes"),
    (NS_DAV, "quota-available-bytes"),
];

const ALL_KNOWN: &[(&str, &str)] = &[
    (NS_DAV, "getlastmodified"),
    (NS_DAV, "getcontentlength"),
    (NS_DAV, "getcontenttype"),
    (NS_DAV, "resourcetype"),
    (NS_DAV, "getetag"),
    (NS_DAV, "quota-used-bytes"),
    (NS_DAV, "quota-available-bytes"),
    (NS_OC, "id"),
    (NS_OC, "fileid"),
    (NS_OC, "size"),
    (NS_OC, "permissions"),
    (NS_OC, "owner-id"),
    (NS_OC, "owner-display-name"),
    (NS_NC, "has-preview"),
];

fn tag(ns: &str, local: &str) -> String {
    match ns {
        NS_DAV => format!("d:{local}"),
        NS_OC => format!("oc:{local}"),
        NS_NC => format!("nc:{local}"),
        _ => format!("x:{local}"),
    }
}

fn open_tag(ns: &str, local: &str) -> String {
    let t = tag(ns, local);
    if t.starts_with("x:") {
        format!("{t} xmlns:x=\"{}\"", ns.replace('"', "&quot;"))
    } else {
        t
    }
}

pub fn render_response(out: &mut String, n: &Node, ctx: &Ctx, req: &PropRequest, x: &NodeExtra) {
    let (ok_props, missing): (PropList, PropList) = match req {
        PropRequest::AllProp => {
            let wanted: Vec<_> = ALLPROP
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect();
            wanted
                .into_iter()
                .partition(|(a, b)| value(n, ctx, x, a, b).is_some())
        }
        PropRequest::PropName => (
            ALL_KNOWN
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
            vec![],
        ),
        PropRequest::Props(p) => p
            .iter()
            .cloned()
            .partition(|(a, b)| value(n, ctx, x, a, b).is_some()),
    };
    out.push_str("<d:response><d:href>");
    escape(&paths::href(ctx.prefix, &n.path, n.is_dir), out);
    out.push_str("</d:href>");
    if !ok_props.is_empty() {
        out.push_str("<d:propstat><d:prop>");
        for (ns, name) in &ok_props {
            if matches!(req, PropRequest::PropName) {
                out.push_str(&format!("<{}/>", open_tag(ns, name)));
                continue;
            }
            let t = tag(ns, name);
            match value(n, ctx, x, ns, name) {
                Some(Val::Text(s)) if s.starts_with('\u{0}') => {
                    out.push_str(&format!(
                        "<{t}><oc:checksum>{}</oc:checksum></{t}>",
                        &s[1..]
                    ));
                }
                Some(Val::Text(s)) => {
                    out.push_str(&format!("<{t}>"));
                    escape(&s, out);
                    out.push_str(&format!("</{t}>"));
                }
                Some(Val::Raw(raw)) => out.push_str(&format!("<{t}>{raw}</{t}>")),
                Some(Val::Empty) => out.push_str(&format!("<{t}/>")),
                None => {}
            }
        }
        out.push_str("</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>");
    }
    if !missing.is_empty() {
        out.push_str("<d:propstat><d:prop>");
        for (ns, name) in &missing {
            out.push_str(&format!("<{}/>", open_tag(ns, name)));
        }
        out.push_str("</d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>");
    }
    out.push_str("</d:response>");
}

pub fn multistatus(responses: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?>\n<d:multistatus xmlns:d=\"DAV:\" xmlns:s=\"http://sabredav.org/ns\" \
         xmlns:oc=\"{NS_OC}\" xmlns:nc=\"{NS_NC}\">{responses}</d:multistatus>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_body_is_allprop() {
        assert_eq!(parse_request(b"").unwrap(), PropRequest::AllProp);
        assert_eq!(parse_request(b"  \n").unwrap(), PropRequest::AllProp);
        assert_eq!(
            parse_request(
                b"<?xml version=\"1.0\"?><d:propfind xmlns:d=\"DAV:\"><d:allprop/></d:propfind>"
            )
            .unwrap(),
            PropRequest::AllProp
        );
    }

    #[test]
    fn parses_props_by_namespace_not_prefix() {
        // client A: d:/oc: prefixes, client B: default namespace + different prefix letters
        let a = br#"<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><d:getetag/><oc:fileid/></d:prop></d:propfind>"#;
        let b = br#"<propfind xmlns="DAV:" xmlns:o="http://owncloud.org/ns"><prop><getetag/><o:fileid/></prop></propfind>"#;
        let want = PropRequest::Props(vec![
            (NS_DAV.into(), "getetag".into()),
            (NS_OC.into(), "fileid".into()),
        ]);
        assert_eq!(parse_request(a).unwrap(), want);
        assert_eq!(parse_request(b).unwrap(), want);
    }

    #[test]
    fn propname_and_garbage() {
        assert_eq!(
            parse_request(b"<d:propfind xmlns:d=\"DAV:\"><d:propname/></d:propfind>").unwrap(),
            PropRequest::PropName
        );
        assert!(parse_request(b"<<<").is_err());
    }

    fn node(is_dir: bool) -> Node {
        Node {
            fileid: 79,
            owner_id: 1,
            parent_id: Some(1),
            name: "h.txt".into(),
            path: "Test Äö/h.txt".into(),
            is_dir,
            size: 12,
            mtime: 1_700_000_100,
            etag: "abc".into(),
            mimetype: "text/plain".into(),
            checksum: None,
        }
    }

    #[test]
    fn renders_known_and_missing_props() {
        let ctx = Ctx {
            prefix: "/remote.php/dav/files/alice",
            instance_id: "ocx",
            owner_uid: "alice",
            owner_display: "Alice",
        };
        let req = PropRequest::Props(vec![
            (NS_DAV.into(), "getetag".into()),
            (NS_OC.into(), "id".into()),
            (NS_OC.into(), "permissions".into()),
            (NS_OC.into(), "checksums".into()),
            (NS_DAV.into(), "getcontentlength".into()),
        ]);
        let mut out = String::new();
        render_response(&mut out, &node(false), &ctx, &req, &NodeExtra::default());
        assert!(
            out.contains("<d:href>/remote.php/dav/files/alice/Test%20%C3%84%C3%B6/h.txt</d:href>")
        );
        assert!(out.contains("<d:getetag>&quot;abc&quot;</d:getetag>"));
        assert!(out.contains("<oc:id>00000079ocx</oc:id>"));
        assert!(out.contains("<oc:permissions>RGDNVW</oc:permissions>"));
        assert!(out.contains("<d:getcontentlength>12</d:getcontentlength>"));
        // checksums absent -> 404 propstat
        assert!(out.contains("<d:prop><oc:checksums/></d:prop><d:status>HTTP/1.1 404 Not Found"));
        let mut dir = String::new();
        render_response(&mut dir, &node(true), &ctx, &req, &NodeExtra::default());
        assert!(dir.contains("<oc:permissions>RGDNVCK</oc:permissions>"));
        assert!(dir.contains("<d:getcontentlength/>")); // no length on collections -> 404 propstat
    }

    #[test]
    fn http_date_format() {
        assert_eq!(http_date(1_700_000_000), "Tue, 14 Nov 2023 22:13:20 GMT");
    }
}
