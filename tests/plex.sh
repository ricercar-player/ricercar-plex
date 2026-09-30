#!/bin/sh
# Start a throwaway Plex Media Server on 127.0.0.1:32400 with generated FLAC
# files: a "Music" library (three 44.1 kHz / 16-bit tracks, two 192 kHz /
# 24-bit ones, one 176.4 kHz / 24-bit; lyrics for two tracks) and, added
# after it, a "Classical" library with one more album. Then run the
# end-to-end test against it. The server stays unclaimed and lets the test
# network in without a token.
#   tests/plex.sh [workdir]      (needs docker, ffmpeg, python3)
set -eu
W=${1:-$(mktemp -d)}
mkdir -p "$W/music/Ensemble/Sessions" "$W/music/Trio/HiRes" "$W/music/Trio/DSDish" \
  "$W/classical/Soloist/Solo" "$W/config"
# A cover embedded in each file, so that albums have art without the
# online metadata agents.
ffmpeg -loglevel error -y -f lavfi -i "color=c=teal:s=300x300" -frames:v 1 "$W/cover.png"
C="-i $W/cover.png -map 0 -map 1 -c:v png -disposition:v attached_pic"
for i in 1 2 3; do
  ffmpeg -loglevel error -y -f lavfi -i "sine=frequency=$((300 * i)):duration=20" $C \
    -ar 44100 -sample_fmt s16 -metadata title="Track $i" -metadata artist=Ensemble \
    -metadata album_artist=Ensemble -metadata album=Sessions -metadata date=2021 \
    -metadata track=$i -metadata genre=Jazz "$W/music/Ensemble/Sessions/0$i.flac"
done
for i in 1 2; do
  ffmpeg -loglevel error -y -f lavfi -i "sine=frequency=$((500 * i)):duration=15" \
    -ar 192000 -sample_fmt s32 -bits_per_raw_sample 24 -metadata title="Hi $i" \
    -metadata artist=Trio -metadata album_artist=Trio -metadata album=HiRes \
    -metadata date=2024 -metadata track=$i "$W/music/Trio/HiRes/0$i.flac"
done
ffmpeg -loglevel error -y -f lavfi -i "sine=frequency=700:duration=10" \
  -ar 176400 -sample_fmt s32 -bits_per_raw_sample 24 -metadata title="Quad 1" \
  -metadata artist=Trio -metadata album_artist=Trio -metadata album=DSDish \
  -metadata date=2023 -metadata track=1 "$W/music/Trio/DSDish/01.flac"
ffmpeg -loglevel error -y -f lavfi -i "sine=frequency=880:duration=10" $C \
  -ar 44100 -sample_fmt s16 -metadata title=Aria -metadata artist=Soloist \
  -metadata album_artist=Soloist -metadata album=Solo -metadata date=2019 \
  -metadata track=1 "$W/classical/Soloist/Solo/01.flac"
# Lyrics next to two tracks: timed (LRC) and plain.
printf '[ar:Ensemble]\n[ti:Track 1]\n[00:01.00]First line\n[00:05.50][00:12.00]Twice\n[00:09.25]Third line\n' \
  > "$W/music/Ensemble/Sessions/01.lrc"
printf 'Plain words\nSecond plain line\n' > "$W/music/Ensemble/Sessions/02.txt"
docker rm -f ricercar-plex-test >/dev/null 2>&1 || true
docker run -d --name ricercar-plex-test -p 127.0.0.1:32400:32400 \
  -e ALLOWED_NETWORKS=0.0.0.0/0 -e TZ=UTC \
  -v "$W/music:/music:ro" -v "$W/classical:/classical:ro" -v "$W/config:/config" plexinc/pms-docker:latest >/dev/null
trap 'docker rm -f ricercar-plex-test >/dev/null' EXIT
J='Accept: application/json'
# Wait until the server has finished starting (no `startState` any more).
for _ in $(seq 90); do
  s=$(curl -sf -H "$J" 127.0.0.1:32400/identity || true)
  [ -n "$s" ] && ! echo "$s" | grep -q startState && break
  sleep 2
done
curl -sf -H "$J" -X POST "127.0.0.1:32400/library/sections?name=Music&type=artist&agent=tv.plex.agents.music&scanner=Plex%20Music&language=xn&location=/music" >/dev/null
for _ in $(seq 60); do
  sleep 2
  curl -sf -H "$J" "127.0.0.1:32400/library/all?type=10" | grep -q '"size":6' &&
    curl -sf -H "$J" "127.0.0.1:32400/library/all?type=9&title=Sessions" | grep -q '"thumb"' &&
    break
done
# The second library once the first is in: its album is the most recent.
sleep 2
curl -sf -H "$J" -X POST "127.0.0.1:32400/library/sections?name=Classical&type=artist&agent=tv.plex.agents.music&scanner=Plex%20Music&language=xn&location=/classical" >/dev/null
for _ in $(seq 60); do
  sleep 2
  curl -sf -H "$J" "127.0.0.1:32400/library/all?type=10" | grep -q '"size":7' &&
    curl -sf -H "$J" "127.0.0.1:32400/library/all?type=9&title=Solo" | grep -q '"thumb"' &&
    break
done
cargo build --release
python3 "$(dirname "$0")/e2e.py" "$W"
