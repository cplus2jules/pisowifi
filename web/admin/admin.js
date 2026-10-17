// Piso WiFi owner dashboard. DOM is built with createElement/textContent
// only, never innerHTML, so device names or notes can't inject markup.
"use strict";

const $ = (id) => document.getElementById(id);
const SVG = "http://www.w3.org/2000/svg";
const AVATAR_COLORS = ["#5A43E6", "#9CB51F", "#3F6FD8", "#C2569B"];

let csrf = null;
let overview = null;
let pollTimer = null;
let currentTab = "overview";
let prevToday = null;      // today's earnings at the last poll, to spot new sales
const knownDevices = {};   // mac → name, model and kind, from every list we've loaded

const REDUCE = matchMedia("(prefers-reduced-motion: reduce)").matches;
const SPRING = CSS.supports("transition-timing-function", "linear(0, 1)")
  ? "linear(0, .008 1.1%, .031 2.2%, .129 4.8%, .257 7.2%, .671 14.2%, .789 16.8%, .881 19.4%, .954 22.2%, 1.006 25.2%, 1.04 28.4%, 1.058 31.9%, 1.062 35.6%, 1.055 39.8%, 1.035 45.4%, 1.012 52.4%, .998 61%, .993 71.7%, 1)"
  : "cubic-bezier(.2, .9, .25, 1.15)";

// ---- helpers --------------------------------------------------------------

function el(tag, props = {}, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(props)) {
    if (k === "class") e.className = v;
    else if (k === "text") e.textContent = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v);
  }
  for (const kid of kids) if (kid != null) e.append(kid);
  return e;
}

function svg(tag, attrs = {}, text) {
  const e = document.createElementNS(SVG, tag);
  for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, v);
  if (text != null) e.textContent = text;
  return e;
}

function icon(name, cls = "ic") {
  const s = svg("svg", { class: cls, "aria-hidden": "true" });
  s.append(svg("use", { href: "#i-" + name }));
  return s;
}

const peso = (n) => "₱" + Number(n || 0).toLocaleString("en-PH");

function clock(secs) {
  const h = Math.floor(secs / 3600), m = Math.floor((secs % 3600) / 60), s = secs % 60;
  return (h ? h + ":" + String(m).padStart(2, "0") : m) + ":" + String(s).padStart(2, "0");
}

function human(secs) {
  const d = Math.floor(secs / 86400), h = Math.floor((secs % 86400) / 3600), m = Math.round((secs % 3600) / 60);
  const out = [];
  if (d) out.push(d + "d");
  if (h) out.push(h + "h");
  if (m || !out.length) out.push(m + "m");
  return out.join(" ");
}

function when(ts) {
  const diff = Math.max(0, (overview ? overview.server_time : Date.now() / 1000) - ts);
  if (diff < 60) return "just now";
  if (diff < 3600) return Math.floor(diff / 60) + " min ago";
  if (diff < 86400) return Math.floor(diff / 3600) + " hr ago";
  return new Date(ts * 1000).toLocaleDateString("en-PH", { month: "short", day: "numeric" });
}

const shortMac = (mac) => "…" + mac.slice(-8);

let toastTimer;
function toast(msg, bad) {
  const t = $("toast");
  t.textContent = msg;
  t.classList.toggle("bad", !!bad);
  t.classList.remove("out");
  t.hidden = false;
  t.style.animation = "none";
  void t.offsetWidth;
  t.style.animation = "";
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    t.classList.add("out");
    toastTimer = setTimeout(() => (t.hidden = true), 220);
  }, 3500);
}

// ---- motion ---------------------------------------------------------------

// Numbers glide to their new value instead of jumping.
function tween(node, to, fmt = (v) => v.toLocaleString("en-PH"), ms = 750) {
  const from = node.dataset.v != null ? Number(node.dataset.v) : 0;
  node.dataset.v = to;
  cancelAnimationFrame(node._raf);
  if (REDUCE || from === to) { node.textContent = fmt(to); return; }
  const t0 = performance.now();
  const step = (now) => {
    const k = Math.min(1, (now - t0) / ms);
    node.textContent = fmt(Math.round(from + (to - from) * (1 - Math.pow(1 - k, 3))));
    if (k < 1) node._raf = requestAnimationFrame(step);
  };
  node._raf = requestAnimationFrame(step);
}

// Remaining time that counts down every second between polls.
function setLeft(node, secs, running, suffix = "", prefix = "") {
  Object.assign(node.dataset, { left: secs, at: performance.now(), run: running ? "1" : "0", suffix, prefix });
  node.textContent = prefix + clock(secs) + suffix;
}

setInterval(() => {
  const now = performance.now();
  for (const n of document.querySelectorAll('[data-run="1"]')) {
    const left = Math.max(0, Number(n.dataset.left) - Math.floor((now - Number(n.dataset.at)) / 1000));
    n.textContent = n.dataset.prefix + clock(left) + n.dataset.suffix;
  }
}, 1000);

// Rebuilds a list only when what it shows has changed.
function keyed(container, key, build) {
  if (container._key === key) return;
  container._key = key;
  container.replaceChildren(...build());
}

// One element per item, updated in place: polling doesn't reset hover
// states or swallow clicks, newcomers pop in and moves glide (FLIP).
function reconcile(container, items, keyOf, build, update, fadeOut) {
  const old = new Map();
  for (const n of container.children) if (n.dataset.key) old.set(n.dataset.key, n);
  const next = items.map((it) => {
    const k = keyOf(it);
    let n = old.get(k);
    if (n) old.delete(k);
    else { n = build(it); n.dataset.key = k; }
    update(n, it);
    return n;
  });
  for (const n of old.values()) {
    n.dataset.key = "";
    if (fadeOut && !REDUCE) { n.classList.add("leaving"); setTimeout(() => n.remove(), 300); }
    else n.remove();
  }
  const current = [...container.children].filter((n) => n.dataset.key);
  if (current.length === next.length && next.every((n, i) => current[i] === n)) return;
  const before = new Map(current.map((n) => [n, n.getBoundingClientRect()]));
  for (const n of next) container.append(n);
  if (REDUCE) return;
  for (const n of next) {
    const a = before.get(n);
    if (!a) continue;
    const b = n.getBoundingClientRect();
    if (a.left !== b.left || a.top !== b.top) {
      n.animate([{ transform: `translate(${a.left - b.left}px, ${a.top - b.top}px)` }, { transform: "none" }], { duration: 550, easing: SPRING });
    }
  }
}

// Little bursts of joy, drawn in a layer above the page.
function confetti(x, y, count = 70) {
  if (REDUCE) return;
  const colors = ["#5A43E6", "#D4F04C", "#B5E1EE", "#F4B6DA", "#7B5FF2", "#9CB51F"];
  for (let i = 0; i < count; i++) {
    const p = el("i", { class: "cf" });
    p.style.left = x + "px";
    p.style.top = y + "px";
    p.style.background = colors[i % colors.length];
    $("fx").append(p);
    const a = Math.random() * Math.PI * 2, v = 90 + Math.random() * 220;
    const dx = Math.cos(a) * v, dy = Math.sin(a) * v - 160;
    p.animate([
      { transform: "translate(-50%, -50%) rotate(0deg) scale(.6)", opacity: 1 },
      { transform: `translate(${dx - 4}px, ${dy - 6}px) rotate(${Math.random() * 540}deg) scale(1)`, opacity: 1, offset: 0.5 },
      { transform: `translate(${dx * 1.2}px, ${dy + 300}px) rotate(${Math.random() * 900}deg) scale(.9)`, opacity: 0 },
    ], { duration: 1500 + Math.random() * 800, easing: "cubic-bezier(.2, .7, .4, 1)" }).onfinish = () => p.remove();
  }
}

function floatUp(x, y, node) {
  $("fx").append(node);
  node.style.left = x + "px";
  node.style.top = y + "px";
  node.animate([
    { transform: "translate(-50%, -30%) scale(.6)", opacity: 0 },
    { transform: "translate(-50%, -90%) scale(1.05)", opacity: 1, offset: 0.25 },
    { transform: "translate(-50%, -250%) scale(1)", opacity: 0 },
  ], { duration: 1700, easing: "cubic-bezier(.22, 1, .36, 1)" }).onfinish = () => node.remove();
}

// A sale: the stat glows, a "+₱5" floats up and its coins drop into the jar.
function celebrateSale(delta) {
  const row = $("today").closest(".srow");
  row.classList.remove("bump");
  void row.offsetWidth;
  row.classList.add("bump");
  if (REDUCE || currentTab !== "overview") return;
  const r = row.getBoundingClientRect();
  floatUp(r.left + r.width * 0.62, r.top + r.height / 2, el("span", { class: "float-amt", text: "+" + peso(delta) }));
  const jar = $("jar").getBoundingClientRect();
  PisoCoins.split(Math.min(delta, 60)).slice(0, 4).forEach((d, i) => {
    const c = PisoCoins.draw(d, 30);
    $("fx").append(c);
    c.style.left = jar.left + 30 + i * 8 + "px";
    c.style.top = jar.top - 40 + "px";
    c.animate([
      { transform: "translateY(-70px) rotate(-30deg)", opacity: 0 },
      { transform: "translateY(-20px) rotate(0deg)", opacity: 1, offset: 0.4 },
      { transform: "translateY(36px) rotate(20deg) scale(.8)", opacity: 0 },
    ], { duration: 900, delay: i * 110, easing: "cubic-bezier(.5, 0, .75, .4)", fill: "backwards" }).onfinish = () => c.remove();
  });
}

// ---- devices: what to call them ----------------------------------------------

const KIND_ICON = { phone: "devices", tablet: "tablet", computer: "laptop", tv: "tv", unknown: "devices" };
const AV_TONES = [["#EEEBFD", "#3E2DB5"], ["#F1FAD0", "#4E5E0A"], ["#DDF0F6", "#1D4E63"], ["#FBE3F0", "#8E2F6A"], ["#FDEBDD", "#8A4B12"], ["#E2F5EC", "#17694A"]];

// Name, model and kind of a device (the server sends them with each list).
function deviceOf(mac) {
  if (!mac) return { name: "Unknown device", detail: "", kind: "unknown" };
  return (overview && overview.devices && overview.devices[mac]) || knownDevices[mac] ||
    { name: "Device …" + mac.replace(/:/g, "").slice(-6).toUpperCase(), detail: "", kind: "unknown" };
}

// A soft tile with the device's shape: phone, tablet, laptop or TV.
function avatar(mac, d, size = "", live = false) {
  let h = 0;
  for (const ch of mac) h = (h * 31 + ch.charCodeAt(0)) >>> 0;
  const [bg, ink] = AV_TONES[h % AV_TONES.length];
  const a = el("span", { class: "av-dev" + (size ? " " + size : ""), "aria-hidden": "true" }, icon(KIND_ICON[d.kind] || "devices"));
  a.style.setProperty("--bg-a", bg);
  a.style.setProperty("--ink-a", ink);
  if (live) {
    const w = svg("svg", { viewBox: "0 0 10 10" });
    w.append(svg("path", { d: "M3.3 6.4a2.4 2.4 0 0 1 3.4 0" }), svg("path", { d: "M1.6 4.6a4.8 4.8 0 0 1 6.8 0" }));
    a.append(el("span", { class: "live", title: "Online" }, w));
  }
  return a;
}

async function api(method, path, body) {
  const headers = { "x-pisowifi": "1" };
  if (csrf) headers["x-csrf-token"] = csrf;
  const opts = { method, headers, cache: "no-store", credentials: "same-origin" };
  if (body !== undefined) {
    headers["content-type"] = "application/json";
    opts.body = JSON.stringify(body);
  }
  const res = await fetch("/api" + path, opts);
  let data = {};
  try { data = await res.json(); } catch (_) { /* empty */ }
  if (res.status === 401 && path !== "/login") {
    showLogin();
    throw new Error(data.error || "Please sign in");
  }
  if (!res.ok) throw new Error(data.error || "Request failed (" + res.status + ")");
  return data;
}

async function busy(btn, fn) {
  if (btn) btn.disabled = true;
  try { return await fn(); }
  catch (e) { toast(e.message, true); }
  finally { if (btn) btn.disabled = false; }
}

// ---- auth -----------------------------------------------------------------

function showLogin(note) {
  clearTimeout(pollTimer);
  clearTimeout(updTimer);
  // Signed out because the vendo restarted into an update.
  if (!note && updRestarting) note = "The vendo restarted to install the update. Sign in again to see how it went.";
  updRestarting = false;
  csrf = null;
  $("shell").hidden = true;
  $("login").hidden = false;
  if (note) $("login-note").textContent = note;
  $("password").focus();
}

$("pw-toggle").addEventListener("click", () => {
  const input = $("password"), btn = $("pw-toggle");
  const show = input.type === "password";
  input.type = show ? "text" : "password";
  btn.setAttribute("aria-pressed", String(show));
  btn.setAttribute("aria-label", show ? "Hide password" : "Show password");
  btn.querySelector("use").setAttribute("href", show ? "#i-eye-off" : "#i-eye");
  input.focus();
});

$("login-form").addEventListener("submit", (ev) => {
  ev.preventDefault();
  // ev.submitter is missing before iOS 15.4.
  const btn = ev.submitter || $("login-form").querySelector("button[type=submit]");
  const err = $("login-error");
  err.hidden = true;
  btn.disabled = true;
  btn.textContent = "Signing in…";
  api("POST", "/login", { password: $("password").value })
    .then((r) => {
      csrf = r.csrf;
      $("password").value = "";
      $("password").type = "password";
      showApp();
    })
    .catch((e) => {
      err.textContent = e.message;
      err.hidden = false;
    })
    .finally(() => {
      btn.disabled = false;
      btn.textContent = "Sign in";
    });
});

$("logout").addEventListener("click", async () => {
  try { await api("POST", "/logout", {}); } catch (_) { /* already out */ }
  showLogin("You're signed out.");
});

// ---- navigation -----------------------------------------------------------

const TABS = ["overview", "sales", "devices", "rates", "vouchers", "customize", "activity", "settings"];

function go(tab) {
  if (!TABS.includes(tab)) tab = "overview";
  const changed = tab !== currentTab;
  currentTab = tab;
  for (const t of TABS) $("tab-" + t).hidden = t !== tab;
  for (const b of document.querySelectorAll(".nav [data-tab]")) {
    if (b.dataset.tab === tab) b.setAttribute("aria-current", "page");
    else b.removeAttribute("aria-current");
  }
  placeNavPill();
  if (location.hash !== "#" + tab) history.replaceState(null, "", "#" + tab);
  if (tab === "sales") loadReport(salesDays, true);
  if (tab === "devices") loadPeople();
  if (tab === "rates") loadRates();
  if (tab === "vouchers") loadVouchers();
  if (tab === "activity") loadAudit();
  if (tab === "customize" && overview) loadBrand(overview.brand);
  if (tab === "settings") loadUpdate();
  else clearTimeout(updTimer);
  if (overview) renderOverview();
  if (changed || !enteredOnce) enterPanel($("tab-" + tab));
  enteredOnce = true;
  window.scrollTo({ top: 0 });
}

// Sections rise in card by card.
let enteredOnce = false;
function enterPanel(panel) {
  if (REDUCE) return;
  const groups = ".grid-top,.grid-mid,.grid-bottom,.two,.kpis,.custom-grid";
  const items = panel.querySelectorAll(`:scope > :not(${groups}), :scope > :is(${groups}) > *`);
  items.forEach((n, i) => {
    n.classList.add("rise");
    n.style.setProperty("--i", Math.min(i, 12));
  });
  panel.classList.remove("enter");
  void panel.offsetWidth;
  panel.classList.add("enter");
  clearTimeout(panel._enter);
  panel._enter = setTimeout(() => panel.classList.remove("enter"), 1300);
}

// The highlight slides to the current section (a bar under it on phones).
function placeNavPill() {
  const nav = document.querySelector(".nav"), pill = $("nav-pill");
  const cur = nav.querySelector('[aria-current="page"]');
  if (!cur || !cur.offsetWidth) return;
  const first = !nav.classList.contains("has-pill");
  if (first) pill.style.transition = "none";
  pill.style.setProperty("--y", cur.offsetTop + "px");
  pill.style.setProperty("--h", cur.offsetHeight + "px");
  pill.style.setProperty("--x", cur.offsetLeft + "px");
  pill.style.setProperty("--w", cur.offsetWidth + "px");
  nav.classList.add("has-pill");
  if (first) { void pill.offsetWidth; pill.style.transition = ""; }
  if (nav.scrollWidth > nav.clientWidth) cur.scrollIntoView({ block: "nearest", inline: "nearest", behavior: REDUCE ? "auto" : "smooth" });
}

function placeSegPill() {
  const seg = $("period"), cur = seg.querySelector('[aria-selected="true"]'), pill = seg.querySelector(".seg-pill");
  if (!cur || !cur.offsetWidth) return;
  pill.style.setProperty("--x", cur.offsetLeft - 3 + "px");
  pill.style.setProperty("--w", cur.offsetWidth + "px");
}

window.addEventListener("resize", () => { placeNavPill(); placeSegPill(); });

document.addEventListener("click", (ev) => {
  const t = ev.target.closest("[data-tab],[data-goto]");
  if (!t) return;
  go(t.dataset.tab || t.dataset.goto);
  const target = t.dataset.scroll && $(t.dataset.scroll);
  if (target) setTimeout(() => target.scrollIntoView({ behavior: REDUCE ? "auto" : "smooth", block: "start" }), 450);
});

document.addEventListener("keydown", (ev) => {
  if (ev.key !== "Escape") return;
  if (!$("box-modal").hidden) $("box-modal").hidden = true;
  else closeDrawer();
});

window.addEventListener("hashchange", () => {
  const tab = location.hash.slice(1);
  if (csrf && tab !== currentTab) go(tab);
});

// ---- overview -------------------------------------------------------------

async function poll() {
  clearTimeout(pollTimer);
  try {
    overview = await api("GET", "/overview");
    renderOverview();
    // Opened straight onto the Customer page: fill its form now we know the brand.
    if (currentTab === "customize" && !brandDraft) loadBrand(overview.brand);
    backfillHomeIcon(overview.brand);
    if (currentTab === "devices" && Date.now() - peopleAt > 20000) loadPeople();
    if (drawerMac && Date.now() - drawerAt > 10000) loadDrawer();
  } catch (_) { /* toast-free polling */ }
  pollTimer = setTimeout(poll, overview && overview.slot.state !== "idle" ? 1000 : 4000);
}

function renderOverview() {
  const o = overview, s = o.sales;
  Object.assign(knownDevices, o.devices || {});
  document.title = o.name + " · Owner dashboard";
  $("site-name").textContent = o.name;
  $("page-title").textContent = o.name;
  $("site-tag").textContent = o.dev ? "Dev" : "Live";
  $("version").textContent = "version " + o.version;
  $("open-portal").href = o.portal_url;
  $("fp").textContent = o.fingerprint || "Dev mode (plain HTTP)";

  tween($("online"), o.active);
  tween($("today"), s.today_pesos, peso);
  tween($("week"), s.week_pesos, peso);
  tween($("month"), s.month_pesos, peso);
  tween($("alltime"), s.all_time_pesos, peso);
  $("nav-count").textContent = o.active || "";

  // A sale landed since the last look: make a little fuss.
  if (prevToday != null && s.today_pesos > prevToday) celebrateSale(s.today_pesos - prevToday);

  renderBanner(o);
  renderHub(o.hub);
  renderChart(s.days);
  renderTop(s.top_customers);
  renderActivity(o);
  renderGoal(o);
  renderCoinbox(o.coinbox);
  renderHealth(o.health);
  renderSlot(o.slot, o.dev);
  renderMiniDevices(o.clients);
  if (currentTab === "devices") renderDevices(o.clients, o.lan_devices || []);
  if (currentTab === "activity") renderRecent(s.recent);
  prevToday = s.today_pesos;
}

function renderHub(h) {
  const s = $("hub-status");
  $("hub-facts").hidden = !h;
  $("hub-help").hidden = !!h;
  if (!h) {
    s.textContent = "Not connected";
    s.className = "status used";
    return;
  }
  const fresh = h.last_sync && overview.server_time - h.last_sync < 90;
  s.textContent = h.online && fresh ? "Connected" : h.last_sync ? "Reconnecting" : "Connecting";
  s.className = "status " + (h.online && fresh ? "on" : "off");
  $("hub-url").textContent = h.url;
  $("hub-name").textContent = h.name + " (" + h.vendo_id + ")";
  $("hub-sync").textContent = h.last_sync ? when(h.last_sync) : "not yet";
  $("hub-passes").textContent = h.passes_here;
  $("hub-err").textContent = h.last_error || "";
  $("hub-err").hidden = $("hub-err-label").hidden = !h.last_error;
}

let bannerDismissed = "";
function renderBanner(o) {
  let text = "", action = "", target = "";
  if (o.sales.unclaimed_pesos > 0) {
    text = peso(o.sales.unclaimed_pesos) + " in coins arrived while nobody had tapped Insert coin. Wiring the acceptor gate stops this.";
    action = "Review"; target = "activity";
  } else if (o.dev) {
    text = "Dev mode: the firewall and coin slot are simulated. Use the test coin buttons below.";
    action = "Open portal"; target = "portal";
  }
  const key = text;
  $("banner").hidden = !text || bannerDismissed === key;
  $("banner-text").textContent = text;
  const btn = $("banner-action");
  btn.textContent = action;
  btn.onclick = () => (target === "portal" ? window.open(o.portal_url, "_blank", "noopener") : go(target));
  $("banner-close").onclick = () => { bannerDismissed = key; $("banner").hidden = true; };
}

// ---- bar charts -------------------------------------------------------------

const shortDate = (iso, opts) => new Date(iso + "T00:00:00").toLocaleDateString("en-PH", opts || { month: "short", day: "numeric" });

// Bars that grow in on a spring, with a tooltip that follows the pointer.
function barChart(box, items, o) {
  const W = Math.max(280, box.clientWidth || 560), H = o.height || 230, top = 22, bottom = 26, left = 4;
  const max = Math.max(10, ...items.map((d) => d.pesos));
  const n = items.length, slot = (W - left) / n, bw = Math.max(2, Math.min(28, slot * 0.62));
  const best = items.reduce((b, d, i) => (d.pesos > (items[b] ? items[b].pesos : -1) ? i : b), -1);
  const plotH = H - top - bottom;
  const root = svg("svg", { viewBox: `0 0 ${W} ${H}`, width: W, height: H });
  for (let i = 0; i <= 3; i++) {
    const y = top + (plotH * i) / 3;
    root.append(svg("line", { x1: 0, x2: W, y1: y, y2: y, class: "grid" }));
  }
  const bars = items.map((d, i) => {
    const h = (plotH * d.pesos) / max;
    const x = left + i * slot + (slot - bw) / 2;
    const y = H - bottom - Math.max(h, 3);
    const isLast = o.today && i === n - 1;
    const cls = d.pesos > 0 ? "bar" + (isLast ? " today" : i === best ? " best" : "") : "bar zero";
    const r = svg("rect", { x, y, width: bw, height: Math.max(h, 3), rx: Math.min(4, bw / 2), class: cls });
    r.style.setProperty("--i", i);
    root.append(r);
    if (o.values && slot >= 34 && d.pesos > 0) root.append(svg("text", { x: x + bw / 2, y: y - 6, "text-anchor": "middle", class: "val" }, peso(d.pesos)));
    const label = o.label(d, i, slot);
    if (label) root.append(svg("text", { x: x + bw / 2, y: H - 8, "text-anchor": "middle", class: "axis" + (isLast ? " now" : "") }, label));
    return { r, x: x + bw / 2, y };
  });
  if (o.avg) {
    const avg = items.reduce((a, d) => a + d.pesos, 0) / Math.max(1, n);
    if (avg > 0) {
      const y = H - bottom - (plotH * avg) / max;
      root.append(svg("line", { x1: 0, x2: W, y1: y, y2: y, class: "avg" }),
        svg("text", { x: W - 2, y: y - 5, "text-anchor": "end", class: "avg-label" }, "avg " + peso(Math.round(avg))));
    }
  }
  const tip = el("div", { class: "tip", "aria-hidden": "true" });
  let lit = null;
  root.addEventListener("pointermove", (ev) => {
    const px = ev.clientX - root.getBoundingClientRect().left;
    const i = Math.max(0, Math.min(n - 1, Math.floor((px - left) / slot)));
    const b = bars[i];
    if (lit !== b.r) {
      if (lit) lit.classList.remove("hl");
      lit = b.r;
      lit.classList.add("hl");
      const [title, line] = o.tip(items[i]);
      tip.replaceChildren(el("b", { text: title }), el("br"), line);
    }
    tip.style.left = Math.max(60, Math.min(W - 60, b.x)) + "px";
    tip.style.top = b.y + "px";
    tip.classList.add("on");
  });
  root.addEventListener("pointerleave", () => {
    tip.classList.remove("on");
    if (lit) lit.classList.remove("hl");
    lit = null;
  });
  box.replaceChildren(root, tip);
  box.classList.toggle("anim", !!o.animate && !REDUCE);
}

const salesLine = (d) => d.count + (d.count === 1 ? " sale" : " sales");

let chartDays = null, chartGrown = false;
function renderChart(days) {
  chartDays = days;
  barChart($("chart"), days, {
    today: true, values: true, animate: !chartGrown,
    label: (d, i, slot) => {
      const every = slot < 24 ? 3 : slot < 34 ? 2 : 1;
      if ((days.length - 1 - i) % every) return null;
      return i === days.length - 1 && slot >= 36 ? "Today" : String(new Date(d.date + "T00:00:00").getDate());
    },
    tip: (d) => [peso(d.pesos), shortDate(d.date, { weekday: "short", month: "short", day: "numeric" }) + " · " + salesLine(d)],
  });
  chartGrown = true;
  $("chart").setAttribute("aria-label", "Daily earnings: " + days.map((d) => d.date + " " + peso(d.pesos)).join(", "));
}

new ResizeObserver(() => chartDays && $("chart").clientWidth && renderChart(chartDays)).observe($("chart"));

function renderTop(list) {
  keyed($("top"), JSON.stringify(list.map((c) => [c.mac, c.pesos, deviceOf(c.mac).name])), () => {
    $("top-empty").hidden = list.length > 0;
    const max = Math.max(1, ...list.map((c) => c.pesos));
    return list.map((c, i) => {
      const color = AVATAR_COLORS[i % AVATAR_COLORS.length];
      const fill = el("i");
      fill.style.width = (100 * c.pesos) / max + "%";
      fill.style.background = color;
      const d = deviceOf(c.mac);
      return el("li", {},
        avatar(c.mac, d, "sm"),
        el("button", { class: "who link-plain", type: "button", text: d.name, title: d.detail || c.mac, onclick: () => openDevice(c.mac) }),
        el("span", { class: "bar-line" }, el("span", { class: "track" }, fill), el("span", { class: "amt", text: peso(c.pesos) })));
    });
  });
}

function renderActivity(o) {
  const s = o.sales;
  const items = (s.unclaimed_pesos > 0 ? 1 : 0);
  const row = $("action-row");
  row.classList.toggle("clear", items === 0);
  $("action-count").textContent = items || "";
  $("action-text").textContent = items ? (items === 1 ? "ACTION ITEM" : "ACTION ITEMS") : "ALL CLEAR";
  row.onclick = () => go(items ? "activity" : "devices");

  const trend = $("m-trend");
  trend.replaceChildren();
  trend.className = "";
  if (s.prev_month_pesos > 0) {
    const pct = Math.round(((s.month_pesos - s.prev_month_pesos) / s.prev_month_pesos) * 100);
    trend.className = pct >= 0 ? "up" : "down";
    trend.append((pct >= 0 ? "+" : "") + pct + "%", icon(pct >= 0 ? "up" : "down"));
  } else {
    trend.textContent = s.month_pesos > 0 ? "New" : "—";
  }
  tween($("m-sales"), s.month_count);
  const avg = s.month_pesos / 30;
  $("m-avg").textContent = peso(avg < 10 ? Math.round(avg * 10) / 10 : Math.round(avg));
  tween($("m-vouchers"), s.vouchers_used_30d);
  $("m-unclaimed").textContent = peso(s.unclaimed_pesos);
  $("m-uptime").textContent = human(o.uptime_secs);
}

function renderSlot(slot, dev) {
  const inserting = slot.state !== "idle";
  $("slot-chip").classList.toggle("busy", inserting);
  $("slot-chip-text").textContent = inserting ? peso(slot.pesos) + " in" : "Idle";
  $("slot-title").textContent = inserting ? deviceOf(slot.mac).name + " is inserting" : "Waiting for a customer";
  $("slot-sub").textContent = !slot.reader_ok
    ? "The coin slot can't be read, so customers can't insert coins. Check the [coin] settings and run: journalctl -u pisowifi"
    : inserting
      ? "Coins go to this device when they tap Done or the window ends."
      : "Customers tap Insert coin on the portal first.";
  if (!slot.reader_ok) $("slot-title").textContent = "Coin slot offline";
  const pct = inserting ? (100 * slot.secs_left) / slot.window_secs : 0;
  $("slot-fill").style.width = pct + "%";
  $("slot-marker").style.left = Math.min(pct, 82) + "%";
  $("slot-pesos").textContent = peso(slot.pesos);
  $("slot-secs").textContent = inserting ? slot.secs_left + "s" : slot.window_secs + "s";
  $("slot-secs-label").textContent = inserting ? "left" : "window";
  $("slot-pulses").textContent = slot.pulses_total;
  $("slot-gate").textContent = slot.gated ? "wired" : "not wired";
  $("test-coins").hidden = !dev;
}

function renderMiniDevices(clients) {
  const list = clients.slice(0, 5);
  $("mini-empty").hidden = list.length > 0;
  $("online-pill").textContent = overview.active + " online";
  const max = Math.max(1, ...list.map((c) => c.remaining_secs));
  reconcile($("mini-devices"), list, (c) => c.mac, (c) => {
    const r = {};
    const li = el("li", {}, r.av = el("span"),
      el("button", { class: "top-line link-plain", type: "button", onclick: () => openDevice(c.mac) },
        r.name = el("span"), r.time = el("span")),
      el("div", { class: "track" }, r.fill = el("div", { class: "fill" })));
    li._r = r;
    return li;
  }, (li, c) => {
    const r = li._r, d = deviceOf(c.mac);
    const avKey = d.kind + c.running;
    if (r.avKey !== avKey) { const a = avatar(c.mac, d, "sm", c.running); r.av.replaceWith(a); r.av = a; r.avKey = avKey; }
    li.className = c.running ? "" : "paused";
    r.name.textContent = d.name;
    r.name.title = d.detail || c.mac;
    setLeft(r.time, c.remaining_secs, c.running, c.running ? " left" : "", c.running ? "" : "Paused · ");
    r.fill.style.width = (100 * c.remaining_secs) / max + "%";
  });
}

for (const b of document.querySelectorAll("[data-coin]")) {
  b.prepend(PisoCoins.draw(Number(b.dataset.coin), 30));
  b.addEventListener("click", () =>
    busy(b, async () => {
      await api("POST", "/test-coin", { pesos: Number(b.dataset.coin) });
      poll();
    }));
}

// ---- devices --------------------------------------------------------------

function addTimeButton(mac, mins, label, cls = "btn small outline") {
  return el("button", {
    class: cls, type: "button", text: label,
    onclick: (ev) => giveTime(mac, mins, ev.currentTarget),
  });
}

function giveTime(mac, mins, btn) {
  return busy(btn, async () => {
    const r = await api("POST", "/clients/" + encodeURIComponent(mac) + "/time", { minutes: mins });
    toast("Added " + human(mins * 60) + " to " + deviceOf(mac).name + (r.paused ? " (paused: it starts when they resume)" : ""));
    await poll();
    if (drawerMac === mac) loadDrawer();
  });
}

function togglePause(mac, running, btn) {
  return busy(btn, async () => {
    await api("POST", "/clients/" + encodeURIComponent(mac) + (running ? "/pause" : "/resume"), {});
    toast((running ? "Paused " : "Resumed ") + deviceOf(mac).name);
    await poll();
    if (drawerMac === mac) loadDrawer();
  });
}

function renderDevices(clients, others) {
  $("devices-empty").hidden = clients.length > 0;
  $("devices-sub").textContent = overview.active + " online · " + (clients.length - overview.active) + " paused";
  const max = Math.max(1, ...clients.map((c) => c.remaining_secs));
  reconcile($("devices"), clients, (c) => c.mac, (c) => {
    const r = {};
    const card = el("div", { class: "dev-card arrive" });
    r.av = el("span");
    r.toggle = el("button", { class: "btn small soft", type: "button", onclick: (ev) => togglePause(c.mac, r.running, ev.currentTarget) });
    card.append(
      el("div", { class: "head" },
        el("button", { class: "open", type: "button", onclick: () => openDevice(c.mac) },
          r.av, el("span", { class: "who" }, r.name = el("b"), r.detail = el("small"))),
        r.status = el("span", { class: "status" })),
      el("div", { class: "time-line" }, r.time = el("b"), r.label = el("span")),
      el("div", { class: "track" }, r.fill = el("div", { class: "fill" })),
      el("div", { class: "actions" }, addTimeButton(c.mac, 30, "+30 min"), addTimeButton(c.mac, 60, "+1 hr"), r.toggle));
    card._r = r;
    return card;
  }, (card, c) => {
    const r = card._r, d = deviceOf(c.mac);
    const avKey = d.kind + c.running;
    if (r.avKey !== avKey) { const a = avatar(c.mac, d, "", c.running); r.av.replaceWith(a); r.av = a; r.avKey = avKey; }
    card.classList.toggle("paused", !c.running);
    r.name.textContent = d.name;
    r.detail.textContent = d.detail || shortMac(c.mac);
    r.status.className = "status " + (c.running ? "on" : "off");
    r.status.textContent = c.running ? "Online" : "Paused";
    setLeft(r.time, c.remaining_secs, c.running);
    r.label.textContent = c.running ? "left" : "saved for later";
    r.fill.style.width = (100 * c.remaining_secs) / max + "%";
    if (r.running !== c.running) {
      r.running = c.running;
      r.toggle.replaceChildren(icon(c.running ? "pause" : "play"), c.running ? "Pause" : "Resume");
    }
  }, true);

  $("lan-card").hidden = others.length === 0;
  keyed($("lan-devices"), JSON.stringify(others.map((d) => [d.mac, d.ip, deviceOf(d.mac).name, !!deviceOf(d.mac).blocked])), () => others.map((o) => {
    const d = deviceOf(o.mac);
    return el("li", { class: "dev-row" + (d.blocked ? " is-blocked" : "") },
      el("button", { class: "open", type: "button", onclick: () => openDevice(o.mac) },
        avatar(o.mac, d), el("span", { class: "who" }, el("b", { text: d.name }), el("small", { text: [d.detail, o.ip].filter(Boolean).join(" · ") }))),
      d.blocked
        ? el("span", { class: "status bad", text: "Blocked" })
        : el("span", { class: "actions" }, addTimeButton(o.mac, 30, "+30 min"), addTimeButton(o.mac, 60, "+1 hr")));
  }));
}

// Everyone who has opened the page or bought time, with what they've spent.
let people = [], peopleAt = 0;
async function loadPeople() {
  try {
    const r = await api("GET", "/devices");
    people = r.devices;
    peopleAt = Date.now();
    for (const p of people) knownDevices[p.mac] = p;
    renderPeople();
  } catch (e) { toast(e.message, true); }
}

let blockedOnly = false;
function renderPeople() {
  const q = $("people-q").value.trim().toLowerCase();
  const blockedCount = people.filter((p) => p.blocked).length;
  if (!blockedCount) blockedOnly = false;
  $("people-blocked").hidden = !blockedCount;
  $("people-blocked").setAttribute("aria-pressed", String(blockedOnly));
  $("blocked-count").textContent = blockedCount;
  const list = people.filter((p) => (!blockedOnly || p.blocked) &&
    (!q || [p.name, p.detail, p.mac, p.nickname, p.hostname].some((v) => v && v.toLowerCase().includes(q))));
  $("people-sub").textContent = people.length
    ? people.length + (people.length === 1 ? " device" : " devices") + " · " + people.filter((p) => p.spend.count).length + " have bought time"
    : "Everyone who has opened your page or bought time";
  $("people-empty").hidden = list.length > 0;
  $("people-empty").textContent = !people.length ? "Nobody yet. Devices show up here the first time they open your page."
    : q ? "No one matches “" + q + "”." : "No blocked devices.";
  keyed($("people"), JSON.stringify([q, blockedOnly, list.map((p) => [p.mac, p.name, p.detail, p.spend.pesos, p.running, p.has_time, p.last_seen, p.blocked])]), () =>
    list.slice(0, 200).map((p, i) => {
      const seen = Math.max(p.last_seen || 0, p.spend.last || 0);
      const li = el("li", { class: "dev-row" + (p.blocked ? " is-blocked" : "") },
        el("button", { class: "open", type: "button", onclick: () => openDevice(p.mac) },
          avatar(p.mac, p, "", p.running),
          el("span", { class: "who" }, el("b", { text: p.name }), el("small", { text: p.detail || shortMac(p.mac) }))),
        el("span", { class: "nums" },
          el("span", {}, el("b", { text: peso(p.spend.pesos) }), "spent"),
          el("span", {}, el("b", { text: String(p.spend.count) }), p.spend.count === 1 ? "purchase" : "purchases"),
          el("span", {}, el("b", { text: p.has_time ? (p.running ? "Online" : "Paused") : seen ? when(seen) : "—" }), p.has_time ? "now" : "last seen")),
        p.blocked ? el("span", { class: "status bad", text: "Blocked" }) : null);
      li.style.setProperty("--i", Math.min(i, 20));
      return li;
    }));
}

$("people-q").addEventListener("input", renderPeople);
$("people-blocked").addEventListener("click", () => {
  blockedOnly = !blockedOnly;
  renderPeople();
});

// ---- device drawer --------------------------------------------------------

let drawerMac = null, drawerOpener = null, drawerAt = 0;

function openDevice(mac) {
  const first = drawerMac === null;
  drawerMac = mac;
  if (first) drawerOpener = document.activeElement;
  fillDrawerHead(mac, deviceOf(mac));
  $("dv-time").hidden = $("dv-give").hidden = true;
  $("dv-sales").replaceChildren();
  $("dv-facts").replaceChildren();
  $("dv-spark").replaceChildren();
  for (const id of ["drawer", "scrim"]) { $(id).classList.remove("out"); $(id).hidden = false; }
  $("dv-close").focus();
  loadDrawer();
}

function fillDrawerHead(mac, d) {
  $("dv-av").replaceChildren(avatar(mac, d, "lg"));
  $("dv-name").textContent = d.name;
  $("dv-detail").textContent = d.detail || "We don't know the model yet: it shows once they open your page";
}

function closeDrawer() {
  if (drawerMac === null) return;
  drawerMac = null;
  for (const id of ["drawer", "scrim"]) $(id).classList.add("out");
  setTimeout(() => { if (drawerMac === null) $("drawer").hidden = $("scrim").hidden = true; }, REDUCE ? 0 : 240);
  if (drawerOpener && drawerOpener.isConnected) drawerOpener.focus();
}

$("dv-close").addEventListener("click", closeDrawer);
$("scrim").addEventListener("click", closeDrawer);

async function loadDrawer() {
  const mac = drawerMac;
  if (!mac) return;
  let r;
  drawerAt = Date.now();
  try { r = await api("GET", "/devices/" + encodeURIComponent(mac)); } catch (e) { toast(e.message, true); return; }
  if (drawerMac !== mac) return;
  const d = r.device;
  knownDevices[mac] = Object.assign(knownDevices[mac] || {}, d);
  fillDrawerHead(mac, d);
  const nick = $("dv-nick");
  if (document.activeElement !== nick) nick.value = d.nickname || "";
  nick.placeholder = d.nickname ? "" : "e.g. " + (d.name.startsWith("Device …") ? "Aling Nena" : d.name);

  const c = r.client;
  $("dv-blocked").hidden = !d.blocked;
  $("dv-block").hidden = !!d.blocked;
  $("dv-time").hidden = !c || d.blocked;
  $("dv-give").hidden = !!c || d.blocked;
  if (c) {
    $("dv-time").classList.toggle("paused", !c.running);
    $("dv-state").textContent = c.running ? "Online now" : "Paused";
    setLeft($("dv-left"), c.remaining_secs, c.running);
    const p = $("dv-pause");
    p.replaceChildren(icon(c.running ? "pause" : "play"), el("span", { text: c.running ? "Pause" : "Resume" }));
    p.onclick = () => togglePause(mac, c.running, p);
  }

  tween($("dv-spent"), r.spend.pesos, peso);
  tween($("dv-visits"), r.spend.count);
  $("dv-hours").textContent = hoursText(r.spend.secs);

  // The last 30 days, one bar each.
  const byDay = {};
  for (const s of r.sales) {
    const k = new Date(s.ts * 1000).toDateString();
    byDay[k] = (byDay[k] || 0) + (s.pesos || 0.5);
  }
  const vals = [];
  for (let i = 29; i >= 0; i--) vals.push(byDay[new Date(Date.now() - i * 86400000).toDateString()] || 0);
  const vmax = Math.max(1, ...vals);
  $("dv-spark").replaceChildren(...vals.map((v, i) => {
    const b = el("i", { class: v ? "on" : "" });
    b.style.height = Math.max(6, (100 * v) / vmax) + "%";
    b.style.setProperty("--i", i);
    return b;
  }));
  $("dv-spark").title = "Purchases over the last 30 days";

  const copy = el("button", { class: "copy", type: "button", title: "Copy address", "aria-label": "Copy address", onclick: () => copyText(mac, "Address copied") }, icon("copy"));
  const facts = [
    ["Address", [el("code", { text: mac }), copy]],
    ["", d.private_mac ? "A private address: this phone made it up for your WiFi, so it may change if they tell the phone to forget the network." : null],
    ["IP address", c ? c.ip : null],
    ["Model", d.model],
    ["System", d.os],
    ["Browser", d.browser],
    ["Its own name", d.hostname],
    ["First seen", d.first_seen ? new Date(d.first_seen * 1000).toLocaleDateString("en-PH", { year: "numeric", month: "short", day: "numeric" }) : null],
    ["Last seen", d.last_seen ? when(d.last_seen) : null],
  ];
  $("dv-facts").replaceChildren(...facts.filter(([, v]) => v).flatMap(([k, v]) => [
    el("dt", { text: k }), el("dd", { class: k ? "" : "sub" }, ...(Array.isArray(v) ? v : [v])),
  ]));

  $("dv-sales").replaceChildren(...(r.sales.length ? r.sales : [null]).map((s) => {
    if (!s) return el("li", { class: "empty", text: "No purchases yet." });
    return el("li", {},
      el("span", { class: "what", text: SOURCE[s.source] || s.source }, el("small", { text: human(s.secs) + " of internet" })),
      el("span", { class: "amt", text: s.pesos ? peso(s.pesos) : "Free" }),
      el("span", { class: "when", text: when(s.ts) }));
  }));
}

const hoursText = (secs) => (secs >= 36000 ? Math.round(secs / 3600) : Math.round(secs / 360) / 10) + " h";

for (const b of document.querySelectorAll("#drawer [data-add]")) {
  b.addEventListener("click", () => drawerMac && giveTime(drawerMac, Number(b.dataset.add), b));
}

$("dv-kick").addEventListener("click", (ev) => {
  const mac = drawerMac;
  if (!mac || !confirm("Disconnect " + deviceOf(mac).name + "? Their remaining time will be removed.")) return;
  busy(ev.currentTarget, async () => {
    await api("POST", "/clients/" + encodeURIComponent(mac) + "/kick", {});
    toast("Disconnected " + deviceOf(mac).name);
    await poll();
    loadDrawer();
  });
});

// A device changed (renamed, blocked): every list showing it follows.
function deviceChanged(mac, device) {
  knownDevices[mac] = Object.assign(knownDevices[mac] || {}, device);
  if (overview && overview.devices) overview.devices[mac] = device;
  const p = people.find((x) => x.mac === mac);
  if (p) Object.assign(p, device);
  fillDrawerHead(mac, device);
  if (overview) renderOverview();
  if (currentTab === "devices") renderPeople();
}

$("dv-rename").addEventListener("submit", (ev) => {
  ev.preventDefault();
  const mac = drawerMac;
  if (!mac) return;
  busy(ev.submitter, async () => {
    const r = await api("PUT", "/devices/" + encodeURIComponent(mac) + "/name", { name: $("dv-nick").value });
    deviceChanged(mac, r.device);
    toast(r.device.nickname ? "Saved as " + r.device.nickname : "Name removed");
  });
});

function setBlocked(blocked, btn) {
  const mac = drawerMac;
  if (!mac) return;
  const d = deviceOf(mac);
  const hasTime = overview && overview.clients.some((c) => c.mac === mac);
  if (blocked && !confirm("Block " + d.name + "? It won't be able to buy time, use vouchers or bring a Piso Pass here until you unblock it." +
    (hasTime ? " Its remaining time is removed." : ""))) return;
  busy(btn, async () => {
    const r = await api("PUT", "/devices/" + encodeURIComponent(mac) + "/blocked", { blocked });
    deviceChanged(mac, r.device);
    toast((blocked ? "Blocked " : "Unblocked ") + r.device.name);
    await poll();
    loadDrawer();
  });
}

$("dv-block-btn").addEventListener("click", (ev) => setBlocked(true, ev.currentTarget));
$("dv-unblock").addEventListener("click", (ev) => setBlocked(false, ev.currentTarget));

async function copyText(text, done) {
  try {
    await navigator.clipboard.writeText(text);
    toast(done);
  } catch (_) {
    toast("Copy isn't available here. Select it instead.", true);
  }
}

// ---- goal, coin box, health -----------------------------------------------

const GOAL_C = 314.16;
let goalCheered = "";
function renderGoal(o) {
  const goal = o.goal || 0, today = o.sales.today_pesos, days = o.sales.days;
  const reached = goal > 0 && today >= goal;
  $("goal-arc").style.strokeDashoffset = String(GOAL_C * (1 - (goal ? Math.min(1, today / goal) : 0)));
  tween($("goal-pct"), goal ? Math.round((100 * today) / goal) : 0, (v) => v + "%");
  $("goal-of").textContent = goal ? peso(today) + " of " + peso(goal) : peso(today) + " today";
  $("goal-card").classList.toggle("reached", reached);
  $("goal-sub").textContent = !goal ? "Set a target and watch it fill" : reached ? "Goal reached. Nice one!" : "Coins collected today";
  $("goal-left").textContent = !goal ? "Tap the pencil to set a daily goal." : reached ? (today > goal ? peso(today - goal) + " over the goal" : "Right on target") : peso(goal - today) + " to go";
  // Days in a row that reached the goal, counting today once it does.
  let streak = 0;
  if (goal) {
    for (let i = days.length - 2; i >= 0 && days[i].pesos >= goal; i--) streak++;
    if (reached) streak++;
  }
  $("streak").hidden = !goal;
  $("streak").classList.toggle("hot", streak >= 2);
  $("streak-days").textContent = streak;
  $("streak-label").textContent = streak === 1 ? "day streak" : "days in a row";
  const day = days.length ? days[days.length - 1].date : "";
  if (reached && prevToday != null && prevToday < goal && goalCheered !== day) {
    goalCheered = day;
    const r = $("goal-ring").getBoundingClientRect();
    confetti(r.left + r.width / 2, r.top + r.height / 2);
    toast("Daily goal reached: " + peso(today) + "!");
  }
}

$("goal-edit").addEventListener("click", () => {
  const f = $("goal-form");
  f.hidden = !f.hidden;
  if (!f.hidden) {
    $("goal-input").value = (overview && overview.goal) || 300;
    $("goal-input").select();
  }
});
$("goal-cancel").addEventListener("click", () => ($("goal-form").hidden = true));
$("goal-form").addEventListener("submit", (ev) => {
  ev.preventDefault();
  busy(ev.submitter, async () => {
    const r = await api("PUT", "/goal", { pesos: Number($("goal-input").value) });
    overview.goal = r.goal;
    $("goal-form").hidden = true;
    renderGoal(overview);
    toast(r.goal ? "Daily goal set to " + peso(r.goal) : "Daily goal turned off");
  });
});

let jarShown = -1;
function renderCoinbox(b) {
  tween($("box-pesos"), b.pesos, peso);
  $("box-since").textContent = (b.since ? "Emptied " + when(b.since) : "Never emptied") + " · " + b.count + (b.count === 1 ? " sale" : " sales");
  $("box-empty").disabled = b.pesos === 0;
  renderJar(b.pesos);
}

// Coins pile up in the jar (more pesos, more coins, up to a full jar).
const JAR_DENOMS = [5, 1, 10, 5, 20, 1, 5, 10, 1];
function renderJar(pesos) {
  const n = pesos > 0 ? Math.min(18, Math.ceil(Math.log2(pesos + 1) * 2)) : 0;
  const box = $("jar-coins");
  if (n === jarShown) return;
  if (n < box.children.length) box.replaceChildren();
  const start = box.children.length;
  for (let i = start; i < n; i++) {
    const row = Math.floor(i / 3), col = i % 3;
    const c = PisoCoins.draw(JAR_DENOMS[i % JAR_DENOMS.length], 30);
    const rest = `rotate(${((i * 37) % 23) - 11}deg)`;
    c.style.left = 4 + col * 25 + (row % 2) * 5 + "px";
    c.style.bottom = 3 + row * 11 + "px";
    c.style.transform = rest;
    box.append(c);
    if (jarShown >= 0 && !REDUCE) {
      c.animate([{ transform: "translateY(-130px) rotate(40deg)", opacity: 0 }, { opacity: 1, offset: 0.25 }, { transform: rest, opacity: 1 }],
        { duration: 800, delay: (i - start) * 80, easing: SPRING, fill: "backwards" });
    }
  }
  jarShown = n;
}

function emptyJar() {
  const coins = [...$("jar-coins").children].reverse();
  jarShown = 0;
  if (REDUCE) { $("jar-coins").replaceChildren(); return; }
  coins.forEach((c, i) => {
    c.animate([{ transform: c.style.transform, opacity: 1 }, { transform: `translate(${((i % 3) - 1) * 26}px, -150px) rotate(${180 + i * 30}deg)`, opacity: 0 }],
      { duration: 650, delay: i * 40, easing: "cubic-bezier(.55, 0, .75, .2)", fill: "forwards" });
  });
  setTimeout(() => { if (jarShown === 0) $("jar-coins").replaceChildren(); }, 700 + coins.length * 40);
}

$("box-empty").addEventListener("click", () => {
  const b = overview.coinbox;
  $("bm-expected").textContent = peso(b.pesos);
  $("bm-counted").value = b.pesos;
  $("bm-note").value = "";
  $("bm-coins").replaceChildren(...PisoCoins.split(Math.min(b.pesos, 76)).slice(0, 6).map((d, i) => {
    const c = PisoCoins.draw(d, 40);
    c.style.setProperty("--i", i);
    return c;
  }));
  $("box-modal").hidden = false;
  $("bm-counted").select();
});

const closeBoxModal = () => ($("box-modal").hidden = true);
$("bm-cancel").addEventListener("click", closeBoxModal);
$("box-modal").addEventListener("click", (ev) => ev.target === $("box-modal") && closeBoxModal());

$("box-form").addEventListener("submit", (ev) => {
  ev.preventDefault();
  busy(ev.submitter, async () => {
    const raw = $("bm-counted").value.trim();
    const r = await api("POST", "/coinbox/collect", { counted: raw === "" ? null : Number(raw), note: $("bm-note").value });
    closeBoxModal();
    overview.coinbox = r.coinbox;
    emptyJar();
    renderCoinbox(r.coinbox);
    const c = r.collection, diff = c.counted - c.expected;
    toast("Coin box emptied: " + peso(c.counted) + (diff < 0 ? " (" + peso(-diff) + " short)" : diff > 0 ? " (" + peso(diff) + " extra)" : ""));
    if (currentTab === "sales") loadReport(salesDays, false);
  });
});

const METERS = [
  ["temp", "thermo", "Temperature"],
  ["cpu", "gear", "Processor"],
  ["mem", "chip", "Memory"],
  ["disk", "disk", "Storage"],
];
function renderHealth(h) {
  const ul = $("meters");
  if (!ul.children.length) {
    for (const [key, ic, label] of METERS) {
      const li = el("li", { class: "meter", id: "meter-" + key }, icon(ic), el("span", { text: label }), el("b", { text: "—" }), el("span", { class: "bar" }, el("i")));
      ul.append(li);
    }
  }
  const gb = (mb) => (mb >= 1024 ? (mb / 1024).toFixed(1) + " GB" : mb + " MB");
  const set = (key, pct, text, warm, hot) => {
    const li = $("meter-" + key);
    li.hidden = pct == null;
    if (pct == null) return;
    li.querySelector("b").textContent = text;
    li.querySelector("i").style.width = Math.max(2, Math.min(100, pct)) + "%";
    li.classList.toggle("warm", pct >= warm && pct < hot);
    li.classList.toggle("hot", pct >= hot);
  };
  const t = h.temp_c;
  set("temp", t == null ? null : (t / 90) * 100, t == null ? "" : Math.round(t) + "°C", 72, 86);
  set("cpu", h.cpu_pct, h.cpu_pct + "% busy", 70, 90);
  set("mem", h.mem_used_pct, h.mem_used_pct + "% of " + gb(h.mem_total_mb || 0), 80, 92);
  set("disk", h.disk_used_pct, gb(h.disk_free_mb || 0) + " free", 85, 95);
  $("health-sub").textContent = h.boot_secs != null ? "Board on for " + human(h.boot_secs) : "How the board is doing";
  const w = $("wan-status");
  w.hidden = h.wan_up == null;
  w.className = "status " + (h.wan_up ? "on" : "bad");
  w.textContent = h.wan_up ? "Internet cable in" : "No internet cable";
}

// ---- sales ----------------------------------------------------------------

let salesDays = 30, report = null;

async function loadReport(days, animate = true) {
  salesDays = days;
  for (const b of $("period").querySelectorAll("button")) b.setAttribute("aria-selected", String(Number(b.dataset.days) === days));
  placeSegPill();
  $("csv").href = "/api/sales.csv?days=" + days;
  try {
    const [r, box] = await Promise.all([api("GET", "/report?days=" + days), api("GET", "/coinbox")]);
    if (days !== salesDays) return;
    report = r;
    renderReport(r, animate);
    renderCollections(box);
  } catch (e) { toast(e.message, true); }
}

function renderReport(r, animate) {
  const yearly = r.group === "month";
  const span = yearly ? "12 months" : r.days + " days";
  tween($("k-pesos"), r.pesos, peso);
  tween($("k-count"), r.count);
  tween($("k-people"), r.customers);
  tween($("k-hours"), Math.round(r.secs / 3600), (v) => v.toLocaleString("en-PH") + " h");
  const trend = $("k-trend");
  trend.className = "";
  if (r.prev_pesos > 0) {
    const pct = Math.round(((r.pesos - r.prev_pesos) / r.prev_pesos) * 100);
    trend.className = pct >= 0 ? "up" : "down";
    trend.replaceChildren(icon(pct >= 0 ? "up" : "down"), (pct >= 0 ? "+" : "") + pct + "% vs the " + span + " before");
  } else {
    trend.textContent = r.pesos > 0 ? "First " + span + " with sales" : "No sales yet";
  }
  $("k-avg").textContent = r.count ? peso(Math.round(r.pesos / r.count)) + " per sale" : " ";
  $("k-per").textContent = r.customers ? peso(Math.round(r.pesos / r.customers)) + " each on average" : " ";
  $("k-hours-sub").textContent = r.secs ? "of internet time" : " ";

  $("sales-chart-title").textContent = yearly ? "Earnings per month" : "Earnings per day";
  $("sales-chart-sub").textContent = "Coins collected, last " + span;
  const items = r.buckets;
  barChart($("sales-chart"), items, {
    height: 260, avg: true, today: true, values: items.length <= 14, animate,
    label: (d, i, slot) => {
      if (yearly) return shortDate(d.date, { month: "short" });
      const every = Math.ceil(30 / slot);
      if ((items.length - 1 - i) % every) return null;
      return items.length <= 7 ? shortDate(d.date, { weekday: "short" }) : String(new Date(d.date + "T00:00:00").getDate());
    },
    tip: (d) => [peso(d.pesos), (yearly ? shortDate(d.date, { month: "long", year: "numeric" }) : shortDate(d.date, { weekday: "short", month: "short", day: "numeric" })) + " · " + salesLine(d)],
  });
  const best = r.best ? "Best " + (yearly ? "month" : "day") + ": " + (yearly ? shortDate(r.best.date, { month: "long" }) : shortDate(r.best.date)) + " · " : "";
  $("best").replaceChildren(
    best ? document.createTextNode(best) : "", best ? el("b", { text: peso(r.best.pesos) }) : "");
  const record = r.best_ever && !(r.best && !yearly && r.best.date === r.best_ever.date) ? r.best_ever : null;
  if (record) $("best").append(el("br"), "Record day: " + shortDate(record.date) + " · ", el("b", { text: peso(record.pesos) }));
  renderHeat(r.heat, animate);
  renderPaid(r.sources);
}

new ResizeObserver(() => report && currentTab === "sales" && $("sales-chart").clientWidth && renderReport(report, false)).observe($("sales-chart"));

const DAYS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const hourName = (h) => (h % 12 || 12) + (h < 12 ? " am" : " pm");

function renderHeat(heat, animate) {
  const box = $("heat");
  const max = Math.max(1, ...heat.flat());
  const cells = [el("span")];
  for (let h = 0; h < 24; h++) cells.push(el("span", { class: "hr", text: h % 6 === 0 ? hourName(h).replace(" ", "") : "" }));
  let top = null;
  heat.forEach((row, d) => {
    cells.push(el("span", { class: "lbl", text: DAYS[d] }));
    row.forEach((v, h) => {
      const c = el("i", { class: v ? "v" : "", title: DAYS[d] + " " + hourName(h) + "–" + hourName((h + 1) % 24) + ": " + peso(v) });
      if (v) c.style.setProperty("--o", (0.18 + 0.82 * Math.sqrt(v / max)).toFixed(3));
      c.style.setProperty("--c", animate ? d + h : 0);
      if (!animate) c.style.animation = "none";
      if (!top || v > top.v) top = { v, d, h };
      cells.push(c);
    });
  });
  box.replaceChildren(...cells);
  box.setAttribute("aria-label", top && top.v ? "Busiest hour: " + DAYS[top.d] + " " + hourName(top.h) : "No sales in this period");
  $("heat-sub").textContent = top && top.v ? "Busiest: " + DAYS[top.d] + "s, " + hourName(top.h) + " to " + hourName((top.h + 1) % 24) : "When customers buy time";
}

const PAID = {
  coin: ["Coins", "#5A43E6"],
  voucher: ["Vouchers", "#9CB51F"],
  manual: ["Added by you", "#3F8FB0"],
  unclaimed: ["Unclaimed coins", "#E89A2C"],
};
const DONUT_C = 2 * Math.PI * 46;

function renderPaid(sources) {
  const donut = $("donut");
  const total = sources.reduce((a, s) => a + s.secs, 0);
  donut.replaceChildren(svg("circle", { cx: 60, cy: 60, r: 46, stroke: "#EEEBFD" }));
  let offset = 0;
  const segs = [];
  for (const s of sources) {
    if (!s.secs || !total) continue;
    const len = (DONUT_C * s.secs) / total;
    const c = svg("circle", { cx: 60, cy: 60, r: 46, stroke: (PAID[s.source] || [s.source, "#6E7183"])[1], "stroke-dasharray": `0 ${DONUT_C}`, "stroke-dashoffset": -offset });
    segs.push([c, `${Math.max(0, len - 1.5)} ${DONUT_C}`]);
    donut.append(c);
    offset += len;
  }
  donut.append(svg("text", { x: 60, y: 62 }, hoursText(total)), svg("text", { x: 60, y: 76, class: "dl" }, "given out"));
  requestAnimationFrame(() => requestAnimationFrame(() => segs.forEach(([c, dash]) => c.setAttribute("stroke-dasharray", dash))));
  $("legend").replaceChildren(...(sources.length ? sources : [null]).map((s) => {
    if (!s) return el("li", { class: "empty", text: "No sales in this period." });
    const [name, color] = PAID[s.source] || [s.source, "#6E7183"];
    const sw = el("i");
    sw.style.background = color;
    const what = s.source === "coin" ? s.count + (s.count === 1 ? " sale" : " sales") + " · " + hoursText(s.secs)
      : s.source === "unclaimed" ? "Dropped with no one inserting · no time given"
        : s.count + (s.count === 1 ? " time" : " times") + " · " + hoursText(s.secs);
    return el("li", {}, sw, el("b", { text: name }), el("span", { class: "amt", text: s.pesos ? peso(s.pesos) : "Free" }), el("small", { text: what }));
  }));
}

function renderCollections(box) {
  $("collections").replaceChildren(...(box.history.length ? box.history : [null]).map((c) => {
    if (!c) return el("li", { class: "empty", text: "You haven't emptied the coin box yet. Tap “I emptied it” on the dashboard when you do." });
    const diff = c.counted - c.expected;
    return el("li", {},
      el("span", { class: "what", text: "Emptied " + new Date(c.ts * 1000).toLocaleDateString("en-PH", { weekday: "short", month: "short", day: "numeric" }) },
        el("small", { text: c.note || "Sales said " + peso(c.expected) })),
      el("span", { class: diff < 0 ? "short" : diff > 0 ? "over" : "when", text: diff < 0 ? peso(-diff) + " short" : diff > 0 ? peso(diff) + " extra" : "Exact" }),
      el("span", { class: "amt", text: peso(c.counted) }));
  }));
}

for (const b of $("period").querySelectorAll("button")) {
  b.addEventListener("click", () => loadReport(Number(b.dataset.days)));
}

// ---- rates ----------------------------------------------------------------

function rateRow(r = { pesos: "", minutes: "" }) {
  const p = el("input", { type: "number", min: "1", max: "1000", required: "", value: r.pesos, "aria-label": "Pesos" });
  const m = el("input", { type: "number", min: "1", max: "86400", required: "", value: r.minutes, "aria-label": "Minutes" });
  const coin = el("span", { class: "coin", text: "₱" });
  const del = el("button", { class: "btn small outline", type: "button", text: "Remove", onclick: () => row.remove() });
  const row = el("div", { class: "rate-row" }, coin, p, el("span", { text: "=" }), m, el("span", { class: "unit", text: "minutes" }), del);
  return row;
}

async function loadRates() {
  await busy(null, async () => {
    const rates = await api("GET", "/rates");
    $("rates").replaceChildren(...rates.map(rateRow));
  });
}

$("add-rate").addEventListener("click", () => $("rates").append(rateRow()));

$("rates-form").addEventListener("submit", (ev) => {
  ev.preventDefault();
  const rates = [...$("rates").querySelectorAll(".rate-row")].map((row) => {
    const [p, m] = row.querySelectorAll("input");
    return { pesos: Number(p.value), minutes: Number(m.value) };
  });
  busy(ev.submitter, async () => {
    const saved = await api("PUT", "/rates", rates);
    $("rates").replaceChildren(...saved.map(rateRow));
    toast("Rates saved");
  });
});

// ---- vouchers -------------------------------------------------------------

let lastCodes = [];

$("voucher-form").addEventListener("submit", (ev) => {
  ev.preventDefault();
  busy(ev.submitter, async () => {
    const r = await api("POST", "/vouchers", {
      count: Number($("v-count").value),
      minutes: Number($("v-minutes").value),
      label: $("v-label").value,
    });
    lastCodes = r.codes;
    const name = overview ? overview.name : "Piso WiFi";
    const url = overview ? overview.portal_url.replace(/^https?:\/\//, "").replace(/\/$/, "") : "";
    $("tickets").replaceChildren(...r.codes.map((code) =>
      el("div", { class: "ticket" },
        el("span", { class: "t-name", text: name + (r.label ? " · " + r.label : "") }),
        el("span", { class: "t-code", text: code }),
        el("span", { class: "t-time", text: human(r.minutes * 60) + " of internet" }),
        el("span", { class: "t-help", text: "Connect to the WiFi, open " + url + ", tap Have a voucher code?" }))));
    $("new-codes").hidden = false;
    toast(r.codes.length + " vouchers created");
    loadVouchers();
  });
});

$("print").addEventListener("click", () => printCard($("new-codes")));
$("copy-codes").addEventListener("click", async () => {
  try {
    await navigator.clipboard.writeText(lastCodes.join("\n"));
    toast("Codes copied");
  } catch (_) {
    toast("Copy isn't available here. Select the codes instead.", true);
  }
});

async function loadVouchers() {
  await busy(null, async () => {
    const { vouchers } = await api("GET", "/vouchers");
    const unused = vouchers.filter((v) => !v.used_at).length;
    $("v-sub").textContent = unused + " unused · " + (vouchers.length - unused) + " used";
    $("vouchers-empty").hidden = vouchers.length > 0;
    $("vouchers").replaceChildren(...vouchers.map((v) => {
      const del = v.used_at ? "" : el("button", {
        class: "btn small danger", type: "button", text: "Delete",
        onclick: (ev) => {
          if (!confirm("Delete this unused voucher? Its code will stop working.")) return;
          busy(ev.currentTarget, async () => {
            await api("DELETE", "/vouchers/" + v.id);
            loadVouchers();
          });
        },
      });
      return el("tr", {},
        el("td", { class: "mono", text: v.id }),
        el("td", { text: human(v.secs) }),
        el("td", { text: v.label || "—" }),
        el("td", { text: when(v.created_at) }),
        el("td", {}, el("span", { class: "status " + (v.used_at ? "used" : "on"), text: v.used_at ? "Used " + when(v.used_at) : "Unused" })),
        el("td", { class: "r" }, del));
    }));
  });
}

// ---- activity -------------------------------------------------------------

const SOURCE = { coin: "Coins", voucher: "Voucher", manual: "Time added by you", unclaimed: "Unclaimed coin" };

function renderRecent(list) {
  keyed($("recent"), JSON.stringify([list.map((s) => [s.ts, s.mac, s.pesos, s.source]), Math.floor(Date.now() / 60000)]), () =>
    (list.length ? list : [null]).map((s) => {
      if (!s) return el("li", { class: "empty", text: "No sales yet." });
      const who = s.mac ? deviceOf(s.mac) : null;
      return el("li", {},
        who ? avatar(s.mac, who, "sm") : null,
        el("span", { class: "what", text: SOURCE[s.source] || s.source }, el("small", { text: who ? who.name : "No device" })),
        el("span", { class: "amt", text: s.pesos ? peso(s.pesos) : human(s.secs) }),
        el("span", { class: "when", text: when(s.ts) }));
    }));
}

const ACTIONS = {
  login: "Signed in", login_failed: "Failed sign-in", add_time: "Added time", kick: "Disconnected device",
  set_rates: "Changed rates", create_vouchers: "Created vouchers", delete_voucher: "Deleted voucher",
  rename: "Renamed portal", password_changed: "Changed password", password_set: "Set password from shell",
  pause: "Paused a device", resume: "Resumed a device", rename_device: "Named a device",
  collect_coins: "Emptied the coin box", set_goal: "Set the daily goal", branding: "Changed the customer page",
  upload_image: "Uploaded an image", remove_image: "Removed an image", mac_clone: "Possible copied address",
  block_device: "Blocked a device", unblock_device: "Unblocked a device",
};

// "da:a1:19:…" in a log line becomes the device's name when we know it.
const namedMacs = (text) => text.replace(/\b([0-9a-f]{2}:){5}[0-9a-f]{2}\b/g, (m) => {
  const d = deviceOf(m);
  return d.name.startsWith("Device …") ? m : d.name;
});

async function loadAudit() {
  if (overview) renderRecent(overview.sales.recent);
  await busy(null, async () => {
    const { entries } = await api("GET", "/audit");
    $("audit").replaceChildren(...(entries.length ? entries : [null]).map((a) => {
      if (!a) return el("li", { class: "empty", text: "Nothing yet." });
      return el("li", {},
        el("span", { class: "what", text: ACTIONS[a.action] || a.action }, el("small", { text: [namedMacs(a.detail), a.actor].filter(Boolean).join(" · ") })),
        el("span", { class: "when", text: when(a.ts) }));
    }));
  });
}


// ---- customer page --------------------------------------------------------

const ACCENTS = [
  ["#5A43E6", "Purple"], ["#2563EB", "Blue"], ["#0E8F7E", "Teal"],
  ["#D9363E", "Red"], ["#C2378A", "Pink"], ["#2B2D42", "Ink"],
];
let brandDraft = null;   // what the form currently shows (unsaved)

function mix(hex, other, t) {
  const p = (h, i) => parseInt(h.slice(i, i + 2), 16);
  const c = [1, 3, 5].map((i) => Math.round(p(hex, i) * (1 - t) + p(other, i) * t));
  return "#" + c.map((v) => v.toString(16).padStart(2, "0")).join("");
}

function luminance(hex) {
  const [r, g, b] = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255)
    .map((v) => (v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4)));
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

function defaultMark(accent) {
  // The built-in coin + WiFi mark, recoloured to the accent.
  const s = svg("svg", { viewBox: "0 0 64 64", "aria-hidden": "true" });
  s.append(svg("rect", { width: 64, height: 64, rx: 18, fill: accent }),
    svg("circle", { cx: 32, cy: 32, r: 20, fill: "#D4F04C" }));
  const g = svg("g", { fill: "none", stroke: accent, "stroke-width": 4, "stroke-linecap": "round" });
  g.append(svg("path", { d: "M27.05 32.05a7 7 0 0 1 9.9 0" }), svg("path", { d: "M22.8 27.8a13 13 0 0 1 18.4 0" }));
  s.append(g, svg("circle", { cx: 32, cy: 38, r: 3, fill: accent }));
  return s;
}

function logoNode(b) {
  if (b.logo) return el("img", { src: b.logo, alt: "" });
  return defaultMark(b.accent);
}

function renderSwatches(current) {
  const row = $("swatches");
  row.replaceChildren();
  const isPreset = ACCENTS.some(([h]) => h.toLowerCase() === current.toLowerCase());
  for (const [hex, name] of ACCENTS) {
    const input = el("input", { type: "radio", name: "accent", value: hex, "aria-label": name });
    if (hex.toLowerCase() === current.toLowerCase()) input.checked = true;
    input.addEventListener("change", () => setAccent(hex));
    const sw = el("label", { class: "swatch", title: name }, input);
    sw.style.background = hex;
    sw.style.color = hex;
    row.append(sw);
  }
  const picker = el("input", { type: "color", value: current, "aria-label": "Custom colour" });
  if (!isPreset) picker.setAttribute("checked", "");
  picker.addEventListener("input", () => {
    for (const r of row.querySelectorAll("input[type=radio]")) r.checked = false;
    setAccent(picker.value.toUpperCase());
  });
  const custom = el("label", { class: "swatch custom", title: "Custom colour" }, picker);
  custom.style.color = isPreset ? "#999" : current;
  row.append(custom);
}

function setAccent(hex) {
  brandDraft.accent = hex;
  $("accent-warn").hidden = luminance(hex) < 0.45;
  renderPreview();
}

function loadBrand(b) {
  brandDraft = Object.assign({}, b);
  $("b-name").value = b.name;
  $("b-wifi").value = b.wifi_name;
  $("b-alt").value = b.poster ? b.poster.alt : "";
  $("b-after").value = b.after_url;
  $("b-notice").value = b.notice || "";
  $("b-sounds").checked = b.sounds;
  renderSwatches(b.accent);
  $("accent-warn").hidden = luminance(b.accent) < 0.45;
  renderThumbs(b);
  renderPreview();
  renderSign();
}

function renderThumbs(b) {
  for (const [name, url] of [["logo", b.logo], ["poster", b.poster && b.poster.url]]) {
    const img = $(name + "-img");
    $(name + "-thumb").classList.toggle("has", !!url);
    if (url) img.src = url; else img.removeAttribute("src");
    $(name + "-remove").hidden = !url;
  }
}

function renderPreview() {
  const b = brandDraft;
  const phone = $("phone");
  phone.style.setProperty("--a", b.accent);
  phone.style.setProperty("--a-soft", mix(b.accent, "#ffffff", 0.88));
  $("ph-name").textContent = $("b-name").value.trim() || "Piso WiFi";
  const notice = $("b-notice").value.trim();
  $("ph-notice").hidden = !notice;
  $("ph-notice-text").textContent = notice;
  $("ph-logo").replaceChildren(logoNode(b));
  const poster = $("ph-poster");
  if (b.poster) { poster.src = b.poster.url; poster.hidden = false; } else { poster.hidden = true; }
}

$("b-name").addEventListener("input", renderPreview);
$("b-notice").addEventListener("input", renderPreview);

$("brand-form").addEventListener("submit", (ev) => {
  ev.preventDefault();
  busy(ev.submitter, async () => {
    const r = await api("PUT", "/branding", {
      name: $("b-name").value,
      accent: brandDraft.accent,
      poster_alt: $("b-alt").value,
      wifi_name: $("b-wifi").value,
      after_url: $("b-after").value,
      notice: $("b-notice").value,
      sounds: $("b-sounds").checked,
    });
    overview.brand = r.brand;
    overview.name = r.brand.name;
    loadBrand(r.brand);
    renderOverview();
    toast("Customer page updated");
  });
});

// Shrinks and re-encodes on this device before upload. Re-encoding through
// a canvas also strips EXIF data such as the GPS location of the photo.
async function prepareImage(file, maxW, maxH, preferPng) {
  const bmp = await createImageBitmap(file);
  const k = Math.min(1, maxW / bmp.width, maxH / bmp.height);
  const canvas = document.createElement("canvas");
  canvas.width = Math.max(1, Math.round(bmp.width * k));
  canvas.height = Math.max(1, Math.round(bmp.height * k));
  canvas.getContext("2d").drawImage(bmp, 0, 0, canvas.width, canvas.height);
  const enc = (type, q) => new Promise((res) => canvas.toBlob(res, type, q));
  if (preferPng) {
    const png = await enc("image/png");
    if (png && png.size < 1.4e6) return png;
  }
  for (const q of [0.86, 0.75, 0.6]) {
    let blob = await enc("image/webp", q);
    if (!blob || blob.type !== "image/webp") blob = await enc("image/jpeg", q);
    if (blob && blob.size < 1.4e6) return blob;
  }
  throw new Error("That image is too large even after shrinking.");
}

// The home-screen icon: the logo on a white square, cropped like the
// customer page's header shows it. Made here: the box can't decode images.
async function makeHomeIcon(source) {
  const bmp = await createImageBitmap(source);
  const size = 256;
  const canvas = document.createElement("canvas");
  canvas.width = canvas.height = size;
  const ctx = canvas.getContext("2d");
  ctx.fillStyle = "#fff";
  ctx.fillRect(0, 0, size, size);
  const k = Math.max(size / bmp.width, size / bmp.height);
  const w = bmp.width * k, h = bmp.height * k;
  ctx.imageSmoothingQuality = "high";
  ctx.drawImage(bmp, (size - w) / 2, (size - h) / 2, w, h);
  const png = await new Promise((res) => canvas.toBlob(res, "image/png"));
  if (!png) throw new Error("Couldn't make the home-screen icon");
  return png;
}

// Logos uploaded before home-screen icons existed get theirs on the next visit.
let iconBackfilled = false;
async function backfillHomeIcon(b) {
  if (iconBackfilled || !b.logo || b.icon) return;
  iconBackfilled = true;
  try {
    const logo = await (await fetch(b.logo, { credentials: "same-origin" })).blob();
    overview.brand = await putImage("icon", await makeHomeIcon(logo));
  } catch (_) { /* customers get the coin mark until then */ }
}

async function uploadImage(name, file) {
  const blob = name === "logo" ? await prepareImage(file, 256, 256, true) : await prepareImage(file, 1080, 1440, false);
  let brand = await putImage(name, blob);
  if (name === "logo") {
    try { brand = await putImage("icon", await makeHomeIcon(blob)); } catch (_) { /* the coin mark stands in */ }
  }
  return brand;
}

async function putImage(name, blob) {
  const res = await fetch("/api/brand/" + name, {
    method: "PUT",
    headers: { "x-pisowifi": "1", "x-csrf-token": csrf, "content-type": blob.type },
    body: blob,
    credentials: "same-origin",
  });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data.error || "Upload failed");
  return data.brand;
}

for (const name of ["logo", "poster"]) {
  const input = $(name + "-file");
  input.addEventListener("change", () => {
    const file = input.files[0];
    input.value = "";
    if (!file) return;
    busy(input.closest(".file-btn"), async () => {
      const b = await uploadImage(name, file);
      overview.brand = b;
      brandDraft.logo = b.logo;
      brandDraft.poster = b.poster;
      renderThumbs(b);
      renderPreview();
      renderSign();
      toast(name === "logo" ? "Logo updated" : "Poster updated");
    });
  });
  $(name + "-remove").addEventListener("click", (ev) => {
    if (!confirm("Remove the " + name + "?")) return;
    busy(ev.currentTarget, async () => {
      const r = await api("DELETE", "/brand/" + name);
      overview.brand = r.brand;
      brandDraft.logo = r.brand.logo;
      brandDraft.poster = r.brand.poster;
      renderThumbs(r.brand);
      renderPreview();
      renderSign();
    });
  });
}

function renderSign() {
  const b = overview.brand;
  $("sign-name").textContent = b.name;
  $("sign-logo").replaceChildren(logoNode(b));
  const hasWifi = !!b.wifi_name;
  $("qr-wifi-fig").hidden = !hasWifi;
  $("sign-warn").hidden = hasWifi;
  $("qr-portal-step").textContent = hasWifi ? "2" : "1";
  $("sign-ssid").textContent = "“" + b.wifi_name + "”";
  const t = Date.now();
  if (hasWifi) $("qr-wifi").src = "/api/qr/wifi?t=" + t;
  $("qr-portal").src = "/api/qr/portal?t=" + t;
  api("GET", "/rates").then((rates) => {
    $("sign-rates").textContent = rates.map((r) => "₱" + r.pesos + " = " + human(r.minutes * 60)).join("   ·   ");
  }).catch(() => {});
}

$("print-sign").addEventListener("click", () => printCard($("sign-card")));

function printCard(card) {
  card.classList.add("printing");
  const done = () => { card.classList.remove("printing"); window.removeEventListener("afterprint", done); };
  window.addEventListener("afterprint", done);
  window.print();
}

// ---- settings -------------------------------------------------------------

$("pw-form").addEventListener("submit", (ev) => {
  ev.preventDefault();
  if ($("pw-new").value !== $("pw-repeat").value) {
    toast("The new passwords don't match", true);
    return;
  }
  busy(ev.submitter, async () => {
    await api("POST", "/password", { current: $("pw-current").value, new: $("pw-new").value });
    ev.target.reset();
    showLogin("Password changed. Sign in with your new password.");
  });
});

// ---- update -----------------------------------------------------------------

let upd = null;
let updTimer = null;
let updRestarting = false;
const UPD_ACTIVE = ["installing", "trial", "rolling_back"];

const day = (ts) => new Date(ts * 1000).toLocaleDateString("en-PH", { month: "short", day: "numeric", year: "numeric" });

async function loadUpdate() {
  clearTimeout(updTimer);
  try {
    upd = await api("GET", "/update");
    renderUpdate();
  } catch (_) {
    // Unreachable while the vendo restarts into the new version: keep asking.
    if (!updRestarting) return;
  }
  const active = updRestarting || (upd && (upd.busy || (upd.state && UPD_ACTIVE.includes(upd.state.phase))));
  if (currentTab === "settings" && active) updTimer = setTimeout(loadUpdate, 2000);
}

function renderUpdate(note, noteBad) {
  const u = upd, st = u.state;
  // An uploaded file comes first; a newer version online second.
  const offer = u.ready || u.available;
  updRestarting = u.busy === "waiting" || !!(st && UPD_ACTIVE.includes(st.phase));
  const working = !!u.busy || updRestarting;

  $("upd-current").textContent = "Version " + u.current;
  let last = "";
  if (st && st.phase === "done") last = "Updated to " + st.to + " on " + day(st.at) + ".";
  else if (st && (st.phase === "rolled_back" || st.phase === "failed")) last = day(st.at) + ": " + st.message;
  $("upd-last").textContent = last;
  $("upd-last").hidden = $("upd-last-label").hidden = !last || updRestarting;

  let pill = "Up to date", cls = "used";
  if (!u.signed) pill = "Off";
  else if (u.busy === "checking") { pill = "Checking"; cls = "off"; }
  else if (u.busy === "downloading") { pill = "Downloading"; cls = "off"; }
  else if (updRestarting) { pill = "Installing"; cls = "off"; }
  else if (u.ready) { pill = "Ready to install"; cls = "on"; }
  else if (u.available) { pill = "Update available"; cls = "on"; }
  $("upd-status").textContent = pill;
  $("upd-status").className = "status " + cls;

  $("upd-notes").hidden = !offer || working;
  if (offer) {
    $("upd-notes-title").textContent = u.ready
      ? "Version " + offer.version + " checks out. Signed " + day(offer.created) + "."
      : "Version " + offer.version + " is available. Released " + day(offer.issued_at) + ".";
    $("upd-notes-text").textContent = offer.notes || "No notes for this version.";
  }

  let msg = note || "", bad = !!noteBad;
  if (!msg) {
    if (!u.signed) msg = "Updates are off: this copy was built without a release key. Reinstall it once from a signed bundle.";
    else if (u.error) { msg = u.error; bad = true; }
    else if (u.busy === "checking") msg = "Checking…";
    else if (u.busy === "downloading") msg = "Downloading and checking version " + (u.available ? u.available.version : "") + "…";
    else if (updRestarting) msg = "Installing. Customers stay online. The dashboard is back in about a minute; you'll need to sign in again.";
  }
  $("upd-msg").textContent = msg;
  $("upd-msg").hidden = !msg;
  $("upd-msg").classList.toggle("bad", bad);

  $("upd-ready-row").hidden = !offer || working;
  $("upd-cancel").hidden = !u.ready;
  $("upd-check-row").hidden = !u.signed || !!u.ready || working;
  $("upd-check").disabled = !u.online;
  $("upd-upload").hidden = !u.signed || !!u.ready || working;
  // Without internet the upload is the way in, so it opens by itself.
  if (!u.online || u.check_error) $("upd-upload").open = true;

  $("upd-default").textContent = u.default_url;
  if (document.activeElement !== $("upd-url")) $("upd-url").value = u.url;
  $("upd-online").checked = u.online;
  $("upd-auto").checked = u.auto;
  $("upd-auto").disabled = !u.online;
  let latest = "";
  if (!u.online) latest = "Online checks are off.";
  else if (u.check_error) latest = "Couldn't check: " + u.check_error;
  else if (u.last_check) latest = (u.available ? "Version " + u.available.version + " is available" : "You have the newest version") + " (checked " + when(u.last_check) + ").";
  else latest = "Not checked yet.";
  $("upd-latest").textContent = latest;
}

$("upd-file").addEventListener("change", async (ev) => {
  const file = ev.target.files[0];
  ev.target.value = "";
  if (!file) return;
  if (file.size > 16 * 1024 * 1024) {
    renderUpdate("That file is too big to be a Piso WiFi update.", true);
    return;
  }
  const pick = $("upd-pick");
  pick.classList.add("busy");
  renderUpdate("Checking " + file.name + "…");
  try {
    const res = await fetch("/api/update/upload", {
      method: "POST",
      headers: { "x-pisowifi": "1", "x-csrf-token": csrf, "content-type": "application/octet-stream" },
      body: file,
      credentials: "same-origin",
      cache: "no-store",
    });
    const data = await res.json().catch(() => ({}));
    if (res.status === 401) return showLogin();
    if (!res.ok) throw new Error(data.error || "Upload failed (" + res.status + ")");
    upd = data;
    renderUpdate();
  } catch (e) {
    renderUpdate(e.message, true);
  } finally {
    pick.classList.remove("busy");
  }
});

$("upd-cancel").addEventListener("click", (ev) => busy(ev.currentTarget, async () => {
  upd = await api("DELETE", "/update/upload");
  renderUpdate();
}));

$("upd-install").addEventListener("click", (ev) => {
  const offer = upd && (upd.ready || upd.available);
  if (!offer) return;
  if (!confirm("Install version " + offer.version + " now?\n\nCustomers stay online. The dashboard restarts, so you'll sign in again. " +
    "If the new version doesn't start, this one is put back by itself.")) return;
  busy(ev.currentTarget, async () => {
    upd = await api("POST", "/update/install");
    renderUpdate();
    loadUpdate();
  });
});

$("upd-check").addEventListener("click", (ev) => busy(ev.currentTarget, async () => {
  upd = await api("POST", "/update/check");
  renderUpdate();
}));

const saveUpdateSettings = () => api("PUT", "/update/settings", {
  online: $("upd-online").checked,
  auto: $("upd-auto").checked,
  url: $("upd-url").value.trim(),
});

for (const id of ["upd-online", "upd-auto"]) {
  $(id).addEventListener("change", async (ev) => {
    try {
      upd = await saveUpdateSettings();
      renderUpdate();
      loadUpdate();
      if (id === "upd-online") toast(upd.online ? "The vendo checks for new versions online" : "Online checks are off");
      else toast(upd.auto ? "New versions will install by themselves at 3 AM" : "Automatic installs are off");
    } catch (e) {
      ev.target.checked = !ev.target.checked;
      toast(e.message, true);
    }
  });
}

$("upd-url-form").addEventListener("submit", (ev) => {
  ev.preventDefault();
  busy(ev.submitter, async () => {
    upd = await saveUpdateSettings();
    renderUpdate();
    loadUpdate();
    toast(upd.url ? "Saved. Checking there now." : "Saved. Using the usual update list.");
  });
});

// ---- start ----------------------------------------------------------------

function showApp() {
  $("login").hidden = true;
  $("shell").hidden = false;
  go(location.hash.slice(1) || "overview");
  poll();
}

(async function init() {
  try {
    const s = await api("GET", "/session");
    if (s.logged_in) {
      csrf = s.csrf;
      showApp();
    } else {
      showLogin(s.needs_setup ? "No password yet. On the device, run: sudo pisowifi set-password" : null);
    }
  } catch (e) {
    showLogin(e.message);
  }
})();
