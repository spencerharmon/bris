#!/usr/bin/env python3
"""Generate a self-contained `corpus-explorer.html` at the corpus
root, inlining the index + per-session reports as JSON literals so
the page works from `file://` with zero web-server gymnastics.

Image thumbnails stay as relative <img src=...> paths into the
corpus tree — browsers permit those from `file://` even when they
forbid fetch().

Usage:
    python3 tools/corpus-explorer/build_explorer.py path/to/bris-corpus

Output:
    <corpus>/corpus-explorer.html
"""

from __future__ import annotations

import json
import sys
from pathlib import Path


HTML_TEMPLATE = r"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<title>Bris corpus explorer</title>
<style>
* { box-sizing: border-box; }
body {
  margin: 0;
  font-family: system-ui, sans-serif;
  background: #1a1d22;
  color: #e6e9ee;
}
header {
  display: flex; align-items: baseline; gap: 1em;
  padding: 0.5em 1em;
  background: #0f1115;
  border-bottom: 1px solid #2b2f36;
}
header h1 { font-size: 1.1em; margin: 0; }
.status { font-size: 0.85em; color: #9aa3b2; }
main { display: flex; height: calc(100vh - 50px); }
nav {
  width: 280px; flex: 0 0 280px;
  border-right: 1px solid #2b2f36;
  overflow-y: auto; padding: 1em;
}
nav h2 { font-size: 0.85em; text-transform: uppercase; color: #9aa3b2; margin: 0 0 0.5em; }
nav ul { list-style: none; padding: 0; margin: 0; }
nav li {
  padding: 0.6em 0.5em; cursor: pointer; border-radius: 4px;
  margin-bottom: 2px; font-size: 0.9em;
}
nav li:hover { background: #232932; }
nav li.active { background: #2b3340; color: #fff; }
nav li .uuid { font-family: monospace; font-size: 0.7em; color: #6e7686; display: block; }
nav li .meta { font-size: 0.7em; color: #9aa3b2; }
#main-view { flex: 1; overflow-y: auto; padding: 1em 1.5em; }
.session-header {
  border-bottom: 1px solid #2b2f36; padding-bottom: 1em; margin-bottom: 1em;
}
.session-header h2 { margin: 0 0 0.3em; }
.session-header .uuid { font-family: monospace; font-size: 0.8em; color: #9aa3b2; }
.session-meta {
  display: grid; grid-template-columns: max-content auto; gap: 0.2em 1em;
  font-size: 0.85em; margin-top: 0.5em;
}
.session-meta dt { color: #9aa3b2; }
.session-meta dd { margin: 0; font-family: monospace; }
.capture { margin-bottom: 2em; padding: 1em; background: #20242b; border-radius: 6px; }
.capture h3 { margin: 0 0 0.3em; font-size: 1em; }
.capture h3 code { font-family: monospace; color: #9aa3b2; font-weight: normal; }
.capture .stats {
  font-size: 0.85em; color: #c4cad4;
  margin-bottom: 0.5em;
}
.capture .stats .pill {
  display: inline-block; padding: 1px 6px; border-radius: 3px;
  background: #2b3340; margin-right: 0.4em;
}
.capture .stats .pill.warn { background: #5a4015; color: #ffe4a0; }
.capture .stats .pill.err  { background: #5a1f1f; color: #ffb8b8; }
.capture .stats .pill.ok   { background: #1f4a1f; color: #b8ffb8; }
.thumbs {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(180px, 1fr));
  gap: 0.5em;
}
.thumb {
  position: relative; cursor: pointer; background: #0f1115;
  border: 1px solid #2b2f36; border-radius: 4px; overflow: hidden;
}
.thumb .stage {
  position: relative; display: block; line-height: 0; width: 100%;
}
.thumb .stage img { width: 100%; height: auto; display: block; }
.thumb .stage svg {
  position: absolute; inset: 0; width: 100%; height: 100%;
  pointer-events: none;
}
.thumb .label {
  position: absolute; left: 0; right: 0; bottom: 0;
  background: rgba(0,0,0,0.7); color: #fff;
  font-size: 0.75em; padding: 2px 4px;
  white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
}
.thumb.sight   { border-color: #4caf50; }
.thumb.rejected{ border-color: #806020; }
.thumb.empty   { border-color: #555; opacity: 0.7; }
.hint { color: #9aa3b2; font-style: italic; padding: 2em; }
#lightbox {
  position: fixed; inset: 0;
  background: rgba(0, 0, 0, 0.92);
  display: none;
  align-items: center; justify-content: center;
  z-index: 100;
}
#lightbox.open { display: flex; }
#lightbox .stage {
  position: relative; max-width: 95vw; max-height: 95vh; line-height: 0;
}
#lightbox img { max-width: 95vw; max-height: 95vh; display: block; }
#lightbox svg {
  position: absolute; inset: 0; width: 100%; height: 100%;
  pointer-events: none;
}
#lightbox .hud {
  position: absolute; top: 0.5em; left: 0.5em;
  background: rgba(0,0,0,0.65); color: #e8eef8;
  font-family: monospace; font-size: 0.85em; line-height: 1.4;
  padding: 0.4em 0.6em; white-space: pre; border-radius: 3px;
  pointer-events: none; max-width: 60%;
}
#lightbox button {
  position: absolute; top: 1em; right: 1em;
  background: rgba(255,255,255,0.15); color: #fff;
  border: 0; border-radius: 50%; width: 40px; height: 40px;
  font-size: 1.5em; cursor: pointer;
}
.fixlist { margin-top: 0.6em; padding-top: 0.4em; border-top: 1px solid #2b2f36; }
.fixlist h4 { margin: 0 0 0.3em; font-size: 0.8em; text-transform: uppercase; color: #9aa3b2; font-weight: normal; }
.fixlist ul { list-style: none; padding: 0; margin: 0; }
.fixlist li { margin: 0.15em 0; }
.fixlist button.fix {
  display: block; width: 100%; text-align: left;
  background: #1a1f28; border: 1px solid #2b3340; color: #c4cad4;
  padding: 4px 8px; border-radius: 3px;
  font-family: monospace; font-size: 0.85em; cursor: pointer;
}
.fixlist button.fix:hover { background: #2b3340; border-color: #5fc9ff; color: #fff; }
#map-modal {
  position: fixed; inset: 0; background: rgba(0,0,0,0.92);
  display: flex; align-items: center; justify-content: center;
  z-index: 200;
}
#map-modal[hidden] { display: none !important; }
#map-stage {
  position: relative;
  width: min(90vmin, 90vh, 900px);
  height: min(90vmin, 90vh, 900px);
}
#map-svg {
  display: block; width: 100%; height: 100%;
  background: #0d1018; border: 1px solid #2b2f36; border-radius: 4px;
}
#map-hud {
  position: absolute; top: 0.5em; left: 0.5em;
  background: rgba(0,0,0,0.7); color: #e8eef8;
  padding: 0.5em 0.7em; font-family: monospace; font-size: 0.85em;
  line-height: 1.4; white-space: pre; border-radius: 3px;
  pointer-events: none;
}
#map-close {
  position: absolute; top: 0.5em; right: 0.5em;
  background: rgba(255,255,255,0.15); color: #fff;
  border: 0; border-radius: 50%; width: 36px; height: 36px;
  font-size: 1.6em; cursor: pointer;
}
</style>
</head>
<body>
<header>
  <h1>Bris corpus explorer</h1>
  <div class="status" id="status"></div>
</header>
<main>
  <nav>
    <h2>Sessions</h2>
    <ul id="session-list"></ul>
  </nav>
  <section id="main-view">
    <p class="hint">Pick a session on the left.</p>
  </section>
</main>
<div id="lightbox" onclick="if(event.target.id==='lightbox')closeLightbox()">
  <button onclick="closeLightbox()">&times;</button>
  <div class="stage">
    <img id="lightbox-img" alt="" />
    <svg id="lightbox-svg" xmlns="http://www.w3.org/2000/svg"></svg>
    <div id="lightbox-hud" class="hud"></div>
  </div>
</div>
<script>
// Data is inlined below by build_explorer.py — no fetch needed.
const INDEX = __INDEX_JSON__;
const REPORTS = __REPORTS_JSON__;  // { "<session_id>": {report object} | {error: "..."} }
const COASTLINE = __COASTLINE_JSON__;  // [[[lon,lat],...], ...]

const statusEl = document.getElementById("status");
const listEl = document.getElementById("session-list");
const viewEl = document.getElementById("main-view");
const lightbox = document.getElementById("lightbox");
const lightboxImg = document.getElementById("lightbox-img");
const lightboxSvg = document.getElementById("lightbox-svg");
const lightboxHud = document.getElementById("lightbox-hud");
const SVG_NS = "http://www.w3.org/2000/svg";

function closeLightbox() {
  lightbox.classList.remove("open");
  lightboxImg.src = "";
  lightboxSvg.innerHTML = "";
  lightboxHud.textContent = "";
}
function openFrame(frame, src) {
  lightboxImg.src = src;
  lightboxSvg.innerHTML = "";
  lightboxSvg.removeAttribute("viewBox");
  const built = buildOverlaySvg(frame);
  if (built) {
    lightboxSvg.setAttribute("viewBox", built.getAttribute("viewBox"));
    lightboxSvg.setAttribute(
      "preserveAspectRatio",
      built.getAttribute("preserveAspectRatio") || "none",
    );
    while (built.firstChild) lightboxSvg.appendChild(built.firstChild);
  }
  lightboxHud.textContent = buildHudText(frame);
  lightbox.classList.add("open");
}

function fmtUnixMs(ms) {
  if (!ms) return "—";
  return new Date(ms).toISOString().replace("T", " ").replace(".000Z", "Z");
}

function pill(text, cls) { return `<span class="pill ${cls||""}">${text}</span>`; }

function renderSidebar() {
  if (!INDEX || !INDEX.sessions) {
    statusEl.textContent = "no index.json data";
    return;
  }
  statusEl.textContent = `${INDEX.sessions.length} session(s) · generated ${fmtUnixMs(INDEX.generated_unix_ms)}`;
  listEl.innerHTML = "";
  INDEX.sessions.forEach((s, i) => {
    const li = document.createElement("li");
    li.innerHTML = `<strong>${escapeHtml(s.session_title || "(untitled)")}</strong>
      <span class="uuid">${s.session_id}</span>
      <span class="meta">${s.capture_count} capture(s)</span>`;
    li.onclick = () => selectSession(s.session_id, li);
    listEl.appendChild(li);
    if (i === 0) li.click();
  });
}

function selectSession(id, li) {
  document.querySelectorAll("nav li").forEach(n => n.classList.remove("active"));
  if (li) li.classList.add("active");
  const r = REPORTS[id];
  if (!r) {
    viewEl.innerHTML = `<p class="hint">No report for ${id}.</p>`;
    return;
  }
  if (r.error) {
    viewEl.innerHTML = `<p class="hint">Report load failed: ${escapeHtml(r.error)}</p>`;
    return;
  }
  renderSession(r);
}

function renderSession(r) {
  let html = `<div class="session-header">
    <h2>${escapeHtml(r.session_title || "(untitled)")}</h2>
    <div class="uuid">${r.session_id}</div>
    <dl class="session-meta">
      <dt>generated:</dt><dd>${fmtUnixMs(r.generated_unix_ms)}</dd>
      <dt>engine:</dt><dd>${escapeHtml((r.engine_build||{}).git_describe || "?")} (${escapeHtml((r.engine_build||{}).git_sha || "?").slice(0,12)})</dd>
      <dt>captures:</dt><dd>${r.captures.length}</dd>
    </dl>
  </div>`;

  if (r.captures.length === 0) {
    html += `<p class="hint">No captures in this session report. (The session.json's
      ordered_capture_ids may be missing some captures present on disk.)</p>`;
  }

  for (const c of r.captures) {
    html += renderCapture(r.session_id, c);
  }
  viewEl.innerHTML = html;
}

function renderCapture(sessionId, c) {
  const rej = c.stage_e_rejection_counts || {};
  const rejParts = Object.entries(rej).filter(([,n])=>n>0).map(([k,n])=>pill(`${k}: ${n}`, "warn"));
  const fixesPill = pill(`fixes: ${c.fixes_published||0}`, c.fixes_published?"ok":"err");
  const sightsPill = pill(`sights: ${c.sights_inserted_total||0}`, c.sights_inserted_total?"ok":"err");

  let html = `<div class="capture">
    <h3><code>${c.capture_id}</code></h3>
    <div class="stats">
      app: ${escapeHtml(c.app_version||"?")} ·
      frames: ${c.frame_count} ·
      ${fixesPill} ${sightsPill} ${rejParts.join(" ")}
    </div>
    ${renderFixesListHtml(sessionId, c.capture_id, c.fixes || [])}`;

  if (!c.frames || c.frames.length === 0) {
    html += `<p class="hint">No per-frame data.</p></div>`;
    return html;
  }

  html += `<div class="thumbs" id="thumbs-${escapeHtml(c.capture_id)}">`;
  for (let fi = 0; fi < c.frames.length; fi++) {
    const f = c.frames[fi];
    const cls = f.sight_emitted ? "sight"
              : (f.stage_e_outcomes && f.stage_e_outcomes.length > 0) ? "rejected"
              : "empty";
    const outcome = summarizeOutcome(f);
    const src = f.render_path;
    const title = `seq ${f.seq} · ${escapeHtml(f.classification||"?")} · ${escapeHtml(outcome)}`;
    const svgMarkup = buildOverlaySvgMarkup(f);
    html += `<div class="thumb ${cls}" title="${title}"
      data-session-cap="${escapeHtml(sessionId)}|${escapeHtml(c.capture_id)}|${fi}">
      <span class="stage">
        <img src="${src}" loading="lazy" alt="frame ${f.seq}">
        ${svgMarkup}
      </span>
      <div class="label">#${f.seq} ${escapeHtml(outcome)}</div>
    </div>`;
  }
  html += `</div></div>`;
  return html;
}

function summarizeOutcome(f) {
  if (f.sight_emitted) {
    const ok = (f.stage_e_outcomes||[]).find(o => o.kind === "Ok");
    if (ok) {
      const d = (ok.altitude_rad * 180 / Math.PI).toFixed(2);
      const am = (ok.sigma_rad * 180 * 60 / Math.PI).toFixed(2);
      return `✓ ${d}° σ=${am}'`;
    }
    return `✓`;
  }
  if (f.stage_e_outcomes && f.stage_e_outcomes.length > 0) {
    return `✗ ${f.stage_e_outcomes[0].error || "?"}`;
  }
  return "(no body/horizon)";
}

// Wire thumb clicks AFTER inserting html. selectSession()
// calls this after innerHTML; we delegate via the document.
document.addEventListener("click", (e) => {
  const t = e.target.closest(".thumb[data-session-cap]");
  if (!t) return;
  const [sid, cid, fiStr] = t.dataset.sessionCap.split("|");
  const rep = REPORTS[sid];
  if (!rep || rep.error) return;
  const cap = rep.captures.find(x => x.capture_id === cid);
  if (!cap) return;
  const frame = cap.frames[parseInt(fiStr, 10)];
  if (!frame || !frame.render_path) return;
  openFrame(frame, frame.render_path);
});

// ---------- SVG overlay (client-side, from JSON) ----------

function buildOverlaySvg(frame) {
  const g = frame.render_geometry;
  if (!g) return null;
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", `0 0 ${g.canvas_width} ${g.canvas_height}`);
  svg.setAttribute("preserveAspectRatio", "none");
  if (frame.horizon) svg.appendChild(buildHorizonLine(frame.horizon, g));
  if (frame.body_centroid) appendCentroidMarker(svg, frame.body_centroid, g);
  return svg;
}
function buildOverlaySvgMarkup(frame) {
  const svg = buildOverlaySvg(frame);
  if (!svg) return "";
  return new XMLSerializer().serializeToString(svg);
}
function buildHorizonLine(h, g) {
  const x1 = 0, x2 = g.source_width - 1;
  const y1 = h.slope * x1 + h.intercept_px;
  const y2 = h.slope * x2 + h.intercept_px;
  const line = document.createElementNS(SVG_NS, "line");
  line.setAttribute("x1", x1 * g.scale);
  line.setAttribute("y1", y1 * g.scale);
  line.setAttribute("x2", x2 * g.scale);
  line.setAttribute("y2", y2 * g.scale);
  line.setAttribute("stroke", "rgb(255,60,60)");
  line.setAttribute("stroke-width", Math.max(1, g.canvas_width * 0.0025));
  line.setAttribute("vector-effect", "non-scaling-stroke");
  return line;
}
function appendCentroidMarker(svg, c, g) {
  const cx = c.x * g.scale, cy = c.y * g.scale;
  const rawR = Math.sqrt((c.area_px || 0) / Math.PI);
  const r = Math.max(10, Math.min(40, Math.round(rawR * g.scale)));
  const disk = document.createElementNS(SVG_NS, "circle");
  disk.setAttribute("cx", cx); disk.setAttribute("cy", cy); disk.setAttribute("r", r);
  disk.setAttribute("fill", "rgb(255,220,0)"); disk.setAttribute("fill-opacity", "0.55");
  svg.appendChild(disk);
  const outline = document.createElementNS(SVG_NS, "circle");
  outline.setAttribute("cx", cx); outline.setAttribute("cy", cy); outline.setAttribute("r", r+1);
  outline.setAttribute("fill", "none"); outline.setAttribute("stroke", "rgb(0,0,0)");
  outline.setAttribute("stroke-width", 1); outline.setAttribute("vector-effect", "non-scaling-stroke");
  svg.appendChild(outline);
  const arm = r + 6;
  for (const [x1,y1,x2,y2] of [[cx-arm,cy,cx+arm,cy],[cx,cy-arm,cx,cy+arm]]) {
    const ln = document.createElementNS(SVG_NS, "line");
    ln.setAttribute("x1", x1); ln.setAttribute("y1", y1);
    ln.setAttribute("x2", x2); ln.setAttribute("y2", y2);
    ln.setAttribute("stroke", "rgb(0,0,0)"); ln.setAttribute("stroke-width", 1);
    ln.setAttribute("vector-effect", "non-scaling-stroke");
    svg.appendChild(ln);
  }
}
function buildHudText(f) {
  const lines = [`FRAME ${f.seq}`];
  if (f.captured_unix_ms) lines.push(`UTC   ${new Date(f.captured_unix_ms).toISOString()}`);
  lines.push(`CLASS ${f.classification||"?"}`);
  const c = f.body_centroid;
  if (c) lines.push(`CENTROID x=${c.x.toFixed(1)} y=${c.y.toFixed(1)} σ=${c.sigma_px.toFixed(2)}px area=${c.area_px}px²`);
  const h = f.horizon;
  if (h) {
    const d = (h.sigma_rad*180/Math.PI).toFixed(3);
    let line = `HORIZON intercept=${h.intercept_px.toFixed(1)} slope=${h.slope.toFixed(4)} provider=${h.provider} σ=${d}°`;
    if (h.model_id) line += ` model=${h.model_id}`;
    lines.push(line);
  }
  if (f.stage_e_outcomes && f.stage_e_outcomes.length > 0) lines.push(`STAGE E: ${summarizeOutcome(f)}`);
  return lines.join("\n");
}

function escapeHtml(s) {
  return String(s).replace(/[&<>"']/g, c => ({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"}[c]));
}

// ---------- fix list + map modal ----------

function renderFixesListHtml(sessionId, captureId, fixes) {
  if (!fixes || fixes.length === 0) return "";
  let html = `<div class="fixlist"><h4>${fixes.length} published fix${fixes.length === 1 ? "" : "es"}</h4><ul>`;
  for (let i = 0; i < fixes.length; i++) {
    const f = fixes[i];
    const lat = fmtLat(f.lat_deg);
    const lon = fmtLon(f.lon_deg);
    const smaj = f.sigma_major_nm.toFixed(1);
    const smin = f.sigma_minor_nm.toFixed(1);
    let line = `${lat} ${lon} · σ ${smaj} × ${smin} nm · ${f.sight_count} sights`;
    if (typeof f.gps_truth_error_nm === "number") {
      line += ` · Δ${f.gps_truth_error_nm.toFixed(1)} nm vs truth`;
    }
    html += `<li><button type="button" class="fix" data-fix="${escapeHtml(sessionId)}|${escapeHtml(captureId)}|${i}">${escapeHtml(line)}</button></li>`;
  }
  html += `</ul></div>`;
  return html;
}

function fmtLat(d) {
  const hemi = d >= 0 ? "N" : "S";
  return `${Math.abs(d).toFixed(4)}°${hemi}`;
}
function fmtLon(d) {
  const hemi = d >= 0 ? "E" : "W";
  return `${Math.abs(d).toFixed(4)}°${hemi}`;
}

function ensureMapModal() {
  let m = document.getElementById("map-modal");
  if (m) return m;
  m = document.createElement("div");
  m.id = "map-modal"; m.hidden = true;
  m.innerHTML = `<div id="map-stage">
    <svg id="map-svg" xmlns="http://www.w3.org/2000/svg"></svg>
    <div id="map-hud"></div>
    <button id="map-close" aria-label="close">×</button>
  </div>`;
  document.body.appendChild(m);
  m.addEventListener("click", (e) => {
    if (e.target === m || e.target.id === "map-close") closeMapModal();
  });
  return m;
}

function closeMapModal() {
  const m = document.getElementById("map-modal");
  if (!m) return;
  m.hidden = true;
  document.getElementById("map-svg").innerHTML = "";
  document.getElementById("map-hud").textContent = "";
}

function openMapModal(fix) {
  const m = ensureMapModal();
  m.hidden = false;
  const svg = document.getElementById("map-svg");
  const hud = document.getElementById("map-hud");
  svg.innerHTML = "";
  const halfNmFromSigma = Math.max(3 * fix.sigma_major_nm, 1);
  let halfNm = halfNmFromSigma;
  if (typeof fix.gps_truth_error_nm === "number") {
    halfNm = Math.max(halfNm, fix.gps_truth_error_nm * 1.3);
  }
  halfNm = Math.max(halfNm, 0.5);
  const project = makeEquirectProjector(fix.lat_deg, fix.lon_deg, halfNm);
  drawMapBackground(svg, project, halfNm);
  drawCoastline(svg, project);
  draw1And2SigmaEllipse(svg, fix, project);
  drawFixPoint(svg, fix, project);
  if (typeof fix.gps_truth_error_nm === "number" &&
      typeof fix.gps_truth_bearing_deg === "number") {
    const truth = projectFromFix(fix, fix.gps_truth_error_nm, fix.gps_truth_bearing_deg);
    drawTruthMarker(svg, truth, project);
    drawErrorLine(svg, fix, truth, project);
  }
  hud.textContent = buildFixHud(fix);
}

function makeEquirectProjector(lat0, lon0, halfNm) {
  const VIEW = 800;
  const cosLat = Math.cos((lat0 * Math.PI) / 180);
  const pxPerNm = (VIEW / 2) / halfNm;
  const project = (lat, lon) => {
    const dLat = lat - lat0;
    const dLon = lon - lon0;
    const nmN = dLat * 60;
    const nmE = dLon * 60 * Math.max(cosLat, 1e-6);
    return [VIEW / 2 + nmE * pxPerNm, VIEW / 2 - nmN * pxPerNm];
  };
  project.pxPerNm = pxPerNm;
  project.view = VIEW;
  project.lat0 = lat0; project.lon0 = lon0;
  project.halfNm = halfNm;
  return project;
}

function drawMapBackground(svg, project, halfNm) {
  const VIEW = project.view;
  svg.setAttribute("viewBox", `0 0 ${VIEW} ${VIEW}`);
  svg.setAttribute("preserveAspectRatio", "xMidYMid meet");
  const bg = document.createElementNS(SVG_NS, "rect");
  bg.setAttribute("x", 0); bg.setAttribute("y", 0);
  bg.setAttribute("width", VIEW); bg.setAttribute("height", VIEW);
  bg.setAttribute("fill", "#0d1018"); svg.appendChild(bg);
  const nmStep = niceStep(halfNm * 2 / 5);
  const grid = document.createElementNS(SVG_NS, "g");
  grid.setAttribute("stroke", "#252b35"); grid.setAttribute("stroke-width", "1");
  for (let n = -10; n <= 10; n++) {
    const nm = n * nmStep;
    if (Math.abs(nm) > halfNm) continue;
    const off = nm * project.pxPerNm;
    const xv = VIEW / 2 + off;
    const lnv = document.createElementNS(SVG_NS, "line");
    lnv.setAttribute("x1", xv); lnv.setAttribute("y1", 0);
    lnv.setAttribute("x2", xv); lnv.setAttribute("y2", VIEW);
    grid.appendChild(lnv);
    const yh = VIEW / 2 - off;
    const lnh = document.createElementNS(SVG_NS, "line");
    lnh.setAttribute("x1", 0); lnh.setAttribute("y1", yh);
    lnh.setAttribute("x2", VIEW); lnh.setAttribute("y2", yh);
    grid.appendChild(lnh);
  }
  svg.appendChild(grid);
  const sbY = VIEW - 22, sbX0 = 18;
  const sbX1 = sbX0 + nmStep * project.pxPerNm;
  const sb = document.createElementNS(SVG_NS, "line");
  sb.setAttribute("x1", sbX0); sb.setAttribute("y1", sbY);
  sb.setAttribute("x2", sbX1); sb.setAttribute("y2", sbY);
  sb.setAttribute("stroke", "#e6e9ee"); sb.setAttribute("stroke-width", "3");
  svg.appendChild(sb);
  const sbT = document.createElementNS(SVG_NS, "text");
  sbT.setAttribute("x", sbX0); sbT.setAttribute("y", sbY - 6);
  sbT.setAttribute("fill", "#e6e9ee"); sbT.setAttribute("font-family", "monospace");
  sbT.setAttribute("font-size", "14"); sbT.textContent = `${nmStep} nm`;
  svg.appendChild(sbT);
  const naX = VIEW - 30, naY = 25;
  const arr = document.createElementNS(SVG_NS, "polygon");
  arr.setAttribute("points", `${naX},${naY - 12} ${naX - 6},${naY + 6} ${naX + 6},${naY + 6}`);
  arr.setAttribute("fill", "#e6e9ee"); svg.appendChild(arr);
  const naT = document.createElementNS(SVG_NS, "text");
  naT.setAttribute("x", naX); naT.setAttribute("y", naY + 22);
  naT.setAttribute("fill", "#e6e9ee"); naT.setAttribute("font-family", "monospace");
  naT.setAttribute("font-size", "12"); naT.setAttribute("text-anchor", "middle");
  naT.textContent = "N"; svg.appendChild(naT);
}

function niceStep(approxNm) {
  if (approxNm <= 0) return 1;
  const k = Math.floor(Math.log10(approxNm));
  const base = Math.pow(10, k);
  const mant = approxNm / base;
  let snap;
  if (mant < 1.5) snap = 1;
  else if (mant < 3.5) snap = 2;
  else if (mant < 7.5) snap = 5;
  else snap = 10;
  return snap * base;
}

// Draw Natural Earth 1:110m coastline (public domain),
// inlined at build time as COASTLINE = [[[lon,lat],...], ...].
// Each LineString gets a single SVG polyline. Coordinates are
// shifted to the central-meridian window so the line doesn't
// snake across the canvas at the antimeridian.
function drawCoastline(svg, project) {
  if (!Array.isArray(COASTLINE) || COASTLINE.length === 0) return;
  const lon0 = project.lon0;
  // Viewport half-extent in degrees longitude (cosLat for
  // the central latitude). Use the projector's halfNm.
  const cosLat = Math.cos((project.lat0 * Math.PI) / 180);
  const halfLonDeg = project.halfNm / (60 * Math.max(cosLat, 1e-6));
  const halfLatDeg = project.halfNm / 60;
  const view = project.view;
  const group = document.createElementNS(SVG_NS, "g");
  group.setAttribute("stroke", "#3a4456");
  group.setAttribute("stroke-width", "1.2");
  group.setAttribute("fill", "none");
  group.setAttribute("stroke-linejoin", "round");
  group.setAttribute("stroke-linecap", "round");
  for (const ls of COASTLINE) {
    const pts = [];
    let prevShifted = null;
    for (const [lonRaw, lat] of ls) {
      // Shift lon into [lon0-180, lon0+180].
      let lon = lonRaw;
      while (lon - lon0 > 180) lon -= 360;
      while (lon - lon0 < -180) lon += 360;
      // Cheap reject: skip vertex if comfortably out of viewport.
      if (Math.abs(lon - lon0) > halfLonDeg * 1.5) {
        if (pts.length > 1) emitPolyline(group, pts);
        pts.length = 0;
        continue;
      }
      if (Math.abs(lat - project.lat0) > halfLatDeg * 1.5) {
        if (pts.length > 1) emitPolyline(group, pts);
        pts.length = 0;
        continue;
      }
      const [x, y] = project(lat, lon);
      pts.push(`${x.toFixed(1)},${y.toFixed(1)}`);
    }
    if (pts.length > 1) emitPolyline(group, pts);
  }
  svg.appendChild(group);
}
function emitPolyline(group, pts) {
  const p = document.createElementNS(SVG_NS, "polyline");
  p.setAttribute("points", pts.join(" "));
  group.appendChild(p);
}

function draw1And2SigmaEllipse(svg, fix, project) {
  const [cx, cy] = project(fix.lat_deg, fix.lon_deg);
  const orientDeg = (fix.orientation_rad * 180) / Math.PI;
  const rotDeg = orientDeg - 90;
  const rxPx = fix.sigma_major_nm * project.pxPerNm;
  const ryPx = fix.sigma_minor_nm * project.pxPerNm;
  for (const [mult, dash, opacity] of [
    [2, "6 4", 0.4],
    [1, null, 0.85],
  ]) {
    const e = document.createElementNS(SVG_NS, "ellipse");
    e.setAttribute("cx", cx); e.setAttribute("cy", cy);
    e.setAttribute("rx", rxPx * mult); e.setAttribute("ry", ryPx * mult);
    e.setAttribute("transform", `rotate(${rotDeg} ${cx} ${cy})`);
    e.setAttribute("fill", "none"); e.setAttribute("stroke", "#5fc9ff");
    e.setAttribute("stroke-width", "2"); e.setAttribute("stroke-opacity", opacity);
    if (dash) e.setAttribute("stroke-dasharray", dash);
    svg.appendChild(e);
  }
}

function drawFixPoint(svg, fix, project) {
  const [x, y] = project(fix.lat_deg, fix.lon_deg);
  const dot = document.createElementNS(SVG_NS, "circle");
  dot.setAttribute("cx", x); dot.setAttribute("cy", y); dot.setAttribute("r", 5);
  dot.setAttribute("fill", "#5fc9ff"); dot.setAttribute("stroke", "#0d1018");
  dot.setAttribute("stroke-width", "1.5"); svg.appendChild(dot);
}

function drawTruthMarker(svg, truth, project) {
  const [x, y] = project(truth.lat, truth.lon);
  for (const [x1,y1,x2,y2] of [[x-7,y,x+7,y],[x,y-7,x,y+7]]) {
    const ln = document.createElementNS(SVG_NS, "line");
    ln.setAttribute("x1", x1); ln.setAttribute("y1", y1);
    ln.setAttribute("x2", x2); ln.setAttribute("y2", y2);
    ln.setAttribute("stroke", "#4caf50"); ln.setAttribute("stroke-width", "2");
    svg.appendChild(ln);
  }
  const lab = document.createElementNS(SVG_NS, "text");
  lab.setAttribute("x", x + 10); lab.setAttribute("y", y - 6);
  lab.setAttribute("fill", "#4caf50"); lab.setAttribute("font-family", "monospace");
  lab.setAttribute("font-size", "12"); lab.textContent = "GPS";
  svg.appendChild(lab);
}

function drawErrorLine(svg, fix, truth, project) {
  const [x1, y1] = project(fix.lat_deg, fix.lon_deg);
  const [x2, y2] = project(truth.lat, truth.lon);
  const ln = document.createElementNS(SVG_NS, "line");
  ln.setAttribute("x1", x1); ln.setAttribute("y1", y1);
  ln.setAttribute("x2", x2); ln.setAttribute("y2", y2);
  ln.setAttribute("stroke", "#ffb840"); ln.setAttribute("stroke-width", "1.5");
  ln.setAttribute("stroke-dasharray", "3 3"); svg.appendChild(ln);
}

function projectFromFix(fix, nm, brgDeg) {
  const brg = (brgDeg * Math.PI) / 180;
  const dN = nm * Math.cos(brg);
  const dE = nm * Math.sin(brg);
  const cosLat = Math.cos((fix.lat_deg * Math.PI) / 180);
  return {
    lat: fix.lat_deg + dN / 60,
    lon: fix.lon_deg + dE / (60 * Math.max(cosLat, 1e-6)),
  };
}

function buildFixHud(fix) {
  const lines = [];
  lines.push(`FIX  ${fmtLat(fix.lat_deg)} ${fmtLon(fix.lon_deg)}`);
  lines.push(
    `σ    major ${fix.sigma_major_nm.toFixed(2)} nm ` +
    `minor ${fix.sigma_minor_nm.toFixed(2)} nm ` +
    `orient ${((fix.orientation_rad * 180) / Math.PI).toFixed(1)}° T`
  );
  lines.push(`SIGHTS ${fix.sight_count}`);
  if (typeof fix.chi_square === "number") {
    lines.push(`χ²/dof ${fix.chi_square.toFixed(2)}`);
  }
  if (typeof fix.gps_truth_error_nm === "number") {
    lines.push(
      `vs GPS truth: ${fix.gps_truth_error_nm.toFixed(2)} nm ` +
      `@ ${fix.gps_truth_bearing_deg.toFixed(1)}° T`
    );
  }
  if (fix.timestamp_unix_ms) {
    lines.push(`UTC  ${new Date(fix.timestamp_unix_ms).toISOString()}`);
  }
  return lines.join("\n");
}

// Wire fix-button clicks via delegation: capture-report
// renderers emit data-fix="sid|cid|index".
document.addEventListener("click", (e) => {
  const b = e.target.closest("button.fix[data-fix]");
  if (!b) return;
  const [sid, cid, idxStr] = b.dataset.fix.split("|");
  const rep = REPORTS[sid];
  if (!rep || rep.error) return;
  const cap = rep.captures.find(x => x.capture_id === cid);
  if (!cap || !cap.fixes) return;
  const fix = cap.fixes[parseInt(idxStr, 10)];
  if (!fix) return;
  openMapModal(fix);
});

renderSidebar();
</script>
</body>
</html>
"""


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print(f"usage: {argv[0]} <corpus-root>", file=sys.stderr)
        return 2
    corpus = Path(argv[1]).resolve()
    index_path = corpus / "index.json"
    if not index_path.is_file():
        print(f"missing {index_path}; run `bris replay --corpus {corpus} --all-sessions --render-frames` first", file=sys.stderr)
        return 1
    index = json.loads(index_path.read_text())
    reports: dict[str, dict] = {}
    for s in index.get("sessions", []):
        rp = corpus / s["report_path"]
        try:
            rep = json.loads(rp.read_text())
        except Exception as e:
            reports[s["session_id"]] = {"error": str(e)}
            continue
        # Frame render_path is relative to the session dir; rewrite it
        # relative to the corpus root (where corpus-explorer.html lives)
        # so <img src=...> resolves under file://.
        session_prefix = f"sessions/{s['session_id']}/"
        for cap in rep.get("captures", []):
            for frame in cap.get("frames", []):
                rpth = frame.get("render_path")
                if rpth and not rpth.startswith(session_prefix):
                    frame["render_path"] = session_prefix + rpth
        reports[s["session_id"]] = rep
    coastline_path = Path(__file__).resolve().parent / "data" / "ne_110m_coastline.min.json"
    if coastline_path.is_file():
        coastline = json.loads(coastline_path.read_text())
    else:
        print(f"warning: missing {coastline_path}; map view will lack coastline", file=sys.stderr)
        coastline = []
    html = (
        HTML_TEMPLATE
        .replace("__INDEX_JSON__", json.dumps(index))
        .replace("__REPORTS_JSON__", json.dumps(reports))
        .replace("__COASTLINE_JSON__", json.dumps(coastline, separators=(",", ":")))
    )
    out = corpus / "corpus-explorer.html"
    out.write_text(html)
    print(f"wrote {out} ({out.stat().st_size:,} bytes)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
