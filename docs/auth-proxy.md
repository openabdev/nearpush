# auth-proxy → nearpushd → iPhone

How [auth-proxy](https://github.com/openabdev/auth-proxy) approval requests are
shown on the iPhone desk display. This is the integration contract from
nearpush#2; the approval mechanism itself is auth-proxy#5 (`require_approval`)
with Muse as the first caller (auth-proxy#6).

nearpush is the **display layer only**. A card can catch the owner's eye and
link to the approval plane; it can never approve anything. Approval happens
only on the approval plane, signed by the owner's key (WebAuthn / device key),
and only auth-proxy executes the request afterwards.

## Architecture

```mermaid
flowchart LR
  subgraph OwnerHost["Owner-controlled host"]
    P[auth-proxy] -->|request parked| N[notifier<br/>local tool]
    N -->|nearpush send<br/>loopback + token| D[nearpushd<br/>127.0.0.1:8787]
    P -->|outcome| N
  end
  D -->|tailscale serve| T[HTTPS on tailnet]
  T -->|wss /ws| I[iPhone SE<br/>Safari page]
  I -->|tap card| A[approval plane<br/>owner signs]
  A -->|approve / deny| P
```

- **auth-proxy** parks a request whose grant says `require_approval` and hands
  a summary to a small local notifier running on the same host.
- **The notifier** is trusted local code, like the hooks: it alone may push
  `allow.` cards, over loopback with the push token. Tailnet viewers can view
  and dismiss cards but can never push one, so a card on the phone always
  originates from the owner's host.
- **The card** carries a one-line summary and the approval-plane link as its
  `--url`. It never carries the request body, a credential, or an approve
  action. Where the text comes from matters, because a tap opens the link:
  the notifier builds `--url` only from its own approval-plane base URL
  plus the request id, never from fields the caller supplied, and
  `--title` is caller-influenced summary text — fine for the owner to
  read, never an authorization signal.
- **The approval plane** is a separate surface (PWA or native app) holding the
  owner's signing key. Tapping the card opens it; nothing about the signature
  or the decision passes through nearpush.

## The contract

Each parked request gets a card, sender `auth-proxy` (which has a built-in
icon, a shield mark). The notifier keeps the event id that `nearpush send`
prints — the number on its `sent event #<id>` line:

```sh
id=$(nearpush send --sender auth-proxy --app auth-proxy --state allow \
  --title "<one-line summary>" \
  --url "<approval-plane link for this request>" \
  | sed -n 's/^sent event #//p')
```

and follows the request to its outcome:

| auth-proxy event | nearpush action |
|---|---|
| request parked | the `nearpush send` above |
| approved and executed | dismiss the `allow.` card, then `nearpush send --sender auth-proxy --app auth-proxy --state all_set --title "<summary> — done"` |
| denied | dismiss the `allow.` card (no replacement card; a denial needs no attention) |
| expired with no decision | dismiss the `allow.` card, so a dead request never lingers as if it were actionable |

Two limits of the display shape what "one card per request" can mean:

- **Only the newest parked request is on the card.** The main card shows
  one event at a time, and dismissal is a high-water mark, not a per-event
  flag: dismissing a newer event also hides every older one. When several
  requests are parked at once, the approval plane's own queue is the
  source of truth for what is still pending — the card points at the
  newest request, it is not the queue. (Per-event hiding would be a
  separate change.)
- **A pending session hides the card.** While any session is waiting on
  the owner (`your_turn`), or one is shown as `working.`, the main card
  shows that session instead of the event stream, so an `allow.` card may
  not be visible until the session resolves. The request is unaffected:
  it stays parked in auth-proxy and still expires on its own schedule.

Dismissal from the notifier uses the loopback path with the push token.
A `404` here means the card is already gone — a viewer dismissed it, or a
newer event's dismissal hid it — which is the state the notifier wanted
anyway, so it treats `404` as success:

```sh
code=$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8787/v1/dismiss \
  -H "Authorization: Bearer $(cat ~/.config/nearpush/token)" \
  -H 'Content-Type: application/json' \
  -d "{\"event_id\": $id}")
# 200: dismissed now. 404: already gone. Both are success.
[ "$code" = 200 ] || [ "$code" = 404 ]
```

## One approval, end to end

```mermaid
sequenceDiagram
  participant C as Caller (Muse)
  participant P as auth-proxy
  participant N as notifier
  participant D as nearpushd
  participant I as iPhone
  participant A as approval plane
  C->>P: write request (surrogate)
  P->>P: grant says require_approval: park
  P-->>C: 202 + request id
  P->>N: parked (summary + approval link)
  N->>D: allow. card
  D-->>I: big card
  I->>A: tap card, open approval link
  A->>A: owner reviews full request, signs
  A->>P: approve (bound to request hash)
  P->>P: execute exactly once, audit
  P-->>C: outcome: executed
  P->>N: outcome
  N->>D: dismiss allow. card, push all set.
```

## Non-goals

- No approve or deny control anywhere in nearpush — not on the card, not in
  the drawer, not in any viewer-reachable endpoint. A compromised viewer must
  gain nothing.
- No request payloads on cards beyond a one-line summary. The full request
  lives on the approval plane only.
- No background notification promise: as the README states, nearpush is a
  display, not a notifier. A parked request that nobody sees simply expires
  in auth-proxy, which is the safe outcome.
