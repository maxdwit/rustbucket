#!/usr/bin/env python3
"""Differential check of the trash bin and file versions: the same scenario against stock Nextcloud 35 and
rustbucket (user "alice" with password PW on both).

  scripts/diff_trash.py --ref http://127.0.0.1:18490 --ours http://127.0.0.1:18481

Volatile values (deletion times, file ids, the ids/timestamps in hrefs) are blanked; lists are compared
order-independently."""
import argparse, base64, json, re, sys, urllib.error, urllib.parse, urllib.request

ap = argparse.ArgumentParser()
ap.add_argument("--ref", required=True); ap.add_argument("--ours", required=True)
ap.add_argument("--user", default="alice"); ap.add_argument("--password", default="refpass-12345")
a = ap.parse_args()

def call(base, method, path, body=None, headers=None, user=None):
    u = user or a.user
    h = {"Authorization": "Basic " + base64.b64encode(f"{u}:{a.password}".encode()).decode()}
    h.update(headers or {})
    r = urllib.request.Request(base + path, data=body.encode() if isinstance(body, str) else body, method=method, headers=h)
    try: resp = urllib.request.urlopen(r)
    except urllib.error.HTTPError as e: resp = e
    return resp.status if hasattr(resp, "status") else resp.code, resp.read().decode("utf-8", "replace")

PF = '<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns"><d:prop><nc:trashbin-filename/><nc:trashbin-original-location/><nc:trashbin-deletion-time/><nc:trashbin-deleted-by-id/><nc:trashbin-deleted-by-display-name/><d:getcontentlength/><d:getcontenttype/><d:resourcetype/><oc:fileid/><d:getlastmodified/><d:getetag/><oc:permissions/><oc:size/><d:displayname/><nc:version-author/><nc:version-label/></d:prop></d:propfind>'
VOLATILE = {"oc:fileid", "nc:trashbin-deletion-time"}

def parse(xml, user):
    out = {}
    for blk in xml.split("<d:response>")[1:]:
        href = re.search(r"<d:href>([^<]*)", blk).group(1)
        href = re.sub(r"\.d\d+", ".dN", href)
        href = re.sub(r"/versions/\d+/", "/versions/ID/", href)
        href = re.sub(r"/versions/ID/\d+$", lambda m: m.group(0), href)
        props = {}
        for ps in re.findall(r"<d:propstat>(.*?)</d:propstat>", blk, re.S):
            status = re.search(r"<d:status>HTTP/1\.1 (\d+)", ps).group(1)
            for name, val in re.findall(r"<((?:d|oc|nc):[\w-]+)(?:/>|>(.*?)</\1>)", ps.split("</d:prop>")[0].split("<d:prop>")[1], re.S):
                if status != "200": props[name] = f"[{status}]"
                elif name in VOLATILE or (name == "d:getlastmodified" and "/trashbin/" in href): props[name] = "~"
                elif name == "d:getetag" and re.fullmatch(r"\d+", val): props[name] = "~ts"
                else: props[name] = val
        out[href] = props
    return out

def listing(base, path, depth="1"):
    st, b = call(base, "PROPFIND", path, PF, {"Depth": depth})
    return [st, parse(b, a.user) if st == 207 else b[:80]]

def scenario(base):
    r = {}
    F = f"/remote.php/dav/files/{a.user}"; T = f"/remote.php/dav/trashbin/{a.user}"; V = f"/remote.php/dav/versions/{a.user}"
    put = lambda p, body, mt="1700000000": call(base, "PUT", F + p, body, {"X-OC-Mtime": mt})[0]
    put("/doc.txt", "doc"); call(base, "MKCOL", F + "/folder"); put("/folder/b.txt", "inside"); call(base, "MKCOL", F + "/folder/sub"); put("/folder/sub/c.txt", "deep"); put("/gone.txt", "to delete")
    r["delete file"] = call(base, "DELETE", F + "/gone.txt")[0]
    r["delete folder"] = call(base, "DELETE", F + "/folder")[0]
    r["trashbin root"] = listing(base, T + "/")
    r["trash d0"] = listing(base, T + "/trash/", "0")
    r["trash d1"] = listing(base, T + "/trash/")
    names = re.findall(r"<d:href>([^<]*trash/[^<]+?)</d:href>", call(base, "PROPFIND", T + "/trash/", PF, {"Depth": "1"})[1])
    folder = next(n for n in names if "folder.d" in n); gone = next(n for n in names if "gone.txt.d" in n)
    r["inside folder"] = listing(base, folder)
    r["inside sub"] = listing(base, folder + "sub/")
    r["get trashed file"] = call(base, "GET", gone)
    r["get nested"] = call(base, "GET", folder + "sub/c.txt")
    r["restore folder"] = call(base, "MOVE", folder, None, {"Destination": base + T + "/restore/x"})[0]
    r["restored tree"] = [call(base, "GET", F + "/folder/b.txt")[1], call(base, "GET", F + "/folder/sub/c.txt")[1]]
    r["trash after restore"] = listing(base, T + "/trash/")
    # name taken
    put("/dup.txt", "first"); call(base, "DELETE", F + "/dup.txt"); put("/dup.txt", "second")
    dup = next(n for n in re.findall(r"<d:href>([^<]*trash/[^<]+?)</d:href>", call(base, "PROPFIND", T + "/trash/", PF, {"Depth": "1"})[1]) if "dup.txt.d" in n)
    r["restore dup"] = call(base, "MOVE", dup, None, {"Destination": base + T + "/restore/x"})[0]
    r["files after dup"] = sorted(re.findall(r"<d:href>[^<]*/(dup[^<]*)</d:href>", call(base, "PROPFIND", F + "/", PF, {"Depth": "1"})[1]))
    # missing parent
    call(base, "MKCOL", F + "/tmpdir"); put("/tmpdir/o.txt", "orphan"); call(base, "DELETE", F + "/tmpdir/o.txt"); call(base, "DELETE", F + "/tmpdir")
    o = next(n for n in re.findall(r"<d:href>([^<]*trash/[^<]+?)</d:href>", call(base, "PROPFIND", T + "/trash/", PF, {"Depth": "1"})[1]) if "o.txt.d" in n)
    r["orphan entry"] = listing(base, o, "0")
    r["restore orphan"] = call(base, "MOVE", o, None, {"Destination": base + T + "/restore/x"})[0]
    r["orphan restored at top"] = call(base, "GET", F + "/o.txt")[1]
    # errors
    r["restore missing"] = call(base, "MOVE", T + "/trash/nope.d123", None, {"Destination": base + T + "/restore/x"})[0]
    put("/e.txt", "e"); call(base, "DELETE", F + "/e.txt")
    e = next(n for n in re.findall(r"<d:href>([^<]*trash/[^<]+?)</d:href>", call(base, "PROPFIND", T + "/trash/", PF, {"Depth": "1"})[1]) if "e.txt.d" in n)
    r["move elsewhere"] = call(base, "MOVE", e, None, {"Destination": base + F + "/e.txt"})[0]
    r["wrong user"] = call(base, "PROPFIND", f"/remote.php/dav/trashbin/other/trash/", PF, {"Depth": "1"})[0]
    r["delete entry"] = call(base, "DELETE", e)[0]
    r["delete entry again"] = call(base, "DELETE", e)[0]
    put("/f.txt", "f"); call(base, "DELETE", F + "/f.txt")
    r["empty trash"] = call(base, "DELETE", T + "/trash")[0]
    r["trash after empty"] = listing(base, T + "/trash/")
    # versions
    put("/v.txt", "content one", "1700000100"); put("/v.txt", "content two longer", "1700000200"); put("/v.txt", "content three", "1700000300")
    fid = re.search(r"<oc:fileid>(\d+)", call(base, "PROPFIND", F + "/v.txt", PF, {"Depth": "0"})[1]).group(1)
    r["versions root"] = listing(base, V + "/")
    r["versions list"] = listing(base, f"{V}/versions/{fid}")
    r["get version"] = call(base, "GET", f"{V}/versions/{fid}/1700000100")
    r["restore version"] = call(base, "MOVE", f"{V}/versions/{fid}/1700000100", None, {"Destination": base + V + "/restore/target"})[0]
    r["content after restore"] = call(base, "GET", F + "/v.txt")[1]
    r["versions after restore"] = listing(base, f"{V}/versions/{fid}")
    r["version unknown ts"] = call(base, "GET", f"{V}/versions/{fid}/42")[0]
    r["versions unknown id"] = call(base, "PROPFIND", f"{V}/versions/999999", PF, {"Depth": "1"})[0]
    r["versions wrong user"] = call(base, "PROPFIND", f"/remote.php/dav/versions/other/versions/{fid}", PF, {"Depth": "1"})[0]
    r["delete version"] = call(base, "DELETE", f"{V}/versions/{fid}/1700000200")[0]
    r["versions after delete"] = listing(base, f"{V}/versions/{fid}")
    put("/once.txt", "once"); oid = re.search(r"<oc:fileid>(\d+)", call(base, "PROPFIND", F + "/once.txt", PF, {"Depth": "0"})[1]).group(1)
    r["fresh file versions"] = [s for s in [call(base, "PROPFIND", f"{V}/versions/{oid}", PF, {"Depth": "1"})[0]]]
    return r

def delta(x, y, path=""):
    if isinstance(x, dict) and isinstance(y, dict):
        out = []
        for k in sorted(set(x) | set(y)):
            if k not in x: out.append(f"{path}/{k}: only ours = {json.dumps(y[k], ensure_ascii=False)[:90]}")
            elif k not in y: out.append(f"{path}/{k}: only ref = {json.dumps(x[k], ensure_ascii=False)[:90]}")
            else: out += delta(x[k], y[k], f"{path}/{k}")
        return out
    if isinstance(x, list) and isinstance(y, list) and len(x) == len(y):
        return [d for i, (u, v) in enumerate(zip(x, y)) for d in delta(u, v, f"{path}[{i}]")]
    return [] if x == y else [f"{path}: ref={json.dumps(x, ensure_ascii=False)[:90]}  ours={json.dumps(y, ensure_ascii=False)[:90]}"]

ref, ours = scenario(a.ref), scenario(a.ours)
bad = 0
for k in ref:
    d = delta(ref[k], ours.get(k))
    if not d: print(f"  same  {k}")
    else:
        bad += 1; print(f"  DIFF  {k}")
        for line in d[:8]: print(f"          {line}")
print(f"{bad} differences")
sys.exit(1 if bad else 0)
