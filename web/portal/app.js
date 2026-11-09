// Piso WiFi customer portal. No innerHTML anywhere: all text goes through
// textContent, so nothing from the server can inject markup.
"use strict";

// Older phones (iOS 12/13, old Android WebViews) lack replaceChildren.
if (!Element.prototype.replaceChildren) {
  Element.prototype.replaceChildren = function (...nodes) {
    while (this.firstChild) this.removeChild(this.firstChild);
    this.append(...nodes);
  };
}

const $ = (id) => document.getElementById(id);
const el = (tag, text, cls) => {
  const e = document.createElement(tag);
  if (text != null) e.textContent = text;
  if (cls) e.className = cls;
  return e;
};
const RING = 339.3;
const REDUCE = matchMedia("(prefers-reduced-motion: reduce)").matches;
const SPRING = CSS.supports("transition-timing-function", "linear(0, 1)")
  ? "linear(0, .008 1.1%, .031 2.2%, .129 4.8%, .257 7.2%, .671 14.2%, .789 16.8%, .881 19.4%, .954 22.2%, 1.006 25.2%, 1.04 28.4%, 1.058 31.9%, 1.062 35.6%, 1.055 39.8%, 1.035 45.4%, 1.012 52.4%, .998 61%, .993 71.7%, 1)"
  : "cubic-bezier(.2, .9, .25, 1.15)";

let state = null;      // last /api/me response
let slotBack = false;  // Cancel's fold-away has played; the button springs back next render
let cancelHold = false; // ...and while it plays, polling must not swap the card out early
let fetchedAt = 0;     // performance.now() when state arrived
let pollTimer = null;
let seenCoins = 0;     // coins already animated this insert session
let shownPesos = 0;    // what the counter currently displays
let firstRender = true;

function animate(node, frames, opts) {
  if (REDUCE || !node.animate) return null;
  return node.animate(frames, Object.assign({ easing: SPRING, fill: "both" }, opts));
}

async function api(path, body) {
  const opts = { method: body === undefined ? "GET" : "POST", headers: { "x-pisowifi": "1" }, cache: "no-store" };
  if (body !== undefined) {
    opts.headers["content-type"] = "application/json";
    opts.body = JSON.stringify(body);
  }
  const res = await fetch("/api" + path, opts);
  let data = {};
  try { data = await res.json(); } catch (_) { /* empty body */ }
  if (!res.ok) {
    const err = new Error(data.error || "Couldn't reach the WiFi box. Check that you're still connected.");
    err.status = res.status;
    err.code = data.code;
    throw err;
  }
  return data;
}

const pad = (n) => String(n).padStart(2, "0");
const clockText = (s) => pad(Math.floor(s / 3600)) + ":" + pad(Math.floor((s % 3600) / 60)) + ":" + pad(s % 60);

function human(secs) {
  const d = Math.floor(secs / 86400), h = Math.floor((secs % 86400) / 3600), m = Math.round((secs % 3600) / 60);
  const parts = [];
  if (d) parts.push(d + (d === 1 ? " day" : " days"));
  if (h) parts.push(h + " hr");
  if (m || !parts.length) parts.push(m + " min");
  return parts.join(" ");
}

let toastTimer;
function toast(msg, bad) {
  const t = $("toast");
  t.hidden = true;
  void t.offsetWidth;
  // Icon: a check for good news, "!" for problems, a clock for running low.
  const ic = el("span", null, "toast-ic");
  const s = svgEl("svg", { viewBox: "0 0 20 20", "aria-hidden": "true" });
  if (bad === "time") s.append(svgEl("circle", { cx: 10, cy: 10, r: 6 }), svgEl("path", { d: "M10 7v3.2l2 1.3" }));
  else if (bad) s.append(svgEl("path", { d: "M10 5.5v5.5" }), svgEl("path", { d: "M10 14.5v.01" }));
  else s.append(svgEl("path", { d: "M5.5 10.5l3 3 6-7" }));
  ic.append(s);
  t.replaceChildren(ic, el("span", msg));
  t.classList.toggle("bad", bad === true);
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    const out = animate(t, [{ opacity: 1, transform: "translate(-50%, 0)" }, { opacity: 0, transform: "translate(-50%, 16px) scale(.96)" }], { duration: 260, easing: "ease-in" });
    if (out) out.onfinish = () => { t.hidden = true; out.cancel(); };
    else t.hidden = true;
  }, 3600);
}

const elapsed = () => Math.floor((performance.now() - fetchedAt) / 1000);

// ---- odometer clock ---------------------------------------------------------

let clockShown = "";
let clockSecs = 0;

function setClock(secs) {
  const text = clockText(secs);
  const c = $("clock");
  c.setAttribute("aria-label", human(secs) + " left");
  if (text === clockShown) return;
  // Counting down rolls digits downward; adding time rolls them up.
  const dir = secs < clockSecs ? 1 : -1;
  if (text.length !== clockShown.length) {
    c.replaceChildren(...[...text].map((ch) => {
      if (ch === ":") return el("span", ":", "colon");
      const slot = el("span", null, "dg");
      slot.append(el("span", ch));
      return slot;
    }));
  } else {
    [...text].forEach((ch, i) => {
      if (ch === clockShown[i] || ch === ":") return;
      const slot = c.children[i];
      const old = slot.lastElementChild;
      const next = el("span", ch);
      slot.append(next);
      const big = Math.abs(clockSecs - secs) > 2;
      const dur = big ? 700 + i * 40 : 520;
      const outA = animate(old, [{ transform: "translateY(0)", opacity: 1 }, { transform: `translateY(${dir * 70}%)`, opacity: 0 }], { duration: dur * 0.7, easing: "ease-in" });
      animate(next, [{ transform: `translateY(${-dir * 90}%)`, opacity: 0 }, { transform: "translateY(0)", opacity: 1 }], { duration: dur });
      if (outA) outA.onfinish = () => old.remove();
      else old.remove();
      // Never let a fast run of updates pile up spans.
      while (slot.children.length > 2) slot.firstElementChild.remove();
    });
  }
  clockShown = text;
  clockSecs = secs;
}

// ---- rendering --------------------------------------------------------------

// "Until about 5:25 AM" over "Tue, Oct 21", by the phone's own clock and language.
function setUntil(sub, left) {
  const t = new Date(Date.now() + left * 1000);
  const time = "Until about " + t.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  const day = t.toLocaleDateString([], { weekday: "short", month: "short", day: "numeric" });
  if (sub.textContent !== time + day) sub.replaceChildren(time, el("span", day, "until-day"));
}

function renderClock() {
  if (!state) return;
  const left = state.running ? Math.max(0, state.remaining_secs - elapsed()) : state.remaining_secs;
  const online = state.running && left > 0, paused = !state.running && left > 0;
  setClock(left);
  const m = $("meter");
  m.classList.toggle("online", online);
  m.classList.toggle("paused", paused);
  m.classList.toggle("empty", left === 0);
  $("status-text").textContent = online ? "Online" : paused ? "Paused" : "Offline";
  $("meter-label").textContent = online ? "You're online" : paused ? "Time saved" : "No time yet";
  if (online) setUntil($("meter-sub"), left);
  else $("meter-sub").textContent = paused ? "Paused. Your time isn't running." : state.blocked ? "Talk to the owner to get online" : "Drop a coin to get online";
  $("pause").hidden = !online;
  $("resume").hidden = !paused;
  renderDrain(left, online);

  const slot = state.slot;
  if (slot.state !== "idle") {
    const sl = Math.max(0, slot.secs_left - elapsed());
    $("secs-left").textContent = sl;
    $("busy-secs").textContent = sl;
    $("ring").style.strokeDashoffset = String(RING * (1 - sl / slot.window_secs));
  }
}

// ---- time bar and running-low nudge -------------------------------------------

// The bar drains against the most time this phone has had since it last ran out.
const PEAK_KEY = "pisowifi-peak";
let peak = 0;
try { peak = Number(localStorage.getItem(PEAK_KEY)) || 0; } catch (_) { /* storage off */ }
let lowWarned = 0;   // minutes left we've already warned about

function savePeak(v) {
  peak = v;
  try {
    if (v) localStorage.setItem(PEAK_KEY, String(v));
    else localStorage.removeItem(PEAK_KEY);
  } catch (_) { /* storage off */ }
}

function renderDrain(left, online) {
  const bar = $("drain");
  bar.hidden = left === 0;
  const low = online && left <= 300;
  $("meter").classList.toggle("low", low);
  $("slot").classList.toggle("nudge", low);
  if (left === 0) {
    if (peak) savePeak(0);
    lowWarned = 0;
    return;
  }
  if (left > peak) savePeak(left);
  bar.classList.toggle("still", !online);
  $("drain-fill").style.transform = "scaleX(" + Math.max(0.015, left / peak).toFixed(4) + ")";
  if (!low) {
    if (left > 330) lowWarned = 0;
    return;
  }
  const mins = Math.ceil(left / 60);
  $("meter-sub").textContent = left <= 60 ? "Less than a minute left. Add time?" : "Only " + mins + " min left. Add time?";
  const mark = left <= 60 ? 1 : 5;
  if (!lowWarned || mark < lowWarned) {
    lowWarned = mark;
    toast(left <= 60 ? "Less than a minute of internet left" : "Only " + mins + " min of internet left", "time");
    if (navigator.vibrate) navigator.vibrate([30, 60, 30]);
  }
}

// ---- first load: sections rise in one after another ---------------------------

function intro() {
  if (REDUCE) return;
  const wrap = $("app");
  [...wrap.children].filter((n) => !n.classList.contains("toast")).forEach((n, i) => n.style.setProperty("--i", Math.min(i, 9)));
  wrap.classList.add("intro");
  setTimeout(() => wrap.classList.remove("intro"), 1600);
}

let lastSlotState = "idle";

function render() {
  if (cancelHold) return;
  const slot = state.slot;
  if (firstRender) intro();
  const changed = slot.state !== lastSlotState && !firstRender;
  const apply = () => {
    applyBrand(state.brand);
    // "Galaxy A53 5G · …A1B2C3": the model when the phone told us, and the
    // short code the owner sees.
    $("device").textContent = state.device_name ? state.device_name + " · " + state.device.replace(/^Device /, "") : state.device;
    $("app").setAttribute("aria-busy", "false");
    // Blocked by the owner: no slot, voucher or pass buttons, just a plain note.
    $("app").classList.toggle("blocked", !!state.blocked);
    $("blocked-card").hidden = !state.blocked;
    $("slot").hidden = slot.state !== "idle";
    $("inserting").hidden = slot.state !== "mine";
    $("busy").hidden = slot.state !== "busy";
    $("slot-title").textContent = state.remaining_secs > 0 ? "Add more time" : "Insert coin";
    $("slot-sub").textContent = state.remaining_secs > 0 ? "Top up before your time runs out" : "Tap here first, then drop your coins";
    $("ob-away").textContent = state.auto_pause_mins
      ? " Leave the WiFi and your time pauses by itself after " + state.auto_pause_mins + " minutes, then starts again when you're back."
      : "";
    renderRates();
    renderClock();
  };
  // After Cancel the card has already folded away: the button springs back
  // on its own instead of a crossfade.
  const back = slotBack && changed && slot.state === "idle";
  // Opening unfolds the card out of the button (Cancel plays it backwards).
  const opening = changed && slot.state === "mine" && lastSlotState === "idle" && !REDUCE;
  if (changed && !REDUCE && document.startViewTransition && !back && !opening) document.startViewTransition(apply);
  else apply();
  if (opening) {
    const c = $("inserting");
    c.classList.remove("opening", "closing");
    void c.offsetWidth;
    c.classList.add("opening");
    clearTimeout(c._open);
    c._open = setTimeout(() => c.classList.remove("opening"), 1000);
  }
  if (back) {
    slotBack = false;
    $("inserting").classList.remove("closing");
    $("slot-pill").textContent = "Slot open";
    const b = $("slot");
    b.classList.remove("back");
    void b.offsetWidth;
    b.classList.add("back");
    setTimeout(() => b.classList.remove("back"), 1400);
  }
  if (changed && slot.state === "mine") Sound.slot();

  if (slot.state === "mine") {
    if (firstRender || lastSlotState !== "mine") {
      // Coins already in (e.g. page refreshed mid-insert): show them, don't replay.
      const already = firstRender ? slot.coins : [];
      $("tray").replaceChildren(...already.flatMap((c) => PisoCoins.split(c)).map((d) => trayCoin(d, false)));
      seenCoins = already.length;
      shownPesos = firstRender ? slot.pesos : 0;
      $("pesos").textContent = shownPesos;
    }
    const fresh = slot.coins.slice(seenCoins);
    seenCoins = slot.coins.length;
    fresh.flatMap((c) => PisoCoins.split(c)).forEach((d, i) => setTimeout(() => dropCoin(d), i * 420));
    const credit = $("credit");
    if (slot.pesos > 0) credit.replaceChildren("That's ", el("b", human(slot.credit_secs)), " of internet");
    else credit.textContent = "Waiting for your first coin…";
    $("done").disabled = slot.pesos === 0;
    $("done").textContent = slot.pesos > 0 ? "Start internet" : "Drop a coin to continue";
    // Backing out is only offered before any coin: paid coins always become time.
    $("cancel-insert").hidden = slot.pesos > 0;
  } else {
    seenCoins = 0;
  }
  lastSlotState = slot.state;
  firstRender = false;
}

// ---- sound effects ------------------------------------------------------------

// Made on the spot with Web Audio: no files to load before the phone has
// internet. Phones only allow sound after a tap, so any tap wakes it; coins
// that land later still play because an earlier tap did.
const SOUND_KEY = "pisowifi-sound";
const Sound = (() => {
  const AC = window.AudioContext || window.webkitAudioContext;
  let ctx = null, out = null;
  let quiet = false;
  try { quiet = localStorage.getItem(SOUND_KEY) === "off"; } catch (_) { /* private mode */ }

  const allowed = () => !!AC && !!state && state.brand.sounds;
  const ready = () => allowed() && !quiet && ctx && ctx.state === "running";

  function wake() {
    if (!allowed() || quiet) return;
    if (!ctx) {
      ctx = new AC();
      // Loud enough for a phone speaker in a busy shop; the limiter keeps
      // coins landing close together from crackling.
      out = ctx.createGain();
      out.gain.value = 1.6;
      const limit = ctx.createDynamicsCompressor();
      limit.threshold.value = -6;
      limit.knee.value = 6;
      limit.ratio.value = 8;
      limit.attack.value = 0.002;
      limit.release.value = 0.2;
      out.connect(limit);
      limit.connect(ctx.destination);
      // Older iPhones only unlock sound once something plays during the tap.
      const blank = ctx.createBufferSource();
      blank.buffer = ctx.createBuffer(1, 1, 22050);
      blank.connect(ctx.destination);
      blank.start(0);
    }
    if (ctx.state !== "running" && ctx.resume) ctx.resume();
  }

  // One voice: a quick strike, then a ringing fade. `glide` bends the pitch.
  function tone(at, freq, dur, vol, type, glide) {
    const t = ctx.currentTime + at;
    const o = ctx.createOscillator(), g = ctx.createGain();
    o.type = type || "sine";
    o.frequency.setValueAtTime(freq, t);
    if (glide) o.frequency.exponentialRampToValueAtTime(glide, t + dur * 0.7);
    g.gain.setValueAtTime(0.0001, t);
    g.gain.exponentialRampToValueAtTime(vol, t + 0.006);
    g.gain.exponentialRampToValueAtTime(0.0001, t + dur);
    o.connect(g);
    g.connect(out);
    o.start(t);
    o.stop(t + dur + 0.02);
  }

  // The slot opening: a soft clunk, then two bubbly blips up.
  function slot() {
    if (!ready()) return;
    tone(0, 260, 0.13, 0.3, "triangle", 110);
    tone(0.07, 520, 0.13, 0.2, "sine", 1040);
    tone(0.17, 780, 0.2, 0.2, "sine", 1560);
  }

  // A coin landing: a metal clink, a two-note ding, and a sparkle for big coins.
  function coin(denom) {
    if (!ready()) return;
    for (const [f, d] of [[2730, 0.08], [4120, 0.06], [5650, 0.05]]) tone(0, f, d, 0.06);
    tone(0.03, 988, 0.09, 0.22, "triangle");
    tone(0.03, 988, 0.08, 0.04, "square");
    tone(0.11, 1319, 0.55, 0.22, "triangle");
    tone(0.11, 1319, 0.3, 0.04, "square");
    if (denom >= 10) tone(0.2, 1976, 0.45, 0.07, "triangle");
    if (denom >= 20) tone(0.28, 2637, 0.4, 0.05, "triangle");
  }

  // Cancelled: the slot closing, two soft notes going down.
  function close() {
    if (!ready()) return;
    tone(0, 660, 0.16, 0.16, "triangle", 520);
    tone(0.12, 440, 0.26, 0.16, "triangle", 300);
    tone(0.3, 150, 0.16, 0.22, "sine", 80);
  }

  // Online: a quick run up a major chord, landing on a shimmer.
  function online() {
    if (!ready()) return;
    [523.25, 659.25, 783.99, 1046.5].forEach((f, i) => tone(i * 0.085, f, 0.34, 0.2, "triangle"));
    [1046.5, 1318.5, 1568].forEach((f) => tone(0.34, f, 1.3, 0.08));
    tone(0.34, 2093, 0.9, 0.035, "triangle");
  }

  function setQuiet(q) {
    quiet = q;
    try { localStorage.setItem(SOUND_KEY, q ? "off" : "on"); } catch (_) { /* private mode */ }
    if (q) return;
    // A ding, so you hear it's on.
    wake();
    if (ctx && ctx.resume) Promise.resolve(ctx.resume()).then(() => coin(5));
  }

  return { wake, slot, coin, close, online, allowed, quiet: () => quiet, setQuiet };
})();

for (const t of ["pointerdown", "pointerup", "touchend", "keydown"]) {
  addEventListener(t, () => Sound.wake(), { capture: true, passive: true });
}

function renderSoundBtn() {
  const b = $("sound");
  b.hidden = !Sound.allowed();
  const on = !Sound.quiet();
  b.setAttribute("aria-pressed", String(on));
  b.firstElementChild.firstElementChild.setAttribute("href", on ? "#i-sound" : "#i-mute");
}
$("sound").addEventListener("click", () => {
  Sound.setQuiet(!Sound.quiet());
  renderSoundBtn();
});

// ---- coins ------------------------------------------------------------------

// A coin with both faces, for where it turns over: the value on the front,
// the plant on the back.
function twoSided(denom, px, cls) {
  const front = PisoCoins.draw(denom, px), back = PisoCoins.draw(denom, px, "back");
  back.classList.add("back");
  back.setAttribute("aria-hidden", "true");
  const coin = el("span", null, cls);
  coin.style.width = coin.style.height = front.getAttribute("width") + "px";
  coin.append(front, back);
  return coin;
}

function trayCoin(denom, pop) {
  const wrap = el("span", null, "mini");
  const art = PisoCoins.draw(denom, 40);
  wrap.style.width = art.getAttribute("width") + "px";
  wrap.style.height = art.getAttribute("height") + "px";
  wrap.append(art);
  if (pop) animate(wrap, [{ transform: "scale(0) rotate(-40deg)" }, { transform: "scale(1) rotate(0)" }], { duration: 600 });
  return wrap;
}

function countTo(target) {
  const node = $("pesos");
  const from = shownPesos;
  shownPesos = target;
  if (REDUCE) { node.textContent = target; return; }
  const t0 = performance.now(), dur = 500;
  const step = (t) => {
    const k = Math.min(1, (t - t0) / dur);
    node.textContent = Math.round(from + (target - from) * (1 - Math.pow(1 - k, 3)));
    if (k < 1) requestAnimationFrame(step);
  };
  requestAnimationFrame(step);
}

function dropCoin(denom) {
  const zone = $("drop-zone");
  const chute = $("chute");
  // Spinning in, it shows its plant side too.
  const fly = twoSided(denom, 92, "coin2 flying");
  fly.style.marginLeft = -parseFloat(fly.style.width) / 2 + "px";
  chute.append(fly);
  if (navigator.vibrate) navigator.vibrate(denom >= 10 ? [20, 40, 20] : 25);

  const done = () => {
    fly.remove();
    Sound.coin(denom);
    countTo(shownPesos + denom);
    $("tray").append(trayCoin(denom, true));
    animate(zone, [{ transform: "scale(1)" }, { transform: "scale(1.07)" }, { transform: "scale(1)" }], { duration: 520 });
    const plus = el("span", "+₱" + denom, "plus");
    zone.append(plus);
    const a = animate(plus, [{ transform: "translate(-50%, 0) scale(.6)", opacity: 0 }, { transform: "translate(-50%, -38px) scale(1)", opacity: 1, offset: 0.4 }, { transform: "translate(-50%, -62px)", opacity: 0 }], { duration: 1100, easing: "ease-out" });
    if (a) a.onfinish = () => plus.remove(); else plus.remove();
  };
  // Toss in from above, spinning like a real coin, then slip into the slot.
  const a = animate(fly, [
    { transform: "translateY(-30px) rotateY(0deg) scale(1.15)", opacity: 0 },
    { transform: "translateY(0px) rotateY(360deg) scale(1.15)", opacity: 1, offset: 0.35 },
    { transform: "translateY(40px) rotateY(540deg) scale(1)", opacity: 1, offset: 0.7 },
    { transform: "translateY(118px) rotateY(720deg) scale(.25)", opacity: 0 },
  ], { duration: 1050, easing: "cubic-bezier(.45, 0, .55, 1)" });
  if (a) a.onfinish = done; else done();
}

// ---- rates ------------------------------------------------------------------

let ratesKey = "";
function renderRates() {
  const key = JSON.stringify(state.rates);
  if (key === ratesKey) return;
  ratesKey = key;
  const best = state.rates.reduce((b, r) => (r.minutes / r.pesos > b.minutes / b.pesos ? r : b), state.rates[0]);
  $("rates").replaceChildren(...state.rates.map((r, i) => {
    const isBest = r === best && state.rates.length > 1;
    const li = el("li", null, "rate" + (isBest ? " best" : ""));
    const art = el("span", null, "art");
    // The coins turn over now and then to show the plant on the back.
    PisoCoins.split(r.pesos).slice(0, 3).forEach((d, j) => {
      const c = twoSided(d, 58, "coin2 stack");
      c.style.animationDelay = i * 0.8 + j * 0.15 + "s";
      art.append(c);
    });
    const price = el("span", null, "price");
    price.append(el("b", "₱" + r.pesos), " · " + perPeso(r));
    li.append(art, el("span", human(r.minutes * 60), "dur"), price);
    if (isBest) li.append(el("span", "Best value", "tag"));
    return li;
  }));
}

function perPeso(r) {
  const m = r.minutes / r.pesos;
  return (m >= 60 ? human(Math.round(m * 60)) : Math.round(m) + " min") + " per peso";
}

// ---- confetti ---------------------------------------------------------------

function confetti(from) {
  if (REDUCE) return;
  const box = from.getBoundingClientRect();
  const layer = $("confetti");
  const b = state && state.brand;
  const colors = b ? [b.accent, b.button || LIME, mix(b.accent, "#ffffff", 0.62), mix(b.accent, "#ffffff", 0.2), "#1E1F2E"] : ["#5A43E6", LIME, "#C3B9F7", "#7B5FF2", "#1E1F2E"];
  for (let i = 0; i < 36; i++) {
    const p = document.createElement("i");
    const w = 6 + Math.random() * 6;
    p.style.width = w + "px";
    p.style.height = (Math.random() < 0.5 ? w : w * 0.45) + "px";
    p.style.borderRadius = Math.random() < 0.4 ? "50%" : "2px";
    p.style.background = colors[i % colors.length];
    p.style.left = box.left + box.width / 2 + "px";
    p.style.top = box.top + box.height / 2 + "px";
    layer.append(p);
    const dx = (Math.random() - 0.5) * 360, up = 160 + Math.random() * 200, spin = (Math.random() - 0.5) * 900;
    if (!p.animate) { p.remove(); continue; }
    const a = p.animate([
      { transform: "translate(0, 0) rotate(0)", opacity: 1 },
      { transform: `translate(${dx * 0.6}px, ${-up}px) rotate(${spin / 2}deg)`, opacity: 1, offset: 0.35 },
      { transform: `translate(${dx}px, ${-up + 420}px) rotate(${spin}deg)`, opacity: 0 },
    ], { duration: 1300 + Math.random() * 700, easing: "cubic-bezier(.2, .6, .4, 1)" });
    a.onfinish = () => p.remove();
  }
}

// ---- owner branding -----------------------------------------------------------

const LIME = "#D4F04C";
const LOGO_WAVES = ["M18.6 27.6A19 19 0 0 1 45.4 27.6", "M12.9 21.9A27 27 0 0 1 51.1 21.9"];
const LOGO_PESO = "M29 47.5V34.5h4a3.7 3.7 0 0 1 0 7.4h-4M25.5 36.8h12.6M25.5 39.6h12.6";

// Relative luminance (0 black … 1 white) of "#RRGGBB".
function luminance(hex) {
  const [r, g, b] = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255)
    .map((v) => (v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4)));
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

function mix(hex, other, t) {
  const p = (h, i) => parseInt(h.slice(i, i + 2), 16);
  return "#" + [1, 3, 5].map((i) => Math.round(p(hex, i) * (1 - t) + p(other, i) * t).toString(16).padStart(2, "0")).join("");
}

const SVG = "http://www.w3.org/2000/svg";
function svgEl(tag, attrs) {
  const e = document.createElementNS(SVG, tag);
  for (const k in attrs) e.setAttribute(k, attrs[k]);
  return e;
}

// Built-in coin + WiFi mark in the owner's accent colour.
// The built-in mark: a peso coin sending out WiFi waves. Keep it in step
// with web/shared/icon.svg and defaultMark in admin.js.
function defaultMark(accent, button) {
  const s = svgEl("svg", { viewBox: "0 0 64 64" });
  const waves = svgEl("g", { fill: "none", stroke: button, "stroke-width": 5, "stroke-linecap": "round" });
  waves.append(svgEl("path", { d: LOGO_WAVES[0] }), svgEl("path", { d: LOGO_WAVES[1] }));
  const peso = svgEl("path", { d: LOGO_PESO, fill: "none", stroke: accent, "stroke-width": 2.6, "stroke-linecap": "round", "stroke-linejoin": "round" });
  s.append(svgEl("rect", { width: 64, height: 64, rx: 18, fill: accent }), waves,
    svgEl("circle", { cx: 32, cy: 41, r: 13, fill: button }), peso);
  return s;
}

// The same mark for the Glass look: a gel tile with soft colour swirling in
// it, a glossy rim, and the coin as a shiny bead.
let glassMarks = 0;
function glassMark(accent, button) {
  const id = "gm" + ++glassMarks, s = svgEl("svg", { viewBox: "0 0 64 64" });
  const radial = (name, cx, cy, r, stops) => {
    const g = svgEl("radialGradient", { id: id + name, cx, cy, r });
    for (const [o, c, a] of stops) g.append(svgEl("stop", { offset: o, "stop-color": c, "stop-opacity": a }));
    return g;
  };
  const sheen = svgEl("linearGradient", { id: id + "s", x1: 0, y1: 0, x2: 0, y2: 1 });
  sheen.append(svgEl("stop", { offset: 0, "stop-color": "#fff", "stop-opacity": 0.55 }), svgEl("stop", { offset: 0.5, "stop-color": "#fff", "stop-opacity": 0 }));
  const defs = svgEl("defs", {});
  defs.append(
    radial("a", 0.15, 0.1, 0.8, [[0, accent, 1], [1, accent, 0]]),
    radial("r", 0.95, 0.2, 0.6, [[0, "#F39BD3", 0.95], [1, "#F39BD3", 0]]),
    radial("p", 0.8, 1, 0.65, [[0, "#FFA27E", 0.85], [1, "#FFA27E", 0]]),
    radial("c", 0.36, 0.3, 0.75, [[0, mix(button, "#ffffff", 0.75), 1], [0.45, button, 1], [1, mix(button, "#000000", 0.22), 1]]),
    sheen);
  const tile = (fill) => svgEl("rect", { width: 64, height: 64, rx: 18, fill });
  const waves = svgEl("g", { fill: "none", stroke: "#fff", "stroke-width": 5, "stroke-linecap": "round", "stroke-opacity": 0.92 });
  waves.append(svgEl("path", { d: LOGO_WAVES[0] }), svgEl("path", { d: LOGO_WAVES[1] }));
  const peso = svgEl("path", { d: LOGO_PESO, fill: "none", stroke: mix(accent, "#000000", 0.15), "stroke-width": 2.6, "stroke-linecap": "round", "stroke-linejoin": "round" });
  s.append(defs, tile(mix(accent, "#ffffff", 0.35)), tile("url(#" + id + "a)"), tile("url(#" + id + "r)"), tile("url(#" + id + "p)"), tile("url(#" + id + "s)"),
    svgEl("rect", { x: 0.9, y: 0.9, width: 62.2, height: 62.2, rx: 17.1, fill: "none", stroke: "#fff", "stroke-opacity": 0.7, "stroke-width": 1.8 }),
    waves,
    svgEl("circle", { cx: 32, cy: 42.5, r: 13, fill: "#000", opacity: 0.12 }),
    svgEl("circle", { cx: 32, cy: 41, r: 13, fill: "url(#" + id + "c)" }),
    svgEl("circle", { cx: 32, cy: 41, r: 12.4, fill: "none", stroke: "#fff", "stroke-opacity": 0.6, "stroke-width": 1.2 }),
    peso);
  return s;
}

function logoNode(b) {
  if (!b.logo) return b.theme === "glass" ? glassMark(b.accent, b.button || LIME) : defaultMark(b.accent, b.button || LIME);
  const img = document.createElement("img");
  img.src = b.logo;
  img.alt = "";
  return img;
}

// The owner's font for the name. Fonts are built into the vendo (no
// internet needed); each is loaded once, only when picked.
const nameFonts = {};
function applyNameFont(b) {
  const root = document.documentElement.style;
  root.setProperty("--name-weight", String(b.name_weight || 800));
  const key = b.name_font;
  if (!key || !window.FontFace || !document.fonts) {
    root.removeProperty("--name-font");
    root.removeProperty("--name-track");
    return;
  }
  const family = "Name " + key;
  if (!nameFonts[key]) {
    nameFonts[key] = new FontFace(family, "url(/fonts/" + key + ".woff2)", { weight: "1 1000", display: "swap", sizeAdjust: (b.name_size || 100) + "%" });
    document.fonts.add(nameFonts[key]);
    nameFonts[key].load().catch(() => {});
  }
  root.setProperty("--name-font", '"' + family + '", var(--font)');
  root.setProperty("--name-track", "0");
}

let brandKey = "";
function applyBrand(b) {
  const key = JSON.stringify(b);
  if (key === brandKey) return;
  brandKey = key;
  document.title = b.name;
  $("name").textContent = b.name;
  // The page arrives with the owner's look already set; this follows a change made while it's open.
  const glass = b.theme === "glass";
  if (glass) document.documentElement.dataset.theme = "glass";
  else delete document.documentElement.dataset.theme;
  document.querySelector('meta[name="theme-color"]').content = glass ? "#EFEBF5" : "#FFFFFF";
  const root = document.documentElement.style;
  root.setProperty("--purple", b.accent);
  root.setProperty("--purple-2", mix(b.accent, "#ffffff", 0.2));
  root.setProperty("--purple-ink", mix(b.accent, "#000000", 0.25));
  root.setProperty("--lavender", mix(b.accent, "#ffffff", 0.9));
  root.setProperty("--lavender-2", mix(b.accent, "#ffffff", 0.8));
  root.setProperty("--lavender-3", mix(b.accent, "#ffffff", 0.62));
  document.querySelector("#ring-grad stop:last-child").setAttribute("stop-color", b.accent);
  // The coin button's colour, with text that stays readable on it.
  const btn = b.button || LIME, dark = luminance(btn) < 0.2;
  root.setProperty("--lime", btn);
  root.setProperty("--lime-ink", dark ? mix(btn, "#ffffff", 0.78) : mix(btn, "#000000", 0.7));
  root.setProperty("--lime-bright", dark ? mix(btn, "#ffffff", 0.55) : btn);
  root.setProperty("--on-lime", dark ? "#ffffff" : "var(--ink)");
  document.querySelector("#ring-grad stop:first-child").setAttribute("stop-color", btn);
  document.querySelector("#ring-gel stop:first-child").setAttribute("stop-color", btn);
  document.querySelector("#ring-gel stop:last-child").setAttribute("stop-color", b.accent);
  $("logo").replaceChildren(logoNode(b));
  $("ob-logo").replaceChildren(logoNode(b));
  $("ob-name").textContent = b.name;
  if (Array.isArray(b.coins) && b.coins.length) acceptedCoins = b.coins;
  if (!$("onboard").hidden) renderCoinIntro();
  $("home-logo").replaceChildren(logoNode(b));
  $("hs-logo").replaceChildren(logoNode(b));
  $("hs-name").textContent = b.name;
  renderSoundBtn();
  renderNotice(b.notice || "");
  applyNameFont(b);

  const fig = $("poster"), img = $("poster-img");
  if (b.poster) {
    img.alt = b.poster.alt || "";
    if (img.getAttribute("src") !== b.poster.url) {
      fig.classList.remove("ready");
      img.onload = () => fig.classList.add("ready");
      img.src = b.poster.url;
    }
    fig.hidden = false;
  } else {
    fig.hidden = true;
    img.removeAttribute("src");
  }
}

// The owner's message: still when it fits, gliding by when it doesn't.
function renderNotice(text) {
  const box = $("notice"), track = $("notice-track");
  box.hidden = !text;
  box.classList.remove("scroll", "wrap");
  track.replaceChildren(el("span", text));
  if (!text) return;
  requestAnimationFrame(() => {
    const w = track.firstChild.offsetWidth;
    if (w <= track.parentElement.clientWidth) return;
    if (REDUCE) { box.classList.add("wrap"); return; }
    const copy = el("span", text);
    copy.setAttribute("aria-hidden", "true");
    track.append(copy);
    track.style.setProperty("--dur", Math.max(8, (w + 48) / 42).toFixed(1) + "s");
    box.classList.add("scroll");
  });
}

// ---- onboarding ---------------------------------------------------------------

const OB_KEY = "pisowifi-onboarded";
let obIndex = 0, obChecked = false;
const OB_COUNT = 3;

function seenOnboarding() {
  try { return localStorage.getItem(OB_KEY) === "1"; } catch (_) { return false; }
}

function maybeOnboard() {
  if (obChecked) return;
  obChecked = true;
  const fromQr = new URLSearchParams(location.search).has("welcome");
  if (fromQr) history.replaceState(null, "", location.pathname);
  if (!seenOnboarding() && !standalone()) openOnboarding();
}

// Only the coins the owner set the machine to take.
let acceptedCoins = [1, 5, 10, 20];
function coinList(coins, last) {
  const names = coins.map((c) => "₱" + c);
  return names.length < 2 ? names.join("") : names.slice(0, -1).join(", ") + " " + last + " " + names.at(-1);
}
function renderCoinIntro() {
  const coins = acceptedCoins;
  $("ob-coin-list").textContent = coins.length === 1 ? "Use " + coinList(coins) + " coins." : coinList(coins, "and") + (coins.length > 2 ? " all work." : " both work.");
  const box = $("ob-coins");
  if (box.dataset.coins !== coins.join()) {
    box.dataset.coins = coins.join();
    box.replaceChildren(...coins.map((d) => PisoCoins.draw(d, 70)));
  }
}

function openOnboarding() {
  $("ob-addr").textContent = location.host;
  renderCoinIntro();
  $("onboard").hidden = false;
  obGo(0);
  $("ob-sheet").focus({ preventScroll: true });
}

function closeOnboarding() {
  $("onboard").hidden = true;
  try { localStorage.setItem(OB_KEY, "1"); } catch (_) { /* private mode: show again next time */ }
  $("help").focus();
}

function obGo(i) {
  obIndex = Math.max(0, Math.min(OB_COUNT - 1, i));
  $("ob-track").style.transform = `translateX(${-100 * obIndex}%)`;
  [...$("ob-progress").children].forEach((bar, j) => bar.classList.toggle("on", j === obIndex));
  $("ob-next").textContent = obIndex === OB_COUNT - 1 ? "Let's go" : "Next";
  $("ob-skip").hidden = obIndex === OB_COUNT - 1;
}

$("ob-next").addEventListener("click", () => (obIndex === OB_COUNT - 1 ? closeOnboarding() : obGo(obIndex + 1)));
$("ob-skip").addEventListener("click", closeOnboarding);
$("help").addEventListener("click", openOnboarding);
$("onboard").addEventListener("click", (ev) => { if (ev.target === $("onboard")) closeOnboarding(); });
$("onboard").addEventListener("keydown", (ev) => {
  if (ev.key === "Escape") closeOnboarding();
  if (ev.key === "ArrowRight") obGo(obIndex + 1);
  if (ev.key === "ArrowLeft") obGo(obIndex - 1);
});

// Swipe between steps; the sheet follows your finger.
(() => {
  const track = $("ob-track");
  let x0 = null, dx = 0;
  track.addEventListener("pointerdown", (ev) => { x0 = ev.clientX; dx = 0; track.classList.add("dragging"); });
  track.addEventListener("pointermove", (ev) => {
    if (x0 === null) return;
    dx = ev.clientX - x0;
    track.style.transform = `translateX(calc(${-100 * obIndex}% + ${dx}px))`;
  });
  const end = () => {
    if (x0 === null) return;
    track.classList.remove("dragging");
    x0 = null;
    if (dx < -50) obGo(obIndex + 1);
    else if (dx > 50) obGo(obIndex - 1);
    else obGo(obIndex);
  };
  track.addEventListener("pointerup", end);
  track.addEventListener("pointercancel", end);
  track.addEventListener("pointerleave", end);
})();

// ---- polling ----------------------------------------------------------------

async function refresh() {
  try {
    state = await api("/me");
    fetchedAt = performance.now();
    render();
    // Its time was paused while it was away and started again just now.
    if (state.welcome_back) toast("Welcome back! Your time started again.");
    renderPass();
    renderHome();
    maybeOnboard();
    maybeClaimPass();
  } catch (e) {
    if (e.status === 403) $("meter-sub").textContent = e.message;
  }
  schedule();
}

function schedule() {
  clearTimeout(pollTimer);
  const fast = state && state.slot.state !== "idle";
  pollTimer = setTimeout(refresh, fast ? 500 : 4000);
}

async function action(btn, fn) {
  btn.disabled = true;
  try {
    await fn();
  } catch (e) {
    toast(e.message, true);
  } finally {
    btn.disabled = false;
    await refresh();
  }
}

const isOnline = () => !!state && state.running && state.remaining_secs - elapsed() > 0;

// Phones decide whether this WiFi "has internet" when the sign-in page loads.
// Load a page after paying so the sign-in sheet closes and apps use WiFi: on
// first getting online, the owner's pick (a cat search, unless they changed
// it); otherwise this page again.
function recheckSoon(landing) {
  setTimeout(() => {
    const url = landing && isOnline() && state.brand.after_url;
    if (url && /^https?:\/\//i.test(url)) location.assign(url);
    else location.replace(location.pathname);
  }, 1800);
}

// Time added and running: a chime, then a fresh page load.
function wentOnline(wasOnline) {
  Sound.online();
  recheckSoon(!wasOnline);
}

// ---- events -----------------------------------------------------------------

$("slot").addEventListener("click", () =>
  action($("slot"), async () => {
    try {
      await api("/coin/start", {});
    } catch (e) {
      if (e.status === 409) return; // render shows the busy state
      throw e;
    }
  }));

$("done").addEventListener("click", () =>
  action($("done"), async () => {
    const before = state.slot.pesos;
    const wasOnline = isOnline();
    // The saved pass goes along, so the coins join it instead of a new pass.
    const saved = storedPass();
    const r = await api("/coin/done", saved ? { pass: saved } : {});
    if (r.pass_unknown) unknownPass = saved;
    if (r.pass_secs !== undefined) autoClaim.done = true;
    if (before > 0) {
      confetti($("done"));
      Sound.online();
      if (r.pass_code) showPassSheet(r.pass_code, !wasOnline);
      else {
        toast(r.pass_secs > 0 ? "You're online, with " + human(r.pass_secs) + " from your Piso Pass too." : "You're online. Enjoy!");
        recheckSoon(!wasOnline);
      }
    }
  }));

// Changed their mind before dropping anything: close the slot so the next
// customer can use it. If a coin lands meanwhile, the server credits it.
// The card plays out the slot closing first (ring unwinds, a cross, then it
// folds away) while the server frees the slot, so backing out is felt.
$("cancel-insert").addEventListener("click", () =>
  action($("cancel-insert"), async () => {
    if (!state.slot || state.slot.pesos > 0) return;
    const card = $("inserting");
    if (!REDUCE) {
      slotBack = true;
      cancelHold = true;
      clearTimeout(card._open);
      card.classList.remove("opening");
      card.classList.add("closing");
      $("slot-pill").textContent = "Closing";
      Sound.close();
      if (navigator.vibrate) navigator.vibrate([18, 60, 30]);
    }
    try {
      await Promise.all([api("/coin/done", {}), new Promise((r) => setTimeout(r, REDUCE ? 0 : 820))]);
      cancelHold = false;
    } catch (e) {
      cancelHold = false;
      slotBack = false;
      card.classList.remove("closing");
      $("slot-pill").textContent = "Slot open";
      throw e;
    }
    toast("Cancelled. Tap Insert coin when you're ready.");
  }));

$("pause").addEventListener("click", () =>
  action($("pause"), async () => {
    await api("/pause", {});
    toast("Paused. Your time is saved.");
  }));

$("resume").addEventListener("click", () =>
  action($("resume"), async () => {
    await api("/resume", {});
    toast("Welcome back. You're online.");
    wentOnline(false);
  }));

$("voucher-form").addEventListener("submit", (ev) => {
  ev.preventDefault();
  const btn = ev.submitter || $("voucher-form").querySelector("button");
  const code = $("code").value;
  const wasOnline = isOnline();
  action(btn, () => withShake($("code"), async () => {
    if (isPassCode(code)) {
      const r = await api("/pass/claim", { code, start: true });
      savePass(code);
      $("code").value = "";
      confetti(btn);
      toast(r.secs > 0 ? "Piso Pass added: " + human(r.secs) + "." : "Piso Pass linked to this phone.");
      if (r.secs > 0) wentOnline(wasOnline);
      return;
    }
    const r = await api("/voucher", { code });
    $("code").value = "";
    confetti(btn);
    toast("Voucher added: " + human(r.secs) + ".");
    wentOnline(wasOnline);
  }));
});

// A code that doesn't work shakes the box, like a head saying no.
async function withShake(input, fn) {
  try {
    return await fn();
  } catch (e) {
    animate(input, [
      { transform: "translateX(0)" }, { transform: "translateX(-9px)" }, { transform: "translateX(7px)" },
      { transform: "translateX(-5px)" }, { transform: "translateX(3px)" }, { transform: "translateX(0)" },
    ], { duration: 450, easing: "ease-out", fill: "none" });
    if (navigator.vibrate) navigator.vibrate([40, 30, 40]);
    throw e;
  }
}

// Tap a price and its coins flip over.
$("rates").addEventListener("click", (ev) => {
  const card = ev.target.closest(".rate");
  if (!card || REDUCE) return;
  card.classList.remove("flip");
  void card.offsetWidth;
  card.classList.add("flip");
  clearTimeout(card._flip);
  card._flip = setTimeout(() => card.classList.remove("flip"), 1000);
});

// Vouchers are XXXX-XXXX-XXXX; passes are PASS-XXXX-XXXX-XXXX-XXXX.
$("code").addEventListener("input", (ev) => {
  let raw = ev.target.value.toUpperCase().replace(/[^A-Z0-9]/g, "");
  const pass = raw.startsWith("PASS") || raw.length > 12;
  if (raw.startsWith("PASS")) raw = raw.slice(4);
  raw = raw.slice(0, pass ? 16 : 12);
  const grouped = (raw.match(/.{1,4}/g) || []).join("-");
  ev.target.value = pass && grouped ? "PASS-" + grouped : grouped;
});

// ---- Piso Pass ------------------------------------------------------------------

// Saved per chain (one owner's hub): every owner's vendos use 10.0.0.1, so a
// phone may carry passes for several chains, and each only works on its own.
// A saved pass is never deleted automatically; only a pass the customer
// claims, or asks for with "Save my time", replaces it.
const PASS_KEY = "pisowifi-pass:";
let unknownPass = null;  // saved pass this chain's hub says doesn't exist

function isPassCode(code) {
  const raw = code.toUpperCase().replace(/[^A-Z0-9]/g, "");
  return raw.startsWith("PASS") || raw.length === 16;
}
function passKey() {
  return state && state.pass.enabled && state.pass.chain ? PASS_KEY + state.pass.chain : null;
}
function storedPass() {
  const k = passKey();
  if (!k) return null;
  try { return localStorage.getItem(k); } catch (_) { return null; }
}
function savePass(code) {
  const raw = code.toUpperCase().replace(/[^A-Z0-9]/g, "").replace(/^PASS/, "");
  const nice = "PASS-" + raw.match(/.{1,4}/g).join("-");
  const k = passKey();
  if (k) {
    try { localStorage.setItem(k, nice); } catch (_) { /* private mode */ }
  }
  return nice;
}

let sheetCode = null;   // pass shown in the sheet (works even if storage is off)
let sheetLanding = false; // shown on first getting online: "Got it" goes on from there

function showPassSheet(code, landing) {
  const nice = savePass(code);
  sheetCode = nice;
  sheetLanding = !!landing;
  $("ps-code").textContent = nice;
  $("ps-brand").textContent = state.brand.name;
  $("pass-sheet").hidden = false;
  $("ps-box").focus({ preventScroll: true });
  renderPass();
}
$("ps-done").addEventListener("click", () => {
  $("pass-sheet").hidden = true;
  toast("You're online. Enjoy!");
  if (state && state.running) recheckSoon(sheetLanding);
});
$("pass-sheet").addEventListener("click", (ev) => { if (ev.target === $("pass-sheet")) $("pass-sheet").hidden = true; });
$("pass-show").addEventListener("click", () => {
  const code = storedPass();
  if (code) showPassSheet(code);
});
$("pass-save").addEventListener("click", () =>
  action($("pass-save"), async () => {
    const r = await api("/pass/save", {});
    showPassSheet(r.pass_code);
  }));

function renderPass() {
  if (!state) return;
  renderWallet();
  const enabled = state.pass.enabled;
  const code = storedPass();
  // A saved pass the hub doesn't know can be replaced, but only by asking.
  const usable = code && code !== unknownPass;
  const canSave = !usable && !state.pass.linked && state.remaining_secs > 0;
  $("code-label").textContent = enabled ? "Have a voucher or Piso Pass?" : "Have a voucher code?";
  const card = $("pass-card");
  card.hidden = !enabled || (!code && !canSave);
  $("pass-show").hidden = !code;
  $("pass-save").hidden = !canSave;
  const line = $("pass-line");
  if (code && !usable) {
    line.textContent = "Your saved pass isn't recognized here";
    line.className = "";
  } else if (code) {
    line.textContent = code.slice(0, 9) + "-••••-••••";
    line.className = "mono";
  } else {
    line.textContent = "Keep your time for any of our locations";
    line.className = "";
  }
}

// ---- wallet passes ----------------------------------------------------------------

const UA = navigator.userAgent;
const IS_IOS = /iPhone|iPad|iPod/.test(UA) || (/Macintosh/.test(UA) && navigator.maxTouchPoints > 1);
const IS_ANDROID = /Android/.test(UA);

// Apple Wallet on iPhones, Google Wallet on Android (Samsung too); both when unsure.
function walletKinds() {
  const w = state.pass.wallet || {};
  const kinds = [];
  if (w.apple && !IS_ANDROID) kinds.push("apple");
  if (w.google && !IS_IOS) kinds.push("google");
  return kinds;
}

function renderWallet() {
  const kinds = walletKinds();
  $("ps-apple").hidden = !kinds.includes("apple");
  $("ps-google").hidden = !kinds.includes("google");
  $("ps-wallet").hidden = !kinds.length;
  $("ps-text").textContent = "This pass is saved on this phone, so at any of our locations your time is waiting for you. "
    + (kinds.length ? "Add it to your wallet so it's always with you." : "Take a screenshot too, just in case.");
}

// The hub builds the pass; its one-time link opens over the internet, so the
// customer's time has to be running.
function addToWallet(kind, btn) {
  action(btn, async () => {
    const left = state.running ? state.remaining_secs - elapsed() : 0;
    if (!(left > 0)) throw new Error("Adding to your wallet needs internet. Start or resume your time first.");
    const r = await api("/pass/wallet", { code: sheetCode || storedPass(), kind });
    location.assign(r.url);
  });
}
$("ps-apple").addEventListener("click", () => addToWallet("apple", $("ps-apple")));
$("ps-google").addEventListener("click", () => addToWallet("google", $("ps-google")));

// ---- add to home screen ------------------------------------------------------------

// Browsers only offer their own install prompt on HTTPS, so on plain
// http://10.0.0.1 the card shows the steps for this browser instead. Where a
// prompt is offered (a future HTTPS portal, or localhost), it's used.
const HOME_KEY = "pisowifi-home";
let installPrompt = null;

function standalone() {
  return navigator.standalone === true || ["standalone", "fullscreen", "minimal-ui"].some((m) => matchMedia("(display-mode: " + m + ")").matches);
}
function homeAdded() {
  try { return localStorage.getItem(HOME_KEY) === "1"; } catch (_) { return false; }
}
function markHomeAdded() {
  try { localStorage.setItem(HOME_KEY, "1"); } catch (_) { /* private mode */ }
}

// Opened from the icon (the manifest's start_url is /?home): tidy the address.
(() => {
  const q = new URLSearchParams(location.search);
  if (!q.has("home")) return;
  markHomeAdded();
  q.delete("home");
  const rest = q.toString();
  history.replaceState(null, "", location.pathname + (rest ? "?" + rest : "") + location.hash);
})();

function icon(id) {
  const s = svgEl("svg", { class: "ic", "aria-hidden": "true" });
  s.append(svgEl("use", { href: "#i-" + id }));
  return s;
}
// A button as it looks in the browser: its icon, its label, or both.
function key(label, ic, name) {
  const k = el("b", null, "key");
  if (ic) k.append(icon(ic));
  if (label) k.append(label);
  else k.setAttribute("aria-label", name);
  return k;
}
const bold = (text) => el("b", text);

// Steps for this browser, or null where it can't make a home-screen icon.
function homeGuide() {
  const addr = () => el("b", location.host, "addr");
  const elsewhere = (browser) => ({
    webview: true,
    title: "Do this in " + browser,
    text: "This window can't add icons, but your browser can.",
    steps: [
      ["Close this window"],
      ["Open ", bold(browser), " and go to ", addr()],
      ["Tap ", bold("Add to home screen"), " there"],
    ],
  });
  const dotsV = () => key(null, "dots-v", "the three dots");
  const dotsH = () => key(null, "dots-h", "the three dots");
  const menu = () => key(null, "menu", "the menu");
  const inApp = /FBAN|FBAV|FB_IAB|Instagram|Line\/|MicroMessenger|GSA\//.test(UA);

  if (IS_IOS) {
    // Apple's sign-in window and apps' built-in browsers don't say "Safari".
    if (!/Safari\//.test(UA) || inApp) return elsewhere("Safari");
    const share = key("Share", "share");
    const confirm = ["Tap ", bold("Add"), " in the top corner"];
    if (/CriOS\//.test(UA)) {
      return { steps: [["Tap ", share, " at the top right"], ["Scroll down and tap ", key("Add to Home Screen", "plus-sq")], confirm] };
    }
    if (/FxiOS\/|EdgiOS\/|OPiOS\/|OPT\//.test(UA)) {
      return { steps: [["Open the menu ", menu(), " or ", dotsH(), " and tap ", share], ["Tap ", key("Add to Home Screen", "plus-sq")], confirm] };
    }
    const version = Number((UA.match(/Version\/(\d+)/) || [])[1]) || 0;
    const ipad = /iPad|Macintosh/.test(UA);
    if (version >= 26) {
      return { steps: [
        ipad ? ["Tap ", share, " at the top"] : ["Tap ", dotsH(), " at the bottom, then ", share],
        ["Tap ", bold("View More"), " if you need to, then ", key("Add to Home Screen", "plus-sq")],
        confirm,
      ] };
    }
    return { steps: [["Tap ", share, ipad ? " at the top" : " at the bottom"], ["Scroll down and tap ", key("Add to Home Screen", "plus-sq")], confirm] };
  }

  if (IS_ANDROID) {
    // The WiFi sign-in window and apps' built-in browsers are WebViews ("wv").
    if (/; wv\)/.test(UA) || inApp) return elsewhere("Chrome");
    if (/SamsungBrowser\//.test(UA)) {
      return { steps: [["Tap ", menu(), " at the bottom right"], ["Tap ", bold("Add page to"), ", then ", bold("Home screen")], ["Tap ", bold("Add")]] };
    }
    if (/EdgA\//.test(UA)) {
      return { steps: [["Tap ", dotsH(), " at the bottom"], ["Tap ", bold("Add to phone")], ["Tap ", bold("Add"), " or ", bold("Install")]] };
    }
    if (/Firefox\//.test(UA)) {
      return { steps: [["Tap ", dotsV()], ["Tap ", bold("Add to Home screen")], ["Tap ", bold("Add")]] };
    }
    if (/Chrome\//.test(UA) && !/OPR\/|MiuiBrowser|HuaweiBrowser|HeyTapBrowser|VivoBrowser|UCBrowser|YaBrowser/.test(UA)) {
      return { steps: [["Tap ", dotsV(), " at the top right"], ["Tap ", bold("Add to home screen")], ["Tap ", bold("Add"), " or ", bold("Install")]] };
    }
    return { steps: [["Open your browser's menu ", dotsV(), " or ", menu()], ["Look for ", bold("Add to home screen")], ["Not there? Open ", addr(), " in ", bold("Chrome")]] };
  }

  // Computers: a shortcut in the Dock or on the desktop.
  const desk = (steps) => ({ title: "Add it to your computer", text: "Then your time is one click away.", steps });
  if (/Edg\//.test(UA)) return desk([["Click ", dotsH(), " at the top right"], ["Click ", bold("Apps"), ", then ", bold("Install this site as an app")], ["Click ", bold("Install")]]);
  if (/Chrome\//.test(UA) && !/OPR\//.test(UA)) return desk([["Click ", dotsV(), " at the top right"], ["Click ", bold("Cast, save, and share"), ", then ", bold("Create shortcut")], ["Click ", bold("Create")]]);
  if (/Macintosh/.test(UA) && /Safari\//.test(UA) && Number((UA.match(/Version\/(\d+)/) || [])[1]) >= 17) {
    return desk([["Click ", bold("File"), " in the menu bar"], ["Click ", bold("Add to Dock")], ["Click ", bold("Add")]]);
  }
  if (/Macintosh/.test(UA) && !/Safari\/|Firefox\//.test(UA)) return elsewhere("Safari"); // the Mac's sign-in window
  return null; // e.g. Firefox on a computer
}
const HOME_GUIDE = homeGuide();

function renderHome() {
  if (!state) return;
  if (standalone()) markHomeAdded();
  const relevant = state.remaining_secs > 0 || !!storedPass();
  $("home-card").hidden = !relevant || homeAdded() || !(installPrompt || HOME_GUIDE);
}

function openHomeSheet() {
  const g = HOME_GUIDE;
  $("hs-title").textContent = g.title || "Add to your home screen";
  $("hs-text").textContent = g.text || "Then your time is one tap away, like an app.";
  $("hs-steps").replaceChildren(...g.steps.map((parts, i) => {
    const li = el("li");
    const t = el("span");
    t.append(...parts);
    li.append(el("span", String(i + 1), "n"), t);
    return li;
  }));
  $("hs-done").textContent = g.webview ? "Got it" : "I added it";
  $("hs-later").hidden = !!g.webview;
  $("home-sheet").hidden = false;
  $("hs-box").scrollTop = 0;
  $("hs-box").focus({ preventScroll: true });
}

function closeHomeSheet() {
  $("home-sheet").hidden = true;
  const add = $("home-add");
  (add.offsetParent ? add : $("help")).focus({ preventScroll: true });
}

$("home-add").addEventListener("click", async () => {
  if (!installPrompt) return openHomeSheet();
  const p = installPrompt;
  installPrompt = null;
  p.prompt();
  const choice = await p.userChoice;
  if (choice.outcome === "accepted") {
    markHomeAdded();
    toast("Added. Tap the icon any time to check your time.");
  }
  renderHome();
});
$("hs-done").addEventListener("click", () => {
  if (!HOME_GUIDE.webview) {
    markHomeAdded();
    toast("Nice! Tap the icon any time to check your time.");
  }
  renderHome();
  closeHomeSheet();
});
$("hs-later").addEventListener("click", closeHomeSheet);
$("home-sheet").addEventListener("click", (ev) => { if (ev.target === $("home-sheet")) closeHomeSheet(); });
$("home-sheet").addEventListener("keydown", (ev) => { if (ev.key === "Escape") closeHomeSheet(); });
addEventListener("beforeinstallprompt", (ev) => {
  ev.preventDefault(); // the card asks instead, when it's useful
  installPrompt = ev;
  renderHome();
});
addEventListener("appinstalled", () => {
  markHomeAdded();
  renderHome();
});

// Opened from a wallet pass ("Use my time" → http://10.0.0.1/?pass=CODE):
// tidy the address bar, then offer to claim it. Never adopted silently, so a
// planted link can't collect other customers' time.
let deepPass = null;
(() => {
  const q = new URLSearchParams(location.search);
  const code = q.get("pass");
  if (code === null) return;
  q.delete("pass");
  const rest = q.toString();
  history.replaceState(null, "", location.pathname + (rest ? "?" + rest : "") + location.hash);
  const raw = code.toUpperCase().replace(/[^A-Z0-9]/g, "").replace(/^PASS/, "");
  if (raw.length === 16) deepPass = "PASS-" + raw.match(/.{4}/g).join("-");
})();

function askDeepPass(code) {
  $("cs-code").textContent = code;
  $("claim-sheet").hidden = false;
  $("cs-box").focus({ preventScroll: true });
}
const closeClaimSheet = () => { $("claim-sheet").hidden = true; deepPass = null; };
$("cs-no").addEventListener("click", closeClaimSheet);
$("claim-sheet").addEventListener("click", (ev) => { if (ev.target === $("claim-sheet")) closeClaimSheet(); });
$("cs-yes").addEventListener("click", () =>
  action($("cs-yes"), async () => {
    const code = deepPass;
    const wasOnline = isOnline();
    closeClaimSheet();
    const r = await api("/pass/claim", { code, start: true });
    savePass(code);
    autoClaim.done = true;
    toast(r.secs > 0 ? "Welcome back! " + human(r.secs) + " from your Piso Pass." : "Piso Pass linked to this phone.");
    if (r.secs > 0) {
      confetti($("meter"));
      wentOnline(wasOnline);
    }
  }));

// At another location: bring this phone's saved pass along automatically.
// Paused time stays paused (the phone may be in a pocket). If the hub can't
// be reached, try again later, waiting longer each time.
const autoClaim = { done: false, busy: false, next: 0, wait: 15000 };

async function maybeClaimPass() {
  if (!state.pass.enabled || !state.pass.chain) return;
  if (deepPass) {
    // The pass already saved on this phone: nothing to ask.
    if (deepPass === storedPass()) deepPass = null;
    else if ($("claim-sheet").hidden) return askDeepPass(deepPass);
    else return;
  }
  if (autoClaim.done || autoClaim.busy || state.pass.linked) return;
  const code = storedPass();
  if (!code || code === unknownPass || performance.now() < autoClaim.next) return;
  autoClaim.busy = true;
  const wasOnline = isOnline();
  try {
    const r = await api("/pass/claim", { code });
    autoClaim.done = true;
    if (r.secs > 0) {
      confetti($("meter"));
      if (r.running) {
        toast("Welcome back! " + human(r.secs) + " from your Piso Pass.");
        wentOnline(wasOnline);
      } else {
        toast("Welcome back! " + human(r.secs) + " from your Piso Pass is saved here. Tap Resume to go online.");
      }
    }
    refresh();
  } catch (e) {
    if (e.code === "unknown_pass") {
      unknownPass = code;
      autoClaim.done = true;
    } else if (!e.status || e.status >= 500) {
      autoClaim.next = performance.now() + autoClaim.wait;
      autoClaim.wait = Math.min(autoClaim.wait * 2, 120000);
    } else {
      autoClaim.done = true; // e.g. this phone's pass time is already here
    }
  } finally {
    autoClaim.busy = false;
  }
  renderPass();
}

document.addEventListener("visibilitychange", () => {
  if (!document.hidden) refresh();
});

$("slot").querySelector(".peek").append(PisoCoins.draw(5, 30));
$("busy").querySelector(".busy-coin").append(PisoCoins.draw(20, 44));
setInterval(renderClock, 1000);
refresh();
