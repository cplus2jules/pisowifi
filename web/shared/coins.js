// Stylized Philippine New Generation Currency coins, drawn as SVG.
// Simplified on purpose (no official portraits or seals); shapes, metals
// and relative sizes follow the real series:
//   ₱20 bimetallic 30 mm, ₱10 round 27 mm, ₱5 nine-sided 25 mm, ₱1 round 23 mm.
"use strict";

(function () {
  const NS = "http://www.w3.org/2000/svg";
  const MM = { 1: 23, 5: 25, 10: 27, 20: 30 };
  const DENOMS = [20, 10, 5, 1];
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

  function draw(denom, px) {
    ensureDefs();
    denom = MM[denom] ? denom : 1;
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
    // Right-hand security panel with fine vertical lines, as on the NGC coins.
    el("rect", { x: panelX, y: 0, width: 100 - panelX, height: 100, fill: "url(#pc-lines)" }, g);
    el("line", { x1: panelX, y1: 0, x2: panelX, y2: 100, stroke: "#8D919E", "stroke-width": ".9", opacity: ".7" }, g);
    // Engraved eight-ray sun from the flag (stand-in for the portrait).
    const bx = (panelX + 50 - inner) / 2 + 3;
    const sr = inner > 40 ? 1 : 0.82;
    const sun = el("g", { fill: "none", stroke: "#858A97", "stroke-linecap": "round", opacity: ".55" }, g);
    el("circle", { cx: bx, cy: 50, r: 6.5 * sr, fill: "#858A97", stroke: "none" }, sun);
    for (let i = 0; i < 8; i++) {
      const a = (i * Math.PI) / 4;
      const r1 = 10 * sr, r2 = 16.5 * sr;
      el("line", { x1: bx + r1 * Math.cos(a), y1: 50 + r1 * Math.sin(a), x2: bx + r2 * Math.cos(a), y2: 50 + r2 * Math.sin(a), "stroke-width": 3.2 * sr }, sun);
    }
    if (size >= 56) {
      const t = el("text", { x: bx + 2, y: 50 - inner + 13, "text-anchor": "middle", "font-size": "5.2", "font-weight": "800", fill: "#7D8290", "font-family": "system-ui, sans-serif", "letter-spacing": ".2" }, g);
      t.textContent = "PILIPINAS";
    }
    const nx = (panelX + 50 + inner) / 2;
    const num = el("text", { x: nx, y: numY, "text-anchor": "middle", "font-size": numSize, "font-weight": "800", fill: "#666B79", "font-family": "system-ui, -apple-system, sans-serif", "letter-spacing": "-1" }, g);
    num.textContent = String(denom);
    const piso = el("text", { x: nx, y: numY + (denom === 20 ? 8 : 11), "text-anchor": "middle", "font-size": denom === 20 ? 5.5 : 6.5, "font-weight": "800", fill: "#727786", "font-family": "system-ui, sans-serif", "letter-spacing": ".3" }, g);
    piso.textContent = "PISO";

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
