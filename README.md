# nearpush

A small daemon on the laptop that receives status events from a CLI and streams them
over WSS to an iPhone SE on the desk. The phone runs a home-screen web page served by
the daemon and reached through `tailscale serve` (tailnet only, never funnel). The page
shows one big status card at a time, built for coding-agent states:

| `--state`   | Card                                  | Top-left           |
|-------------|---------------------------------------|--------------------|
| `allow`     | **allow.** in red                     | `18m so far` (live) |
| `your_turn` | **your** (white) / **turn.** (red)    | `18m so far` (live) |
| `all_set`   | **all** / **set.** in green           | `took 1m`          |
| `working`   | **working.** in grey                  | `5m so far` (live) |
| *(none)*    | the title, coloured by `--level` (info white, warn amber, error red) | |

Below the headline come the subtitle (or body), the task line (the title) and a footer
with the project. A swipe up, or the ☰ button, opens a drawer with the last 100 events;
tapping one shows it until the next event arrives. When an event has a `url`, tapping
the card opens it in Safari.

```
nearpush send ──HTTP+token──▶ nearpushd 127.0.0.1:8787 ◀── tailscale serve (https) ◀── iPhone Safari
                              │  events.jsonl                                        GET /, GET /ws
```

## Layout

- `crates/nearpushd`: the daemon (axum + tokio). The web page, manifest and icons live
  in `crates/nearpushd/assets/` and are compiled into the binary.
- `crates/nearpush`: the CLI (clap + reqwest/rustls).
- `tools/gen_icons.py`: regenerates the PNG icons (standard-library Python).

## Build

Build on macmini, never on the laptop. Both are arm64 macOS, so the default target works.

```sh
rsync -a --delete --exclude target/ --exclude .git/ --exclude .agents/ ./ macmini:~/repo/nearpush/
ssh macmini 'cd ~/repo/nearpush && export PATH=$HOME/.cargo/bin:$PATH && cargo build --release && cargo test'
mkdir -p ~/.local/bin
scp macmini:~/repo/nearpush/target/release/{nearpushd,nearpush} ~/.local/bin/
```

## Files

| Path | What |
|------|------|
| `~/.config/nearpush/config.toml` | optional config (below) |
| `~/.config/nearpush/token` | push token, created on first start with mode 0600 |
| `~/.local/state/nearpush/events.jsonl` | one JSON event per line, reloaded on start |

`NEARPUSH_CONFIG_DIR` and `NEARPUSH_STATE_DIR` (or `--config-dir` / `--state-dir`)
override the two directories.

```toml
# ~/.config/nearpush/config.toml (every key is optional)
listen = "127.0.0.1:8787"        # must be loopback; nearpushd refuses anything else
allowed_logins = ["you@example.com"]  # Tailscale-User-Login values allowed to view
buffer_size = 500                # events kept in memory for replay (1..=10000)
allow_loopback_without_login = false  # local testing: let header-less loopback requests view
```

When `allowed_logins` is empty, nearpushd asks `tailscale status --json` for this node's
own login (it retries every 30 s until tailscaled answers) and allows only that login.
Set the list explicitly to share the display with another tailnet user.

The event log is compacted on start when it holds more than twice `buffer_size` lines
(or a damaged line). The newest events are kept, so ids never go backwards.

## Run

### LaunchAgent

`deploy/install-launchagent.sh` renders `deploy/dev.nearpush.daemon.plist.in` into
`~/Library/LaunchAgents/dev.nearpush.daemon.plist` (replacing `@HOME@`), creates the log
directory and bootstraps it (booting out a previous copy first). The result is:

`~/Library/LaunchAgents/dev.nearpush.daemon.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.nearpush.daemon</string>
  <key>ProgramArguments</key>
  <array><string>/Users/YOU/.local/bin/nearpushd</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/Users/YOU/Library/Logs/nearpush/nearpushd.out.log</string>
  <key>StandardErrorPath</key><string>/Users/YOU/Library/Logs/nearpush/nearpushd.err.log</string>
</dict>
</plist>
```

```sh
mkdir -p ~/Library/Logs/nearpush
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.nearpush.daemon.plist
launchctl kickstart -k gui/$(id -u)/dev.nearpush.daemon   # restart after replacing the binary
```

Keep the binary on the internal disk: launchd agents cannot read files on external volumes.

### Tailscale

Check what is already served first, and do not replace an existing `/` mapping:

```sh
tailscale serve status
tailscale serve --bg http://127.0.0.1:8787               # https://<laptop>.<tailnet>.ts.net/
# if https:443 is taken:
tailscale serve --bg --https=8443 http://127.0.0.1:8787   # https://<laptop>.<tailnet>.ts.net:8443/
```

Never enable funnel. The page and socket rely on the `Tailscale-User-Login` header that
`tailscale serve` adds for tailnet users.

## CLI

```sh
nearpush send --title "build finished"
nearpush send --title "F38 多工位会话拼单" --state allow --subtitle "part of the source"
nearpush send --title "frontend 重構" --state your_turn --project "frontend / 7008" --started-at 18m
nearpush send --title "Claude母公司招股书精读" --state all_set --duration 1m
nearpush send --title "deploy failed" --level error --url https://github.com/... --tag ci
make 2>&1 | tail -20 | nearpush send --title "make output" --body -
```

| Flag | Meaning |
|------|---------|
| `--title` | required; the headline for generic events, the task line for `--state` events (≤ 200 chars) |
| `--body` | text (≤ 4000 chars); `-` reads stdin (≤ 16 KiB) |
| `--level` | `info` (default), `warn`, `error` |
| `--state` | `allow`, `your_turn`, `all_set`, `working` (`your-turn`/`all-set` also accepted) |
| `--subtitle` | short line under the headline (≤ 200 chars) |
| `--project` | footer text (≤ 100 chars) |
| `--started-at` | `now`, unix seconds or milliseconds, or an age such as `18m` / `1h5m`; drives the live "Xm so far" |
| `--duration` | `65`, `90s`, `1m`, `1h5m`; shown as "took Xm" |
| `--url` | http(s) link opened when the card is tapped |
| `--tag` | repeatable, ≤ 10 tags of ≤ 32 chars |
| `--addr` / `NEARPUSH_ADDR` | daemon URL, default `http://127.0.0.1:8787` |
| `--token-file` / `NEARPUSH_TOKEN_FILE` | default `~/.config/nearpush/token` |

It prints `sent event #<id>` and exits 0, or prints the reason and exits 1.

## HTTP API

- `POST /push`: JSON body with `title` and the optional fields above (`started_at` is
  unix ms, `duration` is sent as `duration_ms`). Requires a direct loopback peer, the
  header `Authorization: Bearer <token>`, and no proxy headers: a request carrying any
  `Tailscale-*`, `X-Forwarded-For`, `X-Forwarded-Host` or `Forwarded` header is refused
  with 403, so nothing coming through `tailscale serve` can push. Bodies over 16 KiB get 413;
  unknown fields or values that break a limit get 400. Returns `{"id":N,"ts":ms}`.
- `GET /ws?since=<id>`: WebSocket. Frames are JSON text:
  `hello` (`version`, `last_id`, `oldest_id`, `server_time`), then one `event` per
  buffered event with id > `since`, then `synced`, then live `event`s. A protocol ping and
  a `{"type":"ping"}` frame go out every 20 s. A subscriber that falls behind the live
  channel is resynced from the buffer, not dropped. A browser `Origin` must match the
  `Host` header.
- `GET /`, `/manifest.webmanifest`, `/icon-*.png`: need an allowlisted
  `Tailscale-User-Login` (or the local-testing flag).

## iPhone

1. Install Tailscale on the iPhone and sign in to the same tailnet as the laptop.
2. In Safari open `https://<laptop>.<tailnet>.ts.net/` (append `:8443` if you served on
   that port). You should see a black screen with "waiting." and a green dot.
3. Tap the Share button, then **Add to Home Screen**, then **Add**.
4. Open nearpush from the home screen. It runs full screen, in portrait or landscape.
5. For a desk display: Settings → Display & Brightness → **Auto-Lock → Never**, and keep
   the phone on its charger. The page also asks for a Screen Wake Lock (iOS 16.4+; the
   drawer shows whether it holds one, and a tap re-requests it). To pin the phone to this
   page, use **Guided Access** (Settings → Accessibility → Guided Access, then triple-click
   the side button inside the app), which also lets you disable the Auto-Lock timer for the
   session.

The dot in the top right is green when live, amber while connecting and red while
offline (the drawer header shows the retry countdown).

### Updates and the iOS background limitation

iOS suspends a web page as soon as it leaves the screen, and its WebSocket goes with it.
There is no Web Push here, so **nothing arrives while the page is in the background or the
phone is locked**; it is a display, not a notifier. When the page becomes visible again
(`visibilitychange` / `pageshow`) it reconnects at once with `since=<last seen id>` from
localStorage, so every event pushed in the meantime is replayed, as long as it is still
in the daemon's buffer (`buffer_size`, default 500). While visible it reconnects with
exponential backoff (1 s doubling to 30 s) and treats 50 s of silence as a dead socket.

The hello frame carries the UI version (crate version plus a hash of the embedded
assets). When it differs from the version the page was loaded with, the page reloads
itself, so installing a new `nearpushd` updates the phone with no action on it.
