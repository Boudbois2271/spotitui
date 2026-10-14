# spotitui

A Spotify player for the terminal that takes its colors from your Omarchy
theme. Built with [ratatui](https://ratatui.rs) and
[librespot](https://github.com/librespot-org/librespot); its design and
Spotify integration follow [fusor-rs/spotify-tui](https://github.com/fusor-rs/spotify-tui).

## Requirements

- Spotify Premium (librespot can only stream on Premium accounts).
- Linux with PipeWire or PulseAudio, a Secret Service keyring (gnome-keyring,
  KeePassXC…) on the session D-Bus, and systemd for the background service.
- Rust (stable, via [rustup](https://rustup.rs)) to build it, plus
  `pkg-config`, the ALSA and PulseAudio development files and OpenSSL. On Arch:
  `sudo pacman -S rustup pkgconf alsa-lib libpulse openssl`, then
  `rustup default stable`.
- Optional: [Omarchy](https://omarchy.org) for the theme colors, the bar widget,
  the keybindings and `spotitui-open`. Everything else works on any Linux
  desktop; without an Omarchy theme spotitui uses Spotify's own colors.

## Install

```sh
git clone https://github.com/Boudbois2271/spotitui.git
cd spotitui
cargo build --release
install -Dm755 target/release/spotitui ~/.local/bin/spotitui   # ~/.local/bin must be on PATH
spotitui                                                        # log in once, in the browser
```

`spotitui` starts its background service on its own. To have the service (and
with it media keys) running from login, also install the systemd unit below.

## Run

```sh
spotitui            # the player window; first run: press Enter, approve in the browser
spotitui logout     # forget the saved login
```

spotitui is two programs in one binary:

- **The service** (`spotitui daemon`) plays the music. It logs in with the
  login kept in the Secret Service keyring (gnome-keyring), runs the Spotify
  Connect speaker, publishes MPRIS (media keys, the Omarchy bar widget) and
  reconnects by itself after network loss or sleep. It runs as a systemd user
  service from login, so media keys and the bar widget always work.
- **The player window** (`spotitui`) browses and controls. It talks to the
  service over `$XDG_RUNTIME_DIR/spotitui/daemon.sock` and starts the service if
  it isn't running. Closing the window (`q`) leaves the music playing; pause
  with `Space`, the media keys or the widget.

Install the service, launcher and app-menu entry once:

```sh
install -Dm644 contrib/spotitui.service ~/.config/systemd/user/spotitui.service
systemctl --user daemon-reload
systemctl --user enable --now spotitui.service
journalctl --user -u spotitui -f      # its log

install -Dm755 contrib/spotitui-open ~/.local/bin/spotitui-open          # needs Omarchy
install -Dm644 contrib/spotitui.desktop ~/.local/share/applications/spotitui.desktop
```

`spotitui-open` focuses the spotitui window if one is open, else opens one in the
default terminal with the app-id `spotitui` (via Omarchy's
`omarchy-launch-or-focus`). The app menu, the bar widget's right-click and the
`SUPER+CTRL+ALT+S` binding all use it.

### Omarchy extras

`contrib/` also holds the Omarchy pieces, copied from a working setup:

- `omarchy/boudbois.spotify/`: the bar widget (a Spotify icon; left click opens a
  mini player with controls, progress and the tracks around the current one,
  right click opens spotitui). Install and place it left of the tray:

  ```sh
  cp -r contrib/omarchy/boudbois.spotify ~/.config/omarchy/plugins/
  omarchy bar put boudbois.spotify --before omarchy.tray    # any widget id to sit beside
  omarchy restart shell
  ```

- `hypr/spotitui-bindings.lua`: `SUPER+CTRL+S` toggles the mini player (Share
  moves to `SUPER+SHIFT+CTRL+S`) and `SUPER+CTRL+ALT+S` opens spotitui. Append it
  to `~/.config/hypr/bindings.lua`, then check with `hyprctl configerrors`.

These are copies: edit the live files under `~/.config`, then copy them back here.

Music plays through PulseAudio/PipeWire as a stream named "spotitui". Other
Spotify apps list this computer as **spotitui** and can control it. Idle, the
service uses about 35 MB of memory and no measurable CPU.

## Keys

| Key | Action | Key | Action |
| --- | --- | --- | --- |
| `Space` | play / pause | `n` / `p` | next / previous |
| `←` / `→` | seek 10 s | `s` / `r` | shuffle / repeat |
| `[` / `]` | volume ±5% | `m` | mute / unmute |
| `j` / `k` | down / up a row | `l` / `h` | open / back (as in Yazi) |
| `gg` / `G` | first / last row | `^d` / `^u` | half a screen down / up |
| `/` | search Spotify | `^f` | find in this list |
| `L` | lyrics | | |
| `o` / `a` | open album / artist | `z` / `Z` | add to queue / play next |
| `S` | save to Liked Songs | `A` | add to playlists (`Space` ticks, `l` adds) |
| `?` | keys | `q` `^C` | quit |

`l` opens the highlighted entry (a list in the sidebar, or plays a track) and `h`
goes back up: from an opened album or artist to the list it came from, then to
the sidebar. In the lyrics view, `j`/`k` pick a line, `l` jumps there and `h`
closes the lyrics. The search box is empty each time `/` opens it; `Enter`
searches and `Esc` drops what was typed and goes back to the sidebar's Search
entry (`h` and `l` type letters there). Playback keys also work while the keys
dialog (`?`) or the finder (`^f`) is open.

`^f` finds in the list on screen: type to filter by title, artist or album (every
word must match), `Enter` to browse the matches with `j`/`k`, `gg`/`G`,
`^d`/`^u`, then `l` to play (landing on that row), `z`/`Z`, `S`, `A` or `o`/`a`
on the highlighted one; `^f` types again, `h` (or `Esc` while typing) closes.

`A` lists only the playlists you can add to (your own and collaborative ones);
`Space` ticks several, `l` adds to the ticked ones or, with none ticked, to the
highlighted one. `S` tells you when a track is already in Liked Songs rather
than saving it again.

Mouse: click sidebar entries and buttons, click a row twice to play it, click
the progress bar to seek, scroll lists, lyrics and the volume meter (5% per
notch) with the wheel.

## Integration

- **Theme:** colors come from `~/.local/state/omarchy/current/theme/colors.toml`
  (accent, foreground, background, light_foreground, selection, red), checked
  once a second, so `omarchy theme set` recolors a running player. Without an
  Omarchy theme, Spotify's palette is used. The background stays the terminal's
  own, which keeps foot's transparency.
- **Media keys:** MPRIS as `org.mpris.MediaPlayer2.spotitui`, so the
  `XF86Audio*` keys (`omarchy-shell media …`) and `playerctl -p spotitui`
  work. Covers are downloaded to `$XDG_RUNTIME_DIR/spotitui/covers/` and
  published as `file://` URLs (loading https covers crashed the Omarchy shell).
  Beside MPRIS, the same object serves `io.spotitui.Queue.Skip(offset)`: it
  plays the track `offset` places away in the play order (`-1` goes to the
  previous track even mid-song, at most 20 either way), which the bar widget's
  list uses.
- **Single instance:** one service per user (a second one exits); any number
  of windows can connect to it.
- **Lyrics:** Spotify's own (usually Musixmatch, line-synced). When Spotify has
  none, the track's title, artist, album and length are looked up on
  [LRCLIB](https://lrclib.net).
- **Bar widget:** the tracks around the playing one (one before, three after)
  are written to `$XDG_RUNTIME_DIR/spotitui/queue.json` for the
  `boudbois.spotify` Omarchy bar widget, and removed on exit. They come from
  Spotify's connect-state cluster, read by a second, hidden "observer" session
  (a `hobs_` device, as the web player uses), because Spotify doesn't send the
  cluster to the device that is playing. **Play next** (`Z`) uses that queue
  too: with tracks already queued, it sends the queue back with the new track
  first (`set_queue`, as the phone app reorders); otherwise it adds to the queue,
  which already plays next. The observer logs in with the main session's reusable
  credentials and starts over by itself if it fails or falls behind the speaker.
- **Library:** `S` writes to Liked Songs through Spotify's collection service,
  `A` adds to playlists through the playlist service (playlist4 changes on the
  current revision), both as the official apps do. The service reads Liked Songs
  once per session and keeps it current with the collection's changes, so `S`
  costs one small request. Playlists load again when you enter the sidebar (at
  most every 30 s), and a song saved with `S` shows at the top of Liked Songs.
- **Recovery:** every network request has a time limit; a list that failed to
  load tries again by itself, and `l` on it retries at once. The service exits
  with an error if one of its own tasks crashes, so systemd restarts it. When it
  stops it tells Spotify the device is leaving, and it keeps one device ID per
  computer, so other apps see the same spotitui across restarts.
- **Volume** is remembered in `~/.local/state/spotitui/volume`. **Mute** (`m`) is a
  status of its own, kept in `~/.local/state/spotitui/muted`: it silences the output
  without touching the volume, which Spotify Connect and MPRIS keep showing. Volume
  changes while muted set the level unmuting returns to.

## Develop

```sh
cargo run -- daemon             # a service in the foreground (stop the systemd one first)
cargo run                       # a window; it uses whichever service is running
SPOTITUI_LOG=/tmp/spotitui.log SPOTITUI_LOG_LEVEL=debug cargo run
cargo test                      # unit tests
cargo test live_cluster -- --ignored --nocapture   # Spotify's device list, as it sees it
cargo test -- --ignored         # plus live tests: an LRCLIB lookup, and library tests
                                # that like/unlike a track and add/remove one in a playlist
                                # (they undo what they do; they need a stored login).
                                # live_take_over takes playback from the running service
                                # with a second, silent speaker (the service then stops).
cargo build --release && install -Dm755 target/release/spotitui ~/.local/bin/spotitui \
  && systemctl --user restart spotitui.service
```

`src/daemon/` is the service, `src/client.rs` the window's connection to it,
and `src/protocol.rs` what they say to each other (one JSON message per line).
Bump `protocol::VERSION` when a message changes shape: a window then warns that the
service is older or newer than it, and a request the service doesn't know gets an
error answer instead of a dropped connection.
`src/spotify/` talks to Spotify (login, catalog, speaker, lyrics, MPRIS,
library writes, the queue observer),
`src/app/` holds state and input, `src/ui/` draws, `src/theme.rs` reads the
Omarchy theme. Background work reports back to the main loop as `Msg`s, so all
state changes happen in one place.

Spotify retires a refresh token once it hands out a new one, and every refresh
(the service connecting, a live test) may do so: a copy of an older login from the
keyring stops working. The service keeps a renewed login in memory if the keyring
won't take it, and retries saving it every minute and when stopping.

`Cargo.lock` pins versions known to build; librespot 0.8.0 fails with newer
`vergen` releases.

## Limitations

- spotitui logs in as Spotify's own desktop app (librespot's client ID): Spotify
  streams audio only to its privileged clients, so a client ID of your own can't
  play. Spotify could restrict that ID at any time, as it could for every
  librespot-based player.
- Browsing, Liked Songs, playlists, the play queue and lyrics use Spotify's
  internal, undocumented services, the ones the official apps use. The public Web
  API isn't an option: with librespot's shared client ID it answers 429 (rate
  limited). A change on Spotify's side can break a feature until spotitui or
  librespot catches up; playback itself goes through librespot.
