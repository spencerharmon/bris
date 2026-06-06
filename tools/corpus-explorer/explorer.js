// Bris corpus explorer — pure-vanilla single-page app.
//
// Loads <corpus-root>/index.json (relative to the page),
// renders a session list, fetches the per-session
// bris-replay-report.json on demand, and renders capture and
// per-frame thumbnails with SVG overlays driven from the
// JSON report. The base PNG is the engine-rendered downsample
// of the source PGM (one PNG per frame, idempotent across
// replays); the overlay (horizon line, centroid, HUD text)
// is rendered client-side from the report JSON so multi-mode
// replays don't re-encode PNGs.

const CORPUS_ROOT = "../../"; // tools/corpus-explorer/ → corpus root

const statusEl = document.getElementById("status");
const sessionListEl = document.getElementById("session-list");
const mainViewEl = document.getElementById("main-view");
const lightbox = document.getElementById("lightbox");
const lightboxImg = document.getElementById("lightbox-img");
const lightboxSvg = document.getElementById("lightbox-svg");
const lightboxHud = document.getElementById("lightbox-hud");
const lightboxClose = document.getElementById("lightbox-close");

const SVG_NS = "http://www.w3.org/2000/svg";

lightboxClose.addEventListener("click", closeLightbox);
lightbox.addEventListener("click", (e) => {
  if (e.target === lightbox) closeLightbox();
});

function closeLightbox() {
  lightbox.hidden = true;
  lightboxImg.src = "";
  lightboxSvg.innerHTML = "";
  lightboxHud.textContent = "";
}

function setStatus(text, kind) {
  statusEl.textContent = text;
  statusEl.className = "status " + (kind || "");
}

async function fetchJson(url) {
  const res = await fetch(url, { cache: "no-store" });
  if (!res.ok) {
    throw new Error(`HTTP ${res.status} fetching ${url}`);
  }
  return res.json();
}

async function loadIndex() {
  try {
    const idx = await fetchJson(CORPUS_ROOT + "index.json");
    if (idx.schema_version !== 1) {
      setStatus(
        `index.json schema_version=${idx.schema_version}, this explorer expects 1`,
        "warn",
      );
    } else {
      setStatus(`${idx.sessions.length} session(s) found`, "ok");
    }
    renderSessionList(idx.sessions || []);
  } catch (e) {
    setStatus(
      `could not load index.json (run \`bris replay --corpus <root> --all-sessions --render-frames\` first): ${e}`,
      "err",
    );
  }
}

function renderSessionList(sessions) {
  sessionListEl.innerHTML = "";
  if (sessions.length === 0) {
    const li = document.createElement("li");
    li.textContent = "(no sessions)";
    li.className = "empty";
    sessionListEl.appendChild(li);
    return;
  }
  sessions.sort((a, b) =>
    (a.session_title || "").localeCompare(b.session_title || ""),
  );
  for (const s of sessions) {
    const li = document.createElement("li");
    const a = document.createElement("a");
    a.href = "#" + s.session_id;
    a.dataset.reportPath = s.report_path;
    a.innerHTML =
      `<strong>${escapeHtml(s.session_title || "(untitled)")}</strong>` +
      `<br/><span class="uuid">${escapeHtml(s.session_id)}</span>` +
      `<br/><span class="meta">${s.capture_count} capture(s)</span>`;
    a.addEventListener("click", (e) => {
      e.preventDefault();
      selectSession(s, a);
    });
    li.appendChild(a);
    sessionListEl.appendChild(li);
  }
}

let currentLink = null;
async function selectSession(session, linkEl) {
  if (currentLink) currentLink.classList.remove("active");
  linkEl.classList.add("active");
  currentLink = linkEl;
  mainViewEl.innerHTML = "<p>loading session report…</p>";
  try {
    const url = CORPUS_ROOT + session.report_path;
    const report = await fetchJson(url);
    renderSession(session, report);
  } catch (e) {
    mainViewEl.innerHTML = `<p class="err">failed to load report: ${escapeHtml(String(e))}</p>`;
  }
}

function renderSession(meta, report) {
  const sessionDir = "sessions/" + report.session_id + "/";
  mainViewEl.innerHTML = "";

  const titleH2 = document.createElement("h2");
  titleH2.textContent = report.session_title || "(untitled)";
  mainViewEl.appendChild(titleH2);

  const metaP = document.createElement("p");
  metaP.className = "meta";
  metaP.innerHTML =
    `id <code>${escapeHtml(report.session_id)}</code>` +
    ` · generated ${fmtTs(report.generated_unix_ms)}` +
    ` · build ${escapeHtml(report.engine_build?.git_describe || report.engine_build?.crate_version || "?")}`;
  mainViewEl.appendChild(metaP);

  if (!report.captures || report.captures.length === 0) {
    const p = document.createElement("p");
    p.textContent = "(no captures in this session report)";
    mainViewEl.appendChild(p);
    return;
  }
  for (const cap of report.captures) {
    mainViewEl.appendChild(renderCapture(cap, sessionDir));
  }
}

function renderCapture(cap, sessionDir) {
  const section = document.createElement("section");
  section.className = "capture";

  const header = document.createElement("header");
  const h3 = document.createElement("h3");
  h3.textContent = `capture ${cap.capture_id}`;
  header.appendChild(h3);

  const meta = document.createElement("p");
  meta.className = "meta";
  meta.innerHTML =
    `app <code>${escapeHtml(cap.app_version || "?")}</code>` +
    ` · frames ${cap.frame_count}` +
    ` · pushed ${cap.frames_pushed}` +
    ` · fixes ${cap.fixes_published}` +
    ` · sights ${cap.sights_inserted_total}`;
  header.appendChild(meta);

  const hist = renderHistogram(cap.stage_e_rejection_counts || {});
  if (hist) header.appendChild(hist);
  section.appendChild(header);

  const grid = document.createElement("div");
  grid.className = "thumbgrid";
  for (const f of cap.frames || []) {
    if (!f.render_path) continue;
    const fullUrl = CORPUS_ROOT + sessionDir + f.render_path;
    const thumb = renderFrameThumb(f, fullUrl);
    grid.appendChild(thumb);
  }
  section.appendChild(grid);
  return section;
}

function renderFrameThumb(frame, fullUrl) {
  const a = document.createElement("a");
  a.className = "thumb";
  a.href = "#";
  a.title =
    `frame ${frame.seq} · ${frame.classification} · ` +
    (frame.stage_e_outcomes && frame.stage_e_outcomes.length > 0
      ? summarizeStageE(frame.stage_e_outcomes)
      : "no stage E attempt");

  const stage = document.createElement("span");
  stage.className = "stage";

  const img = document.createElement("img");
  img.loading = "lazy";
  img.src = fullUrl;
  img.alt = `frame ${frame.seq}`;
  stage.appendChild(img);

  const svg = buildOverlaySvg(frame);
  if (svg) stage.appendChild(svg);
  a.appendChild(stage);

  const seq = document.createElement("span");
  seq.className = "seq";
  seq.textContent = String(frame.seq);
  a.appendChild(seq);

  if (frame.sight_emitted) {
    const ok = document.createElement("span");
    ok.className = "ok";
    ok.textContent = "✓";
    a.appendChild(ok);
  }

  a.addEventListener("click", (e) => {
    e.preventDefault();
    openLightbox(frame, fullUrl);
  });
  return a;
}

function openLightbox(frame, fullUrl) {
  lightboxImg.src = fullUrl;
  lightboxSvg.innerHTML = "";
  const svg = buildOverlaySvg(frame);
  if (svg) {
    // Move children from the built svg into the static one
    // so attributes (viewBox, preserveAspectRatio) carry.
    lightboxSvg.setAttribute("viewBox", svg.getAttribute("viewBox") || "");
    lightboxSvg.setAttribute(
      "preserveAspectRatio",
      svg.getAttribute("preserveAspectRatio") || "none",
    );
    while (svg.firstChild) lightboxSvg.appendChild(svg.firstChild);
  } else {
    lightboxSvg.removeAttribute("viewBox");
  }
  lightboxHud.textContent = buildHudText(frame);
  lightbox.hidden = false;
}

// Build an <svg> element sized to the base image's canvas
// coordinate system. The viewBox = canvas pixels; the SVG
// scales to fill its container while preserving the aspect.
// Horizon + centroid coordinates from the report are in
// SOURCE pixels and get multiplied by render_geometry.scale.
function buildOverlaySvg(frame) {
  const g = frame.render_geometry;
  if (!g) return null;
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", `0 0 ${g.canvas_width} ${g.canvas_height}`);
  svg.setAttribute("preserveAspectRatio", "none");
  if (frame.horizon) {
    svg.appendChild(buildHorizonLine(frame.horizon, g));
  }
  if (frame.body_centroid) {
    appendCentroidMarker(svg, frame.body_centroid, g);
  }
  return svg;
}

// Horizon: source-pixel line y = slope·x + intercept. Project
// the two source-pixel endpoints (x=0 and x=source_width-1)
// into canvas pixels.
function buildHorizonLine(h, g) {
  const x1 = 0;
  const x2 = g.source_width - 1;
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

// Centroid: yellow disk + black crosshair + outline, matching
// the look of the legacy baked-in renderer.
function appendCentroidMarker(svg, c, g) {
  const cx = c.x * g.scale;
  const cy = c.y * g.scale;
  // Radius from sqrt(area/π) in source pixels, then to canvas,
  // clamped to [10, 40] canvas px (matches debug_render.rs).
  const rawRSrc = Math.sqrt((c.area_px || 0) / Math.PI);
  const r = Math.max(10, Math.min(40, Math.round(rawRSrc * g.scale)));

  const disk = document.createElementNS(SVG_NS, "circle");
  disk.setAttribute("cx", cx);
  disk.setAttribute("cy", cy);
  disk.setAttribute("r", r);
  disk.setAttribute("fill", "rgb(255,220,0)");
  disk.setAttribute("fill-opacity", "0.55");
  svg.appendChild(disk);

  const outline = document.createElementNS(SVG_NS, "circle");
  outline.setAttribute("cx", cx);
  outline.setAttribute("cy", cy);
  outline.setAttribute("r", r + 1);
  outline.setAttribute("fill", "none");
  outline.setAttribute("stroke", "rgb(0,0,0)");
  outline.setAttribute("stroke-width", 1);
  outline.setAttribute("vector-effect", "non-scaling-stroke");
  svg.appendChild(outline);

  const arm = r + 6;
  for (const [x1, y1, x2, y2] of [
    [cx - arm, cy, cx + arm, cy],
    [cx, cy - arm, cx, cy + arm],
  ]) {
    const ln = document.createElementNS(SVG_NS, "line");
    ln.setAttribute("x1", x1);
    ln.setAttribute("y1", y1);
    ln.setAttribute("x2", x2);
    ln.setAttribute("y2", y2);
    ln.setAttribute("stroke", "rgb(0,0,0)");
    ln.setAttribute("stroke-width", 1);
    ln.setAttribute("vector-effect", "non-scaling-stroke");
    svg.appendChild(ln);
  }
}

function buildHudText(frame) {
  const lines = [];
  lines.push(`FRAME ${frame.seq}`);
  if (frame.captured_unix_ms) {
    lines.push(`UTC   ${new Date(frame.captured_unix_ms).toISOString()}`);
  }
  lines.push(`CLASS ${frame.classification || "?"}`);
  const c = frame.body_centroid;
  if (c) {
    lines.push(
      `CENTROID x=${c.x.toFixed(1)} y=${c.y.toFixed(1)} σ=${c.sigma_px.toFixed(2)}px area=${c.area_px}px²`,
    );
  }
  const h = frame.horizon;
  if (h) {
    const sigmaDeg = ((h.sigma_rad * 180) / Math.PI).toFixed(3);
    let line = `HORIZON intercept=${h.intercept_px.toFixed(1)} slope=${h.slope.toFixed(4)} provider=${h.provider} σ=${sigmaDeg}°`;
    if (h.model_id) line += ` model=${h.model_id}`;
    lines.push(line);
  }
  if (frame.stage_e_outcomes && frame.stage_e_outcomes.length > 0) {
    lines.push(`STAGE E: ${summarizeStageE(frame.stage_e_outcomes)}`);
  }
  return lines.join("\n");
}

function renderHistogram(hist) {
  const keys = Object.keys(hist).filter((k) => hist[k] > 0);
  if (keys.length === 0) {
    const p = document.createElement("p");
    p.className = "meta";
    p.textContent = "no Stage E rejections";
    return p;
  }
  keys.sort((a, b) => hist[b] - hist[a]);
  const ul = document.createElement("ul");
  ul.className = "hist";
  for (const k of keys) {
    const li = document.createElement("li");
    li.innerHTML = `<code>${escapeHtml(k)}</code>: ${hist[k]}`;
    ul.appendChild(li);
  }
  return ul;
}

function summarizeStageE(outcomes) {
  const oks = outcomes.filter((o) => o.kind === "Ok");
  if (oks.length > 0) {
    const o = oks[0];
    const deg = ((o.altitude_rad * 180) / Math.PI).toFixed(2);
    const arcmin = ((o.sigma_rad * 180 * 60) / Math.PI).toFixed(2);
    return `sight alt=${deg}° σ=${arcmin}′`;
  }
  const counts = {};
  for (const o of outcomes) {
    if (o.kind === "Err") counts[o.error] = (counts[o.error] || 0) + 1;
  }
  const keys = Object.keys(counts).sort((a, b) => counts[b] - counts[a]);
  if (keys.length === 0) return `${outcomes.length} attempt(s)`;
  return `${outcomes.length}/${outcomes.length} rejected (${keys[0]})`;
}

function escapeHtml(s) {
  return String(s).replace(
    /[&<>"']/g,
    (c) =>
      ({
        "&": "&amp;",
        "<": "&lt;",
        ">": "&gt;",
        '"': "&quot;",
        "'": "&#39;",
      })[c],
  );
}
function fmtTs(ms) {
  if (!ms) return "?";
  return new Date(ms).toISOString().replace("T", " ").slice(0, 19) + " UTC";
}

loadIndex();
