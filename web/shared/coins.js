// Stylized Philippine New Generation Currency coins, drawn as SVG.
// Simplified on purpose (a generic bust and seal, no official portraits or
// seals); shapes, metals, relative sizes and each coin's native plant follow
// the real series:
//   ₱20 bimetallic 30 mm, nilad       ₱10 round 27 mm, kapa-kapa
//   ₱5 nine-sided 25 mm, tayabak      ₱1 round 23 mm, waling-waling
// The front has the bust and the value; the back has the plant and the seal.
"use strict";

(function () {
  const NS = "http://www.w3.org/2000/svg";
  const MM = { 1: 23, 5: 25, 10: 27, 20: 30 };
  const PLANT = { 1: "WALING-WALING", 5: "TAYABAK", 10: "KAPA-KAPA", 20: "NILAD" };
  const DENOMS = [20, 10, 5, 1];
  const INK = "#858A97";     // engraved relief
  const SHINE = "#F4F5F8";   // its lit edges
  let uid = 0;

  function el(tag, attrs, parent) {
    const e = document.createElementNS(NS, tag);
    for (const k in attrs) e.setAttribute(k, attrs[k]);
    if (parent) parent.append(e);
    return e;
  }

  function stop(g, offset, color) {
    el("stop", { offset, "stop-color": color }, g);
  }

  function ensureDefs() {
    if (document.getElementById("pc-silver")) return;
    const svg = el("svg", { width: 0, height: 0, "aria-hidden": "true" });
    svg.style.position = "absolute";
    const d = el("defs", {}, svg);
    let g = el("radialGradient", { id: "pc-silver", cx: "36%", cy: "30%", r: "78%" }, d);
    stop(g, "0", "#FFFFFF"); stop(g, ".35", "#E6E8EE"); stop(g, ".75", "#BCC0CB"); stop(g, "1", "#979BA8");
    g = el("linearGradient", { id: "pc-rim", x1: "0", y1: "0", x2: "1", y2: "1" }, d);
    stop(g, "0", "#F6F7FA"); stop(g, ".5", "#B4B8C4"); stop(g, "1", "#80848F");
    g = el("radialGradient", { id: "pc-copper", cx: "35%", cy: "28%", r: "80%" }, d);
    stop(g, "0", "#F9D3B3"); stop(g, ".4", "#E3A072"); stop(g, ".8", "#C17241"); stop(g, "1", "#97522D");
    const p = el("pattern", { id: "pc-lines", width: "1.6", height: "4", patternUnits: "userSpaceOnUse" }, d);
    el("rect", { width: ".6", height: "4", fill: "#FFFFFF", opacity: ".45" }, p);
    document.body.append(svg);
  }

  // Nine-sided ₱5 outline with gently bulging edges, like the real coin.
  function nonagon(r) {
    const pts = [];
    for (let i = 0; i < 9; i++) {
      const a = -Math.PI / 2 + (i * 2 * Math.PI) / 9;
      pts.push([50 + r * Math.cos(a), 50 + r * Math.sin(a)]);
    }
    let dpath = `M${pts[0][0].toFixed(2)} ${pts[0][1].toFixed(2)}`;
    for (let i = 0; i < 9; i++) {
      const a = -Math.PI / 2 + ((i + 0.5) * 2 * Math.PI) / 9;
      const [x, y] = pts[(i + 1) % 9];
      const cx = 50 + r * 1.07 * Math.cos(a), cy = 50 + r * 1.07 * Math.sin(a);
      dpath += ` Q${cx.toFixed(2)} ${cy.toFixed(2)} ${x.toFixed(2)} ${y.toFixed(2)}`;
    }
    return dpath + "Z";
  }

  function face(svg, shape, r, clipId) {
    // shape: "circle" | "nonagon"
    const clip = el("clipPath", { id: clipId }, el("defs", {}, svg));
    if (shape === "circle") el("circle", { cx: 50, cy: 50, r }, clip);
    else el("path", { d: nonagon(r) }, clip);
    return clip;
  }

  // Engraved artwork, drawn around (0, 0) about 48 units tall, then placed.
  function relief(parent, x, y, s) {
    return el("g", { transform: `translate(${x} ${y}) scale(${s})`, fill: INK, stroke: "none", "stroke-linecap": "round", "stroke-linejoin": "round" }, parent);
  }

  // A head-and-shoulders bust facing right: a stand-in for the hero's portrait.
  function bust(g) {
    el("path", { d: "M-6 0C-11-4-12-16-5-20C-1-23 6-22 8.5-16L9-12L11.5-8.5L9-7.5C9.5-5 9-2.5 6.5-1L5 1L6 4C12 6 15 10 15 20H-15C-15 10-11 5-5 4Z" }, g);
    el("path", { d: "M-5-19C-1-22 5-21 8-16", fill: "none", stroke: SHINE, "stroke-width": "1.2", opacity: ".8" }, g);
    el("path", { d: "M6 4C11 6 14 10 14.5 15", fill: "none", stroke: SHINE, "stroke-width": "1.1", opacity: ".6" }, g);
  }

  // ₱1: the waling-waling orchid, a five-petalled flower on a stem with strap leaves.
  function walingWaling(g) {
    el("path", { d: "M0-6C1.5 4-2 12 0 24", fill: "none", stroke: INK, "stroke-width": "2.4" }, g);
    el("path", { d: "M0 17C-7 12-14 13-20 19C-13 17-7 17.5 0 20Z" }, g);
    el("path", { d: "M0 12C-7 7-14 8-19 13C-13 12-7 13 0 15Z", transform: "scale(-1 1)" }, g);
    for (let k = 0; k < 5; k++) {
      el("ellipse", { cx: 0, cy: -19.5, rx: 5.6, ry: 8.2, transform: `rotate(${k * 72} 0 -10)`, stroke: SHINE, "stroke-width": "1" }, g);
    }
    el("circle", { cx: 0, cy: -10, r: 3.4, fill: "#6F7482", stroke: SHINE, "stroke-width": ".8" }, g);
  }

  // ₱5: the tayabak (jade vine), claw-shaped flowers hanging from a vine.
  function tayabak(g) {
    el("path", { d: "M-19-21C-9-25 5-24 17-17", fill: "none", stroke: INK, "stroke-width": "2.2" }, g);
    for (const [a, l] of [[-55, 9], [5, 10], [65, 9]]) {
      el("ellipse", { cx: 12, cy: -19 - l / 2, rx: 3.4, ry: l / 2, transform: `rotate(${a} 12 -19)`, stroke: SHINE, "stroke-width": ".7" }, g);
    }
    el("path", { d: "M-3-23C-1-10-2 6 0 24", fill: "none", stroke: INK, "stroke-width": "1.6" }, g);
    const claw = "M0 0C6 1.5 11-2 12-11C9-5 5-2 0-2.4Z";
    [-15, -8.5, -2, 4.5, 11, 17].forEach((y, i) => {
      const s = 1.05 - i * 0.1, x = -2 + (i * 2) / 6;
      el("path", { d: claw, transform: `translate(${x} ${y}) scale(${s})`, stroke: SHINE, "stroke-width": ".7" }, g);
      el("path", { d: claw, transform: `translate(${x} ${y + 2.5}) scale(${-s} ${s})`, stroke: SHINE, "stroke-width": ".7" }, g);
    });
  }

  // ₱10: the kapa-kapa, big drooping bracts over a hanging cone of florets.
  function kapaKapa(g) {
    for (const t of ["", "scale(-1 1)"]) {
      el("path", { d: "M-1-17C-11-28-21-22-22-10C-15-14-8-15-1-17Z", transform: t, stroke: SHINE, "stroke-width": ".8" }, g);
    }
    el("path", { d: "M0-25V-15", fill: "none", stroke: INK, "stroke-width": "2.2" }, g);
    el("path", { d: "M-11-7C-10-13-5-16 0-16C5-16 10-13 11-7C7-9 3-8.5 0-7C-3-8.5-7-9-11-7Z", stroke: SHINE, "stroke-width": ".9" }, g);
    el("path", { d: "M-14 1C-13-5-6-9 0-8C6-9 13-5 14 1C9-1.5 4-1 0 1C-4-1-9-1.5-14 1Z", stroke: SHINE, "stroke-width": ".9" }, g);
    [[5, 5, 2], [9, 5, 1.9], [13, 4, 1.8], [17, 3, 1.7], [21, 2, 1.6], [24.5, 1, 1.5]].forEach(([y, n, r], row) => {
      const w = 9 - row * 1.5;
      for (let i = 0; i < n; i++) {
        const x = n === 1 ? 0 : -w + (2 * w * i) / (n - 1);
        el("circle", { cx: x, cy: y, r, stroke: SHINE, "stroke-width": ".6" }, g);
      }
    });
  }

  // ₱20: the nilad, a mangrove shrub with paired glossy leaves and small flowers.
  function nilad(g) {
    el("path", { d: "M0 25C-1 10 1-6 0-19", fill: "none", stroke: INK, "stroke-width": "2.2" }, g);
    const leaf = "M0 0C5-6 15-8 18-3C15 2 6 2 0 0Z";
    [[15, 1], [4, 0.88], [-7, 0.74]].forEach(([y, s]) => {
      for (const sx of [s, -s]) {
        el("path", { d: leaf, transform: `translate(0 ${y}) scale(${sx} ${s})`, stroke: SHINE, "stroke-width": ".8" }, g);
      }
    });
    for (const [x, y] of [[0, -21], [-4.5, -17], [4.5, -17]]) {
      el("circle", { cx: x, cy: y, r: 2.6, stroke: SHINE, "stroke-width": ".8" }, g);
    }
  }
  const PLANTS = { 1: walingWaling, 5: tayabak, 10: kapaKapa, 20: nilad };

  // Eight-ray sun inside a double ring: a stand-in for the central bank's seal.
  function seal(parent, x, y, r) {
    const g = el("g", { fill: "none", stroke: INK, "stroke-linecap": "round" }, parent);
    el("circle", { cx: x, cy: y, r, "stroke-width": r * 0.12 }, g);
    el("circle", { cx: x, cy: y, r: r * 0.74, "stroke-width": r * 0.06 }, g);
    el("circle", { cx: x, cy: y, r: r * 0.2, fill: INK, stroke: "none" }, g);
    for (let i = 0; i < 8; i++) {
      const a = (i * Math.PI) / 4;
      el("line", { x1: x + r * 0.32 * Math.cos(a), y1: y + r * 0.32 * Math.sin(a), x2: x + r * 0.55 * Math.cos(a), y2: y + r * 0.55 * Math.sin(a), "stroke-width": r * 0.12 }, g);
    }
  }

  // Lettering that follows the rim, upright: over the top or under the
  // bottom. `r` is where its letters stay inside of.
  function arcText(svg, g, text, r, top, size) {
    const id = "pc" + ++uid + "a";
    const a = top ? r - size : r;
    const d = `M${50 - a} 50A${a} ${a} 0 0 ${top ? 1 : 0} ${50 + a} 50`;
    el("path", { id, d, fill: "none" }, el("defs", {}, svg));
    const t = el("text", { "font-size": size, "font-weight": "800", fill: "#7D8290", "font-family": "system-ui, sans-serif", "letter-spacing": ".4", "text-anchor": "middle" }, g);
    el("textPath", { href: "#" + id, startOffset: "50%" }, t).textContent = text;
  }

  /** One face of a coin: "front" (bust and value) or "back" (plant and seal). */
  function draw(denom, px, side) {
    ensureDefs();
    denom = MM[denom] ? denom : 1;
    const back = side === "back";
    const size = Math.round((px * MM[denom]) / 30);
    const svg = el("svg", { viewBox: "0 0 100 100", width: size, height: size, class: "coin-art", role: "img", "aria-label": `${denom} peso coin` });
    const id = "pc" + ++uid;
    const shape = denom === 5 ? "nonagon" : "circle";
    const outer = (attrs) => (shape === "circle"
      ? el("circle", Object.assign({ cx: 50, cy: 50 }, attrs), svg)
      : el("path", Object.assign({ d: nonagon(attrs.r) }, attrs), svg));

    let inner, panelX, numSize, numY;
    if (denom === 20) {
      el("circle", { cx: 50, cy: 50, r: 49, fill: "url(#pc-copper)" }, svg);
      el("circle", { cx: 50, cy: 50, r: 48, fill: "none", stroke: "#8E4C27", "stroke-opacity": ".35", "stroke-width": "1" }, svg);
      for (let i = 0; i < 40; i++) {
        const a = (i / 40) * Math.PI * 2;
        el("circle", { cx: 50 + 43.5 * Math.cos(a), cy: 50 + 43.5 * Math.sin(a), r: ".95", fill: "#7E4021", opacity: ".55" }, svg);
      }
      inner = 36; panelX = 60; numSize = 19; numY = 56;
    } else {
      outer({ r: 49, fill: "url(#pc-rim)" });
      inner = 44.5; panelX = 61; numSize = denom === 10 ? 26 : 36; numY = denom === 10 ? 58 : 62;
    }
    const clipId = id + "c";
    face(svg, shape, inner, clipId);
    if (shape === "circle") el("circle", { cx: 50, cy: 50, r: inner, fill: "url(#pc-silver)" }, svg);
    else el("path", { d: nonagon(inner), fill: "url(#pc-silver)" }, svg);

    const g = el("g", { "clip-path": `url(#${clipId})` }, svg);
    const k = inner / 44.5;          // the ₱20's silver core is smaller
    const lettered = size >= 56;     // too small to read below that
    if (back) {
      // The plant on the left, the seal on the right, like the real reverse.
      PLANTS[denom](el("g", { opacity: ".62" }, relief(g, 50 - 17 * k, 52, 0.95 * k)));
      seal(el("g", { opacity: ".62" }, g), 50 + 21 * k, 50, 11 * k);
      if (lettered) {
        arcText(svg, g, PLANT[denom], inner - 4, true, 5.2 * Math.max(k, 0.9));
        if (denom !== 20) arcText(svg, g, "BANGKO SENTRAL NG PILIPINAS", inner - 4, false, 4.6);
      }
    } else {
      // Right-hand security panel with fine vertical lines, as on the NGC coins.
      el("rect", { x: panelX, y: 0, width: 100 - panelX, height: 100, fill: "url(#pc-lines)" }, g);
      el("line", { x1: panelX, y1: 0, x2: panelX, y2: 100, stroke: "#8D919E", "stroke-width": ".9", opacity: ".7" }, g);
      const bx = (panelX + 50 - inner) / 2 + 3;
      bust(el("g", { opacity: ".6" }, relief(g, bx, 56, 1.25 * k)));
      if (lettered) arcText(svg, g, "REPUBLIKA NG PILIPINAS", inner - 4, true, 4.8 * Math.max(k, 0.9));
      const nx = (panelX + 50 + inner) / 2;
      const num = el("text", { x: nx, y: numY, "text-anchor": "middle", "font-size": numSize, "font-weight": "800", fill: "#666B79", "font-family": "system-ui, -apple-system, sans-serif", "letter-spacing": "-1" }, g);
      num.textContent = String(denom);
      const piso = el("text", { x: nx, y: numY + (denom === 20 ? 8 : 11), "text-anchor": "middle", "font-size": denom === 20 ? 5.5 : 6.5, "font-weight": "800", fill: "#727786", "font-family": "system-ui, sans-serif", "letter-spacing": ".3" }, g);
      piso.textContent = "PISO";
    }

    // Inner rim + shine.
    if (shape === "circle") el("circle", { cx: 50, cy: 50, r: inner, fill: "none", stroke: "#8D919E", "stroke-width": "1.1", opacity: ".8" }, svg);
    else el("path", { d: nonagon(inner), fill: "none", stroke: "#8D919E", "stroke-width": "1.1", opacity: ".8" }, svg);
    el("path", { d: "M18 36 A34 34 0 0 1 40 16", fill: "none", stroke: "#fff", "stroke-width": "2.4", "stroke-linecap": "round", opacity: ".7" }, svg);
    return svg;
  }

  /** Breaks an amount into real coins, largest first (₱7 → 5, 1, 1). */
  function split(pesos) {
    const out = [];
    for (const d of DENOMS) while (pesos >= d) { out.push(d); pesos -= d; }
    return out;
  }

  window.PisoCoins = { draw, split, isCoin: (d) => d in MM };
})();
