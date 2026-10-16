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
  // Icon: a check for good news, "!" for problems.
  const ic = el("span", null, "toast-ic");
  const s = svgEl("svg", { viewBox: "0 0 20 20", "aria-hidden": "true" });
  if (bad) s.append(svgEl("path", { d: "M10 5.5v5.5" }), svgEl("path", { d: "M10 14.5v.01" }));
  else s.append(svgEl("path", { d: "M5.5 10.5l3 3 6-7" }));
  ic.append(s);
  t.replaceChildren(ic, el("span", msg));
  t.classList.toggle("bad", !!bad);
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
  $("meter-sub").textContent = online
    ? "Until about " + new Date(Date.now() + left * 1000).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })
    : paused ? "Paused. Your time isn't running." : "Drop a coin to get online";
  $("pause").hidden = !online;
  $("resume").hidden = !paused;

  const slot = state.slot;
  if (slot.state !== "idle") {
    const sl = Math.max(0, slot.secs_left - elapsed());
    $("secs-left").textContent = sl;
    $("busy-secs").textContent = sl;
    $("ring").style.strokeDashoffset = String(RING * (1 - sl / slot.window_secs));
  }
}

let lastSlotState = "idle";

function render() {
  const slot = state.slot;
  const changed = slot.state !== lastSlotState && !firstRender;
  const apply = () => {
    applyBrand(state.brand);
    $("device").textContent = state.device;
    $("app").setAttribute("aria-busy", "false");
    $("slot").hidden = slot.state !== "idle";
    $("inserting").hidden = slot.state !== "mine";
    $("busy").hidden = slot.state !== "busy";
    $("slot-title").textContent = state.remaining_secs > 0 ? "Add more time" : "Insert coin";
    $("slot-sub").textContent = state.remaining_secs > 0 ? "Top up before your time runs out" : "Tap here first, then drop your coins";
    renderRates();
    renderClock();
  };
  if (changed && !REDUCE && document.startViewTransition) document.startViewTransition(apply);
  else apply();

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
  } else {
    seenCoins = 0;
  }
  lastSlotState = slot.state;
  firstRender = false;
}

// ---- coins ------------------------------------------------------------------

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
  const art = PisoCoins.draw(denom, 92);
  const size = Number(art.getAttribute("width"));
  const fly = el("div", null, "flying");
  fly.style.width = fly.style.height = size + "px";
  fly.style.marginLeft = -size / 2 + "px";
  fly.append(art);
  chute.append(fly);
  if (navigator.vibrate) navigator.vibrate(denom >= 10 ? [20, 40, 20] : 25);

  const done = () => {
    fly.remove();
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
  $("rates").replaceChildren(...state.rates.map((r) => {
    const isBest = r === best && state.rates.length > 1;
    const li = el("li", null, "rate" + (isBest ? " best" : ""));
    const art = el("span", null, "art");
    for (const d of PisoCoins.split(r.pesos).slice(0, 3)) {
      const c = PisoCoins.draw(d, 58);
      c.classList.add("stack");
      art.append(c);
    }
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
  const colors = ["#5A43E6", "#D4F04C", "#C3B9F7", "#7B5FF2", "#1E1F2E"];
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
function defaultMark(accent) {
  const s = svgEl("svg", { viewBox: "0 0 64 64" });
  const g = svgEl("g", { fill: "none", stroke: accent, "stroke-width": 4, "stroke-linecap": "round" });
  g.append(svgEl("path", { d: "M27.05 32.05a7 7 0 0 1 9.9 0" }), svgEl("path", { d: "M22.8 27.8a13 13 0 0 1 18.4 0" }));
  s.append(svgEl("rect", { width: 64, height: 64, rx: 18, fill: accent }), svgEl("circle", { cx: 32, cy: 32, r: 20, fill: "#D4F04C" }), g,
    svgEl("circle", { cx: 32, cy: 38, r: 3, fill: accent }));
  return s;
}

function logoNode(b) {
  if (!b.logo) return defaultMark(b.accent);
  const img = document.createElement("img");
  img.src = b.logo;
  img.alt = "";
  return img;
}

let brandKey = "";
function applyBrand(b) {
  const key = JSON.stringify(b);
  if (key === brandKey) return;
  brandKey = key;
  document.title = b.name;
  $("name").textContent = b.name;
  const root = document.documentElement.style;
  root.setProperty("--purple", b.accent);
  root.setProperty("--purple-2", mix(b.accent, "#ffffff", 0.2));
  root.setProperty("--purple-ink", mix(b.accent, "#000000", 0.25));
  root.setProperty("--lavender", mix(b.accent, "#ffffff", 0.9));
  root.setProperty("--lavender-2", mix(b.accent, "#ffffff", 0.8));
  root.setProperty("--lavender-3", mix(b.accent, "#ffffff", 0.62));
  document.querySelector("#ring-grad stop:last-child").setAttribute("stop-color", b.accent);
  $("logo").replaceChildren(logoNode(b));
  $("ob-logo").replaceChildren(logoNode(b));
  $("ob-name").textContent = b.name;

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
  if (!seenOnboarding()) openOnboarding();
}

function openOnboarding() {
  $("ob-addr").textContent = location.host;
  if (!$("ob-coins").children.length) $("ob-coins").append(...[1, 5, 10, 20].map((d) => PisoCoins.draw(d, 70)));
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
    renderPass();
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

// Phones decide whether this WiFi "has internet" when the sign-in page loads.
// Load it again after paying so the sign-in sheet closes and apps use WiFi.
function recheckSoon() {
  setTimeout(() => location.replace(location.pathname), 1800);
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
    // The saved pass goes along, so the coins join it instead of a new pass.
    const saved = storedPass();
    const r = await api("/coin/done", saved ? { pass: saved } : {});
    if (r.pass_unknown) unknownPass = saved;
    if (r.pass_secs !== undefined) autoClaim.done = true;
    if (before > 0) {
      confetti($("done"));
      if (r.pass_code) showPassSheet(r.pass_code);
      else {
        toast(r.pass_secs > 0 ? "You're online, with " + human(r.pass_secs) + " from your Piso Pass too." : "You're online. Enjoy!");
        recheckSoon();
      }
    }
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
    recheckSoon();
  }));

$("voucher-form").addEventListener("submit", (ev) => {
  ev.preventDefault();
  const btn = ev.submitter || $("voucher-form").querySelector("button");
  const code = $("code").value;
  action(btn, async () => {
    if (isPassCode(code)) {
      const r = await api("/pass/claim", { code, start: true });
      savePass(code);
      $("code").value = "";
      confetti(btn);
      toast(r.secs > 0 ? "Piso Pass added: " + human(r.secs) + "." : "Piso Pass linked to this phone.");
      if (r.secs > 0) recheckSoon();
      return;
    }
    const r = await api("/voucher", { code });
    $("code").value = "";
    confetti(btn);
    toast("Voucher added: " + human(r.secs) + ".");
    recheckSoon();
  });
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

function showPassSheet(code) {
  const nice = savePass(code);
  sheetCode = nice;
  $("ps-code").textContent = nice;
  $("ps-brand").textContent = state.brand.name;
  $("pass-sheet").hidden = false;
  $("ps-box").focus({ preventScroll: true });
  renderPass();
}
$("ps-done").addEventListener("click", () => {
  $("pass-sheet").hidden = true;
  toast("You're online. Enjoy!");
  if (state && state.running) recheckSoon();
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
    closeClaimSheet();
    const r = await api("/pass/claim", { code, start: true });
    savePass(code);
    autoClaim.done = true;
    toast(r.secs > 0 ? "Welcome back! " + human(r.secs) + " from your Piso Pass." : "Piso Pass linked to this phone.");
    if (r.secs > 0) {
      confetti($("meter"));
      recheckSoon();
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
  try {
    const r = await api("/pass/claim", { code });
    autoClaim.done = true;
    if (r.secs > 0) {
      confetti($("meter"));
      if (r.running) {
        toast("Welcome back! " + human(r.secs) + " from your Piso Pass.");
        recheckSoon();
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
