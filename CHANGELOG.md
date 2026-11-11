## 0.3.0

Relay

- The relay puller ingests turn events from the kiro nearpush adapter. A relay body with an event key is read as the adapter shape and mapped to the same event the local hook sends. turn_start shows a working card that starts at the event timestamp. turn_end gets the rules verdict on the reply tail, carries the took time from the open turn, and goes through the classifier. session_end clears the open turn and the session's pending state and stores no card.
- Relay events go through the same ingest path as POST /push, so they get the same classifier and pending handling. Adapter events carry their relay id, so the restart-time dedupe sees them.
- A relay body that is malformed or names an unknown event is skipped as invalid_event and logged.
- The rules classifier, redaction, duration formatting and RFC 3339 parsing moved into the new nearpush-common crate, which nearpush and nearpushd both use. The code moved without being rewritten.

Card

- Swiping the main card on the phone past 35% of its width, or flicking it, dismisses it. A dismissed your_turn leaves the pending list, and the card moves to the next pending session or to all set. A working or stored card is hidden, and the page shows "nothing pending." until the next event.
- A "Dismissed · Undo" toast stays for 5 seconds. Other viewers follow the dismissal. When the undo window ends, the daemon writes a dismissed history record that is never a notification. Open dismissals are kept in control.json, and a restart makes them final.
- The dismissal uses a narrow viewer write path: POST /v1/dismiss and POST /v1/undismiss, with a body naming exactly one session or event id. They are accepted from an allowed Tailscale login with a same-origin Origin and a JSON content type, or from direct loopback with the push token. They are limited to 10 calls per minute per caller. They can only remove or restore a pending entry or hide a card. /push, /status and /control are unchanged.
- The swipe animates only transform and opacity, with up to 8 degrees of tilt. Vertical scrolling still works, a swipe that starts in the left 20 px is ignored, and reduced motion fades only.
- An "N wf waiting" footer is shown as information on all_set cards only. your_turn and other cards carry no count, and running counts are never sent or shown.
- Waiting workflows never force your_turn and never skip the classifier.

Classifier

- The turn-end verdict comes from the reply only, first the provider and then the rules. The waiting-workflow count is data for the footer and is applied after the verdict, so it cannot change it.

macOS app

- Nearpush runs as a standalone Dock app. It opens a settings window at launch and when the Dock icon is clicked, and keeps running when the window closes. The settings window has Status, Display, Classifier, Phone, Relay, Diagnostics and General sections. LSUIElement is removed from Info.plist.
- The app has an icon: a near-black rounded card with a red six-armed asterisk and two push arcs. The SVG source and a render script are in the repo, and build-app.sh copies AppIcon.icns into the bundle.
- The menu bar icon is optional (Show menu bar icon, on by default). Its menu is slimmed to a status line, Settings… (Command-comma), Pause pushes and Quit Nearpush. A failed action shows as a warning in the status line until the next success. The other menu items now live in the settings window.
- The summarizer toggle (Session labels) is in the Classifier section of the settings window.
- The main menu still stays open while the cursor rests on it, with the 60 second fallback timer.

Hook CLI

- The hook CLI is refreshed. On Stop it sends the waiting-workflow count of its own session as a separate field, read-only and with a 40 ms budget. No workflow count is part of the verdict, the title, the detail or the classifier input. The daemon still accepts and drops workflows_running from older hook builds.

Deploy

- The kiro nearpush adapter is applied to one host.
- The same change is prepared for a second host but not applied. Nothing in this release changes that host.

## 0.2.0

Relay

- The eventrelay crate is a store-and-forward relay. Producers post events with a per-producer source token. The laptop daemon reads them with the consumer token and acknowledges a watermark. Events are kept in one SQLite file and pruned by count and age.
- nearpushd has a relay puller. It long-polls the consumer path, runs each event through the same path as /push, saves the cursor, and then acks the batch. Duplicate and crash-window events do not show a card twice.
- The relay cursor advances past batches made only of events the daemon has already stored, so a batch like that no longer triggers an immediate re-fetch.
- Relay failures back off exponentially, up to 60 seconds with jitter. The backoff resets only after a batch is fully processed and acknowledged. A rejected token disables the puller with one error log.
- /status reports a relay object with enabled, connected, last event id, last success time, and last error class. It carries no event bodies and no tokens.

Card

- The main card is driven by the pending your_turn sessions. A session stays pending from its your_turn Stop until it prompts again, sends an all_set Stop, or ends. When several sessions are pending, the card shows the most recent one with a count of how many are waiting.
- When the last pending session is answered, the card reads working until that session's next Stop or SessionEnd.
- Pending cards are saved to control.json and reloaded at startup. /status exposes the pending count and the pending sessions.
- The your_turn question line sits directly under the headline and is larger than the secondary text. It is clamped to three lines.
- The card footer wraps so that the app line, the timer, and the tag fit above the workflow count and the time.
- A live working card for a pending session is sent when the user submits a new prompt. The hook accepts the daemon's working-card reply.

Classifier

- The turn-end classifier has three providers: kiro (default), OpenRouter, and TypeSafe Jev 1.13. Each provider decides whether a Stop is your_turn or all_set. If the provider fails or its key is missing, the rules verdict is used.
- The Jev providers answer a choice question with the labels your_turn and all_set. A malformed or unknown answer falls back to the rules verdict.
- A your_turn verdict is accepted only when the reply has a question line. Otherwise the rules verdict applies.
- Future-tense plans are not your_turn. A future marker such as a later step or after that discounts an asking phrase inside its own clause. A blocking question or an explicit choice in the same message still counts as your_turn.
- A waiting workflow does not change the verdict or the card. The classifier runs on every Stop.
- Working cards are hidden by default. The default event filter does not show working cards.
- Held Stops of one session commit in arrival order, so an older your_turn cannot land after a newer all_set.
- The README and docs/kiro.md describe the providers, their keychain items, the 15 second hold, and the rules fallback. The README also states that the CLI and daemon must roll back together.

Menu bar app

- The main menu stays open while the cursor rests on it. Poll, diagnostics, and supervisor updates are held while the menu is open and applied when it closes. The hold is bounded by a 60 second timer so a missed close still flushes the updates.
- The menu bar app has a classifier picker with three checkable items for kiro, OpenRouter, and TypeSafe. It shows a disabled state when the classifier is off in config.toml. Diagnostics shows one classifier line that reports whether a key is present, without reading the key value.
- The app decodes the classifier and pending_your_turn fields from /status. An older daemon still decodes.
- The iPhone URL QR window no longer crashes AppKit. The hosting controller is created once, and the QR image is built once.
- A poll change that skipped unchanged results was reverted by a later commit on this branch. It is listed here because it is part of the history since 0.1.0.

Deploy files

- deploy/eventrelay holds the Dockerfile, five Kubernetes manifests for the example-relay namespace, and a README. The image runs as a non-root user on a distroless base with a read-only root filesystem.
- The README creates the namespace first, applies the manifests one file at a time, and reads the consumer token from the keychain without putting it on the command line or in a file.
- Preparation for two per-host hooks is written up but not applied. Nothing in this release changes those hosts.
