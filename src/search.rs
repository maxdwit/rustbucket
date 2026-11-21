//! WebDAV `SEARCH` (RFC 5323 `basicsearch`, as sent by the mobile apps for the media tab and
//! "recent files") and the `REPORT filter-files` favourites query (we have no favourites: empty).

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use percent_encoding::percent_decode_str;
use quick_xml::NsReader;
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;

use crate::auth::AuthUser;
use crate::dav::{internal, owner, sabre_error};
use crate::paths;
use crate::propfind::{self, Ctx, NS_DAV, NS_NC, NS_OC, PropRequest};
use crate::routes::AppState;
use crate::storage::Node;
use crate::view::{Item, ViewCtx};

#[derive(Debug, Default)]
struct El {
    ns: String,
    name: String,
    text: String,
    kids: Vec<El>,
}

impl El {
    fn child(&self, name: &str) -> Option<&El> {
        self.kids.iter().find(|k| k.name == name)
    }
}

fn parse_tree(body: &[u8]) -> Result<El, ()> {
    let mut r = NsReader::from_reader(body);
    r.config_mut().trim_text(true);
    let mut stack: Vec<El> = vec![El::default()];
    loop {
        let (ns, ev) = r.read_resolved_event().map_err(|_| ())?;
        let uri = match &ns {
            ResolveResult::Bound(n) => n.as_ref().to_string(),
            _ => String::new(),
        };
        match ev {
            Event::Start(e) => stack.push(El {
                ns: uri,
                name: e.local_name().as_ref().to_string(),
                ..El::default()
            }),
            Event::Empty(e) => {
                let el = El {
                    ns: uri,
                    name: e.local_name().as_ref().to_string(),
                    ..El::default()
                };
                stack.last_mut().ok_or(())?.kids.push(el);
            }
            Event::Text(t) => {
                let raw: &str = &t;
                stack.last_mut().ok_or(())?.text.push_str(raw);
            }
            Event::GeneralRef(r) => {
                let top = stack.last_mut().ok_or(())?;
                if let Some(c) = r.resolve_char_ref().map_err(|_| ())? {
                    top.text.push(c);
                } else {
                    let name: &str = &r;
                    top.text.push_str(match name {
                        "amp" => "&",
                        "lt" => "<",
                        "gt" => ">",
                        "quot" => "\"",
                        "apos" => "'",
                        _ => return Err(()),
                    });
                }
            }
            Event::End(_) => {
                let el = stack.pop().ok_or(())?;
                stack.last_mut().ok_or(())?.kids.push(el);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let mut root = stack.pop().ok_or(())?;
    if !stack.is_empty() || root.kids.len() != 1 {
        return Err(());
    }
    Ok(root.kids.remove(0))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Eq,
    Gt,
    Gte,
    Lt,
    Lte,
    Like,
}

type Prop = (String, String);

#[derive(Debug)]
enum Cond {
    And(Vec<Cond>),
    Or(Vec<Cond>),
    Not(Box<Cond>),
    Cmp(Op, Prop, String),
    IsCollection,
}

fn parse_cond(el: &El) -> Result<Cond, ()> {
    let op = match el.name.as_str() {
        "and" => {
            return Ok(Cond::And(
                el.kids.iter().map(parse_cond).collect::<Result<_, _>>()?,
            ));
        }
        "or" => {
            return Ok(Cond::Or(
                el.kids.iter().map(parse_cond).collect::<Result<_, _>>()?,
            ));
        }
        "not" => return Ok(Cond::Not(Box::new(parse_cond(el.kids.first().ok_or(())?)?))),
        "is-collection" => return Ok(Cond::IsCollection),
        "eq" => Op::Eq,
        "gt" => Op::Gt,
        "gte" => Op::Gte,
        "lt" => Op::Lt,
        "lte" => Op::Lte,
        "like" => Op::Like,
        _ => return Err(()),
    };
    let prop = el.child("prop").and_then(|p| p.kids.first()).ok_or(())?;
    let lit = el.child("literal").ok_or(())?;
    Ok(Cond::Cmp(
        op,
        (prop.ns.clone(), prop.name.clone()),
        lit.text.clone(),
    ))
}

#[derive(Debug)]
struct SearchReq {
    select: Vec<Prop>,
    scope: String,
    depth: String,
    cond: Option<Cond>,
    order: Vec<(Prop, bool)>,
    first: usize,
    limit: Option<usize>,
}

fn parse_search(body: &[u8]) -> Result<SearchReq, ()> {
    let root = parse_tree(body)?;
    if root.name != "searchrequest" {
        return Err(());
    }
    let bs = root.child("basicsearch").ok_or(())?;
    let select = bs
        .child("select")
        .and_then(|s| s.child("prop"))
        .map(|p| {
            p.kids
                .iter()
                .map(|k| (k.ns.clone(), k.name.clone()))
                .collect()
        })
        .unwrap_or_default();
    let scope_el = bs.child("from").and_then(|f| f.child("scope")).ok_or(())?;
    let scope = scope_el.child("href").ok_or(())?.text.clone();
    let depth = scope_el.child("depth").map_or("infinity".to_string(), |d| {
        d.text.trim().to_ascii_lowercase()
    });
    let cond = match bs.child("where").and_then(|w| w.kids.first()) {
        Some(c) => Some(parse_cond(c)?),
        None => None,
    };
    let mut order = Vec::new();
    if let Some(ob) = bs.child("orderby") {
        for o in ob.kids.iter().filter(|k| k.name == "order") {
            let prop = o.child("prop").and_then(|p| p.kids.first()).ok_or(())?;
            order.push((
                (prop.ns.clone(), prop.name.clone()),
                o.child("descending").is_some(),
            ));
        }
    }
    let num = |name: &str| {
        bs.child("limit")
            .and_then(|l| l.child(name))
            .and_then(|n| n.text.trim().parse::<usize>().ok())
    };
    Ok(SearchReq {
        select,
        scope,
        depth,
        cond,
        order,
        first: num("firstresult").unwrap_or(0),
        limit: num("nresults"),
    })
}

#[derive(Debug, PartialEq, PartialOrd)]
enum V {
    N(i64),
    S(String),
}

fn value(n: &Node, p: &Prop) -> Option<V> {
    Some(match (p.0.as_str(), p.1.as_str()) {
        (NS_DAV, "getcontenttype") => V::S(if n.is_dir {
            "httpd/unix-directory".into()
        } else {
            n.mimetype.clone()
        }),
        (NS_DAV, "displayname") => V::S(n.name.clone()),
        (NS_DAV, "getetag") => V::S(n.etag.clone()),
        (NS_DAV, "getlastmodified") | (NS_NC, "upload_time") => V::N(n.mtime),
        (NS_DAV, "getcontentlength") | (NS_OC, "size") => V::N(n.size),
        (NS_OC, "fileid") => V::N(n.fileid),
        (NS_NC, "creation_time") | (NS_OC, "favorite") => V::N(0),
        _ => return None,
    })
}

/// SQL `LIKE` (`%` any run, `_` one char), case-insensitive.
fn like(pat: &[char], s: &[char]) -> bool {
    match pat.split_first() {
        None => s.is_empty(),
        Some(('%', rest)) => (0..=s.len()).any(|i| like(rest, &s[i..])),
        Some((c, rest)) => s.split_first().is_some_and(|(h, t)| {
            (*c == '_' || c.eq_ignore_ascii_case(h) || c == h) && like(rest, t)
        }),
    }
}

fn literal_number(lit: &str) -> Option<i64> {
    let lit = lit.trim();
    lit.parse::<i64>().ok().or_else(|| {
        httpdate::parse_http_date(lit)
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
    })
}

fn matches(c: &Cond, n: &Node) -> bool {
    match c {
        Cond::And(v) => v.iter().all(|c| matches(c, n)),
        Cond::Or(v) => v.iter().any(|c| matches(c, n)),
        Cond::Not(c) => !matches(c, n),
        Cond::IsCollection => n.is_dir,
        Cond::Cmp(op, prop, lit) => {
            let Some(v) = value(n, prop) else {
                return false;
            };
            match (op, v) {
                (Op::Like, V::S(s)) => like(
                    &lit.chars().collect::<Vec<_>>(),
                    &s.chars().collect::<Vec<_>>(),
                ),
                (Op::Like, V::N(x)) => like(
                    &lit.chars().collect::<Vec<_>>(),
                    &x.to_string().chars().collect::<Vec<_>>(),
                ),
                (op, V::N(x)) => literal_number(lit).is_some_and(|l| match op {
                    Op::Eq => x == l,
                    Op::Gt => x > l,
                    Op::Gte => x >= l,
                    Op::Lt => x < l,
                    Op::Lte => x <= l,
                    Op::Like => false,
                }),
                (op, V::S(s)) => match op {
                    Op::Eq => s == *lit,
                    Op::Gt => s.as_str() > lit.as_str(),
                    Op::Gte => s.as_str() >= lit.as_str(),
                    Op::Lt => s.as_str() < lit.as_str(),
                    Op::Lte => s.as_str() <= lit.as_str(),
                    Op::Like => false,
                },
            }
        }
    }
}

fn bad(msg: &str) -> Response {
    sabre_error(
        StatusCode::BAD_REQUEST,
        "Sabre\\DAV\\Exception\\BadRequest",
        msg,
    )
}

fn multistatus_response(body: String) -> Response {
    let mut r = (StatusCode::MULTI_STATUS, body).into_response();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/xml; charset=\"utf-8\""),
    );
    r
}

/// `SEARCH /remote.php/dav/` with a `basicsearch` body.
pub async fn search(State(st): State<AppState>, user: AuthUser, req: Request) -> Response {
    if req.method().as_str() != "SEARCH" {
        return if req.method() == axum::http::Method::OPTIONS {
            let mut r = StatusCode::OK.into_response();
            r.headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("OPTIONS, SEARCH"));
            r
        } else {
            sabre_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "Sabre\\DAV\\Exception\\MethodNotAllowed",
                "Method not allowed",
            )
        };
    }
    let Ok(body) = axum::body::to_bytes(req.into_body(), 1 << 20).await else {
        return bad("Request body too large");
    };
    let Ok(q) = parse_search(&body) else {
        return bad("Invalid search request");
    };
    // scope: /files/<user>[/path] (optionally with the /remote.php/dav prefix)
    let href = percent_decode_str(&q.scope)
        .decode_utf8_lossy()
        .into_owned();
    let href = href.strip_prefix("/remote.php/dav").unwrap_or(&href);
    let Some(rest) = href.trim_start_matches('/').strip_prefix("files/") else {
        return bad("Search is only supported below /files/<user>");
    };
    let (u, rel) = rest.split_once('/').unwrap_or((rest, ""));
    if u != user.uid {
        return sabre_error(
            StatusCode::FORBIDDEN,
            "Sabre\\DAV\\Exception\\Forbidden",
            "Search is only allowed in your own files",
        );
    }
    let Ok(path) = paths::normalize(rel) else {
        return bad("Invalid scope");
    };
    let me = owner(&user);
    let vc = match ViewCtx::new(&st.store, &me, &user.display_name).await {
        Ok(v) => v,
        Err(e) => return internal(e),
    };
    let r = vc.resolve(&path);
    let scope = match st.store.get(&r.owner, &r.path).await {
        Ok(Some(n)) => n,
        Ok(None) => {
            return sabre_error(
                StatusCode::NOT_FOUND,
                "Sabre\\DAV\\Exception\\NotFound",
                "Search scope not found",
            );
        }
        Err(e) => return internal(e),
    };
    let candidates = match q.depth.as_str() {
        "0" => Ok(vec![vc.item(&r, scope.clone())]),
        "1" => vc.children(&r, &scope).await,
        _ => vc.descendants(&r, &scope).await,
    };
    let mut hits: Vec<Item> = match candidates {
        Ok(c) => c
            .into_iter()
            .filter(|i| q.cond.as_ref().is_none_or(|c| matches(c, &i.node)))
            .collect(),
        Err(e) => return internal(e),
    };
    hits.sort_by(|a, b| {
        for (prop, desc) in &q.order {
            let o = value(&a.node, prop)
                .partial_cmp(&value(&b.node, prop))
                .unwrap_or(std::cmp::Ordering::Equal);
            let o = if *desc { o.reverse() } else { o };
            if o != std::cmp::Ordering::Equal {
                return o;
            }
        }
        a.node.path.cmp(&b.node.path)
    });
    let hits = hits
        .into_iter()
        .skip(q.first)
        .take(q.limit.unwrap_or(usize::MAX));
    let instance = match st.store.instance_id().await {
        Ok(i) => i,
        Err(e) => return internal(e),
    };
    let prefix = format!("/remote.php/dav/files/{}", user.uid);
    let ctx = Ctx {
        prefix: &prefix,
        instance_id: &instance,
        owner_uid: &user.uid,
        owner_display: &user.display_name,
    };
    let preq = if q.select.is_empty() {
        PropRequest::AllProp
    } else {
        PropRequest::Props(q.select.clone())
    };
    let mut out = String::new();
    for it in hits {
        propfind::render_response(&mut out, &it.node, &ctx, &preq, &it.extra);
    }
    multistatus_response(propfind::multistatus(&out))
}

/// `REPORT` below `/remote.php/dav/files/<user>`: only `oc:filter-files` (favourites/tags) is
/// known; nothing is ever favourited or tagged, so the answer is an empty multistatus.
pub async fn report(body: &[u8]) -> Response {
    let Ok(root) = parse_tree(body) else {
        return bad("Invalid XML body");
    };
    if root.name == "filter-files" {
        return multistatus_response(propfind::multistatus(""));
    }
    sabre_error(
        StatusCode::NOT_IMPLEMENTED,
        "Sabre\\DAV\\Exception\\ReportNotSupported",
        "The report you requested is not supported on this url.",
    )
}

/// Entry point for fuzzing: must never panic, whatever the bytes are.
#[doc(hidden)]
pub fn fuzz_search_request(data: &[u8]) -> bool {
    parse_search(data).is_ok()
}

/// Entry point for fuzzing the REPORT body parser.
#[doc(hidden)]
pub fn fuzz_report_body(data: &[u8]) -> bool {
    parse_tree(data).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = r#"<?xml version="1.0"?>
<d:searchrequest xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns"><d:basicsearch>
<d:select><d:prop><oc:fileid/><d:getcontenttype/></d:prop></d:select>
<d:from><d:scope><d:href>/files/alice/Photos</d:href><d:depth>infinity</d:depth></d:scope></d:from>
<d:where><d:and><d:or><d:like><d:prop><d:getcontenttype/></d:prop><d:literal>image/%</d:literal></d:like>
<d:like><d:prop><d:getcontenttype/></d:prop><d:literal>video/%</d:literal></d:like></d:or>
<d:gt><d:prop><d:getlastmodified/></d:prop><d:literal>1000</d:literal></d:gt></d:and></d:where>
<d:orderby><d:order><d:prop><d:getlastmodified/></d:prop><d:descending/></d:order></d:orderby>
<d:limit><d:nresults>5</d:nresults><d:firstresult>2</d:firstresult></d:limit></d:basicsearch></d:searchrequest>"#;

    #[test]
    fn parses_a_media_tab_search() {
        let q = parse_search(BODY.as_bytes()).unwrap();
        assert_eq!(q.select.len(), 2);
        assert_eq!(q.scope, "/files/alice/Photos");
        assert_eq!(q.order.len(), 1);
        assert!(q.order[0].1);
        assert_eq!((q.first, q.limit), (2, Some(5)));
        assert!(matches!(q.cond, Some(Cond::And(_))));
    }

    #[test]
    fn like_is_case_insensitive_with_wildcards() {
        let l = |p: &str, s: &str| {
            like(
                &p.chars().collect::<Vec<_>>(),
                &s.chars().collect::<Vec<_>>(),
            )
        };
        assert!(l("image/%", "image/png") && l("%.JPG", "a.jpg") && l("a_c", "abc"));
        assert!(!l("image/%", "video/mp4") && !l("a_c", "ac"));
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(parse_search(b"<nope/>").is_err());
        assert!(parse_search(b"not xml <").is_err());
    }
}
