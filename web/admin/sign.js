/* The printable sign as a glass poster: gel-filled glass objects (bottles,
   cubes, stars, drinks, peso coins...) around a frosted card with the QR
   codes. Every design comes from a number, so Shuffle gives thousands of
   variations and the owner can always get a favourite back by its number.
   Drawn on a canvas at A4 / 150 dpi, so it can be saved as PNG or PDF. */
(() => {
  "use strict";
  const W = 1240, H = 1754; // A4 at 150 dpi

  const PALETTES = {
    dopamine: { name: "Dopamine", colors: ["#FF6B1A", "#FF3D8B", "#3D7BFF", "#FFD12E", "#2ED47A", "#9B5CFF"] },
    candy: { name: "Candy", colors: ["#FF8AC8", "#B79CFF", "#6FCFFF", "#FFE36E", "#8BEBB9", "#FF9F80"] },
    sunset: { name: "Sunset", colors: ["#FF5E3A", "#FF9A3C", "#FFC93C", "#FF6FA8", "#C04BFF", "#FFB199"] },
    ocean: { name: "Ocean", colors: ["#1FA2FF", "#12D8C4", "#6C8CFF", "#8FE0FF", "#3DDC97", "#5B5BFF"] },
    citrus: { name: "Citrus", colors: ["#B8E62E", "#FFE14D", "#FF9E2C", "#2ED47A", "#00C2A8", "#FF6B6B"] },
    brand: { name: "My colours", colors: null }, // from the owner's accent and button colours
  };
  const SHAPES = {
    bottle: "Bottles", jar: "Jars", tube: "Tubes", dropper: "Droppers", cube: "Cubes", star: "Stars",
    wave: "Wavy blobs", orb: "Orbs", capsule: "Capsules", cup: "Drinks", coin: "Peso coins", wifi: "WiFi waves", face: "Faces",
  };
  const LAYOUTS = { scatter: "Scatter", grid: "Tiles", spotlight: "Spotlight" };
  const BGS = { white: "White", soft: "Soft colour", night: "Night" };

  // ---- small helpers ----------------------------------------------------------
  function rng(seed) {
    let a = (seed >>> 0) || 1;
    return () => {
      a = (a + 0x6D2B79F5) | 0;
      let t = Math.imul(a ^ (a >>> 15), 1 | a);
      t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
      return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
  }
  const between = (r, a, b) => a + r() * (b - a);
  const pick = (r, list) => list[Math.floor(r() * list.length)];
  const rgb = (h) => [1, 3, 5].map((i) => parseInt(h.slice(i, i + 2), 16));
  const mix = (a, b, t) => "#" + rgb(a).map((v, i) => Math.round(v * (1 - t) + rgb(b)[i] * t).toString(16).padStart(2, "0")).join("");
  const rgba = (h, al) => "rgba(" + rgb(h).join(",") + "," + al + ")";
  function shuffle(r, list) {
    const a = list.slice();
    for (let i = a.length - 1; i > 0; i--) { const j = Math.floor(r() * (i + 1)); [a[i], a[j]] = [a[j], a[i]]; }
    return a;
  }
  function rr(p, x, y, w, h, r) {
    r = Math.min(r, w / 2, h / 2);
    p.moveTo(x + r, y);
    p.arcTo(x + w, y, x + w, y + h, r);
    p.arcTo(x + w, y + h, x, y + h, r);
    p.arcTo(x, y + h, x, y, r);
    p.arcTo(x, y, x + w, y, r);
    p.closePath();
    return p;
  }
  const rrPath = (x, y, w, h, r) => rr(new Path2D(), x, y, w, h, r);

  // Soft blur. Canvas filters where the browser has them; elsewhere draw small
  // and scale up, which blurs nearly as well.
  const FILTER = (() => { const c = document.createElement("canvas").getContext("2d"); c.filter = "blur(2px)"; return c.filter === "blur(2px)"; })();
  function soft(ctx, radius, draw) {
    if (radius < 0.5) { ctx.save(); draw(ctx); ctx.restore(); return; }
    if (FILTER) { ctx.save(); ctx.filter = "blur(" + radius.toFixed(1) + "px)"; draw(ctx); ctx.restore(); return; }
    const k = Math.max(1, radius / 2.5), oc = document.createElement("canvas");
    oc.width = Math.ceil(W / k); oc.height = Math.ceil(H / k);
    const o = oc.getContext("2d"), m = ctx.getTransform();
    o.setTransform(m.a / k, m.b / k, m.c / k, m.d / k, m.e / k, m.f / k);
    draw(o);
    ctx.save(); ctx.setTransform(1, 0, 0, 1, 0, 0); ctx.imageSmoothingQuality = "high"; ctx.drawImage(oc, 0, 0, W, H); ctx.restore();
  }

  function starPath(p, cx, cy, outer, inner, n) {
    for (let i = 0; i < n * 2; i++) {
      const a = -Math.PI / 2 + (i * Math.PI) / n, rad = i % 2 ? inner : outer;
      const x = cx + Math.cos(a) * rad, y = cy + Math.sin(a) * rad;
      i ? p.lineTo(x, y) : p.moveTo(x, y);
    }
    p.closePath();
    return p;
  }
  // A star with soft, rounded points (the gel star).
  function blobStar(s, n, r) {
    const p = new Path2D(), pts = [];
    for (let i = 0; i < n * 2; i++) {
      const a = -Math.PI / 2 + (i * Math.PI) / n, rad = (i % 2 ? 0.48 : 1) * s * (0.92 + r() * 0.16);
      pts.push([Math.cos(a) * rad, Math.sin(a) * rad]);
    }
    for (let i = 0; i < pts.length; i++) {
      const [x0, y0] = pts[i], [x1, y1] = pts[(i + 1) % pts.length];
      const mx = (x0 + x1) / 2, my = (y0 + y1) / 2;
      i ? p.quadraticCurveTo(x0, y0, mx, my) : p.moveTo(mx, my);
    }
    const [x0, y0] = pts[0], [x1, y1] = pts[1];
    p.quadraticCurveTo(x0, y0, (x0 + x1) / 2, (y0 + y1) / 2);
    p.closePath();
    return p;
  }

  // ---- shapes -------------------------------------------------------------------
  // Each is drawn around (0, 0) at size s. body: the glass; gel: what glows
  // inside it; cap: a solid lid; free: no glass around it (a gel blob with a halo).
  function vgrad(ctx, y0, y1, c1, c2) {
    const g = ctx.createLinearGradient(0, y0, 0, y1);
    g.addColorStop(0, c1); g.addColorStop(1, c2);
    return g;
  }
  function gelFill(ctx, path, y0, y1, c1, c2) { ctx.fillStyle = vgrad(ctx, y0, y1, c1, c2); ctx.fill(path); }

  const BUILD = {
    bottle(s, c) {
      const body = rrPath(-0.36 * s, -0.42 * s, 0.72 * s, 1.08 * s, 0.17 * s);
      rr(body, -0.19 * s, -0.6 * s, 0.38 * s, 0.24 * s, 0.05 * s);
      return { body, label: [0, 0.18 * s], cap: rrPath(-0.23 * s, -0.8 * s, 0.46 * s, 0.24 * s, 0.05 * s),
        gel: (ctx) => gelFill(ctx, rrPath(-0.34 * s, -0.12 * s, 0.68 * s, 0.76 * s, 0.15 * s), -0.12 * s, 0.64 * s, c.c1, c.c2) };
    },
    jar(s, c) {
      const body = rrPath(-0.5 * s, -0.36 * s, 1 * s, 0.92 * s, 0.2 * s);
      return { body, label: [0, 0.15 * s], cap: rrPath(-0.53 * s, -0.6 * s, 1.06 * s, 0.27 * s, 0.07 * s),
        gel: (ctx) => {
          ctx.fillStyle = vgrad(ctx, -0.2 * s, 0.55 * s, c.c1, c.c2);
          for (let i = 0; i < 9; i++) { const p = new Path2D(); p.ellipse(-0.3 * s + (i % 3) * 0.3 * s, -0.12 * s + Math.floor(i / 3) * 0.24 * s, 0.17 * s, 0.11 * s, 0.5, 0, 7); ctx.fill(p); }
        } };
    },
    tube(s, c) {
      const body = new Path2D();
      body.moveTo(-0.34 * s, -0.62 * s); body.lineTo(0.34 * s, -0.62 * s); body.lineTo(0.2 * s, 0.5 * s);
      body.quadraticCurveTo(0, 0.6 * s, -0.2 * s, 0.5 * s); body.closePath();
      return { body, label: [0, -0.3 * s], cap: rrPath(-0.16 * s, 0.52 * s, 0.32 * s, 0.26 * s, 0.05 * s),
        gel: (ctx) => { const g = new Path2D(); g.rect(-0.4 * s, -0.15 * s, 0.8 * s, 0.8 * s); gelFill(ctx, g, -0.15 * s, 0.6 * s, c.c1, c.c2); } };
    },
    dropper(s, c) {
      const body = rrPath(-0.32 * s, -0.3 * s, 0.64 * s, 0.9 * s, 0.22 * s);
      rr(body, -0.12 * s, -0.46 * s, 0.24 * s, 0.2 * s, 0.04 * s);
      const cap = new Path2D(); cap.ellipse(0, -0.72 * s, 0.13 * s, 0.2 * s, 0, 0, 7); rr(cap, -0.17 * s, -0.56 * s, 0.34 * s, 0.13 * s, 0.04 * s);
      return { body, cap, label: [0, 0.22 * s],
        gel: (ctx) => { gelFill(ctx, rrPath(-0.3 * s, -0.05 * s, 0.6 * s, 0.64 * s, 0.2 * s), -0.05 * s, 0.6 * s, c.c1, c.c2);
          ctx.fillStyle = rgba(c.c2, 0.6); ctx.fill(rrPath(-0.05 * s, -0.46 * s, 0.1 * s, 0.7 * s, 0.05 * s)); } };
    },
    cube(s, c) {
      const a = 0.55 * s, h = a * 0.58;
      const top = new Path2D(); top.moveTo(0, -2 * h); top.lineTo(a, -h); top.lineTo(0, 0); top.lineTo(-a, -h); top.closePath();
      const left = new Path2D(); left.moveTo(-a, -h); left.lineTo(0, 0); left.lineTo(0, 2 * h); left.lineTo(-a, h); left.closePath();
      const right = new Path2D(); right.moveTo(a, -h); right.lineTo(0, 0); right.lineTo(0, 2 * h); right.lineTo(a, h); right.closePath();
      const body = new Path2D(); body.moveTo(0, -2 * h); body.lineTo(a, -h); body.lineTo(a, h); body.lineTo(0, 2 * h); body.lineTo(-a, h); body.lineTo(-a, -h); body.closePath();
      return { body, label: [-a / 2, h * 0.4], labelRot: 0.52, shrinkGel: 0.12,
        gel: (ctx) => { ctx.fillStyle = mix(c.c1, "#ffffff", 0.35); ctx.fill(top); ctx.fillStyle = c.c1; ctx.fill(left); ctx.fillStyle = c.c2; ctx.fill(right); } };
    },
    star(s, c, r) {
      const body = blobStar(0.62 * s, 5, r);
      return { body, free: true, label: [0.08 * s, 0.05 * s], gel: (ctx) => { ctx.fillStyle = vgrad(ctx, -0.6 * s, 0.6 * s, c.c1, c.c2); ctx.fill(body); } };
    },
    wave(s, c) {
      const body = new Path2D(), n = 64, hh = 0.68 * s, w = 0.3 * s, amp = 0.09 * s;
      for (let i = 0; i <= n; i++) { const t = i / n, y = -hh + 2 * hh * t, x = w + amp * Math.sin(t * Math.PI * 6); i ? body.lineTo(x, y) : body.moveTo(x, y); }
      for (let i = n; i >= 0; i--) { const t = i / n, y = -hh + 2 * hh * t, x = -w - amp * Math.sin(t * Math.PI * 6); body.lineTo(x, y); }
      body.closePath();
      return { body, free: true, label: [0, 0.3 * s], gel: (ctx) => { ctx.fillStyle = vgrad(ctx, -hh, hh, c.c1, c.c2); ctx.fill(body); } };
    },
    orb(s, c) {
      const body = new Path2D(); body.arc(0, 0, 0.55 * s, 0, 7);
      return { body, label: [0, 0.3 * s],
        gel: (ctx) => { const g = ctx.createLinearGradient(0, -0.55 * s, 0, 0.55 * s);
          g.addColorStop(0, mix(c.c1, "#ffffff", 0.25)); g.addColorStop(0.47, c.c1); g.addColorStop(0.53, c.c2); g.addColorStop(1, mix(c.c2, "#000000", 0.1));
          ctx.fillStyle = g; ctx.fill(body); } };
    },
    capsule(s, c) {
      const body = rrPath(-0.25 * s, -0.62 * s, 0.5 * s, 1.24 * s, 0.25 * s);
      return { body, label: [0, 0.3 * s], gel: (ctx) => { ctx.fillStyle = c.c1; ctx.fillRect(-0.3 * s, -0.7 * s, 0.6 * s, 0.7 * s); ctx.fillStyle = c.c2; ctx.fillRect(-0.3 * s, 0, 0.6 * s, 0.7 * s); } };
    },
    cup(s, c) {
      const body = new Path2D();
      body.moveTo(-0.42 * s, -0.5 * s); body.lineTo(0.42 * s, -0.5 * s); body.lineTo(0.3 * s, 0.6 * s);
      body.quadraticCurveTo(0, 0.68 * s, -0.3 * s, 0.6 * s); body.closePath();
      return { body, label: [0, 0.28 * s],
        gel: (ctx) => { ctx.fillStyle = c.c1; ctx.fillRect(-0.5 * s, -0.3 * s, s, 0.42 * s); ctx.fillStyle = c.c2; ctx.fillRect(-0.5 * s, 0.1 * s, s, 0.6 * s); },
        detail: (ctx) => { // the straw
          ctx.lineCap = "round"; ctx.lineJoin = "round"; ctx.lineWidth = 0.07 * s; ctx.strokeStyle = "rgba(255,255,255,.85)";
          ctx.beginPath(); ctx.moveTo(0.06 * s, 0.4 * s); ctx.lineTo(0.12 * s, -0.72 * s); ctx.lineTo(0.38 * s, -0.84 * s); ctx.stroke();
        } };
    },
    coin(s, c) {
      const body = new Path2D(); body.arc(0, 0, 0.52 * s, 0, 7);
      return { body,
        gel: (ctx) => { const g = ctx.createRadialGradient(-0.15 * s, -0.2 * s, 0.05 * s, 0, 0, 0.6 * s);
          g.addColorStop(0, mix(c.c1, "#ffffff", 0.3)); g.addColorStop(1, c.c2); ctx.fillStyle = g; ctx.fill(body); },
        detail: (ctx) => {
          ctx.strokeStyle = "rgba(255,255,255,.75)"; ctx.lineWidth = 0.025 * s; ctx.beginPath(); ctx.arc(0, 0, 0.42 * s, 0, 7); ctx.stroke();
          ctx.fillStyle = "rgba(255,255,255,.92)"; ctx.font = "800 " + Math.round(0.5 * s) + "px system-ui, sans-serif";
          ctx.textAlign = "center"; ctx.textBaseline = "middle"; ctx.fillText("₱", 0, 0.03 * s);
        } };
    },
    wifi(s, c) {
      const body = new Path2D();
      for (const [rad, w] of [[0.62, 0.14], [0.38, 0.14]]) {
        const ro = rad * s + (w * s) / 2, ri = rad * s - (w * s) / 2, a0 = -Math.PI * 0.78, a1 = -Math.PI * 0.22;
        body.moveTo(Math.cos(a0) * ro, 0.3 * s + Math.sin(a0) * ro);
        body.arc(0, 0.3 * s, ro, a0, a1);
        body.arc(0, 0.3 * s, ri, a1, a0, true);
        body.closePath();
      }
      body.moveTo(0.1 * s, 0.3 * s); body.arc(0, 0.3 * s, 0.1 * s, 0, 7);
      return { body, free: true, gel: (ctx) => { ctx.fillStyle = vgrad(ctx, -0.4 * s, 0.4 * s, c.c1, c.c2); ctx.fill(body); } };
    },
    face(s, c) {
      const body = rrPath(-0.52 * s, -0.46 * s, 1.04 * s, 0.92 * s, 0.2 * s);
      return { body,
        gel: (ctx) => { ctx.fillStyle = vgrad(ctx, -0.5 * s, 0.5 * s, mix(c.c1, "#ffffff", 0.15), c.c1); ctx.fill(body); },
        detail: (ctx) => {
          for (const x of [-0.18, 0.18]) {
            ctx.fillStyle = "#fff"; ctx.beginPath(); ctx.ellipse(x * s, -0.08 * s, 0.11 * s, 0.13 * s, 0, 0, 7); ctx.fill();
            ctx.fillStyle = "#15161F"; ctx.beginPath(); ctx.arc(x * s + 0.025 * s, -0.06 * s, 0.065 * s, 0, 7); ctx.fill();
            ctx.strokeStyle = "rgba(255,255,255,.8)"; ctx.lineWidth = 0.03 * s; ctx.lineCap = "round";
            ctx.beginPath(); ctx.moveTo(x * s - 0.07 * s, -0.3 * s); ctx.lineTo(x * s + 0.05 * s, -0.25 * s); ctx.stroke();
          }
          ctx.fillStyle = "rgba(255,255,255,.6)"; ctx.fill(rrPath(-0.24 * s, 0.17 * s, 0.48 * s, 0.09 * s, 0.03 * s));
        } };
    },
    // The owner's logo, set in a frosted glass badge.
    logo(s, c, r, data) {
      const body = rrPath(-0.5 * s, -0.5 * s, s, s, 0.26 * s);
      return { body, noSparkle: true,
        gel: (ctx) => { const g = ctx.createRadialGradient(0, 0, 0.1 * s, 0, 0, 0.75 * s); g.addColorStop(0, rgba(c.c1, 0.9)); g.addColorStop(1, rgba(c.c2, 0.6)); ctx.fillStyle = g; ctx.fill(body); },
        detail: (ctx) => {
          if (!data.logo) return;
          const tile = rrPath(-0.32 * s, -0.32 * s, 0.64 * s, 0.64 * s, 0.14 * s);
          ctx.save(); ctx.clip(tile);
          ctx.drawImage(data.logo, -0.32 * s, -0.32 * s, 0.64 * s, 0.64 * s);
          if (!data.logo.glassy) glaze(ctx, tile, -0.32 * s, -0.32 * s, 0.64 * s, 0.64 * s);
          ctx.restore();
          ctx.strokeStyle = "rgba(255,255,255,.8)"; ctx.lineWidth = 0.015 * s; ctx.stroke(rrPath(-0.32 * s, -0.32 * s, 0.64 * s, 0.64 * s, 0.14 * s));
        } };
    },
  };

  // ---- one glass object -----------------------------------------------------------
  function drawObject(ctx, o, data, opts) {
    const s = o.s, shape = BUILD[o.type](s, o, o.r, data);
    const night = opts.bg === "night";
    ctx.save();
    ctx.translate(o.x, o.y);
    ctx.rotate(o.rot);
    // shadow on the paper
    soft(ctx, 0.08 * s, (c) => { c.translate(0.02 * s, 0.08 * s); c.fillStyle = night ? "rgba(0,0,0,.45)" : rgba(o.c2, 0.22); c.fill(shape.body); });
    if (shape.free) {
      // a gel blob with a bright frosted halo, no glass around it
      soft(ctx, 0.07 * s, (c) => { c.lineWidth = 0.14 * s; c.lineJoin = "round"; c.strokeStyle = night ? "rgba(255,255,255,.35)" : "rgba(255,255,255,.95)"; c.stroke(shape.body); c.fillStyle = "rgba(255,255,255,.6)"; c.fill(shape.body); });
      soft(ctx, 0.05 * s, (c) => shape.gel(c));
    } else {
      // the glass: milky, a little tinted
      soft(ctx, 0.03 * s, (c) => { c.lineWidth = 0.06 * s; c.strokeStyle = night ? "rgba(255,255,255,.18)" : "rgba(255,255,255,.9)"; c.stroke(shape.body); });
      ctx.fillStyle = night ? "rgba(255,255,255,.12)" : vgrad(ctx, -0.6 * s, 0.6 * s, "rgba(255,255,255,.75)", rgba(mix(o.c1, "#ffffff", 0.75), 0.6));
      ctx.fill(shape.body);
      // what's inside glows through it, blurred by the glass
      ctx.save();
      ctx.clip(shape.body);
      soft(ctx, 0.045 * s, (c) => { if (shape.shrinkGel) c.scale(1 - shape.shrinkGel, 1 - shape.shrinkGel); shape.gel(c); });
      // frost over it, and a bright inner edge
      ctx.fillStyle = vgrad(ctx, -0.7 * s, 0.7 * s, night ? "rgba(255,255,255,.16)" : "rgba(255,255,255,.34)", night ? "rgba(255,255,255,0)" : "rgba(255,255,255,.06)");
      ctx.fill(shape.body);
      soft(ctx, 0.02 * s, (c) => { c.lineWidth = 0.07 * s; c.strokeStyle = "rgba(255,255,255,.85)"; c.stroke(shape.body); });
      // a highlight down the left side
      soft(ctx, 0.04 * s, (c) => { c.fillStyle = "rgba(255,255,255,.55)"; c.fill(rrPath(-0.32 * s, -0.4 * s, 0.09 * s, 0.6 * s, 0.045 * s)); });
      ctx.restore();
      ctx.lineWidth = 2; ctx.strokeStyle = night ? "rgba(255,255,255,.22)" : "rgba(40,30,80,.07)"; ctx.stroke(shape.body);
    }
    // sparkles
    if (opts.sparkles && !shape.noSparkle) {
      ctx.save(); ctx.clip(shape.body);
      ctx.fillStyle = "rgba(255,255,255,.92)";
      const n = 2 + Math.floor(o.r() * 5);
      for (let i = 0; i < n; i++) {
        const x = between(o.r, -0.35, 0.35) * s, y = between(o.r, -0.35, 0.5) * s, k = between(o.r, 0.025, 0.055) * s;
        ctx.fill(starPath(new Path2D(), x, y, k, k * 0.32, 4));
      }
      ctx.restore();
    }
    // a little printed label, like on a real product
    if (shape.label && o.text) {
      ctx.save();
      ctx.translate(shape.label[0], shape.label[1]);
      if (shape.labelRot) ctx.transform(1, shape.labelRot, 0, 1, 0, 0);
      ctx.fillStyle = "rgba(255,255,255,.88)"; ctx.textAlign = "center"; ctx.textBaseline = "middle";
      const lines = o.text.split("\n"), fs = Math.round(0.075 * s);
      lines.forEach((ln, i) => {
        ctx.font = (i ? "600 " : "800 ") + (i ? Math.round(fs * 0.62) : fs) + "px system-ui, sans-serif";
        ctx.fillText(ln, 0, (i - (lines.length - 1) / 2) * fs * 0.95);
      });
      ctx.restore();
    }
    // solid parts: lids and the like
    if (shape.cap) {
      ctx.fillStyle = vgrad(ctx, -0.85 * s, -0.5 * s, mix(o.cap, "#ffffff", 0.25), o.cap);
      ctx.fill(shape.cap);
      soft(ctx, 0.01 * s, (c) => { c.fillStyle = "rgba(255,255,255,.35)"; c.fill(rrPath(-0.15 * s, -0.77 * s, 0.05 * s, 0.14 * s, 0.02 * s)); });
    }
    if (shape.detail) shape.detail(ctx);
    ctx.restore();
  }

  // ---- text -------------------------------------------------------------------------
  function wrap(ctx, text, maxW) {
    const words = String(text).split(/\s+/).filter(Boolean), lines = [];
    let line = "";
    for (const w of words) {
      const t = line ? line + " " + w : w;
      if (ctx.measureText(t).width <= maxW || !line) line = t; else { lines.push(line); line = w; }
    }
    if (line) lines.push(line);
    return lines;
  }
  // The biggest size (up to max) at which text fits in at most `rows` lines.
  function fit(ctx, text, font, max, min, maxW, rows) {
    let size = max, lines = [];
    for (; size >= min; size -= 4) {
      ctx.font = font(size);
      lines = wrap(ctx, text, maxW);
      if (lines.length <= rows && lines.every((l) => ctx.measureText(l).width <= maxW)) break;
    }
    return { size: Math.max(size, min), lines };
  }

  function headline(ctx, data, opts, x, y, maxW, align, max, ink) {
    const font = (sz) => data.fontWeight + " " + sz + "px " + data.fontFamily;
    const { size, lines } = fit(ctx, opts.headline || data.name, font, max, 48, maxW, 3);
    ctx.save();
    ctx.font = font(size); ctx.textAlign = align; ctx.textBaseline = "alphabetic"; ctx.fillStyle = ink;
    ctx.shadowColor = ink === "#FFFFFF" ? "rgba(0,0,0,.35)" : "rgba(255,255,255,.95)"; ctx.shadowBlur = 36;
    let cy = y + size * 0.86;
    for (const ln of lines) { ctx.fillText(ln, x, cy); cy += size * 1.02; }
    cy -= size * 1.02;
    let bottom = cy + size * 0.24;
    if (opts.tagline) {
      ctx.shadowBlur = 20;
      ctx.font = "500 " + Math.round(W * 0.03) + "px system-ui, sans-serif";
      ctx.fillStyle = ink === "#FFFFFF" ? "rgba(255,255,255,.82)" : "rgba(30,31,46,.68)";
      for (const ln of wrap(ctx, opts.tagline, maxW)) { bottom += W * 0.045; ctx.fillText(ln, x, bottom); }
      bottom += W * 0.012;
    }
    ctx.restore();
    return bottom;
  }

  // ---- the frosted card with the QR codes ---------------------------------------------
  function card(ctx, data, opts, pal, x, y, w, withTitle, measureOnly) {
    const pad = w * 0.07, inner = w - pad * 2;
    const qrs = data.hasWifi ? [[data.qrWifi, "Scan to join", data.ssid], [data.qrPortal, "Scan to open the page", ""]] : [[data.qrPortal, "Scan to open the page", ""]];
    const gap = w * 0.06, q = Math.min(inner * 0.44, (inner - gap * (qrs.length - 1)) / qrs.length);
    const showLogo = opts.logoCard && data.logo;
    let h = pad;
    const titleSize = Math.round(w * 0.075);
    if (withTitle) { ctx.font = data.fontWeight + " " + titleSize + "px " + data.fontFamily; h += wrap(ctx, opts.headline || data.name, inner - (showLogo ? titleSize * 1.4 : 0)).length * titleSize * 1.05 + w * 0.04; }
    else if (showLogo) h += w * 0.13;
    h += q + w * 0.075; // codes and captions
    ctx.font = "500 " + Math.round(w * 0.031) + "px system-ui, sans-serif";
    const steps = opts.steps ? wrap(ctx, data.steps, inner) : [];
    h += steps.length * w * 0.045 + (steps.length ? w * 0.025 : 0);
    // rate pills, wrapped into rows
    const pillFont = "700 " + Math.round(w * 0.03) + "px system-ui, sans-serif", ph = w * 0.06, pg = w * 0.018;
    ctx.font = pillFont;
    const rows = [];
    if (opts.rates) {
      let row = [], rw = 0;
      for (const t of data.rates) {
        const pw = ctx.measureText(t).width + ph * 0.9;
        if (row.length && rw + pg + pw > inner) { rows.push([row, rw]); row = []; rw = 0; }
        row.push([t, pw]); rw += (row.length > 1 ? pg : 0) + pw;
      }
      if (row.length) rows.push([row, rw]);
      h += rows.length * (ph + pg) + w * 0.01;
    }
    h += pad * 0.8;
    if (measureOnly) return h;

    const r = w * 0.07, path = rrPath(x, y, w, h, r);
    const night = opts.bg === "night";
    // the card is real frosted glass: the art behind it shows through, blurred
    soft(ctx, 26, (c) => { c.translate(0, 16); c.fillStyle = night ? "rgba(0,0,0,.5)" : "rgba(52,36,104,.22)"; c.fill(path); });
    if (FILTER) {
      const snap = document.createElement("canvas"); snap.width = W; snap.height = H; snap.getContext("2d").drawImage(ctx.canvas, 0, 0);
      ctx.save(); ctx.clip(path); ctx.filter = "blur(34px) saturate(1.5)"; ctx.drawImage(snap, 0, 0); ctx.restore();
    }
    ctx.fillStyle = night ? vgrad(ctx, y, y + h, "rgba(40,40,58,.62)", "rgba(28,28,40,.72)") : vgrad(ctx, y, y + h, FILTER ? "rgba(255,255,255,.66)" : "rgba(255,255,255,.9)", FILTER ? "rgba(255,255,255,.5)" : "rgba(255,255,255,.86)");
    ctx.fill(path);
    ctx.save(); ctx.clip(path);
    soft(ctx, 6, (c) => { c.lineWidth = 16; c.strokeStyle = night ? "rgba(255,255,255,.18)" : "rgba(255,255,255,.95)"; c.stroke(path); });
    ctx.restore();

    const ink = night ? "#FFFFFF" : "#1E1F2E", muted = night ? "rgba(255,255,255,.72)" : "rgba(30,31,46,.62)";
    let cy = y + pad;
    if (withTitle) {
      ctx.font = data.fontWeight + " " + titleSize + "px " + data.fontFamily; ctx.fillStyle = ink; ctx.textAlign = "center"; ctx.textBaseline = "alphabetic";
      const tw = inner - (showLogo ? titleSize * 1.4 : 0), lines = wrap(ctx, opts.headline || data.name, tw);
      const lw = Math.max(...lines.map((l) => ctx.measureText(l).width));
      const startX = x + w / 2 + (showLogo ? titleSize * 0.7 : 0);
      if (showLogo) drawLogo(ctx, data.logo, startX - lw / 2 - titleSize * 1.35, cy + titleSize * 0.02, titleSize * 1.05);
      lines.forEach((ln, i) => ctx.fillText(ln, startX, cy + titleSize * (0.84 + i * 1.05)));
      cy += lines.length * titleSize * 1.05 + w * 0.04;
    } else if (showLogo) {
      drawLogo(ctx, data.logo, x + w / 2 - w * 0.05, cy - w * 0.005, w * 0.1);
      cy += w * 0.13;
    }
    // the codes, each on a white tile so they always scan
    const total = qrs.length * q + (qrs.length - 1) * gap;
    qrs.forEach(([img, label, extra], i) => {
      const qx = x + (w - total) / 2 + i * (q + gap);
      ctx.save();
      ctx.shadowColor = "rgba(52,36,104,.18)"; ctx.shadowBlur = 18; ctx.shadowOffsetY = 6;
      ctx.fillStyle = "#FFFFFF"; ctx.fill(rrPath(qx, cy, q, q, q * 0.09));
      ctx.restore();
      if (img) ctx.drawImage(img, qx + q * 0.03, cy + q * 0.03, q * 0.94, q * 0.94);
      // numbered caption
      const n = String(i + 1), fs = Math.round(w * 0.029), by = cy + q + w * 0.045;
      ctx.font = "650 " + fs + "px system-ui, sans-serif";
      const text = label + (extra ? " “" + extra + "”" : "");
      let tw = ctx.measureText(text).width, size = fs;
      while (tw + fs * 1.6 > q + gap * 0.9 && size > 14) { size -= 1; ctx.font = "650 " + size + "px system-ui, sans-serif"; tw = ctx.measureText(text).width; }
      const bx = qx + q / 2 - (tw + size * 1.5) / 2;
      ctx.fillStyle = pal[i % pal.length]; ctx.beginPath(); ctx.arc(bx + size * 0.55, by - size * 0.34, size * 0.62, 0, 7); ctx.fill();
      ctx.fillStyle = "#fff"; ctx.textAlign = "center"; ctx.textBaseline = "alphabetic"; ctx.fillText(n, bx + size * 0.55, by);
      ctx.fillStyle = ink; ctx.textAlign = "left"; ctx.fillText(text, bx + size * 1.5, by);
    });
    cy += q + w * 0.075;
    if (steps.length) {
      ctx.font = "500 " + Math.round(w * 0.031) + "px system-ui, sans-serif"; ctx.fillStyle = muted; ctx.textAlign = "center";
      steps.forEach((ln, i) => ctx.fillText(ln, x + w / 2, cy + w * 0.032 + i * w * 0.045));
      cy += steps.length * w * 0.045 + w * 0.025;
    }
    for (const [row, rw] of rows) {
      let px = x + (w - rw) / 2;
      row.forEach(([t, pw], i) => {
        const col = pal[(i + 2) % pal.length];
        ctx.fillStyle = night ? rgba(col, 0.35) : rgba(mix(col, "#ffffff", 0.55), 0.85);
        ctx.fill(rrPath(px, cy, pw, ph, ph / 2));
        ctx.lineWidth = 2; ctx.strokeStyle = "rgba(255,255,255,.9)"; ctx.stroke(rrPath(px, cy, pw, ph, ph / 2));
        ctx.font = pillFont; ctx.fillStyle = ink; ctx.textAlign = "center"; ctx.textBaseline = "middle";
        ctx.fillText(t, px + pw / 2, cy + ph / 2 + 1);
        px += pw + pg;
      });
      cy += ph + pg;
    }
    ctx.textBaseline = "alphabetic";
    return h;
  }

  function drawLogo(ctx, img, x, y, size) {
    ctx.save();
    ctx.shadowColor = "rgba(52,36,104,.25)"; ctx.shadowBlur = 14; ctx.shadowOffsetY = 4;
    ctx.fillStyle = "#fff"; ctx.fill(rrPath(x, y, size, size, size * 0.26));
    ctx.restore();
    const tile = rrPath(x, y, size, size, size * 0.26);
    ctx.save(); ctx.clip(tile); ctx.drawImage(img, x, y, size, size);
    if (!img.glassy) glaze(ctx, tile, x, y, size, size);
    ctx.restore();
  }

  // A glass finish over a logo: a soft shine across the top and a bright rim,
  // so any uploaded logo sits on the poster like the glass objects around it.
  function glaze(ctx, tile, x, y, w, h) {
    const g = ctx.createLinearGradient(0, y, 0, y + h);
    g.addColorStop(0, "rgba(255,255,255,.42)"); g.addColorStop(0.45, "rgba(255,255,255,.06)"); g.addColorStop(0.5, "rgba(255,255,255,0)");
    ctx.fillStyle = g; ctx.fill(tile);
    soft(ctx, Math.max(1, w * 0.02), (c) => { c.lineWidth = w * 0.07; c.strokeStyle = "rgba(255,255,255,.75)"; c.stroke(tile); });
  }

  // ---- the page ----------------------------------------------------------------------
  function background(ctx, opts, pal) {
    if (opts.bg === "night") {
      ctx.fillStyle = "#15161F"; ctx.fillRect(0, 0, W, H);
    } else {
      ctx.fillStyle = opts.bg === "soft" ? "#F4F1F8" : "#FFFFFF"; ctx.fillRect(0, 0, W, H);
    }
    if (opts.bg !== "white") {
      const spots = [[0.05, 0.05, 0], [0.98, 0.25, 1], [0.9, 0.98, 2], [0.02, 0.8, 3]];
      for (const [px, py, i] of spots) {
        const g = ctx.createRadialGradient(px * W, py * H, 0, px * W, py * H, W * 0.75);
        g.addColorStop(0, rgba(pal[i % pal.length], opts.bg === "night" ? 0.35 : 0.42)); g.addColorStop(1, rgba(pal[i % pal.length], 0));
        ctx.fillStyle = g; ctx.fillRect(0, 0, W, H);
      }
    }
  }

  function footer(ctx, data, opts, ink) {
    const y = H * 0.955;
    ctx.save();
    ctx.fillStyle = ink === "#FFFFFF" ? "rgba(255,255,255,.55)" : "rgba(30,31,46,.42)";
    ctx.font = "300 " + Math.round(W * 0.07) + "px system-ui, sans-serif"; ctx.textAlign = "left"; ctx.textBaseline = "alphabetic";
    ctx.fillText("*", W * 0.06, y + W * 0.02);
    ctx.font = "800 " + Math.round(W * 0.019) + "px system-ui, sans-serif"; ctx.textAlign = "right";
    ctx.fillText(data.name.toUpperCase() + "  ·  No. " + String(opts.seed).padStart(4, "0"), W * 0.94, y - W * 0.024);
    ctx.font = "500 " + Math.round(W * 0.019) + "px system-ui, sans-serif";
    ctx.fillText("Drop a coin. Get online.", W * 0.94, y);
    ctx.restore();
  }

  function labelFor(r, data) {
    const coin = pick(r, data.coins);
    return pick(r, [
      data.name.toUpperCase().slice(0, 14) + "\nPISO WIFI",
      "0" + (1 + Math.floor(r() * 9)) + "/0" + (1 + Math.floor(r() * 9)),
      "₱" + coin + "\nDROP A COIN",
      data.rates.length ? data.rates[Math.floor(r() * data.rates.length)].toUpperCase() : "WIFI",
      "ONLINE\nIN SECONDS",
      "",
    ]);
  }

  function makeObject(r, type, x, y, s, pal, data) {
    const cols = shuffle(r, pal);
    return { type, x, y, s, rot: between(r, -0.6, 0.6), c1: cols[0], c2: cols[1], cap: cols[2], r, text: labelFor(r, data) };
  }

  function types(r, opts, data) {
    const list = opts.shapes.filter((t) => BUILD[t]);
    const pool = list.length ? list : Object.keys(SHAPES);
    return () => pick(r, pool);
  }

  function draw(canvas, opts, data) {
    canvas.width = W; canvas.height = H;
    const ctx = canvas.getContext("2d");
    const r = rng(opts.seed * 7919 + 13);
    const pal = shuffle(r, opts.palette === "brand" || !PALETTES[opts.palette]
      ? [data.accent, data.button, mix(data.accent, "#ffffff", 0.4), "#F39BD3", "#FFA27E", "#93C9FF"]
      : PALETTES[opts.palette].colors);
    const nextType = types(r, opts, data);
    const ink = opts.bg === "night" ? "#FFFFFF" : "#1E1F2E";
    const withLogoArt = opts.logoArt && data.logo;
    background(ctx, opts, pal);

    if (opts.layout === "grid") {
      const cols = 3, rows = 4, tw = W / cols, th = H / rows;
      let logoAt = withLogoArt ? Math.floor(r() * cols * rows) : -1;
      for (let i = 0; i < cols * rows; i++) {
        const tx = (i % cols) * tw, ty = Math.floor(i / cols) * th, col = pal[i % pal.length];
        ctx.save();
        ctx.beginPath(); ctx.rect(tx, ty, tw, th); ctx.clip();
        const g = ctx.createLinearGradient(tx, ty, tx, ty + th);
        g.addColorStop(0, opts.bg === "night" ? mix(col, "#15161F", 0.55) : mix(col, "#ffffff", 0.18));
        g.addColorStop(1, opts.bg === "night" ? mix(col, "#15161F", 0.7) : col);
        ctx.fillStyle = g; ctx.fillRect(tx, ty, tw, th);
        const big = r() < 0.3;
        const o = makeObject(r, i === logoAt ? "logo" : nextType(), tx + tw * between(r, 0.4, 0.6), ty + th * (big ? between(r, 0.6, 0.8) : between(r, 0.45, 0.55)), Math.min(tw, th) * (big ? between(r, 0.95, 1.2) : between(r, 0.6, 0.75)), pal, data);
        if (i === logoAt) { o.rot *= 0.3; o.s = Math.min(tw, th) * 0.62; o.y = ty + th / 2; }
        o.c1 = mix(pal[(i + 1) % pal.length], "#ffffff", 0.1); o.c2 = pal[(i + 3) % pal.length];
        drawObject(ctx, o, data, opts);
        ctx.restore();
      }
      const cw = W * 0.8, ch = card(ctx, data, opts, pal, 0, 0, cw, true, true);
      card(ctx, data, opts, pal, (W - cw) / 2, Math.max(H * 0.08, (H - ch) / 2), cw, true, false);
    } else if (opts.layout === "spotlight") {
      // one big object filling the top half and bleeding off a side; the
      // words sit in the opposite top corner, the card in the lower half
      const right = r() < 0.5;
      const ax = right ? 0.1 : 0.9;
      drawObject(ctx, makeObject(r, nextType(), ax * W, H * 0.9, W * between(r, 0.2, 0.26), pal, data), data, opts);
      drawObject(ctx, makeObject(r, nextType(), (1 - ax) * W, H * between(r, 0.62, 0.7), W * between(r, 0.18, 0.22), pal, data), data, opts);
      const hero = makeObject(r, withLogoArt ? "logo" : nextType(), W * (right ? 0.66 : 0.34), H * 0.3, W * (withLogoArt ? 0.6 : between(r, 0.74, 0.86)), pal, data);
      hero.rot *= 0.6;
      drawObject(ctx, hero, data, opts);
      const bottom = headline(ctx, data, opts, right ? W * 0.07 : W * 0.93, H * 0.045, W * 0.62, right ? "left" : "right", Math.round(W * 0.11), ink);
      const cw = W * 0.84, ch = card(ctx, data, opts, pal, 0, 0, cw, false, true);
      const cardY = Math.min(H * 0.92 - ch, Math.max(bottom + H * 0.03, H * 0.53));
      card(ctx, data, opts, pal, (W - cw) / 2, cardY, cw, false, false);
      footer(ctx, data, opts, ink);
    } else {
      // scatter: objects around the edges, words and card in the middle
      const n = 8 + Math.floor(r() * 3), start = r() * Math.PI * 2;
      const objs = [];
      for (let i = 0; i < n; i++) {
        const a = start + (i / n) * Math.PI * 2 + between(r, -0.18, 0.18);
        const x = W / 2 + Math.cos(a) * W * between(r, 0.42, 0.56), y = H / 2 + Math.sin(a) * H * between(r, 0.4, 0.52);
        objs.push(makeObject(r, nextType(), x, y, W * between(r, 0.24, 0.36), pal, data));
      }
      if (withLogoArt) { const o = objs[Math.floor(r() * objs.length)]; o.type = "logo"; o.s = W * 0.24; o.rot *= 0.4; }
      for (const o of shuffle(r, objs)) drawObject(ctx, o, data, opts);
      const bottom = headline(ctx, data, opts, W / 2, H * 0.17, W * 0.8, "center", Math.round(W * 0.13), ink);
      const cw = W * 0.8, ch = card(ctx, data, opts, pal, 0, 0, cw, false, true);
      card(ctx, data, opts, pal, (W - cw) / 2, Math.min(H * 0.9 - ch, bottom + H * 0.035), cw, false, false);
      footer(ctx, data, opts, ink);
    }
  }

  // ---- the glass filter: any uploaded logo, made of glass -------------------------------------
  // Its own colours, blurred and saturated, become the gel glowing behind it;
  // frost goes over that, then the logo itself with a soft white halo, a
  // shine across the top, a bright rim and a couple of sparkles.
  function glassify(img) {
    const N = 512, c = document.createElement("canvas");
    c.width = c.height = N;
    const ctx = c.getContext("2d"), tile = rrPath(0, 0, N, N, N * 0.24);
    ctx.save();
    ctx.clip(tile);
    ctx.fillStyle = "#fff"; ctx.fillRect(0, 0, N, N);
    // the gel: the logo, huge and melted
    const glow = (k, blur, sat) => {
      if (FILTER) { ctx.save(); ctx.filter = "blur(" + blur + "px) saturate(" + sat + ")"; ctx.drawImage(img, N * (1 - k) / 2, N * (1 - k) / 2, N * k, N * k); ctx.restore(); }
      else { const t = document.createElement("canvas"); t.width = t.height = 12; t.getContext("2d").drawImage(img, 0, 0, 12, 12); ctx.imageSmoothingQuality = "high"; ctx.drawImage(t, N * (1 - k) / 2, N * (1 - k) / 2, N * k, N * k); }
    };
    glow(1.9, N * 0.09, 1.8);
    glow(1.2, N * 0.05, 1.5);
    // frost
    const f = ctx.createLinearGradient(0, 0, 0, N);
    f.addColorStop(0, "rgba(255,255,255,.42)"); f.addColorStop(1, "rgba(255,255,255,.16)");
    ctx.fillStyle = f; ctx.fillRect(0, 0, N, N);
    // the logo, floating in the glass
    const k = 0.7, x = N * (1 - k) / 2;
    ctx.save();
    ctx.shadowColor = "rgba(255,255,255,.95)"; ctx.shadowBlur = N * 0.06;
    ctx.drawImage(img, x, x, N * k, N * k);
    ctx.restore();
    // sparkles
    ctx.fillStyle = "rgba(255,255,255,.95)";
    for (const [sx, sy, sz] of [[0.84, 0.18, 0.045], [0.14, 0.8, 0.032], [0.8, 0.84, 0.024]]) ctx.fill(starPath(new Path2D(), sx * N, sy * N, sz * N, sz * N * 0.3, 4));
    glaze(ctx, tile, 0, 0, N, N);
    ctx.restore();
    c.glassy = true;
    return c;
  }

  // ---- PDF: one A4 page holding the poster as a JPEG -------------------------------------
  async function pdf(canvas) {
    const jpeg = new Uint8Array(await (await new Promise((res) => canvas.toBlob(res, "image/jpeg", 0.92))).arrayBuffer());
    const enc = new TextEncoder(), parts = [], offsets = [];
    let len = 0;
    const add = (x) => { const b = typeof x === "string" ? enc.encode(x) : x; parts.push(b); len += b.length; };
    const obj = (n, body) => { offsets[n] = len; add(n + " 0 obj\n"); body(); add("\nendobj\n"); };
    const content = "q 595.28 0 0 841.89 0 0 cm /Im0 Do Q";
    add("%PDF-1.4\n%\xE2\xE3\xCF\xD3\n");
    obj(1, () => add("<< /Type /Catalog /Pages 2 0 R >>"));
    obj(2, () => add("<< /Type /Pages /Kids [3 0 R] /Count 1 >>"));
    obj(3, () => add("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595.28 841.89] /Resources << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>"));
    obj(4, () => { add("<< /Type /XObject /Subtype /Image /Width " + canvas.width + " /Height " + canvas.height + " /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length " + jpeg.length + " >>\nstream\n"); add(jpeg); add("\nendstream"); });
    obj(5, () => add("<< /Length " + content.length + " >>\nstream\n" + content + "\nendstream"));
    const xref = len;
    add("xref\n0 6\n0000000000 65535 f \n" + offsets.slice(1).map((o) => String(o).padStart(10, "0") + " 00000 n \n").join(""));
    add("trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n" + xref + "\n%%EOF\n");
    return new Blob(parts, { type: "application/pdf" });
  }

  window.GlassSign = { draw, pdf, glassify, PALETTES, SHAPES, LAYOUTS, BGS, W, H };
})();
