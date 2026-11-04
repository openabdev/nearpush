# Kiro CLI → nearpushd → iPhone

How Kiro CLI sessions drive the iPhone desk display.

> Status: the `working` filter, the Stop classifier (`your_turn` vs `all_set`), the
> workflow-step skip rule and the workflow count are built. The menu bar app is built but
> not yet installed; the section marked *(planned)* describes it until it is.

## Architecture

```mermaid
flowchart LR
  subgraph Laptop
    K[Kiro CLI session] -->|hook stdin JSON| H[nearpush hook]
    H -->|POST /push<br/>loopback + token| D[nearpushd<br/>127.0.0.1:8787]
    D -->|prompt| S[Haiku 5.5<br/>summarizer]
    S -->|label| D
    D --> J[(events.jsonl)]
    A[Menu bar app] -->|/status /control| D
  end
  D -->|tailscale serve| T[HTTPS on tailnet]
  T -->|wss /ws| P[iPhone SE<br/>Safari page]
```

- **`nearpush hook`** runs on every Kiro hook event, reads the event JSON on stdin, turns it
  into a card, posts it to nearpushd and exits. It always exits 0, prints nothing on
  stdout, gives the daemon under a second and stays quiet when the daemon is down, so it
  never blocks or breaks the agent. `NEARPUSH_DISABLE=1` makes it exit immediately.
- **nearpushd** listens on loopback only. It stores events in
  `~/.local/state/nearpush/events.jsonl`, applies the display filter, broadcasts to
  viewers, and asks the summarizer for a per-session purpose label in the background.
- **Tailscale** (`tailscale serve`) exposes HTTPS to the tailnet only. Requests that arrive
  through it may view (`/`, `/ws`) but are refused on `/push`, `/status` and `/control`.
- **Menu bar app** *(planned)* talks to nearpushd over loopback `/status` and `/control`.

## One turn

```mermaid
sequenceDiagram
  participant U as You
  participant K as Kiro
  participant H as nearpush hook
  participant D as nearpushd
  participant P as iPhone
  U->>K: prompt
  K->>H: UserPromptSubmit
  H->>D: working + start time
  D-->>P: filtered out (no card)
  D->>D: Haiku summary (async)
  D-->>P: session_update label
  K->>K: work, tools
  K->>H: Stop + final reply
  H->>H: question? -> your_turn<br/>else -> all_set
  H->>D: card + took Xm + wf count
  D-->>P: big card
```

1. **UserPromptSubmit**: the hook records the turn start and sends a `working` event. The
   default filter (`[filter] states = ["all_set", "your_turn", "allow"]` in
   `~/.config/nearpush/config.toml`, or `POST /control {"states": [...]}` at runtime) keeps
   `working` off the iPhone: the event is stored and listed in `/status` but not
   broadcast, so neither the big card nor the history drawer shows it. The start time
   still drives `took Xm`. Events without a state (`nearpush send` without `--state`)
   always show.
2. **Session label**: nearpushd sends the prompt to the summarizer
   (`kiro-cli chat --no-interactive --model claude-haiku-5.5 --trust-tools=`, run with
   `NEARPUSH_DISABLE=1` so it does not trigger the hook again, and with `KIRO_API_KEY`
   read from Keychain service `nearpush-kiro-api-key`). When the label arrives, a
   `session_update` frame updates the card and history rows in place.
3. **Stop** (the turn ended): the hook classifies Kiro's final reply. Kiro's Stop input
   carries only `hook_event_name`, `cwd` and `session_id`, with no reply text
   ([hook docs](https://kiro.dev/docs/hooks/types/), [Kiro#6258](https://github.com/kirodotdev/Kiro/issues/6258)).
   The hook therefore reads the reply from the transcript Kiro CLI writes before the hook
   runs: `~/.kiro/sessions/<workspace-hash>/<session_id>/messages.jsonl`, the last
   `assistant` record with `operationType: "Say"` after the latest `turn_start`
   (`Reasoning` records are skipped). A `response` field on stdin wins if Kiro adds one.
   - **`your turn.`** (white + red) when the reply waits on you, judged on its last 8
     lines outside code blocks: the last line ends with `?`/`？`; or a list of two or
     more options (`1.`, `A)`, `①`) follows an intro line that ends with `?`, or ends
     with `:` and names a choice (option, which, 選, 方案, 哪); or one of the last 3 lines
     has a phrase such as 請選, 要不要, 你決定, 跟我說, 可以嗎, "should I", "which option",
     "let me know", "your call". The question line (redacted, at most 80 characters)
     becomes the detail.
   - **`your turn.`** also when a workflow run this session started is paused waiting
     on you; the detail is `N wf waiting`.
   - **`all set.`** (green) otherwise, with `took Xm`.
   The card carries the duration, the session label, the project/branch and, when any
   are active, the number of running or waiting workflows. Those come from
   `~/.kiro/sessions/<hash>/workflows/wf_*/workflow-state.json` (`status`,
   `parentSessionId` = this session): `running` counts while `run.beat` was touched in
   the last 120 s, `paused` counts as waiting when the state changed in the last 24 h.
4. **Reconnect**: the iPhone reconnects with `since=<last id>` and nearpushd replays what it
   missed; a small note appears if events fell out of the buffer.

## Card fields

| Field | Source |
|-------|--------|
| icon / app | `sender = kiro`, `app = Kiro` |
| session tag | Kiro `session_id` |
| session label | Haiku purpose summary (first prompt, truncated, until it arrives) |
| project | cwd basename + git branch |
| detail | question line (`your_turn`) or `took Xm` (`all_set`) |
| wf count | running / waiting workflows for this session (page footer, e.g. `1 wf running · 1 wf waiting`) |

## Install

Global hook file, which applies to every agent and workspace: `~/.kiro/hooks/nearpush.json`.

```json
{
  "version": "v1",
  "hooks": [
    { "name": "nearpush-prompt", "trigger": "UserPromptSubmit", "action": { "type": "command", "command": "/Users/<you>/.local/bin/nearpush hook" }, "timeout_ms": 3000 },
    { "name": "nearpush-stop",   "trigger": "Stop",             "action": { "type": "command", "command": "/Users/<you>/.local/bin/nearpush hook" }, "timeout_ms": 3000 },
    { "name": "nearpush-end",    "trigger": "SessionEnd",       "action": { "type": "command", "command": "/Users/<you>/.local/bin/nearpush hook" }, "timeout_ms": 3000 }
  ]
}
```

- `PreToolUse` is optional. It only produces `working` cards (`tool: summary`), so add it
  only if `working` is enabled in the filter.
- Use either this global file or an agent profile's `hooks` array, not both, or each event
  is pushed twice.
- Hooks load when a session starts. Open a new Kiro session after installing.
- Remove the file to turn the integration off.

## Limitations

- Kiro has no "waiting for approval" hook, and `PreToolUse` input does not say whether a
  tool call will prompt, so hooks never produce `allow.`. Use
  `nearpush send --state allow` from another tool if needed.
- The `your_turn` / `all_set` split is a heuristic on the final reply. A casual closing
  question ("要不要我說明一下？") is shown as `your_turn`.
- Workflow step sessions also fire global hooks. The hook skips a session whose
  `session.json` has a `rootConversationId` other than its own id, a
  `_meta.kiro.workflow` object or a `_meta.kiro.serverSession.ownerSessionId`, so only
  the session you talk to notifies. `[hook] include_subagents = true` in config.toml
  turns the skip off. A missing `session.json` counts as a top-level session.
- iOS suspends Safari in the background. Keep the page in front (Auto-Lock: Never, or
  Guided Access) for live updates; missed events replay on return.
