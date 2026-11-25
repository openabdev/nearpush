# Spike report: Apple Watch support through Web Push (Option A)

Date: 2026-11-25. Base: `eventrelay` @ 89c8b673. Spike code: worktree
`.worktrees/apple-watch-webpush`, branch `apple-watch-webpush-spike`, commits
`70c1489` and `e3f92bb` (local only, not pushed). The spike code is throwaway
(`tools/webpush-spike/`, Python + pywebpush 2.5.0) and is not part of nearpushd.

## Setup

- Spike server `tools/webpush-spike/server.py` on `127.0.0.1:8797`. It served a
  minimal home-screen web app (manifest, `sw.js`, the nearpush icons), stored
  subscriptions, and sent pushes with VAPID (P-256, generated locally).
- Fronted by `tailscale serve --bg --https=8450 http://127.0.0.1:8797`, tailnet
  only. Funnel was never enabled. The live mapping (`/` → 127.0.0.1:8787) and
  the unrelated `:8444` mapping were not touched.
- Secrets: VAPID private key and subscriptions in
  `~/.local/share/nearpush-webpush-spike/state/` (dir 0700, files 0600). They were
  never printed. The log records the endpoint host only.
- Device: pahud's carried iPhone, iOS 18.7 (UA `iPhone OS 18_7 … Version/27.2
  Mobile/15E148 Safari/604.1`), paired Apple Watch.

## Q1. Does a Web Push to an iOS home-screen web app mirror to a paired Apple Watch when the iPhone is locked?

**PASS (observed).**

- pahud locked the iPhone and put on the Watch before the sends.
- The laptop sent `your_turn` at 15:44:34. The Watch vibrated and chimed.
- Photo IMG_6219 (copy at `/tmp/np-w-IMG6219.JPG`) shows the Watch notification:
  - the app's red asterisk icon and "now"
  - the title "nearpush: your turn"
  - "from np-spike"
  - the body "spike: agent is waiting for you (15:44:34)"
  - the dismiss (X) and mute buttons
- So the web app's own icon and name appear on the Watch, and the payload title and
  body come through unchanged.

Not confirmed (pahud did not report these, and the result does not depend on them):

- whether `allow` (15:44:54) and `all_set` (15:45:15) also showed on the Watch.
  Apple accepted both; see Q2.
- whether the locked iPhone also lit its lock screen
- the watchOS version
- whether the Watch app → Notifications entry for np-spike reads "Mirror my iPhone".
  It worked with whatever the default is.

## Q2. Can iOS subscribe to Web Push for a tailnet `https://<laptop>.<tailnet>.ts.net` origin through tailscale serve, and does delivery work from the laptop?

**PASS (observed).**

Subscription, from the page's on-screen log and the server log at 15:43:40–15:43:44:

- The page was opened from the Home Screen at origin `https://<laptop>.<tailnet>.ts.net:8450`.
- The page checks reported:
  - `navigator.standalone=true`
  - display-mode standalone `true`
  - `'serviceWorker' in navigator=true`
  - `'PushManager' in window=true`
  - `typeof Notification=function`
- The service worker registered.
- Tapping the button and then Allow moved permission from `default` to `granted`.
- `pushManager.subscribe` returned an endpoint on `web.push.apple.com`, and the
  server stored it with status 200.
- tailscale serve passed `Tailscale-User-Login: <your-email>` on every
  request. The existing header requirement therefore still works for the push page.

Delivery, sent by the laptop directly to `web.push.apple.com` with VAPID, TTL 300
and `Urgency: high`:

| State | Sent | Apple status | Round trip |
|---|---|---|---|
| your_turn | 15:44:34 | 201 Created | 249 ms |
| allow | 15:44:54 | 201 Created | 209 ms |
| all_set | 15:45:15 | 201 Created | 234 ms |

End-to-end delivery is confirmed for `your_turn`: it arrived on the Watch within
the "now" window, per Q1. For `allow` and `all_set`, Apple's push service accepted
the push, but nobody reported seeing it on a device.

Notes for the design phase (observations, not decisions):

- **A ts.net origin works.** Apple's push service needs no public reachability of
  the origin. Only outbound HTTPS from the daemon to `web.push.apple.com` is needed.
- **The PWA origin is per port.** Port 8450 is a different origin from the live `/`
  on 443. A subscription belongs to the origin that created it. The real feature
  should therefore subscribe from the production origin
  (`https://<laptop>.<tailnet>.ts.net/`) so that the user keeps one Home Screen app.
- **Wrong-URL trap.** The first attempt failed because the Home Screen app had been
  added from the wrong URL; the server logged zero iPhone requests. Opening the
  page in Safari is not enough. Notification support appears only when the page is
  launched from the Home Screen icon.
- **One unexplained JS error.** The page logged one detail-less `Script error. @0:0`
  at 15:43:28, before the reload that subscribed. It did not block anything, and
  its cause was not found.

## Q3. Should the desk iPhone SE also receive pushes, or only the carried iPhone?

**Open decision for pahud. The spike does not choose.**

| Option | What happens | Trade-off |
|---|---|---|
| A. Carried iPhone only | Only subscriptions created on the carried phone get pushes. The SE keeps showing the live page. | No duplicate chimes at the desk. The desk display already shows the state, so a push there adds little. Needs a way to tell devices apart, for example a per-subscription label or an opt-in toggle on the page. |
| B. Every subscribed device | Any device that tapped Enable gets pushes, the SE included. | Simplest. When the SE's screen is on with the page open, the alert shows on the SE, which may be noisy beside the live display. |
| C. Per-device opt-in with a default of off | The page offers Enable per device, and each device decides. | Most flexible, but adds a little UI and state. It is equivalent to A if pahud just never enables the SE. |

Unknowns pahud may care about: whether the SE gets an alert while its own
foreground page is visible, and whether that matters on an always-on desk display.
Neither was tested.

## Cleanup done

- Removed the tailnet serve port: ran `tailscale serve --https=8450 off`.
  `tailscale serve status` now shows only `/` → 127.0.0.1:8787 and `:8444` →
  127.0.0.1:8795, as before.
- Stopped the spike server on 127.0.0.1:8797 (no process left). The live page at
  `https://<laptop>.<tailnet>.ts.net/` still returns 200.
- Left in place:
  - the VAPID key and subscription, in `~/.local/share/nearpush-webpush-spike/state/`
    (0600)
  - the venv
  - the np-spike Home Screen icon on the iPhone
- pahud can delete the iPhone icon. Remove the state directory with
  `rm -rf ~/.local/share/nearpush-webpush-spike` once the design phase no longer
  needs it. The stored subscription only works on the 8450 origin.

## Result

Q1 PASS, Q2 PASS (both observed). Q3 is left open with options above.
