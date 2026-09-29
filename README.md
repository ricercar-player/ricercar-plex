# ricercar-plex

A [ricercar](https://github.com/ricercar-player/ricercar) source plugin that plays the
music of your own [Plex Media Server](https://www.plex.tv).

- **Browse:** recently added, albums, artists, playlists, favourites and
  most played, across all the music libraries of the server.
- **Library:** your server's albums, artists and tracks join ricercar's own
  Albums, Artists and Tracks pages and its search, marked *Plex*; your
  audio playlists join its Playlists. Artists without a picture on the
  server get the cover of one of their albums.
- **Home:** recently added, recently played and most played albums, as
  shelves on ricercar's Home page. With several music libraries on the
  server, each shelf mixes them.
- **Search:** artists, albums, tracks and playlists.
- **Bit-perfect:** tracks play from the original file, byte for byte
  (seekable). Only when your DAC cannot take a file's sample rate does the
  plugin ask the server for FLAC at the closest rate the DAC accepts (same
  44.1/48 kHz family, never higher than the original). Plex keeps the bit
  depth of the file when it transcodes, so a 24-bit file on a 16-bit-only
  DAC is reported as unavailable on that output.
- **Favourites** are five-star ratings, shared with the other Plex apps.
- **Plays** show on the server while they last (its "now playing") and
  count in its play history and "most played" once a track is heard to the
  end, or half of it, or four minutes.
- **Loudness:** when the server has analysed loudness, the track gain and
  peak are passed to ricercar.

The plugin uses the documented
[Plex Media Server API](https://developer.plex.tv/pms/) and plex.tv's
sign-in only. Tested with Plex Media Server 1.43.

## Install

**From ricercar (0.4.0 and later):** open **Plugins** in the sidebar and
install *Plex*.

**By hand:** download `plex-x86_64` or `plex-aarch64` from the
[releases](https://github.com/ricercar-player/ricercar-plex/releases), check it
against its `.sha256` file, make it executable, and declare it in
`~/.config/ricercar/config.toml`:

```toml
[[plugins]]
id = "plex"
command = "/home/you/.local/bin/plex-x86_64"
# args = ["--server", "Attic"]                      # one server among several
# args = ["--server", "http://192.168.1.10:32400"]  # or its address
```

**From source:**

```sh
cargo build --release
# target/release/ricercar-plex
```

## Sign in

Click **Sign in** next to Plex in ricercar. The Plex sign-in page opens in
your browser; once you have signed in there, the plugin finds your server
and ricercar shows it. Your password goes to plex.tv only.

- With several servers on the account, the plugin uses the first one you
  own that answers. Pick another with `--server <name>`.
- The plugin tries the addresses plex.tv knows for the server (local first,
  then remote, then Plex's relay), and switches to another one when the
  current address stops answering (at home, away). Pass
  `--server <address>` to use a fixed address instead.
- The server token is kept in
  `~/.local/share/ricercar/plugins/plex/auth.json` (mode 600). Signing out
  of ricercar forgets it; to revoke it, remove *ricercar* from Authorized
  Devices in your Plex account settings.

Signing in from another computer than the one running ricercar: paste
`<server address> <token>` in ricercar's sign-in field, with an
`X-Plex-Token` of your account.

## Notes

- Stream **and cover** URLs carry the token as a query parameter, as Plex
  web clients do. Stream URLs are never stored; cover URLs are part of item
  metadata, so they can end up in ricercar's saved queue and playlists.
- Transcoded streams have no known length, so they cannot be seeked.
  Originals can.
- To use two servers at once, declare the plugin twice with different `id`s
  and `--server` values.

## Protocol

Plugin protocol 1, as described in ricercar's
[docs/plugins.md](https://github.com/ricercar-player/ricercar/blob/main/docs/plugins.md),
with the `library` capability (`library.albums`, `library.artists`,
`library.tracks`, `library.playlists`) and `home` shelves.

| Ref | Meaning |
|---|---|
| `recent`, `albums`, `artists`, `playlists`, `favorites`, `frequent` | Top-level sections |
| `recent`, `played`, `top` | Home shelves: albums recently added, recently played, most played |
| `t/<ratingKey>` | Track |
| `a/<ratingKey>` | Album |
| `r/<ratingKey>` | Artist (its albums) |
| `p/<ratingKey>` | Playlist |

Error codes follow the protocol: a refused token marks the session expired
(`auth_required`) and sends `auth.changed`; missing items answer
`not_found`; files the output cannot take answer `unavailable`; unreachable
servers answer `network`.

## Development

```sh
cargo test
cargo clippy --all-targets
tests/plex.sh     # end to end against a throwaway Plex Media Server (docker, ffmpeg)
```

The CI builds static binaries (musl) for x86_64 and aarch64 on every tag
`v*` and attaches them, with their SHA-256, to a GitHub release.
`contrib/hub-entry.toml` is the entry for the
[ricercar plugin hub](https://github.com/ricercar-player/ricercar-plugins).

## Licence

MIT. Plex is a trademark of Plex, Inc.; this plugin is not affiliated with
or endorsed by Plex.
