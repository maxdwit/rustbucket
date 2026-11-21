#!/usr/bin/env python3
"""Differential check of the mobile-client endpoints: same requests against stock Nextcloud 35 and rustbucket.

  scripts/diff_mobile.py --ref http://127.0.0.1:18490 --ours http://127.0.0.1:18481 [--user admin --password ...]

Both servers need the same user. Compares status, content-type, selected headers and a normalised body
(JSON/XML structure with ids/times blanked, PNG sizes). Differences are printed; exit 1 if any are not
in KNOWN (documented deviations)."""
import argparse, base64, io, json, re, struct, sys, urllib.request, urllib.error, zlib

ap = argparse.ArgumentParser()
ap.add_argument("--ref", required=True); ap.add_argument("--ours", required=True)
ap.add_argument("--user", default="admin"); ap.add_argument("--password", default="refpass-12345")
a = ap.parse_args()

def png(w, h):
    raw = b"".join(b"\x00" + bytes(v for x in range(w) for v in (x * 255 // w, y * 255 // h, 128)) for y in range(h))
    ch = lambda t, d: struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)
    return b"\x89PNG\r\n\x1a\n" + ch(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) + ch(b"IDAT", zlib.compress(raw)) + ch(b"IEND", b"")

def req(base, method, path, body=None, headers=None, auth=True):
    h = {"OCS-APIRequest": "true"}
    if auth: h["Authorization"] = "Basic " + base64.b64encode(f"{a.user}:{a.password}".encode()).decode()
    h.update(headers or {})
    r = urllib.request.Request(base + path, data=body, method=method, headers=h)
    try: resp = urllib.request.urlopen(r)
    except urllib.error.HTTPError as e: resp = e
    return resp.status if hasattr(resp, "status") else resp.code, resp.headers, resp.read()

def norm_xml(b):
    s = b.decode("utf-8", "replace")
    s = re.sub(r"<(oc:fileid|oc:id|d:getetag|d:getlastmodified|nc:upload_time)>[^<]*<", r"<\1>X<", s)
    s = re.sub(r"\s+", " ", s).strip()
    s = re.sub(r"<([\w:]+)([^>]*)></\1>", r"<\1\2/>", s)   # <a></a> == <a/>
    return s

def shape(status, headers, body):
    ct = headers.get("Content-Type", "").split(";")[0]
    out = {"status": status, "ct": ct, "cache": headers.get("Cache-Control")}
    if ct == "image/png":
        out["png"] = struct.unpack(">II", body[16:24])
    elif ct == "application/json":
        try: out["json"] = json.loads(body)
        except Exception: out["body"] = body[:80]
    elif "xml" in ct: out["xml"] = norm_xml(body)
    else: out["len"] = len(body) if status >= 400 else None
    return out

PROP = b'<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><oc:fileid/></d:prop></d:propfind>'
SEARCH = b'''<?xml version="1.0"?><d:searchrequest xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns"><d:basicsearch><d:select><d:prop><d:getcontenttype/><d:getcontentlength/><nc:has-preview/></d:prop></d:select><d:from><d:scope><d:href>/files/USER</d:href><d:depth>infinity</d:depth></d:scope></d:from><d:where><d:and><d:like><d:prop><d:getcontenttype/></d:prop><d:literal>image/%</d:literal></d:like><d:like><d:prop><d:displayname/></d:prop><d:literal>difftest-%</d:literal></d:like></d:and></d:where><d:orderby><d:order><d:prop><d:getlastmodified/></d:prop><d:descending/></d:order></d:orderby><d:limit><d:nresults>100</d:nresults></d:limit></d:basicsearch></d:searchrequest>'''.replace(b"USER", a.user.encode())
FAV = b'<?xml version="1.0"?><oc:filter-files xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><oc:fileid/></d:prop><oc:filter-rules><oc:favorite>1</oc:favorite></oc:filter-rules></oc:filter-files>'

# deviations we accept on purpose (documented): previews are re-encoded by a different library
KNOWN = {"preview png bytes": "pixel data differs by encoder, sizes and headers are compared"}

def setup(base):
    req(base, "PUT", f"/remote.php/dav/files/{a.user}/difftest-pic.png", png(800, 600), {"X-OC-Mtime": "1600000000"})
    req(base, "PUT", f"/remote.php/dav/files/{a.user}/difftest-note.txt", b"hello", {"X-OC-Mtime": "1600000000"})
    req(base, "MKCOL", f"/remote.php/dav/files/{a.user}/difftest-dir")
    st, _, b = req(base, "PROPFIND", f"/remote.php/dav/files/{a.user}/difftest-pic.png", PROP, {"Depth": "0"})
    return re.search(rb"<oc:fileid>(\d+)<", b).group(1).decode()

ids = {"ref": setup(a.ref), "ours": setup(a.ours)}
CASES = [
 ("preview cover", "GET", "/index.php/core/preview.png?file=/difftest-pic.png&x=64&y=64&a=1&mode=cover&forceIcon=0", None, {}),
 ("preview by id", "GET", "/index.php/core/preview?fileId={id}&x=128&y=128&a=true", None, {}),
 ("preview no params", "GET", "/index.php/core/preview.png?file=/difftest-pic.png", None, {}),
 ("preview text file", "GET", "/index.php/core/preview.png?file=/difftest-note.txt&x=64&y=64&a=1", None, {}),
 ("preview dir", "GET", "/index.php/core/preview.png?file=/difftest-dir&x=64&y=64", None, {}),
 ("preview missing id", "GET", "/index.php/core/preview?fileId=99999999&x=64&y=64", None, {}),
 ("preview no file", "GET", "/index.php/core/preview.png?x=64&y=64", None, {}),
 ("preview x=0", "GET", "/index.php/core/preview.png?file=/difftest-pic.png&x=0&y=0", None, {}),
 ("thumbnail", "GET", "/index.php/apps/files/api/v1/thumbnail/64/64/difftest-pic.png", None, {}),
 ("notifications", "GET", "/ocs/v2.php/apps/notifications/api/v2/notifications?format=json", None, {}),
 ("notification 1", "GET", "/ocs/v2.php/apps/notifications/api/v2/notifications/1?format=json", None, {}),
 ("notification delete", "DELETE", "/ocs/v2.php/apps/notifications/api/v2/notifications/5?format=json", None, {}),
 ("user_status", "GET", "/ocs/v2.php/apps/user_status/api/v1/user_status?format=json", None, {}),
 ("predefined_statuses", "GET", "/ocs/v2.php/apps/user_status/api/v1/predefined_statuses?format=json", None, {}),
 ("wipe check", "POST", "/index.php/core/wipe/check", b"token=abc", {"Content-Type": "application/x-www-form-urlencoded"}),
 ("wipe success", "POST", "/index.php/core/wipe/success", b"token=abc", {"Content-Type": "application/x-www-form-urlencoded"}),
 ("shares", "GET", "/ocs/v2.php/apps/files_sharing/api/v1/shares?format=json&path=/difftest-pic.png&reshares=true", None, {}),
 ("search media", "SEARCH", "/remote.php/dav/", SEARCH, {"Content-Type": "text/xml"}),
 ("report favorites", "REPORT", f"/remote.php/dav/files/{a.user}/", FAV, {}),
 ("has-preview", "PROPFIND", f"/remote.php/dav/files/{a.user}/difftest-pic.png", b'<d:propfind xmlns:d="DAV:" xmlns:nc="http://nextcloud.org/ns"><d:prop><nc:has-preview/></d:prop></d:propfind>', {"Depth": "0"}),
 ("preview noauth", "GET", "/index.php/core/preview.png?file=/difftest-pic.png&x=64&y=64", None, {"_noauth": 1}),
]
bad = 0
for name, m, p, body, h in CASES:
    res = {}
    for who, base in (("ref", a.ref), ("ours", a.ours)):
        h2 = dict(h); noauth = h2.pop("_noauth", None)
        res[who] = shape(*req(base, m, p.replace("{id}", ids[who]), body, h2, auth=not noauth))
    if res["ref"] == res["ours"]: print(f"  same  {name}")
    else:
        bad += 1; print(f"  DIFF  {name}")
        for k in sorted(set(res["ref"]) | set(res["ours"])):
            if res["ref"].get(k) != res["ours"].get(k):
                print(f"        {k}: ref={json.dumps(res['ref'].get(k), ensure_ascii=False)[:300]}\n        {' ' * len(k)}  ours={json.dumps(res['ours'].get(k), ensure_ascii=False)[:300]}")
print(f"{bad} differences")
sys.exit(1 if bad else 0)
