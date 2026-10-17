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
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.hidden = true), 3500);
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

const TABS = ["overview", "devices", "rates", "vouchers", "customize", "activity", "settings"];

function go(tab) {
  if (!TABS.includes(tab)) tab = "overview";
  currentTab = tab;
  for (const t of TABS) $("tab-" + t).hidden = t !== tab;
  for (const b of document.querySelectorAll(".nav [data-tab]")) {
    if (b.dataset.tab === tab) b.setAttribute("aria-current", "page");
    else b.removeAttribute("aria-current");
  }
  if (location.hash !== "#" + tab) history.replaceState(null, "", "#" + tab);
  if (tab === "rates") loadRates();
  if (tab === "vouchers") loadVouchers();
  if (tab === "activity") loadAudit();
  if (tab === "customize" && overview) loadBrand(overview.brand);
  if (tab === "settings") loadUpdate();
  else clearTimeout(updTimer);
  if (overview) renderOverview();
  window.scrollTo({ top: 0 });
}

document.addEventListener("click", (ev) => {
  const t = ev.target.closest("[data-tab],[data-goto]");
  if (t) go(t.dataset.tab || t.dataset.goto);
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
  } catch (_) { /* toast-free polling */ }
  pollTimer = setTimeout(poll, overview && overview.slot.state !== "idle" ? 1000 : 4000);
}

function renderOverview() {
  const o = overview, s = o.sales;
  document.title = o.name + " · Owner dashboard";
  $("site-name").textContent = o.name;
  $("page-title").textContent = o.name;
  $("site-tag").textContent = o.dev ? "Dev" : "Live";
  $("version").textContent = "version " + o.version;
  $("open-portal").href = o.portal_url;
  $("fp").textContent = o.fingerprint || "Dev mode (plain HTTP)";

  $("online").textContent = o.active;
  $("today").textContent = peso(s.today_pesos);
  $("week").textContent = peso(s.week_pesos);
  $("month").textContent = peso(s.month_pesos);
  $("alltime").textContent = peso(s.all_time_pesos);
  $("nav-count").textContent = o.active || "";

  renderBanner(o);
  renderHub(o.hub);
  renderChart(s.days);
  renderTop(s.top_customers);
  renderActivity(o);
  renderSlot(o.slot, o.dev);
  renderMiniDevices(o.clients);
  if (currentTab === "devices") renderDevices(o.clients, o.lan_devices || []);
  if (currentTab === "activity") renderRecent(s.recent);
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

function renderChart(days) {
  const W = Math.max(280, $("chart").clientWidth || 560), H = 230, top = 22, bottom = 26, left = 4;
  const max = Math.max(10, ...days.map((d) => d.pesos));
  const n = days.length, slot = (W - left) / n, bw = Math.min(26, slot * 0.58);
  const best = days.reduce((b, d, i) => (d.pesos > (days[b] ? days[b].pesos : -1) ? i : b), -1);
  const root = svg("svg", { viewBox: `0 0 ${W} ${H}`, width: W, height: H });
  for (let i = 0; i <= 3; i++) {
    const y = top + ((H - top - bottom) * i) / 3;
    root.append(svg("line", { x1: 0, x2: W, y1: y, y2: y, class: "grid" }));
  }
  days.forEach((d, i) => {
    const h = ((H - top - bottom) * d.pesos) / max;
    const x = left + i * slot + (slot - bw) / 2;
    const y = H - bottom - h;
    const isToday = i === n - 1;
    const cls = "bar" + (isToday ? " today" : i === best && d.pesos > 0 ? " best" : "");
    root.append(svg("rect", { x, y: Math.min(y, H - bottom - 3), width: bw, height: Math.max(h, 3), rx: 3, class: d.pesos > 0 ? cls : "bar zero" }));
    if (d.pesos > 0) root.append(svg("text", { x: x + bw / 2, y: y - 6, "text-anchor": "middle", class: "val" }, peso(d.pesos)));
    const date = new Date(d.date + "T00:00:00");
    const every = slot < 24 ? 3 : slot < 34 ? 2 : 1;
    if ((n - 1 - i) % every === 0) {
      const label = isToday && slot >= 36 ? "Today" : String(date.getDate());
      root.append(svg("text", { x: x + bw / 2, y: H - 8, "text-anchor": "middle", class: "axis" + (isToday ? " now" : "") }, label));
    }
  });
  $("chart").replaceChildren(root);
  chartDays = days;
  $("chart").setAttribute("aria-label", "Daily earnings: " + days.map((d) => d.date + " " + peso(d.pesos)).join(", "));
}

let chartDays = null;
new ResizeObserver(() => chartDays && renderChart(chartDays)).observe($("chart"));

function renderTop(list) {
  const ul = $("top");
  ul.replaceChildren();
  $("top-empty").hidden = list.length > 0;
  const max = Math.max(1, ...list.map((c) => c.pesos));
  list.forEach((c, i) => {
    const color = AVATAR_COLORS[i % AVATAR_COLORS.length];
    const av = el("span", { class: "av", text: c.mac.slice(-2).toUpperCase() });
    av.style.background = color;
    const fill = el("i");
    fill.style.width = (100 * c.pesos) / max + "%";
    fill.style.background = color;
    ul.append(el("li", {},
      av,
      el("span", { class: "who", text: shortMac(c.mac) }),
      el("span", { class: "bar-line" }, el("span", { class: "track" }, fill), el("span", { class: "amt", text: peso(c.pesos) }))));
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
  $("m-sales").textContent = s.month_count;
  const avg = s.month_pesos / 30;
  $("m-avg").textContent = peso(avg < 10 ? Math.round(avg * 10) / 10 : Math.round(avg));
  $("m-vouchers").textContent = s.vouchers_used_30d;
  $("m-unclaimed").textContent = peso(s.unclaimed_pesos);
  $("m-uptime").textContent = human(o.uptime_secs);
}

function renderSlot(slot, dev) {
  const inserting = slot.state !== "idle";
  $("slot-chip").classList.toggle("busy", inserting);
  $("slot-chip-text").textContent = inserting ? peso(slot.pesos) + " in" : "Idle";
  $("slot-title").textContent = inserting ? "Device " + shortMac(slot.mac) + " is inserting" : "Waiting for a customer";
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
  const ul = $("mini-devices");
  ul.replaceChildren();
  const list = clients.slice(0, 5);
  $("mini-empty").hidden = list.length > 0;
  $("online-pill").textContent = overview.active + " online";
  const max = Math.max(1, ...list.map((c) => c.remaining_secs));
  for (const c of list) {
    const fill = el("div", { class: "fill" });
    fill.style.width = (100 * c.remaining_secs) / max + "%";
    ul.append(el("li", { class: c.running ? "" : "paused" },
      el("div", { class: "top-line" },
        el("span", { text: shortMac(c.mac) }),
        el("span", { text: (c.running ? "" : "Paused · ") + human(c.remaining_secs) })),
      el("div", { class: "track" }, fill)));
  }
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

function addTimeButton(mac, mins, label) {
  return el("button", {
    class: "btn small outline", type: "button", text: label,
    onclick: (ev) => busy(ev.currentTarget, async () => {
      const r = await api("POST", "/clients/" + encodeURIComponent(mac) + "/time", { minutes: mins });
      toast("Added " + label.slice(1) + " to " + shortMac(mac) + (r.paused ? " (paused: it starts when they resume)" : ""));
      poll();
    }),
  });
}

function renderDevices(clients, others) {
  const tb = $("devices");
  tb.replaceChildren();
  $("devices-empty").hidden = clients.length > 0;
  $("devices-sub").textContent = overview.active + " online · " + (clients.length - overview.active) + " paused";
  for (const c of clients) {
    const kick = el("button", {
      class: "btn small danger", type: "button", text: "Disconnect",
      onclick: (ev) => {
        if (!confirm("Disconnect " + c.mac + "? Their remaining time will be removed.")) return;
        busy(ev.currentTarget, async () => {
          await api("POST", "/clients/" + encodeURIComponent(c.mac) + "/kick", {});
          toast("Disconnected " + shortMac(c.mac));
          poll();
        });
      },
    });
    tb.append(el("tr", {},
      el("td", { class: "mono", text: c.mac }),
      el("td", { class: "mono", text: c.ip }),
      el("td", {}, el("span", { class: "status " + (c.running ? "on" : "off"), text: c.running ? "Online" : "Paused" })),
      el("td", { class: "time", text: clock(c.remaining_secs) }),
      el("td", { class: "r" }, el("span", { class: "actions" }, addTimeButton(c.mac, 30, "+30 min"), addTimeButton(c.mac, 60, "+1 hr"), kick))));
  }

  const ob = $("lan-devices");
  ob.replaceChildren();
  $("lan-empty").hidden = others.length > 0;
  for (const d of others) {
    // The name is whatever the device told the DHCP server: shown as text only.
    ob.append(el("tr", {},
      el("td", {}, el("span", { class: "mono", text: d.mac }), d.name ? el("div", { class: "sub", text: d.name }) : null),
      el("td", { class: "mono", text: d.ip }),
      el("td", { class: "r" }, el("span", { class: "actions" }, addTimeButton(d.mac, 30, "+30 min"), addTimeButton(d.mac, 60, "+1 hr")))));
  }
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
  $("recent").replaceChildren(...(list.length ? list : [null]).map((s) => {
    if (!s) return el("li", { class: "empty", text: "No sales yet." });
    return el("li", {},
      el("span", { class: "what", text: SOURCE[s.source] || s.source }, el("small", { text: s.mac || "no device" })),
      el("span", { class: "amt", text: s.pesos ? peso(s.pesos) : human(s.secs) }),
      el("span", { class: "when", text: when(s.ts) }));
  }));
}

const ACTIONS = {
  login: "Signed in", login_failed: "Failed sign-in", add_time: "Added time", kick: "Disconnected device",
  set_rates: "Changed rates", create_vouchers: "Created vouchers", delete_voucher: "Deleted voucher",
  rename: "Renamed portal", password_changed: "Changed password", password_set: "Set password from shell",
};

async function loadAudit() {
  if (overview) renderRecent(overview.sales.recent);
  await busy(null, async () => {
    const { entries } = await api("GET", "/audit");
    $("audit").replaceChildren(...(entries.length ? entries : [null]).map((a) => {
      if (!a) return el("li", { class: "empty", text: "Nothing yet." });
      return el("li", {},
        el("span", { class: "what", text: ACTIONS[a.action] || a.action }, el("small", { text: [a.detail, a.actor].filter(Boolean).join(" · ") })),
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
  $("ph-logo").replaceChildren(logoNode(b));
  const poster = $("ph-poster");
  if (b.poster) { poster.src = b.poster.url; poster.hidden = false; } else { poster.hidden = true; }
}

$("b-name").addEventListener("input", renderPreview);

$("brand-form").addEventListener("submit", (ev) => {
  ev.preventDefault();
  busy(ev.submitter, async () => {
    const r = await api("PUT", "/branding", {
      name: $("b-name").value,
      accent: brandDraft.accent,
      poster_alt: $("b-alt").value,
      wifi_name: $("b-wifi").value,
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

async function uploadImage(name, file) {
  const blob = name === "logo" ? await prepareImage(file, 256, 256, true) : await prepareImage(file, 1080, 1440, false);
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
