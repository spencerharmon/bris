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
    </div>`;

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
    html = (
        HTML_TEMPLATE
        .replace("__INDEX_JSON__", json.dumps(index))
        .replace("__REPORTS_JSON__", json.dumps(reports))
    )
    out = corpus / "corpus-explorer.html"
    out.write_text(html)
    print(f"wrote {out} ({out.stat().st_size:,} bytes)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
