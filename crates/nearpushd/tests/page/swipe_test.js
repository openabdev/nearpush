// Headless check of the page's swipe-to-dismiss, run by scripts/test-page.sh with
// JavaScriptCore (`jsc`). This file is appended after a fake DOM (below, PRELUDE) and the
// page's own <script>, so it drives the real page code through WebSocket frames and
// pointer events and checks the requests it sends and what the card shows.

/* PRELUDE */
var CLOCK = 0, NOW = 0, timers = [], timerId = 0, rafQ = [], fetches = [], FETCH_STATUS = 200, socket = null;
function El(id) {
  this.id = id; this.children = []; this.style = {}; this.hidden = false; this._text = ''; this.listeners = {};
  this.attrs = {}; this.className = ''; this.clientWidth = 400; this.clientHeight = 800; this.scrollWidth = 0;
  this.scrollHeight = 0; this.offsetWidth = 0; this.scrollTop = 0; this.innerHTML = '';
  var set = {};
  this.classList = {
    add: function (c) { set[c] = true; }, remove: function (c) { delete set[c]; },
    contains: function (c) { return !!set[c]; },
    toggle: function (c, on) { if (on === undefined) on = !set[c]; if (on) set[c] = true; else delete set[c]; return on; },
  };
}
Object.defineProperty(El.prototype, 'textContent', {
  get: function () { return this._text + this.children.map(function (c) { return c.textContent; }).join(''); },
  set: function (v) { this._text = String(v); this.children = []; },
});
El.prototype.appendChild = function (c) { this.children.push(c); return c; };
El.prototype.append = function () { for (var i = 0; i < arguments.length; i++) this.children.push(arguments[i]); };
El.prototype.addEventListener = function (t, f) { (this.listeners[t] = this.listeners[t] || []).push(f); };
El.prototype.fire = function (t, ev) { (this.listeners[t] || []).forEach(function (f) { f(ev); }); };
El.prototype.getAttribute = function (k) { return this.attrs[k] === undefined ? null : this.attrs[k]; };
El.prototype.setAttribute = function (k, v) { this.attrs[k] = v; };
El.prototype.removeAttribute = function (k) { delete this.attrs[k]; };
El.prototype.closest = function () { return null; };
var els = {};
var document = {
  visibilityState: 'visible', body: new El('body'), listeners: {},
  getElementById: function (id) { return els[id] || (els[id] = new El(id)); },
  createElement: function (t) { return new El(t); },
  addEventListener: function (t, f) { (this.listeners[t] = this.listeners[t] || []).push(f); },
};
var window = {
  innerWidth: 400, innerHeight: 800, PointerEvent: function () {},
  matchMedia: function () { return { matches: false }; },
  addEventListener: function () {}, open: function () {},
};
var navigator = {};
// Web Audio stub: counts contexts and oscillators started (a chime is two oscillators).
var audioLog = { contexts: 0, oscillators: 0, resumes: 0 };
function FakeParam() {}
FakeParam.prototype.setValueAtTime = function () {};
FakeParam.prototype.exponentialRampToValueAtTime = function () {};
function FakeNode() { this.frequency = new FakeParam(); this.gain = new FakeParam(); }
FakeNode.prototype.connect = function () {};
FakeNode.prototype.start = function () {};
FakeNode.prototype.stop = function () {};
function FakeAudioContext() { audioLog.contexts++; this.state = 'suspended'; this.currentTime = 0; this.destination = {}; }
FakeAudioContext.prototype.resume = function () {
  audioLog.resumes++; this.state = 'running'; if (this.onstatechange) this.onstatechange(); return Promise.resolve();
};
FakeAudioContext.prototype.createBuffer = function () { return {}; };
FakeAudioContext.prototype.createBufferSource = function () { return new FakeNode(); };
FakeAudioContext.prototype.createGain = function () { return new FakeNode(); };
FakeAudioContext.prototype.createOscillator = function () { audioLog.oscillators++; return new FakeNode(); };
window.AudioContext = FakeAudioContext;
var location = { protocol: 'http:', host: 'laptop.example.ts.net', reload: function () { throw new Error('unexpected reload'); } };
var store = {};
var localStorage = { getItem: function (k) { return store[k] === undefined ? null : store[k]; },
                     setItem: function (k, v) { store[k] = String(v); }, removeItem: function (k) { delete store[k]; } };
var sessionStorage = { getItem: function () { return null; }, setItem: function () {}, removeItem: function () {} };
var performance = { now: function () { return NOW; } };
function setTimeout(cb, ms) { timers.push({ id: ++timerId, at: CLOCK + (ms || 0), cb: cb }); return timerId; }
function clearTimeout(id) { timers = timers.filter(function (t) { return t.id !== id; }); }
function setInterval() { return 0; }
function requestAnimationFrame(cb) { rafQ.push(cb); return rafQ.length; }
function cancelAnimationFrame() {}
function WebSocket(url) { this.url = url; this.readyState = 1; socket = this; }
WebSocket.OPEN = 1; WebSocket.CONNECTING = 0;
WebSocket.prototype.close = function () {};
function fetch(path, init) {
  fetches.push({ path: path, body: JSON.parse(init.body), headers: init.headers, method: init.method });
  return Promise.resolve({ status: FETCH_STATUS });
}
/* END PRELUDE */

/* PAGE */

/* TEST */
(function () {
  'use strict';
  function check(cond, msg) { if (!cond) throw new Error('FAIL: ' + msg); }
  function flushRaf() { for (var n = 0; n < 10 && rafQ.length; n++) { var q = rafQ; rafQ = []; q.forEach(function (f) { f(); }); } }
  function advance(ms) {
    var end = CLOCK + ms;
    for (;;) {
      timers.sort(function (a, b) { return a.at - b.at; });
      var t = timers[0];
      if (!t || t.at > end) break;
      timers.shift(); CLOCK = t.at; t.cb(); flushRaf();
    }
    CLOCK = end; flushRaf();
  }
  function frame(f) { socket.onmessage({ data: JSON.stringify(f) }); flushRaf(); }
  function headline() { return document.getElementById('headline').textContent; }
  var card = document.getElementById('card');
  function swipe(x0, dx, dy, ms) {
    var target = { closest: function () { return null; } };
    NOW += 1000;
    card.fire('pointerdown', { pointerId: 1, pointerType: 'touch', button: 0, clientX: x0, clientY: 300, target: target });
    var steps = 5;
    for (var i = 1; i <= steps; i++) {
      NOW += ms / steps;
      card.fire('pointermove', { pointerId: 1, clientX: x0 + (dx * i) / steps, clientY: 300 + (dy * i) / steps,
                                 cancelable: true, preventDefault: function () {} });
    }
    flushRaf();
    card.fire('pointerup', { pointerId: 1 });
    advance(400);
  }
  async function settle() { for (var i = 0; i < 5; i++) await null; flushRaf(); }

  async function run() {
    var VERSION = '__NEARPUSH_VERSION__';
    frame({ type: 'hello', version: VERSION, last_id: 0, oldest_id: 0, missed: 0, server_time: Date.now(), labels: [] });
    frame({ type: 'event', event: { id: 1, ts: Date.now(), title: 'your turn.', state: 'your_turn', level: 'info', session: 'TEST-S' } });
    frame({ type: 'main_card', card: { state: 'your_turn', session: 'TEST-S', ts: 5, count: 1, question_line: 'Pick?' } });
    frame({ type: 'synced', last_id: 1 });
    check(/your\s*turn/.test(headline()), 'your_turn shown, got ' + headline());

    // Below the threshold, slowly: springs back, no request.
    swipe(100, 60, 0, 600);
    check(fetches.length === 0, 'short drag sends nothing');
    check(card.style.transform === '', 'card springs back');
    // From the left screen edge (Safari back gesture): ignored.
    swipe(10, 300, 0, 300);
    check(fetches.length === 0, 'edge swipe ignored');
    // Vertical: the drawer's gesture.
    swipe(100, 5, -200, 300);
    check(fetches.length === 0, 'vertical drag ignored');

    // Past 35% of the width: dismissed.
    swipe(100, 160, 10, 400);
    check(fetches.length === 1, 'one dismiss request');
    check(fetches[0].path === '/v1/dismiss' && fetches[0].body.session === 'TEST-S', 'dismisses the session');
    check(fetches[0].headers['Content-Type'] === 'application/json', 'JSON body');
    await settle();
    check(!document.getElementById('toast').hidden && !document.getElementById('undo').hidden, 'toast with Undo');
    check(/all\s*set/.test(headline()), 'no pending left: all set, got ' + headline());

    // Undo within 5 s: the card comes back.
    document.getElementById('undo').fire('click', { stopPropagation: function () {} });
    await settle();
    check(fetches.length === 2 && fetches[1].path === '/v1/undismiss' && fetches[1].body.session === 'TEST-S', 'undo request');
    check(/your\s*turn/.test(headline()), 'card restored, got ' + headline());
    check(document.getElementById('toast').hidden, 'toast closed');

    // A fast flick below the threshold also dismisses.
    swipe(100, -70, 0, 60);
    check(fetches.length === 3 && fetches[2].body.session === 'TEST-S', 'flick dismisses');
    await settle();
    // The toast goes after 5 s and the dismissal is final.
    advance(5100);
    check(document.getElementById('toast').hidden, 'toast gone after 5 s');

    // A stored all_set card: hidden by event id, then the idle screen.
    frame({ type: 'main_card', card: null });
    frame({ type: 'event', event: { id: 2, ts: Date.now(), title: 'all set.', state: 'all_set', level: 'info', session: 'TEST-T' } });
    check(/all\s*set/.test(headline()), 'all_set shown');
    swipe(100, 200, 0, 300);
    check(fetches.length === 4 && fetches[3].body.event_id === 2 && fetches[3].body.session === undefined, 'dismisses by event id');
    await settle();
    check(/nothing\s*pending/.test(headline()), 'idle screen, got ' + headline());
    // Another viewer's dismissal arrives as a hidden frame; a dismissal record stays in history only.
    frame({ type: 'hidden', through: 2 });
    frame({ type: 'event', event: { id: 3, ts: Date.now(), title: 'dismissed', dismissed: true, level: 'info', session: 'TEST-T' } });
    check(/nothing\s*pending/.test(headline()), 'a dismissal record is not a card');
    swipe(100, 200, 0, 300);
    check(fetches.length === 4, 'nothing to dismiss on the idle screen');
    frame({ type: 'event', event: { id: 4, ts: Date.now(), title: 'Build done', level: 'info' } });
    check(/Build done/.test(headline()), 'the next event shows again');

    // A failed dismiss puts the card back.
    FETCH_STATUS = 403;
    swipe(100, 200, 0, 300);
    await settle();
    check(/Build done/.test(headline()), 'refused dismiss restores the card, got ' + headline());
    check(document.getElementById('toastmsg').textContent === "Couldn't dismiss", 'failure toast');
    print('PASS page swipe-to-dismiss');

    // ---- sound cue ----
    var live = 4;
    function ev(id, state, extra) {
      var e = { id: id, ts: Date.now(), title: state.replace('_', ' ') + '.', state: state, level: 'info', session: 'TEST-C' };
      for (var k in extra || {}) e[k] = extra[k];
      frame({ type: 'event', event: e });
    }
    var hint = document.getElementById('soundhint'), sound = document.getElementById('sound');
    check(audioLog.contexts === 0 && audioLog.oscillators === 0, 'no audio before a gesture, even for event 1 (replayed your_turn)');
    check(!hint.hidden, 'unlock hint shown until a tap');
    check(sound.getAttribute('aria-pressed') === 'true', 'sound on by default');
    ev(++live, 'your_turn');
    check(audioLog.oscillators === 0, 'locked audio stays silent');
    navigator.audioSession = { type: 'auto' };
    document.listeners.click.forEach(function (f) { f({}); });
    await settle();
    check(audioLog.contexts === 1 && navigator.audioSession.type === 'playback', 'first tap creates the context, playback session');
    check(hint.hidden, 'hint hidden once unlocked');

    ev(++live, 'your_turn');
    check(audioLog.oscillators === 2, 'live your_turn plays one two-note chime, got ' + audioLog.oscillators);
    ev(live, 'your_turn');
    check(audioLog.oscillators === 2, 'same id twice plays once');
    ev(++live, 'all_set');
    check(audioLog.oscillators === 2, 'all_set does not play');
    ev(++live, 'working');
    frame({ type: 'session_update', session: 'TEST-C', label: 'relabelled' });
    frame({ type: 'main_card', card: { state: 'your_turn', session: 'TEST-C', ts: 9, count: 1 } });
    frame({ type: 'main_card', card: null });
    check(audioLog.oscillators === 2, 'working, session_update and main_card frames do not play');

    // Reconnect: events between hello and synced are a replay.
    frame({ type: 'hello', version: VERSION, last_id: live + 1, oldest_id: 0, missed: 0, server_time: Date.now(), labels: [] });
    ev(++live, 'your_turn');
    frame({ type: 'synced', last_id: live });
    check(audioLog.oscillators === 2, 'replayed your_turn does not play');
    ev(++live, 'your_turn');
    check(audioLog.oscillators === 4, 'live again after synced');

    document.visibilityState = 'hidden';
    ev(++live, 'your_turn');
    document.visibilityState = 'visible';
    check(audioLog.oscillators === 4, 'hidden page does not play');

    sound.fire('click', { stopPropagation: function () {} });
    check(store['nearpush.sound'] === '0' && sound.classList.contains('off') && hint.hidden, 'toggle off saved');
    ev(++live, 'your_turn');
    check(audioLog.oscillators === 4, 'toggle off does not play');
    sound.fire('click', { stopPropagation: function () {} });
    check(store['nearpush.sound'] === '1' && !sound.classList.contains('off'), 'toggle on saved');
    ev(++live, 'your_turn');
    check(audioLog.oscillators === 6, 'toggle on plays again');
    print('PASS page sound cue');
  }
  run().catch(function (e) { print(String(e && e.stack ? e + '\n' + e.stack : e)); });
})();
