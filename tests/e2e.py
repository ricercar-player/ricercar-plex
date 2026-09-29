#!/usr/bin/env python3
"""End-to-end test of the plugin against a live Plex Media Server (see
plex.sh): JSON-RPC over stdio, the streams with ffprobe. The server is
unclaimed and open to the test network, so any token passes; the plex.tv
sign-in (PIN) cannot be tested here beyond creating the PIN."""
import json, subprocess, sys, threading, queue, urllib.request, time, os, stat
S = sys.argv[1]
BIN = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "release", "ricercar-plex")
PMS = "http://127.0.0.1:32400"
DATA = S + "/pdata"; os.makedirs(DATA, exist_ok=True)
OUT = {"device": "hw:9,0", "bit_perfect": True, "max_rate": 96000, "max_bits": 24, "rates": [44100, 48000, 88200, 96000]}

class P:
    def __init__(s, *args):
        s.p = subprocess.Popen([BIN, *args], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=open(S + "/plugin.log", "a"), text=True)
        s.q = {}; s.notes = queue.Queue(); s.n = 0
        threading.Thread(target=s.read, daemon=True).start()
    def read(s):
        for l in s.p.stdout:
            m = json.loads(l)
            if "id" in m: s.q[m["id"]].put(m)
            else: s.notes.put(m)
    def call(s, method, params=None):
        s.n += 1; i = s.n; s.q[i] = queue.Queue()
        s.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": i, "method": method, "params": params or {}}) + "\n"); s.p.stdin.flush()
        m = s.q[i].get(timeout=30); return m.get("result", m.get("error"))
    def notify(s, method, params):
        s.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": method, "params": params}) + "\n"); s.p.stdin.flush()

def pms(path):
    r = urllib.request.Request(PMS + path, headers={"Accept": "application/json"})
    return json.load(urllib.request.urlopen(r))["MediaContainer"]

def probe(url, path):
    open(path, "wb").write(urllib.request.urlopen(url).read())
    return subprocess.run(["ffprobe", "-v", "error", "-show_entries", "stream=sample_rate,bits_per_raw_sample",
                           "-of", "csv=p=0", path], capture_output=True, text=True).stdout.strip()

bad = 0
def check(c, msg):
    global bad
    print(("PASS " if c else "FAIL ") + msg); bad += 0 if c else 1

for f in ("auth.json",):
    try: os.remove(DATA + "/" + f)
    except FileNotFoundError: pass
p = P()
init = p.call("initialize", {"protocol": 1, "data_dir": DATA, "locale": "fr-FR", "output": OUT})
check(init["capabilities"]["library"] and init["plugin"]["id"] == "plex", "initialize")
cid = open(DATA + "/client_id").read()
check(cid.startswith("ricercar-"), "client id kept: " + cid)
check(p.call("auth.status")["state"] == "signed_out", "signed out at first")
check(p.call("browse.root").get("code") == -32001, "browse before sign-in -> auth_required")
b = p.call("auth.begin")
check(b.get("url", "").startswith("https://app.plex.tv/auth#?clientID=" + cid + "&code=") and not b["expects_input"]
      and "Plex" in b["instructions"], "auth.begin: plex.tv PIN url")
check(p.call("auth.complete", {"input": "only-one-word?"}).get("code") == -32602, "bad paste refused")
check(p.call("auth.complete", {"input": "127.0.0.1:9 tok"}).get("code") == -32005, "unreachable server -> network")
st = p.call("auth.complete", {"input": "127.0.0.1:32400 test-token"})
check(st["state"] == "signed_in" and "32400" in st["account"]["detail"], "paste sign-in: " + json.dumps(st["account"], ensure_ascii=False))
n = p.notes.get(timeout=5); check(n["method"] == "auth.changed" and n["params"]["state"] == "signed_in", "auth.changed")
a = json.load(open(DATA + "/auth.json")); mode = stat.S_IMODE(os.stat(DATA + "/auth.json").st_mode)
check(a["token"] == "test-token" and a["server"] == "http://127.0.0.1:32400" and mode == 0o600, "auth.json mode %o" % mode)

root = p.call("browse.root")
check([x["ref"] for x in root["sections"]] == ["recent", "albums", "artists", "playlists", "favorites", "frequent"], "root sections")
al = p.call("browse.list", {"ref": "albums", "offset": 0, "limit": 50}); names = [x["title"] for x in al["items"]]
check(names == ["DSDish", "HiRes", "Sessions"] and al["total"] == 3, "albums " + str(names))
sess = [x for x in al["items"] if x["title"] == "Sessions"][0]
check(sess["artist"] == "Ensemble" and sess["year"] == 2021 and sess["art"].startswith(PMS + "/photo/:/transcode?X-Plex-Token=test-token"), "album fields")
check(urllib.request.urlopen(sess["art"]).headers.get("Content-Type") == "image/jpeg", "cover url serves a jpeg")
rec = p.call("browse.list", {"ref": "recent", "offset": 0, "limit": 1}); check(len(rec["items"]) == 1 and rec["has_more"], "recent, paged")
tr = p.call("browse.list", {"ref": sess["ref"], "offset": 0, "limit": 200})
check([t["track_no"] for t in tr["items"]] == [1, 2, 3] and tr["items"][0]["format"]["codec"] == "flac", "album tracks")
pg = p.call("browse.list", {"ref": sess["ref"], "offset": 1, "limit": 1}); check(len(pg["items"]) == 1 and pg["has_more"] and pg["total"] == 3, "paging")
ar = p.call("browse.list", {"ref": "artists", "offset": 0, "limit": 50}); check(sorted(x["title"] for x in ar["items"]) == ["Ensemble", "Trio"], "artists")
tri = [x for x in ar["items"] if x["title"] == "Trio"][0]
check([x["title"] for x in p.call("browse.list", {"ref": tri["ref"], "offset": 0, "limit": 10})["items"]] == ["HiRes", "DSDish"], "artist albums, newest first")
sr = p.call("search", {"query": "hi", "offset": 0, "limit": 10}); g = {x["kind"]: len(x["items"]) for x in sr["groups"]}
check(g == {"artist": 0, "album": 1, "track": 2, "playlist": 0}, "search " + str(g))
check(len(p.call("search", {"query": "TRACK", "kinds": ["track"], "offset": 0, "limit": 10})["groups"][0]["items"]) == 3, "search tracks only, any case")
for m, exp in (("library.albums", 3), ("library.artists", 2), ("library.tracks", 6)):
    r = p.call(m, {"offset": 0, "limit": 200}); check(len(r["items"]) == exp and r["total"] == exp, "%s: %d" % (m, len(r["items"])))
t1 = tr["items"][0]; it = p.call("item.get", {"ref": t1["ref"]})
check(it["title"] == "Track 1" and it["format"] == {"sample_rate": 44100, "bits": 16, "channels": 1, "codec": "flac"}, "item.get track, with format")
check(p.call("item.get", {"ref": "t/999999"}).get("code") == -32002, "item.get missing -> not_found")
check(p.call("item.get", {"ref": "t/../x"}).get("code") == -32002, "bad ref -> not_found")
# playlists (made through the API, as a Plex app would)
mid = pms("/identity")["machineIdentifier"]; pl_ids = ",".join(x["ref"][2:] for x in tr["items"][:2])
urllib.request.urlopen(urllib.request.Request(f"{PMS}/playlists?type=audio&title=Road%20Mix&smart=0&uri=server://{mid}/com.plexapp.plugins.library/library/metadata/{pl_ids}", method="POST"))
pls = p.call("browse.list", {"ref": "playlists", "offset": 0, "limit": 50}); mix = [x for x in pls["items"] if x["title"] == "Road Mix"]
check(len(mix) == 1 and mix[0]["subtitle"] == "2 ♪", "playlists")
check(len(p.call("browse.list", {"ref": mix[0]["ref"], "offset": 0, "limit": 50})["items"]) == 2, "playlist tracks")
check(p.call("item.get", {"ref": mix[0]["ref"]})["kind"] == "playlist", "item.get playlist")
check([x["title"] for x in p.call("search", {"query": "road", "kinds": ["playlist"], "offset": 0, "limit": 5})["groups"][0]["items"]] == ["Road Mix"], "search playlists")
# favourites
check(p.call("favorites.set", {"ref": t1["ref"], "on": True}) is None, "rate track")
check(p.call("favorites.set", {"ref": sess["ref"], "on": True}) is None, "rate album")
fav = p.call("browse.list", {"ref": "favorites", "offset": 0, "limit": 50}); check(sorted(x["kind"] for x in fav["items"]) == ["album", "track"], "favorites list")
p.call("favorites.set", {"ref": sess["ref"], "on": False}); check(len(p.call("browse.list", {"ref": "favorites", "offset": 0, "limit": 50})["items"]) == 1, "unrate album")
check(p.call("favorites.set", {"ref": mix[0]["ref"], "on": True}).get("code") == -32003, "playlist cannot be rated")
# direct resolve
r = p.call("track.resolve", {"ref": t1["ref"], "purpose": "play"})
check("/library/parts/" in r["url"] and r["duration_ms"] == 20000 and r["format"]["sample_rate"] == 44100, "resolve direct")
h = urllib.request.urlopen(r["url"]); body = h.read()
check(h.headers.get("Content-Length") == str(len(body)) and body[:4] == b"fLaC", "direct stream: original file with Content-Length")
rq = urllib.request.Request(r["url"], headers={"Range": "bytes=100-199"}); h = urllib.request.urlopen(rq)
check(h.status == 206 and len(h.read()) == 100, "direct stream: ranges (seekable)")
# hi-res on a 96k DAC
lib = p.call("library.tracks", {"offset": 0, "limit": 200})["items"]
hi = [x for x in lib if x["title"] == "Hi 1"][0]; quad = [x for x in lib if x["title"] == "Quad 1"][0]
r = p.call("track.resolve", {"ref": hi["ref"], "purpose": "play"})
check("transcode/universal/start.flac" in r.get("url", "") and r["format"]["sample_rate"] == 96000 and r["format"]["bits"] == 24, "192k on a 96k DAC -> " + json.dumps(r.get("format", r)))
got = probe(r["url"], S + "/hi.flac"); check(got == "96000,24", "transcoded file is " + got)
r = p.call("track.resolve", {"ref": quad["ref"], "purpose": "preload"})
got = probe(r["url"], S + "/quad.flac"); check(r["format"]["sample_rate"] == 88200 and got == "88200,24", "176.4k -> same family 88.2k: " + got)
p.notify("output.changed", {"output": {"bit_perfect": True, "max_rate": 48000, "max_bits": 24, "rates": [44100, 48000]}}); time.sleep(0.2)
r = p.call("track.resolve", {"ref": hi["ref"], "purpose": "play"}); got = probe(r["url"], S + "/hi48.flac")
check(r["format"]["sample_rate"] == 48000 and got == "48000,24", "48k DAC: 192k -> " + got)
p.notify("output.changed", {"output": {"bit_perfect": True, "max_rate": 44100, "max_bits": 16, "rates": [44100]}}); time.sleep(0.2)
r = p.call("track.resolve", {"ref": hi["ref"], "purpose": "play"}); check(r.get("code") == -32003, "CD DAC, 24-bit file -> unavailable: " + r.get("message", ""))
r = p.call("track.resolve", {"ref": t1["ref"], "purpose": "play"}); check("/library/parts/" in r.get("url", ""), "CD DAC, CD file -> direct")
p.notify("output.changed", {"output": OUT})
# reporting
t2 = tr["items"][1]; t3 = tr["items"][2]
vc = lambda ref: pms("/library/metadata/" + ref[2:])["Metadata"][0].get("viewCount", 0)
before = vc(t3["ref"])
p.notify("playback.started", {"ref": t3["ref"]}); time.sleep(1)
now = pms("/status/sessions").get("Metadata", [])
check(any(m.get("ratingKey") == t3["ref"][2:] for m in now), "timeline: now playing on the server")
p.notify("playback.ended", {"ref": t3["ref"], "listened_ms": 20000, "reason": "ended"}); time.sleep(1.5)
check(vc(t3["ref"]) == before + 1, "played to the end: counted once (viewCount %s -> %s)" % (before, vc(t3["ref"])))
b2 = vc(t2["ref"])
p.notify("playback.started", {"ref": t2["ref"]}); time.sleep(0.5)
p.notify("playback.ended", {"ref": t2["ref"], "listened_ms": 3000, "reason": "skipped"}); time.sleep(1)
check(vc(t2["ref"]) == b2, "short skip not counted")
fr = p.call("browse.list", {"ref": "frequent", "offset": 0, "limit": 10}); check(t3["ref"] in [x["ref"] for x in fr["items"]], "most played")
p.call("shutdown")
# restart: session restored
p = P(); p.call("initialize", {"protocol": 1, "data_dir": DATA, "locale": "en", "output": OUT})
check(p.call("auth.status")["state"] == "signed_in" and open(DATA + "/client_id").read() == cid, "session and client id restored")
# the address changed (home / away): another known address is used
a = json.load(open(DATA + "/auth.json")); a["server"] = "http://127.0.0.1:9"; a["connections"] = ["http://127.0.0.1:9", "http://127.0.0.1:32400"]
json.dump(a, open(DATA + "/auth.json", "w")); p.call("shutdown")
p = P(); p.call("initialize", {"protocol": 1, "data_dir": DATA, "output": OUT})
r = p.call("browse.list", {"ref": "albums", "offset": 0, "limit": 5})
check(len(r.get("items", [])) == 3 and json.load(open(DATA + "/auth.json"))["server"] == "http://127.0.0.1:32400", "dead address -> other connection")
check(p.call("auth.sign_out") is None and not os.path.exists(DATA + "/auth.json") and p.call("auth.status")["state"] == "signed_out", "sign out")
check(p.call("nope").get("code") == -32601, "unknown method")
p.call("shutdown")
print("FAILURES:", bad); sys.exit(1 if bad else 0)
