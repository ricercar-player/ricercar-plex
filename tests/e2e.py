#!/usr/bin/env python3
"""End-to-end test of the plugin against a live Plex Media Server (see
plex.sh): JSON-RPC over stdio, the streams with ffprobe. The server is
unclaimed and open to the test network, so any token passes; the plex.tv
sign-in (PIN) cannot be tested here beyond creating the PIN."""
import json, subprocess, sys, threading, queue, urllib.request, urllib.parse, time, os, stat
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
check(all(init["capabilities"][k] for k in ("lyrics", "playlist_edit", "details", "radio")), "new capabilities")
check([(x["key"], x["default"]) for x in init["settings"]] == [("report_playback", True), ("transcode", "auto")]
      and init["settings"][0]["label"] == "Signaler mes écoutes", "settings declared, in French")
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
check([x["ref"] for x in root["home"]] == ["recent", "played", "top"] and all(x["browsable"] for x in root["home"]), "home shelves")
al = p.call("browse.list", {"ref": "albums", "offset": 0, "limit": 50}); names = [x["title"] for x in al["items"]]
check(names == ["DSDish", "HiRes", "Sessions", "Solo"] and al["total"] == 4, "albums of both libraries " + str(names))
sess = [x for x in al["items"] if x["title"] == "Sessions"][0]
check(sess["artist"] == "Ensemble" and sess["year"] == 2021 and sess["art"].startswith(PMS + "/photo/:/transcode?X-Plex-Token=test-token"), "album fields")
check(urllib.request.urlopen(sess["art"]).headers.get("Content-Type") == "image/jpeg", "cover url serves a jpeg")
rec = p.call("browse.list", {"ref": "recent", "offset": 0, "limit": 1})
check([x["title"] for x in rec["items"]] == ["Solo"] and rec["has_more"], "recently added, across libraries, paged")
tr = p.call("browse.list", {"ref": sess["ref"], "offset": 0, "limit": 200})
check([t["track_no"] for t in tr["items"]] == [1, 2, 3] and tr["items"][0]["format"]["codec"] == "flac", "album tracks")
pg = p.call("browse.list", {"ref": sess["ref"], "offset": 1, "limit": 1}); check(len(pg["items"]) == 1 and pg["has_more"] and pg["total"] == 3, "paging")
ar = p.call("browse.list", {"ref": "artists", "offset": 0, "limit": 50}); check(sorted(x["title"] for x in ar["items"]) == ["Ensemble", "Soloist", "Trio"], "artists")
tri = [x for x in ar["items"] if x["title"] == "Trio"][0]
check([x["title"] for x in p.call("browse.list", {"ref": tri["ref"], "offset": 0, "limit": 10})["items"]] == ["HiRes", "DSDish"], "artist albums, newest first")
sr = p.call("search", {"query": "hi", "offset": 0, "limit": 10}); g = {x["kind"]: len(x["items"]) for x in sr["groups"]}
check(g == {"artist": 0, "album": 1, "track": 2, "playlist": 0}, "search " + str(g))
sr = p.call("search", {"query": "SOLO", "offset": 0, "limit": 10}); g = {x["kind"]: [i["title"] for i in x["items"]] for x in sr["groups"]}
check(g == {"artist": ["Soloist"], "album": ["Solo"], "track": [], "playlist": []}, "search groups " + str(g))
check(len(p.call("search", {"query": "TRACK", "kinds": ["track"], "offset": 0, "limit": 10})["groups"][0]["items"]) == 3, "search tracks only, any case")
for m, exp in (("library.albums", 4), ("library.artists", 3), ("library.tracks", 7)):
    r = p.call(m, {"offset": 0, "limit": 200}); check(len(r["items"]) == exp and r["total"] == exp, "%s: %d" % (m, len(r["items"])))
la = p.call("library.albums", {"offset": 0, "limit": 200})["items"]
check(all(x["kind"] == "album" and x["browsable"] and x.get("artist") and x.get("year") and x.get("art") for x in la), "library albums: artist, year, art")
lr = p.call("library.artists", {"offset": 0, "limit": 200})["items"]
check(all(x["kind"] == "artist" and x["browsable"] and x.get("art") for x in lr), "library artists: art (album cover when none) " + str([x["title"] for x in lr if not x.get("art")]))
solo = [x for x in lr if x["title"] == "Soloist"][0]
check([x["title"] for x in p.call("browse.list", {"ref": solo["ref"], "offset": 0, "limit": 10})["items"]] == ["Solo"], "library artist -> its albums")
check(p.call("library.playlists", {"offset": 0, "limit": 200}) == {"items": [], "total": 0, "has_more": False}, "library.playlists, none yet")
t1 = tr["items"][0]; it = p.call("item.get", {"ref": t1["ref"]})
check(it["title"] == "Track 1" and it["format"] == {"sample_rate": 44100, "bits": 16, "channels": 1, "codec": "flac"}, "item.get track, with format")
check(p.call("item.get", {"ref": "t/999999"}).get("code") == -32002, "item.get missing -> not_found")
check(p.call("item.get", {"ref": "t/../x"}).get("code") == -32002, "bad ref -> not_found")
# links, actions, lyrics, details, radio
check(p.call("browse.list", {"ref": t1["album_ref"], "offset": 0, "limit": 10})["items"][0]["ref"] == t1["ref"], "track album_ref browses")
check(p.call("item.get", {"ref": t1["artist_ref"]})["title"] == "Ensemble" and sess["artist_ref"] == t1["artist_ref"], "artist_ref of track and album")
check(t1["favorite"] is False and [a["id"] for a in t1["actions"]] == ["sonic"] and t1["actions"][0]["label"] == "Titres au son proche", "track actions, in French")
ens = [x for x in ar["items"] if x["title"] == "Ensemble"][0]
check([(a["id"], a["kind"]) for a in ens["actions"]] == [("radio", "play"), ("similar", "browse")], "artist actions")
for a in t1["actions"] + sess["actions"] + ens["actions"]:
    r = p.call("browse.list", {"ref": a["ref"], "offset": 0, "limit": 50})
    check("items" in r, "action %s browses: %d items" % (a["ref"], len(r.get("items", []))))
rad = p.call("browse.list", {"ref": ens["actions"][0]["ref"], "offset": 0, "limit": 50})["items"]
check(sorted(x["title"] for x in rad) == ["Track 1", "Track 2", "Track 3"], "artist radio: its tracks (no similar artists here)")
ly = p.call("lyrics.get", {"ref": t1["ref"]})
check([(l["time_ms"], l["text"]) for l in ly.get("synced", [])] == [(1000, "First line"), (5500, "Twice"), (9250, "Third line"), (12000, "Twice")], "synced lyrics " + json.dumps(ly))
ly = p.call("lyrics.get", {"ref": tr["items"][1]["ref"]})
check(ly == {"plain": "Plain words\nSecond plain line"}, "plain lyrics " + json.dumps(ly))
check(p.call("lyrics.get", {"ref": tr["items"][2]["ref"]}).get("code") == -32002, "no lyrics -> not_found")
check(p.call("lyrics.get", {"ref": sess["ref"]}).get("code") == -32002, "lyrics of an album -> not_found")
d = p.call("item.details", {"ref": ens["ref"]})
check([x["title"] for x in d.get("related", [])] == ["Most Popular Tracks"] and len(d["related"][0]["items"]) == 3, "artist details: shelves " + json.dumps(d)[:300])
d = p.call("item.details", {"ref": sess["ref"]})
check(any(f["label"] == "Sortie" and f["value"].startswith("2021") for f in d.get("facts", [])), "album details: facts " + json.dumps(d.get("facts"), ensure_ascii=False))
check(p.call("item.details", {"ref": "t/999999"}).get("code") == -32002, "details of a missing item -> not_found")
rn = p.call("radio.next", {"seed": t1["ref"], "exclude": [tr["items"][1]["ref"]], "limit": 5})
check([x["ref"] for x in rn["items"]] == [tr["items"][2]["ref"]], "radio.next: artist tracks, seed and exclude left out")
rn = p.call("radio.next", {"seed": sess["ref"], "exclude": [], "limit": 2})
check(len(rn["items"]) == 2 and all(x["playable"] for x in rn["items"]), "radio.next from an album, limited")
check(p.call("radio.next", {"seed": "nope", "exclude": [], "limit": 2}).get("code") == -32602, "radio.next bad seed")
# playlists (made through the API, as a Plex app would)
mid = pms("/identity")["machineIdentifier"]; pl_ids = ",".join(x["ref"][2:] for x in tr["items"][:2])
urllib.request.urlopen(urllib.request.Request(f"{PMS}/playlists?type=audio&title=Road%20Mix&smart=0&uri=server://{mid}/com.plexapp.plugins.library/library/metadata/{pl_ids}", method="POST"))
pls = p.call("browse.list", {"ref": "playlists", "offset": 0, "limit": 50}); mix = [x for x in pls["items"] if x["title"] == "Road Mix"]
check(len(mix) == 1 and mix[0]["subtitle"] == "2 ♪", "playlists")
lp = p.call("library.playlists", {"offset": 0, "limit": 200})
check([(x["ref"], x["kind"], x["browsable"]) for x in lp["items"]] == [(mix[0]["ref"], "playlist", True)] and lp["total"] == 1 and not lp["has_more"], "library.playlists")
check(len(p.call("browse.list", {"ref": mix[0]["ref"], "offset": 0, "limit": 50})["items"]) == 2, "playlist tracks")
check(p.call("item.get", {"ref": mix[0]["ref"]})["kind"] == "playlist", "item.get playlist")
check(mix[0]["editable"] is True, "a playlist of one's own is editable")
check([x["title"] for x in p.call("search", {"query": "road", "kinds": ["playlist"], "offset": 0, "limit": 5})["groups"][0]["items"]] == ["Road Mix"], "search playlists")
sr = p.call("search", {"query": "Mix", "offset": 0, "limit": 5}); g = {x["kind"]: [i["title"] for i in x["items"]] for x in sr["groups"]}
check(g["playlist"] == ["Road Mix"] and g["artist"] == [], "search, all groups: playlist found")
# favourites
check(p.call("favorites.set", {"ref": t1["ref"], "on": True}) is None, "rate track")
check(p.call("favorites.set", {"ref": sess["ref"], "on": True}) is None, "rate album")
fav = p.call("browse.list", {"ref": "favorites", "offset": 0, "limit": 50}); check(sorted(x["kind"] for x in fav["items"]) == ["album", "track"], "favorites list")
p.call("favorites.set", {"ref": sess["ref"], "on": False}); check(len(p.call("browse.list", {"ref": "favorites", "offset": 0, "limit": 50})["items"]) == 1, "unrate album")
check(p.call("favorites.set", {"ref": mix[0]["ref"], "on": True}).get("code") == -32003, "playlist cannot be rated")
check(p.call("item.get", {"ref": t1["ref"]})["favorite"] is True and p.call("item.get", {"ref": sess["ref"]})["favorite"] is False, "favorite flags")
# playlist editing
new = p.call("playlists.create", {"name": "Fresh", "description": "Made <b>here</b>", "public": True})
check(new.get("kind") == "playlist" and new["title"] == "Fresh" and new["editable"] and new.get("track_count") == 0, "playlists.create " + json.dumps(new)[:200])
tref = [x["ref"] for x in tr["items"]]
check(p.call("playlists.add", {"ref": new["ref"], "items": tref}) is None, "playlists.add")
check(p.call("playlists.add", {"ref": new["ref"], "items": ["p/1"]}).get("code") == -32602, "playlists.add of a playlist refused")
ent = lambda: [(x["ref"], x["entry_id"]) for x in p.call("browse.list", {"ref": new["ref"], "offset": 0, "limit": 50})["items"]]
e = ent(); check([x[0] for x in e] == tref and all(x[1] for x in e), "entries " + str(e))
check(p.call("playlists.move", {"ref": new["ref"], "entry": e[2][1], "to": 0}) is None, "move last to first")
check([x[0] for x in ent()] == [tref[2], tref[0], tref[1]], "moved to the top")
check(p.call("playlists.move", {"ref": new["ref"], "entry": e[2][1], "to": 2}) is None, "move first to the end")
check([x[0] for x in ent()] == tref, "moved to the end")
check(p.call("playlists.move", {"ref": new["ref"], "entry": e[0][1], "to": 1}) is None, "move first to second")
check([x[0] for x in ent()] == [tref[1], tref[0], tref[2]], "moved one down")
check(p.call("playlists.move", {"ref": new["ref"], "entry": "999999", "to": 1}).get("code") == -32002, "move of a missing entry")
check(p.call("playlists.remove", {"ref": new["ref"], "entries": [e[0][1], e[2][1]]}) is None, "playlists.remove")
check([x[0] for x in ent()] == [tref[1]], "removed two entries")
check(p.call("playlists.rename", {"ref": new["ref"], "name": "Renamed"}) is None and p.call("item.get", {"ref": new["ref"]})["title"] == "Renamed", "playlists.rename")
check(pms("/playlists/" + new["ref"][2:])["Metadata"][0]["summary"] == "Made <b>here</b>", "description kept")
urllib.request.urlopen(urllib.request.Request(f"{PMS}/playlists?type=audio&title=Smart&smart=1&uri=" + urllib.parse.quote(f"server://{mid}/com.plexapp.plugins.library/library/sections/1/all?type=10&title=Track", safe=""), method="POST"))
smart = [x for x in p.call("library.playlists", {"offset": 0, "limit": 200})["items"] if x["title"] == "Smart"][0]
check(smart["editable"] is False and p.call("playlists.rename", {"ref": smart["ref"], "name": "x"}).get("code") == -32602, "smart playlist not editable")
check(p.call("playlists.delete", {"ref": new["ref"]}) is None and p.call("item.get", {"ref": new["ref"]}).get("code") == -32002, "playlists.delete")
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
p.notify("settings.changed", {"settings": {"report_playback": True, "transcode": "never"}}); time.sleep(0.2)
p.notify("output.changed", {"output": OUT}); time.sleep(0.2)
r = p.call("track.resolve", {"ref": hi["ref"], "purpose": "play"})
check("/library/parts/" in r.get("url", "") and r["format"]["sample_rate"] == 192000, "transcode never: original file")
p.notify("settings.changed", {"settings": {"report_playback": True, "transcode": "auto"}}); time.sleep(0.2)
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
p.notify("settings.changed", {"settings": {"report_playback": False, "transcode": "auto"}}); time.sleep(0.2)
p.notify("playback.started", {"ref": t2["ref"]}); time.sleep(0.5)
p.notify("playback.ended", {"ref": t2["ref"], "listened_ms": 20000, "reason": "ended"}); time.sleep(1)
check(vc(t2["ref"]) == b2, "report_playback off: nothing sent")
p.notify("settings.changed", {"settings": {"report_playback": True, "transcode": "auto"}})
fr = p.call("browse.list", {"ref": "frequent", "offset": 0, "limit": 10}); check(t3["ref"] in [x["ref"] for x in fr["items"]], "most played")
for shelf in ("played", "top"):
    sh = p.call("browse.list", {"ref": shelf, "offset": 0, "limit": 10})
    check(sh["items"] and sh["items"][0]["ref"] == sess["ref"] and all(x["kind"] == "album" for x in sh["items"]), "home shelf %s: %s" % (shelf, [x["title"] for x in sh["items"]]))
p.call("shutdown")
# restart: session restored
p = P(); p.call("initialize", {"protocol": 1, "data_dir": DATA, "locale": "en", "output": OUT})
check(p.call("auth.status")["state"] == "signed_in" and open(DATA + "/client_id").read() == cid, "session and client id restored")
# the address changed (home / away): another known address is used
a = json.load(open(DATA + "/auth.json")); a["server"] = "http://127.0.0.1:9"; a["connections"] = ["http://127.0.0.1:9", "http://127.0.0.1:32400"]
json.dump(a, open(DATA + "/auth.json", "w")); p.call("shutdown")
p = P(); p.call("initialize", {"protocol": 1, "data_dir": DATA, "output": OUT})
r = p.call("browse.list", {"ref": "albums", "offset": 0, "limit": 5})
check(len(r.get("items", [])) == 4 and json.load(open(DATA + "/auth.json"))["server"] == "http://127.0.0.1:32400", "dead address -> other connection")
check(p.call("auth.sign_out") is None and not os.path.exists(DATA + "/auth.json") and p.call("auth.status")["state"] == "signed_out", "sign out")
check(p.call("nope").get("code") == -32601, "unknown method")
p.call("shutdown"); time.sleep(0.5)
log = open(S + "/plugin.log").read()
check("test-token" not in log and "signed in as" not in log, "no token or account name in the log")
print("FAILURES:", bad); sys.exit(1 if bad else 0)
