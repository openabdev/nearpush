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

The top line shows the sender icon, the app name, the timer and a short session tag
(`#` plus the last 4 characters of the session id). Below the headline come the project
(and host), the session label, the task line (the title, unless it only repeats the
headline) and the detail line (`detail`, else the subtitle or body). Every line is cut
with an ellipsis, so the card never scrolls. The footer holds the time and, after a gap
in the replay, a small "N events missed" note for two minutes. A swipe up, or the ☰
button, opens a drawer with the last 100 events, each with its icon and session tag;
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
- `macos/`: the Dock app with a settings window and an optional menu bar icon (SwiftPM; `NearpushCore` library + `Nearpush` executable).
  `scripts/build-app.sh` bundles it with nearpushd into `Nearpush.app`.
- `tools/gen_icons.py`: regenerates the PNG icons (standard-library Python).
- [`docs/kiro.md`](docs/kiro.md): how Kiro sessions drive the display, with diagrams.

## Build

Build on a build host (arm64 macOS, with Rust installed), never on the laptop. Replace
`<build-host>` with its SSH name. The default target works on both machines.

```sh
rsync -a --delete --exclude target/ --exclude .git/ --exclude .agents/ ./ <build-host>:~/repo/nearpush/
ssh <build-host> 'cd ~/repo/nearpush && export PATH=$HOME/.cargo/bin:$PATH && cargo build --release && cargo test'
mkdir -p ~/.local/bin
scp <build-host>:~/repo/nearpush/target/release/{nearpushd,nearpush} ~/.local/bin/
```

## Files

| Path | What |
|------|------|
| `~/.config/nearpush/config.toml` | optional config (below) |
| `~/.config/nearpush/token` | push token, created on first start with mode 0600 |
| `~/.local/state/nearpush/events.jsonl` | one JSON event per line, reloaded on start (0600) |
| `~/.local/state/nearpush/sessions.json` | per-session prompt history (capped) and summarized labels (0600) |
| `~/.local/state/nearpush/hooks/*.json` | `nearpush hook` per-session state (0600) |
| `~/.local/state/nearpush/control.json` | runtime pause and filter set through `POST /control` (0600) |
| `~/Library/Logs/nearpush/nearpushd.log` | daemon log when started by the menu bar app (`--log-file`, 0600) |
| `~/.config/nearpush/icons/<sender>.{svg,png}` | optional icon overrides |

The state directory is forced to 0700 and its files to 0600 on every start.

`NEARPUSH_CONFIG_DIR` and `NEARPUSH_STATE_DIR` (or `--config-dir` / `--state-dir`)
override the two directories.

```toml
# ~/.config/nearpush/config.toml (every key is optional)
listen = "127.0.0.1:8787"        # must be loopback; nearpushd refuses anything else
allowed_logins = ["you@example.com"]  # Tailscale-User-Login values allowed to view
buffer_size = 500                # events kept in memory for replay (1..=10000)
allow_loopback_without_login = false  # local testing: let header-less loopback requests view
public_url = "https://laptop.example.ts.net/"  # shown in /status; unset: from tailscale status

[filter]                         # which agent states the page shows
states = ["all_set", "your_turn", "allow"]   # default; add "working" to see turn starts

[hook]                           # read by `nearpush hook` only
include_subagents = false        # true: workflow-step / sub-agent sessions push too

[summarizer]                     # LLM that labels each Kiro session with its purpose
enabled = true
command = ["kiro-cli", "chat", "--no-interactive", "--model", "claude-haiku-5.5", "--trust-tools="]
timeout_secs = 30
keychain_service = "nearpush-kiro-api-key"  # KIRO_API_KEY for each run; "" = no lookup

[classifier]                     # decides your_turn vs all_set at each Stop (see below)
enabled = true                   # false: rules verdict only, no provider calls
provider = "kiro"                # "kiro" | "openrouter" | "typesafe"; menu bar app can change it live
threshold = 0.5                  # openrouter/typesafe: noul at or above this is your_turn (0..=1)
timeout_secs = 30                # provider's own limit (1..=30); the 15 s card hold is the binding limit
```

**Display filter.** An event is shown when it is not muted (see pause below) and either
has no `state` (generic `nearpush send` events always show) or has a state listed in
`[filter] states`. Hidden events are stored in `events.jsonl` and listed in
`GET /status` with `"shown": false`, but they are never broadcast or replayed, so
neither the main card nor the history drawer shows them. `POST /control {"states": [...]}`
changes the list at runtime and applies to open sockets at once; the override is kept in
`~/.local/state/nearpush/control.json` (0600) and from then on wins over `[filter]` in
config.toml (delete `control.json` and restart to go back to the config value). The
`[filter]` and `[hook]` sections need the nearpushd from this version: the older
daemon rejects unknown config keys.

**Swipe to dismiss.** On the phone, swipe the main card left or right (past about a
third of its width, or a quick flick) to dismiss it; a "Dismissed · Undo" toast stays
for 5 s. A `your_turn` card leaves the pending list, so the card moves to the next
pending session or to all set; any other card is hidden and the page shows "nothing
pending." until the next event. Every viewer follows. When the undo window ends, the
daemon writes a `"dismissed": true` record to history (no notification). The page calls
`POST /v1/dismiss` and `POST /v1/undismiss` with `{"session": "<id>"}` or
`{"event_id": N}`. They are the only routes a tailnet viewer can write to, and they
accept a request only when its `Tailscale-User-Login` is in `allowed_logins`, its
`Origin` matches the host, and its body is JSON. A direct loopback request with the push
token also works. Each caller gets 10 calls per minute. These routes can remove or
restore a pending entry or hide a card. They cannot create, edit or push an event, and
they cannot change pause, the filter or the classifier. The page test runs with
`scripts/test-page.sh` (JavaScriptCore, macOS).

The broadcast channel per viewer is never larger than `buffer_size`, so a viewer that
falls behind can normally resync from the buffer. When it cannot, or when a reconnecting
page asks for events older than the buffer, the daemon says how many were skipped and the
page shows the "N events missed" note.

When `allowed_logins` is empty, nearpushd asks `tailscale status --json` for this node's
own login (it retries every 30 s until tailscaled answers) and allows only that login.
Set the list explicitly to share the display with another tailnet user.

The event log is compacted on start when it holds more than twice `buffer_size` lines
(or a damaged line), and again at runtime whenever it passes twice `buffer_size`. The
newest events are kept, so ids never go backwards.

### Session labels

`nearpush hook` sends the user's prompt with each UserPromptSubmit event (field
`prompt`). The daemon never writes it to `events.jsonl`; it keeps the first prompt and
the last four per session (500 characters each, 100 sessions) in `sessions.json`, and
runs the summarizer on a session's first prompt, every fifth prompt, and when a prompt
shares almost nothing with the earlier ones. Runs are debounced, one at a time, killed
after `timeout_secs`. The command gets a fixed instruction plus the prompts on stdin and
must print the label on stdout; it is cut to 30 CJK or 50 other characters. It runs in
`~/.local/state/nearpush/summarizer/` with `NEARPUSH_DISABLE=1` set, which makes
`nearpush hook` exit at once, so a summarizer kiro-cli run with the nearpush hooks
installed does not report itself. A new label is broadcast as a `session_update` frame;
the page updates that session's cards in place, and later events of the session carry
the label. If the summarizer is missing, fails or times out, the hook's provisional
label (the first prompt, truncated to 40 characters) stays and a warning is logged.
After a failed run the session gets no new run for 1 minute, doubling per consecutive
failure up to 1 hour, so a logged-out kiro-cli does not start a process on every prompt.

**Cost and data flow.** The summarizer is on by default. Each run is one
`kiro-cli chat` call on `claude-haiku-5.5` (0.06x credits) billed to the Kiro account
kiro-cli is logged in with, and it sends that session's stored prompts (first + last
four, 500 characters each) to Kiro's model service. Expect about one run per session
plus one per five prompts or topic change. Set `[summarizer] enabled = false` to stop
this: the daemon then keeps no prompts at all (nothing is written to `sessions.json`)
and the cards show the hook's provisional label.

The LaunchAgent's environment has no `KIRO_API_KEY`, so before each run nearpushd reads
it from the login keychain with
`/usr/bin/security find-generic-password -s nearpush-kiro-api-key -w` and sets it only
in that run's environment. Only exit status 0 counts (a missing item exits 44); the
trailing newline is trimmed, the key is never logged, written to state or put on argv,
and it is replaced by `[redacted]` in the stderr excerpt of a failed run. Without a key
the run is skipped (with the same backoff) and the provisional label stays. Store the
key once:

```sh
security add-generic-password -a "$USER" -s nearpush-kiro-api-key -T /usr/bin/security -w
```

`[summarizer] keychain_service = ""` turns the lookup off (the command then has to find
its own login, e.g. `kiro-cli login`); `security_program` (default `/usr/bin/security`)
exists for tests.

### Stop classifier

At each Stop with a reply, the card is held up to 15 s while a classifier decides between
`your turn.` and `all set.`. The hook sends the last 8 non-empty reply lines, redacted,
with the rules verdict; the daemon redacts them again before any provider call. The
provider answers, and the daemon then commits the card. If the provider fails, is missing
its key, or misses the 15 s hold, the card goes out with the rules verdict. A
`working.` card is never classified. A waiting workflow run never forces `your turn.` and
never skips the classifier.

| Provider | Model | Where the reply tail goes | Keychain service |
|----------|-------|---------------------------|------------------|
| `kiro` (default) | Haiku 5.5 through `kiro-cli chat --no-interactive` | Kiro's model service, billed to the Kiro account | `nearpush-kiro-api-key` |
| `openrouter` | Jev 1.13 (`typesafe/jev-1.13`) at `https://openrouter.ai/api/v1/systemone` | OpenRouter, which forwards the request in plaintext to the Jev provider | `nearpush-openrouter-api-key` |
| `typesafe` | Jev 1.13.0 (`jev-1.13.0`) at `https://api.typesafe.ai/v1/systemone` | TypeSafe, directly | `nearpush-typesafe-api-key` |

Jev answers with a probability (`noul`) for the question "is the assistant's final message
asking the user for a decision or input?". Values at or above `threshold` mean `your turn.`.
Jev is English-first, and its answer is probabilistic, so expect some misses on Chinese
replies. Each Stop with the `kiro` provider runs one `kiro-cli` call.

Store the keys you use (`kiro` uses the same item as the summarizer):

```sh
security add-generic-password -a "$USER" -s nearpush-openrouter-api-key -T /usr/bin/security -w
security add-generic-password -a "$USER" -s nearpush-typesafe-api-key -T /usr/bin/security -w
```

The menu bar app's *Classifier* submenu switches the provider at runtime
(`POST /control {"classifier": "typesafe"}`, kept in `control.json`), and Diagnostics shows
whether the selected key is present. `[classifier] enabled = false` turns the provider
calls off; then the classifier submenu is disabled.

The hook sends two fields the older daemon does not know (`classify` on Stop and
`turn_start` on each prompt), and the daemon rejects unknown fields. Roll back the CLI
together with the daemon: restore `~/.local/bin/nearpush` from the previous build
(`nearpush.prev` in the deploy folder) in the same step as the app or daemon. A new CLI
with an older daemon loses each prompt and Stop event with a 400.

## Run

### Dock app (recommended)

`Nearpush.app` is a Dock app (macOS 14+). It opens a settings window at launch and when
you click its Dock icon. The window has Status, Display (pause and Show on iPhone per state),
Classifier, Phone (URL, copy, QR code), Relay, Diagnostics, and General (Launch at Login, Show menu
bar icon, Quit). Closing the window keeps the app running. The menu bar icon is optional and is
controlled by the "Show menu bar icon" setting. The app bundles nearpushd at
`Contents/MacOS/nearpushd` and keeps it running. Build it on the build host and
install it on the laptop:

```sh
rsync -a --delete --exclude target/ --exclude .build/ --exclude .agents/ --exclude .git/ --exclude build/ ./ <build-host>:~/repo/nearpush/
ssh <build-host> 'cd ~/repo/nearpush && export PATH=$HOME/.cargo/bin:$PATH && NEARPUSH_BUILD_HOST="<build-host-glob>" scripts/build-app.sh'
scp <build-host>:~/repo/nearpush/build/Nearpush.zip "$TMPDIR"/ && ditto -x -k "$TMPDIR"/Nearpush.zip ~/Applications/
scp <build-host>:~/repo/nearpush/build/nearpush ~/.local/bin/nearpush   # the matching CLI
open ~/Applications/Nearpush.app
```

`scripts/build-app.sh` runs `cargo build --release`, `swift build -c release`, assembles
`build/Nearpush.app`, signs it ad hoc (`SIGN_ID` overrides the identity) and zips it.
It refuses to run unless the host name matches `NEARPUSH_BUILD_HOST` (a shell glob, for
example `build-*`). `NEARPUSH_ALLOW_LOCAL_BUILD=1` skips that check.

**Daemon lifecycle.** The app registers the bundled daemon as a launchd agent with
`SMAppService.agent` (`Contents/Library/LaunchAgents/dev.nearpush.agent.plist`, label
`dev.nearpush.agent`, `KeepAlive {SuccessfulExit = false}`). This works with the ad-hoc
signature on macOS 26 and is the mode in use on the laptop. The daemon keeps running when
the menu is quit, and launchd restarts it after a crash. If registration fails, the app
unregisters, remembers `daemonMode = child` with the reason (shown under Diagnostics) and
runs nearpushd as a child process, restarted with a 1 s to 30 s backoff; Quit then stops
it. `defaults write dev.nearpush.app daemonMode agent|child` forces a mode.

Before starting anything the app probes 127.0.0.1:8787. If another nearpushd answers (an
older one without `/status`, or one at a different path), the menu says so and the app
starts nothing. nearpushd itself exits 0 when the port is taken, so launchd never
crash-loops against another daemon.

**Menu.** Status line (running or paused, iPhone count, latest event); *iPhone URL* with
a QR code window and Copy URL; the last 5 events (click to copy; filtered ones are marked
"(hidden)"); *Pause pushes*; *Show on iPhone* (one checkbox per state, applied live via
`/control`); *Summarizer* (edits `[summarizer] enabled` in config.toml and restarts the
daemon); *Classifier* (Kiro · Haiku 5.5, OpenRouter · Jev 1.13 or TypeSafe · Jev 1.13.0,
applied live through `/control`); *Launch at Login* (`SMAppService.mainApp`); *Diagnostics* (daemon reachable,
token file, `tailscale serve` maps to 8787, Keychain item present, by exit status only,
the daemon mode and the login-item status; *Set up tailscale serve* appears only when
there is no serve config at all); Restart daemon; Open logs; Quit.

Logs: `~/Library/Logs/nearpush/nearpushd.log` (0600, rotated to `.1` past 10 MiB at
start). The app reads the token from `~/.config/nearpush/token`.

`~/.local/bin/nearpushd` is not used by the app. It is left in place as the rollback
binary for the hand-written LaunchAgent below.

### Rollback to the hand-written LaunchAgent

1. In the menu, turn off *Launch at Login*, then Quit Nearpush.
2. Stop the bundled agent: `launchctl bootout gui/$(id -u)/dev.nearpush.agent`. To
   remove its registration for good, delete `~/Applications/Nearpush.app` (or toggle
   it off in System Settings › General › Login Items & Extensions).
3. Check that nothing holds the port: `pgrep -fl nearpushd` prints nothing.
4. `mv ~/Library/LaunchAgents/dev.nearpush.daemon.plist.bak-menubar ~/Library/LaunchAgents/dev.nearpush.daemon.plist && launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.nearpush.daemon.plist`
5. `mv ~/.local/bin/nearpush.bak-menubar ~/.local/bin/nearpush`, in the same step as the daemon
   restore in step 4.
6. Remove `[filter]`, `[hook]`, `[classifier]` and `public_url` from
   `~/.config/nearpush/config.toml` if you added them; the old daemon refuses to start
   with unknown keys. The `classifier` key that `control.json` may hold is ignored by the
   old daemon, so that file needs no change.
7. `~/.local/state/nearpush/control.json` can stay (the old daemon ignores it). Events
   stored while paused carry `"muted": true`; the old daemon still loads them (its
   `Event` accepts unknown fields) but will replay them like any other event.

### LaunchAgent (without the app)

Use this or the menu bar app, not both: only one nearpushd can own 8787.

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
  <array><string>/Users/<user>/.local/bin/nearpushd</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/Users/<user>/Library/Logs/nearpush/nearpushd.out.log</string>
  <key>StandardErrorPath</key><string>/Users/<user>/Library/Logs/nearpush/nearpushd.err.log</string>
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
| `--started-at` | `now`, unix seconds or milliseconds, an RFC 3339 time such as `2026-11-04T18:49:33Z` or `2026-11-05T02:49:33+08:00` (an offset is required), or an age such as `18m` / `1h5m`; drives the live "Xm so far" |
| `--duration` | `65`, `90s`, `1m`, `1h5m`; shown as "took Xm" |
| `--url` | http(s) link opened when the card is tapped |
| `--tag` | repeatable, ≤ 10 tags of ≤ 32 chars |
| `--sender` | icon key, `[a-z0-9-]`, ≤ 32 chars; default `cli` |
| `--app` | app name on the top line (≤ 32 chars); default derived from the sender |
| `--session` | stable session id (≤ 128 chars); its last 4 characters become the tag |
| `--session-label` | what the session is for (≤ 100 chars) |
| `--host` | machine name (≤ 64 chars) |
| `--detail` | extra context line (≤ 300 chars) |
| `--addr` / `NEARPUSH_ADDR` | daemon URL, default `http://127.0.0.1:8787` |
| `--token-file` / `NEARPUSH_TOKEN_FILE` | default `~/.config/nearpush/token` |

It prints `sent event #<id>` and exits 0, or prints the reason and exits 1.

## Kiro CLI hooks

`nearpush hook` reads a [Kiro hook event](https://kiro.dev/docs/hooks/types/) on stdin
(`hook_event_name`, `cwd`, `session_id`, plus `prompt` or `tool_name`/`tool_input`) and
pushes a card with sender `kiro`, app `Kiro`, the project (cwd basename and git branch,
read from `.git/HEAD`), the session id, the label, the host and a detail line:

| Kiro event | Card |
|------------|------|
| `UserPromptSubmit` | `working.` (hidden by the default filter); starts the turn timer and sends the prompt to the summarizer |
| `PreToolUse` | `working.` with `tool: summary`, e.g. `execute_bash: cargo test` (the same tool is re-sent at most every 5 s); optional, see below |
| `Stop` | `your turn.` with the question line when the reply waits on you, else `all set.` with `took Xm` |
| `SessionStart`, `PostToolUse` | nothing |
| `SessionEnd` | removes the session's state file |

**Stop: `your turn.` vs `all set.`** Kiro's Stop input carries only `hook_event_name`,
`cwd` and `session_id`, no reply text ([hook docs](https://kiro.dev/docs/hooks/types/),
[Kiro#6258](https://github.com/kirodotdev/Kiro/issues/6258)). The hook reads the reply
from the transcript Kiro CLI writes before the hook runs,
`~/.kiro/sessions/<workspace-hash>/<session_id>/messages.jsonl`: the last `assistant`
record with `operationType: "Say"` after the latest `turn_start`, from the file's last
512 KiB. A `response`, `assistant_response` or `last_assistant_message` string on stdin
wins if Kiro adds one. A fast local heuristic, run on the last 8 lines outside code
blocks, picks `your turn.` when the last line ends with `?`/`？`; when two or more
numbered or lettered options follow an intro line that ends with `?`, or with `:` and a
choice word (option, choose, pick, which, 選, 方案, 哪); or when one of the last 3 lines
contains a phrase such as 請選, 要不要, 你決定, 跟我說, 告訴我, 是否要, 可以嗎, 好嗎,
選哪, 哪一個, "which option", "should I", "let me know", "your call", "do you want",
"would you like", "shall I", "want me to", "please confirm". The question line
(redacted, at most 80 characters) becomes the detail. The card is sent at once; no model
is asked. Without text the card is `all set.`.

**Workflow runs.** At Stop the hook counts this session's waiting workflow runs from
`~/.kiro/sessions/<hash>/workflows/wf_*/workflow-state.json` (`parentSessionId` = the
session, `status` = `paused`, state changed in the last 24 h; read-only, 40 ms budget,
skipped on any error) and sends the count as a separate `workflows_waiting` field. The
daemon shows it only as an FYI footer `N wf waiting` on a card whose final verdict is
`all set.`. It never changes the verdict, a `your turn.` card shows no count, and running
counts are not shown.

**Sub-agent sessions.** Workflow-step sessions fire global hooks too. The hook skips a
session whose `session.json` has a `rootConversationId` other than its own id, a
`_meta.kiro.workflow` object or a `_meta.kiro.serverSession.ownerSessionId`, so only the
session you talk to notifies. `[hook] include_subagents = true` turns the skip off.

It never shows `allow.`: the hook input carries no approval information (see below). The
tool summary uses only `command`, `path`/`file_path`, `url`, `query` or `pattern`, keeps
the first line, and hides env assignments (`TOKEN=…`), values after secret-looking flags,
`Bearer` credentials, URL passwords and token-shaped words.

The hook always exits 0, prints nothing on stdout (Kiro adds a successful
SessionStart/UserPromptSubmit hook's stdout to the agent's context), gives the daemon
under a second, and stays quiet when the daemon is down. With `NEARPUSH_DISABLE=1` it
exits before reading anything.

Add the entries in [`examples/kiro-agent-hooks.json`](examples/kiro-agent-hooks.json) to
the `hooks` array of an agent profile in `~/.kiro/agents/<agent>.json`:

```json
"hooks": [
  { "name": "nearpush-prompt", "trigger": "UserPromptSubmit", "action": { "type": "command", "command": "~/.local/bin/nearpush hook" }, "timeout_ms": 3000 },
  { "name": "nearpush-stop",   "trigger": "Stop",             "action": { "type": "command", "command": "~/.local/bin/nearpush hook" }, "timeout_ms": 3000 },
  { "name": "nearpush-end",    "trigger": "SessionEnd",       "action": { "type": "command", "command": "~/.local/bin/nearpush hook" }, "timeout_ms": 3000 }
]
```

Optional: `PreToolUse` only produces `working.` cards, so add it only when `working` is
in `[filter] states`:

```json
  { "name": "nearpush-tool",   "trigger": "PreToolUse",       "action": { "type": "command", "command": "~/.local/bin/nearpush hook" }, "timeout_ms": 3000 }
```

With the default config, the prompts of every hooked session also go to the session
summarizer, an LLM call billed to your Kiro account (see [Session labels](#session-labels)
for the data flow and how to turn it off).

Replace `~` with your home directory if your shell setup does not expand it. To cover
every agent instead, put the same entries in a standalone hook file,
`~/.kiro/hooks/nearpush.json`, as `{ "version": "v1", "hooks": [ ... ] }` (CLI V3 loads
global hook files for every workspace). Use one of the two placements, not both, or each
event is pushed twice. [`docs/kiro.md`](docs/kiro.md) walks through one turn with
diagrams.

### Why there is no `allow.` from hooks

Kiro's `PreToolUse` fires for every tool call, before the permission check is known to
the hook, and its input has no field saying whether the call will prompt. Whether it
prompts depends on rules from several scopes (Kiro's built-in asks, managed policy,
`~/.kiro/settings/permissions.yaml`, the workspace, the agent's `permissions`, approvals
given earlier in the session, and the `approve-for-me` classifier), so the hook cannot
reproduce the decision. Kiro also has no hook for "waiting for approval" or "waiting for
input": `Stop` (the turn finished) is the only moment the agent waits for the user. The
hook maps it to `your turn.` when the final reply asks something and to `all set.`
otherwise (see above). Use `nearpush send --state allow` by hand or from another tool.

## HTTP API

- `POST /push`: JSON body with `title` and the optional fields above (`started_at` is
  unix ms, `duration` is sent as `duration_ms`). Requires a direct loopback peer, the
  header `Authorization: Bearer <token>`, and no proxy headers: a request carrying any
  `Tailscale-*`, `X-Forwarded-For`, `X-Forwarded-Host` or `Forwarded` header is refused
  with 403, so nothing coming through `tailscale serve` can push. Bodies over 16 KiB get 413;
  unknown fields or values that break a limit get 400. Returns `{"id":N,"ts":ms}`.
- `GET /status`: same checks as `/push` (loopback peer, bearer token, no proxy headers;
  through `tailscale serve` it is 403). Returns `version`, `ui_version`, `pid`, `exe`,
  `started_at_ms`, `uptime_secs`, `viewers` (open WebSockets), `last_event_id`,
  `paused`, `filter` (`states`, `source`: `config` or `control`), `summarizer`
  (`enabled`, `healthy`, `last_ok_ms`, `last_error_ms`), `public_url` and `recent` (the
  last 5 events, hidden ones included, each with `shown`).
- `POST /control`: same checks. Body `{"paused"?: bool, "states"?: [state, …]}` (at
  least one key, unknown keys or states → 400); returns `{paused, filter}`. Changes apply
  to open sockets at once and are saved in `control.json`.
  **Pause** means: `/push` still stores the event, marked `"muted": true` in
  `events.jsonl`, but it is never broadcast and never replayed, even after unpausing or
  a restart. There is no muted marker frame; the page simply gets nothing.
- `GET /ws?since=<id>`: WebSocket. Frames are JSON text:
  `hello` (`version`, `last_id`, `oldest_id`, `missed`, `server_time`, `labels`), then
  one `event` per buffered event with id > `since`, then `synced`, then live `event`s.
  `missed` counts events after `since` that are no longer buffered; `labels` lists known
  `{session, label}` pairs. A protocol ping and a `{"type":"ping"}` frame go out every
  20 s. A subscriber that falls behind the live channel is resynced from the buffer, not
  dropped; if events fell out of the buffer meanwhile it first gets `{"type":"missed",
  "missed":N}`. `{"type":"session_update","session":…,"label":…}` arrives when the
  summarizer labels a session. A browser `Origin` must match the `Host` header.
- `GET /icons/<sender>`: the sender's icon. `~/.config/nearpush/icons/<sender>.svg`, then
  `.png`, then a built-in icon (`kiro`: the official logo from <https://kiro.dev/icon.svg>),
  then a generated tile with the sender's first letter. Only `[a-z0-9-]{1,32}` names
  resolve; anything else is 404.
- `GET /`, `/manifest.webmanifest`, `/icon-*.png`, `/icons/*`: need an allowlisted
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
