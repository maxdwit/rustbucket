#!/usr/bin/env python3
"""Differential check of sharing: the same scenario against stock Nextcloud 35 and rustbucket.

  scripts/diff_shares.py --ref http://127.0.0.1:18490 --ours http://127.0.0.1:18481

Both servers need users alice, bob, carol (password PW), group "team" containing bob and nothing else
shared yet. Prints every difference (volatile ids/times blanked); exit 1 if any. Stock rate-limits
share creation to 20 per 10 minutes and user, so run it on a fresh reference container."""
import argparse, base64, json, re, sys, urllib.error, urllib.parse, urllib.request

ap = argparse.ArgumentParser()
ap.add_argument("--ref", required=True); ap.add_argument("--ours", required=True)
ap.add_argument("--password", default="refpass-12345")
a = ap.parse_args()

def call(base, user, method, path, body=None, headers=None):
    h = {"OCS-APIRequest": "true", "Authorization": "Basic " + base64.b64encode(f"{user}:{a.password}".encode()).decode()}
    h.update(headers or {})
    data = body.encode() if isinstance(body, str) else body
    r = urllib.request.Request(base + path, data=data, method=method, headers=h)
    try: resp = urllib.request.urlopen(r)
    except urllib.error.HTTPError as e: resp = e
    return resp.status if hasattr(resp, "status") else resp.code, resp.read()

OCS = "/ocs/v2.php/apps/files_sharing/api/v1"
FORM = {"Content-Type": "application/x-www-form-urlencoded"}
def dav(base, user, method, path, body=None, headers=None):
    return call(base, user, method, f"/remote.php/dav/files/{user}{urllib.parse.quote(path)}", body, headers)[0]
def ocs(base, user, method, path, body=None):
    st, b = call(base, user, method, f"{OCS}{path}{'&' if '?' in path else '?'}format=json", body, FORM)
    try: return st, json.loads(b)["ocs"]
    except Exception: return st, {"raw": b[:100].decode("utf-8", "replace")}

# has_preview: stock also renders text previews, we only decode images (documented deviation)
VOLATILE = {"id", "stime", "item_mtime", "file_parent", "file_source", "item_source", "storage", "item_size", "parent", "has_preview"}
def norm(x):
    if isinstance(x, dict): return {k: ("~" if k in VOLATILE else norm(v)) for k, v in x.items()}
    if isinstance(x, (list, tuple)): return [norm(v) for v in x]
    return x

PF = '<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns"><d:prop><oc:permissions/><oc:share-types/><oc:owner-id/><oc:owner-display-name/><nc:mount-type/><nc:is-mount-root/><nc:sharees/></d:prop></d:propfind>'
def pf(base, user, path, depth, only=()):
    st, b = call(base, user, "PROPFIND", f"/remote.php/dav/files/{user}{urllib.parse.quote(path)}", PF, {"Depth": depth})
    out = {}
    for blk in b.decode().split("<d:response>")[1:]:
        href = re.search(r"<d:href>([^<]*)", blk).group(1).replace(f"/remote.php/dav/files/{user}", "") or "/"
        if only and not any(o in href for o in only): continue
        props = {m[0]: m[1] for m in re.findall(r"<(?:oc|nc):(permissions|owner-id|owner-display-name|mount-type|is-mount-root)>([^<]*)<", blk)}
        props.setdefault("mount-type", "")   # <nc:mount-type/> is the empty value
        props["share-types"] = re.findall(r"<oc:share-type>(\d)", blk)
        props["sharees"] = re.findall(r"<nc:id>([^<]*)</nc:id>", blk)
        out[href] = props
    return st, out

def scenario(base):
    r = {}
    A = lambda m, p, b=None, h=None: dav(base, "alice", m, p, b, h)
    A("MKCOL", "/proj"); A("PUT", "/proj/a.txt", "a-content"); A("MKCOL", "/proj/sub"); A("PUT", "/proj/sub/deep.txt", "deep"); A("PUT", "/note.txt", "n")
    B = lambda m, p, b=None, h=None: dav(base, "bob", m, p, b, h)
    B("MKCOL", "/Documents"); B("PUT", "/own.txt", "bobs")
    # create
    r["create folder 15 -> bob"] = ocs(base, "alice", "POST", "/shares", "shareType=0&shareWith=bob&path=/proj&permissions=15")
    r["create file 1 -> bob"] = ocs(base, "alice", "POST", "/shares", "shareType=0&shareWith=bob&path=/note.txt&permissions=1")
    r["create folder default -> carol"] = ocs(base, "alice", "POST", "/shares", "shareType=0&shareWith=carol&path=/proj")
    r["create file default -> carol"] = ocs(base, "alice", "POST", "/shares", "shareType=0&shareWith=carol&path=/note.txt")
    # errors
    for name, form in [("no user", "shareType=0&shareWith=nobody&path=/proj"), ("bad path", "shareType=0&shareWith=bob&path=/nope"),
                       ("self", "shareType=0&shareWith=alice&path=/proj"), ("bad perms", "shareType=0&shareWith=bob&path=/proj&permissions=99"),
                       ("no group", "shareType=1&shareWith=nogroup&path=/proj"), ("no path", "shareType=0&shareWith=bob"),
                       ("unknown type", "shareType=99&shareWith=bob&path=/proj")]:
        st, o = ocs(base, "alice", "POST", "/shares", form); r["error " + name] = (st, o.get("meta"))
    # lists / get / update / recipient views
    r["list alice"] = ocs(base, "alice", "GET", "/shares")
    r["list alice path"] = ocs(base, "alice", "GET", "/shares?path=/proj")
    r["list bob incoming"] = ocs(base, "bob", "GET", "/shares?shared_with_me=true")
    st, o = r["list alice"]; ids = [s["id"] for s in o["data"]] if isinstance(o.get("data"), list) else []
    if ids:
        r["get as owner"] = ocs(base, "alice", "GET", f"/shares/{ids[0]}")
        r["get as recipient"] = ocs(base, "bob", "GET", f"/shares/{ids[0]}")
        r["update by recipient"] = ocs(base, "bob", "PUT", f"/shares/{ids[0]}", "permissions=1")
        r["update no params"] = ocs(base, "alice", "PUT", f"/shares/{ids[0]}")
        r["update permissions"] = ocs(base, "alice", "PUT", f"/shares/{ids[0]}", "permissions=1")
    r["get missing"] = ocs(base, "alice", "GET", "/shares/987654")
    r["sharees b"] = ocs(base, "alice", "GET", "/sharees?search=b&itemType=file&lookup=false&perPage=200")
    r["sharees exact team"] = ocs(base, "alice", "GET", "/sharees?search=team&itemType=file&lookup=false")
    # recipient tree
    r["bob root"] = pf(base, "bob", "/", "1", only=("proj", "note", "own"))
    r["bob inside proj (read only now)"] = pf(base, "bob", "/proj", "infinity")
    r["alice view"] = pf(base, "alice", "/", "1", only=("proj", "note"))
    # operations by bob in the read-only share
    d = lambda p: {"Destination": f"{base}/remote.php/dav/files/bob{urllib.parse.quote(p)}"}
    r["ro PUT new"] = B("PUT", "/proj/new.txt", "x")
    r["ro PUT overwrite"] = B("PUT", "/proj/a.txt", "x")
    r["ro MKCOL"] = B("MKCOL", "/proj/d2")
    r["ro DELETE"] = B("DELETE", "/proj/a.txt")
    r["ro MOVE"] = B("MOVE", "/proj/a.txt", None, d("/proj/b.txt"))
    r["ro COPY within"] = B("COPY", "/proj/a.txt", None, d("/proj/c.txt"))
    r["ro COPY out"] = B("COPY", "/proj/a.txt", None, d("/copy-of-a.txt"))
    # read-write: set 15 again
    ocs(base, "alice", "PUT", f"/shares/{ids[0]}", "permissions=15")
    r["rw PUT new"] = B("PUT", "/proj/new.txt", "x")
    r["rw PUT overwrite"] = B("PUT", "/proj/a.txt", "x2")
    r["rw MOVE inside"] = B("MOVE", "/proj/new.txt", None, d("/proj/sub/new.txt"))
    r["rw MOVE own->share"] = B("MOVE", "/own.txt", None, d("/proj/own.txt"))
    r["rw MOVE share->own"] = B("MOVE", "/proj/own.txt", None, d("/taken.txt"))
    r["rw alice sees"] = (dav(base, "alice", "GET", "/proj/a.txt"), dav(base, "alice", "GET", "/proj/own.txt"), dav(base, "alice", "GET", "/proj/sub/new.txt"))
    # mount point operations
    r["name collision"] = None
    r["mount rename"] = B("MOVE", "/proj", None, d("/proj-renamed"))
    r["mount into subfolder"] = B("MOVE", "/proj-renamed", None, d("/Documents/proj-sub"))
    r["bob root after move"] = pf(base, "bob", "/Documents", "1", only=("proj",))
    r["mount into share"] = B("MOVE", "/Documents/proj-sub", None, d("/Documents/proj-sub/inner"))
    r["mount delete"] = B("DELETE", "/Documents/proj-sub")
    r["alice data intact"] = (dav(base, "alice", "GET", "/proj/a.txt"), dav(base, "alice", "GET", "/proj/sub/deep.txt"))
    r["list bob after unshare"] = ocs(base, "bob", "GET", "/shares?shared_with_me=true")
    r["list alice after unshare"] = ocs(base, "alice", "GET", "/shares")
    r["alice deletes share"] = ocs(base, "alice", "DELETE", f"/shares/{ids[1]}") if len(ids) > 1 else None
    # group share: bob is a member of "team", carol is not
    A("MKCOL", "/Gdir"); A("PUT", "/Gdir/in.txt", "inside")
    r["create group share"] = ocs(base, "alice", "POST", "/shares", "shareType=1&shareWith=team&path=/Gdir&permissions=15")
    r["group: bob sees mount"] = pf(base, "bob", "/", "1", only=("Gdir",))
    r["group: carol does not"] = pf(base, "carol", "/", "1", only=("Gdir",))
    r["group: bob reads"] = B("GET", "/Gdir/in.txt")
    r["group: carol cannot read"] = dav(base, "carol", "GET", "/Gdir/in.txt")
    r["group: bob writes"] = B("PUT", "/Gdir/from-bob.txt", "b")
    r["group: bob incoming list"] = ocs(base, "bob", "GET", "/shares?shared_with_me=true")
    r["group: alice list by path"] = ocs(base, "alice", "GET", "/shares?path=/Gdir")
    r["group: bob removes it for himself"] = B("DELETE", "/Gdir")
    r["group: bob view after"] = pf(base, "bob", "/", "1", only=("Gdir",))
    r["group: bob incoming after"] = ocs(base, "bob", "GET", "/shares?shared_with_me=true")
    r["group: alice still shares"] = ocs(base, "alice", "GET", "/shares?path=/Gdir")
    r["group: alice data intact"] = (dav(base, "alice", "GET", "/Gdir/in.txt"), dav(base, "alice", "GET", "/Gdir/from-bob.txt"))
    return r

def delta(x, y, path=""):
    """Field-level differences between two normalised structures."""
    if isinstance(x, dict) and isinstance(y, dict):
        out = []
        for k in sorted(set(x) | set(y)):
            if k not in x: out.append(f"{path}/{k}: only ours = {json.dumps(y[k], ensure_ascii=False)[:80]}")
            elif k not in y: out.append(f"{path}/{k}: only ref = {json.dumps(x[k], ensure_ascii=False)[:80]}")
            else: out += delta(x[k], y[k], f"{path}/{k}")
        return out
    if isinstance(x, list) and isinstance(y, list) and len(x) == len(y):
        out = []
        for i, (u, v) in enumerate(zip(x, y)): out += delta(u, v, f"{path}[{i}]")
        return out
    return [] if x == y else [f"{path}: ref={json.dumps(x, ensure_ascii=False)[:80]}  ours={json.dumps(y, ensure_ascii=False)[:80]}"]

ref, ours = scenario(a.ref), scenario(a.ours)
bad = 0
for k in ref:
    d = delta(norm(ref[k]), norm(ours.get(k)))
    if not d: print(f"  same  {k}")
    else:
        bad += 1; print(f"  DIFF  {k}")
        for line in d[:8]: print(f"          {line}")
print(f"{bad} differences")
sys.exit(1 if bad else 0)
