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

  const fixesList = renderFixesList(cap.fixes || []);
  if (fixesList) header.appendChild(fixesList);

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

// ---------- fix-list + map modal ----------

function renderFixesList(fixes) {
  if (!fixes || fixes.length === 0) return null;
  const wrap = document.createElement("div");
  wrap.className = "fixlist";
  const h = document.createElement("h4");
  h.textContent = `${fixes.length} published fix${fixes.length === 1 ? "" : "es"}`;
  wrap.appendChild(h);
  const ul = document.createElement("ul");
  for (const f of fixes) {
    const li = document.createElement("li");
    const button = document.createElement("button");
    button.className = "fix";
    button.type = "button";
    const lat = fmtLat(f.lat_deg);
    const lon = fmtLon(f.lon_deg);
    const smaj = f.sigma_major_nm.toFixed(1);
    const smin = f.sigma_minor_nm.toFixed(1);
    let line = `${lat} ${lon} · σ ${smaj} × ${smin} nm · ${f.sight_count} sights`;
    if (typeof f.gps_truth_error_nm === "number") {
      line += ` · Δ${f.gps_truth_error_nm.toFixed(1)} nm vs truth`;
    }
    button.textContent = line;
    button.addEventListener("click", () => openMapModal(f));
    li.appendChild(button);
    ul.appendChild(li);
  }
  wrap.appendChild(ul);
  return wrap;
}

function fmtLat(d) {
  const hemi = d >= 0 ? "N" : "S";
  const a = Math.abs(d);
  return `${a.toFixed(4)}°${hemi}`;
}
function fmtLon(d) {
  const hemi = d >= 0 ? "E" : "W";
  const a = Math.abs(d);
  return `${a.toFixed(4)}°${hemi}`;
}

const mapModal = (() => {
  let m = document.getElementById("map-modal");
  if (m) return m;
  m = document.createElement("div");
  m.id = "map-modal";
  m.hidden = true;
  m.innerHTML = `
    <div id="map-stage">
      <svg id="map-svg" xmlns="http://www.w3.org/2000/svg"></svg>
      <div id="map-hud"></div>
      <button id="map-close" aria-label="close">×</button>
    </div>`;
  document.body.appendChild(m);
  m.addEventListener("click", (e) => {
    if (e.target === m || e.target.id === "map-close") closeMapModal();
  });
  return m;
})();

function closeMapModal() {
  mapModal.hidden = true;
  const svg = document.getElementById("map-svg");
  if (svg) svg.innerHTML = "";
  const hud = document.getElementById("map-hud");
  if (hud) hud.textContent = "";
}

function openMapModal(fix) {
  mapModal.hidden = false;
  const svg = document.getElementById("map-svg");
  const hud = document.getElementById("map-hud");
  svg.innerHTML = "";
  // Choose a viewport: encompass 3·σ_major around the fix
  // plus the GPS truth point (if present), with sane minimum
  // extent so a tiny fix is still visible.
  const halfNmFromSigma = Math.max(3 * fix.sigma_major_nm, 1);
  let halfNm = halfNmFromSigma;
  if (typeof fix.gps_truth_error_nm === "number") {
    halfNm = Math.max(halfNm, fix.gps_truth_error_nm * 1.3);
  }
  halfNm = Math.max(halfNm, 0.5);
  const project = makeEquirectProjector(fix.lat_deg, fix.lon_deg, halfNm);
  drawMapBackground(svg, project, halfNm);
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

// Equirectangular projection centered on (lat0, lon0).
// Returns a function (lat, lon) -> (svg_x, svg_y) in a
// 800×800 viewBox. nm-per-degree-latitude is 60; nm-per-
// degree-longitude scales by cos(lat0).
function makeEquirectProjector(lat0, lon0, halfNm) {
  const VIEW = 800;
  const nmPerDegLat = 60;
  const cosLat = Math.cos((lat0 * Math.PI) / 180);
  const nmPerDegLon = 60 * Math.max(cosLat, 1e-6);
  // Scale: nm -> svg px so that ranging ±halfNm fills the
  // viewport.
  const pxPerNm = (VIEW / 2) / halfNm;
  const project = (lat, lon) => {
    const dLat = lat - lat0;
    const dLon = lon - lon0;
    const nmN = dLat * nmPerDegLat;
    const nmE = dLon * nmPerDegLon;
    const x = VIEW / 2 + nmE * pxPerNm;
    const y = VIEW / 2 - nmN * pxPerNm;
    return [x, y];
  };
  project.pxPerNm = pxPerNm;
  project.view = VIEW;
  project.lat0 = lat0;
  project.lon0 = lon0;
  project.halfNm = halfNm;
  return project;
}

function drawMapBackground(svg, project, halfNm) {
  const VIEW = project.view;
  svg.setAttribute("viewBox", `0 0 ${VIEW} ${VIEW}`);
  svg.setAttribute("preserveAspectRatio", "xMidYMid meet");
  // Backdrop.
  const bg = document.createElementNS(SVG_NS, "rect");
  bg.setAttribute("x", 0); bg.setAttribute("y", 0);
  bg.setAttribute("width", VIEW); bg.setAttribute("height", VIEW);
  bg.setAttribute("fill", "#0d1018");
  svg.appendChild(bg);
  // Lat/lon grid: roughly 5 lines across viewport, in nice
  // round nm intervals.
  const nmStep = niceStep(halfNm * 2 / 5);
  const gridGroup = document.createElementNS(SVG_NS, "g");
  gridGroup.setAttribute("stroke", "#252b35");
  gridGroup.setAttribute("stroke-width", "1");
  for (let n = -10; n <= 10; n++) {
    const nm = n * nmStep;
    if (Math.abs(nm) > halfNm) continue;
    const offsetPx = nm * project.pxPerNm;
    // vertical (constant longitude offset)
    const xv = VIEW / 2 + offsetPx;
    const lnv = document.createElementNS(SVG_NS, "line");
    lnv.setAttribute("x1", xv); lnv.setAttribute("y1", 0);
    lnv.setAttribute("x2", xv); lnv.setAttribute("y2", VIEW);
    gridGroup.appendChild(lnv);
    // horizontal (constant latitude offset; remember svg y inverted)
    const yh = VIEW / 2 - offsetPx;
    const lnh = document.createElementNS(SVG_NS, "line");
    lnh.setAttribute("x1", 0); lnh.setAttribute("y1", yh);
    lnh.setAttribute("x2", VIEW); lnh.setAttribute("y2", yh);
    gridGroup.appendChild(lnh);
  }
  svg.appendChild(gridGroup);
  // Scale bar bottom-left: one nmStep.
  const sbY = VIEW - 22;
  const sbX0 = 18;
  const sbX1 = sbX0 + nmStep * project.pxPerNm;
  const sb = document.createElementNS(SVG_NS, "line");
  sb.setAttribute("x1", sbX0); sb.setAttribute("y1", sbY);
  sb.setAttribute("x2", sbX1); sb.setAttribute("y2", sbY);
  sb.setAttribute("stroke", "#e6e9ee"); sb.setAttribute("stroke-width", "3");
  svg.appendChild(sb);
  const sbT = document.createElementNS(SVG_NS, "text");
  sbT.setAttribute("x", sbX0); sbT.setAttribute("y", sbY - 6);
  sbT.setAttribute("fill", "#e6e9ee");
  sbT.setAttribute("font-family", "monospace");
  sbT.setAttribute("font-size", "14");
  sbT.textContent = `${nmStep} nm`;
  svg.appendChild(sbT);
  // North arrow top-right.
  const naX = VIEW - 30;
  const naY = 25;
  const arrow = document.createElementNS(SVG_NS, "polygon");
  arrow.setAttribute("points",
    `${naX},${naY - 12} ${naX - 6},${naY + 6} ${naX + 6},${naY + 6}`);
  arrow.setAttribute("fill", "#e6e9ee");
  svg.appendChild(arrow);
  const naT = document.createElementNS(SVG_NS, "text");
  naT.setAttribute("x", naX); naT.setAttribute("y", naY + 22);
  naT.setAttribute("fill", "#e6e9ee");
  naT.setAttribute("font-family", "monospace");
  naT.setAttribute("font-size", "12");
  naT.setAttribute("text-anchor", "middle");
  naT.textContent = "N";
  svg.appendChild(naT);
}

function niceStep(approxNm) {
  // Snap to {1,2,5} × 10^k.
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

// Draw 1σ (solid) and 2σ (dashed) ellipses oriented by
// orientation_rad (major axis from north, clockwise).
// SVG rotation is clockwise from east, so we convert.
function draw1And2SigmaEllipse(svg, fix, project) {
  const [cx, cy] = project(fix.lat_deg, fix.lon_deg);
  // orientation_rad is from north clockwise; SVG ellipse rx
  // axis aligns with the x axis (east). Rotation that takes
  // east -> north-clockwise(θ) is (θ - 90°). We want the
  // major axis along (north clockwise θ): that's a rotation
  // of (orientation_deg - 90) about the centre, since SVG
  // angles are clockwise too.
  const orientDeg = (fix.orientation_rad * 180) / Math.PI;
  const rotDeg = orientDeg - 90;
  const rxNm = fix.sigma_major_nm;
  const ryNm = fix.sigma_minor_nm;
  const rxPx = rxNm * project.pxPerNm;
  const ryPx = ryNm * project.pxPerNm;
  // 2σ ellipse first so 1σ sits on top.
  for (const [mult, stroke, dash, opacity] of [
    [2, "#5fc9ff", "6 4", 0.4],
    [1, "#5fc9ff", null, 0.85],
  ]) {
    const e = document.createElementNS(SVG_NS, "ellipse");
    e.setAttribute("cx", cx); e.setAttribute("cy", cy);
    e.setAttribute("rx", rxPx * mult); e.setAttribute("ry", ryPx * mult);
    e.setAttribute("transform", `rotate(${rotDeg} ${cx} ${cy})`);
    e.setAttribute("fill", "none");
    e.setAttribute("stroke", stroke);
    e.setAttribute("stroke-width", "2");
    e.setAttribute("stroke-opacity", opacity);
    if (dash) e.setAttribute("stroke-dasharray", dash);
    svg.appendChild(e);
  }
}

function drawFixPoint(svg, fix, project) {
  const [x, y] = project(fix.lat_deg, fix.lon_deg);
  const dot = document.createElementNS(SVG_NS, "circle");
  dot.setAttribute("cx", x); dot.setAttribute("cy", y); dot.setAttribute("r", 5);
  dot.setAttribute("fill", "#5fc9ff");
  dot.setAttribute("stroke", "#0d1018");
  dot.setAttribute("stroke-width", "1.5");
  svg.appendChild(dot);
}

function drawTruthMarker(svg, truth, project) {
  const [x, y] = project(truth.lat, truth.lon);
  // Cross marker.
  for (const [x1,y1,x2,y2] of [[x-7,y,x+7,y],[x,y-7,x,y+7]]) {
    const ln = document.createElementNS(SVG_NS, "line");
    ln.setAttribute("x1", x1); ln.setAttribute("y1", y1);
    ln.setAttribute("x2", x2); ln.setAttribute("y2", y2);
    ln.setAttribute("stroke", "#4caf50");
    ln.setAttribute("stroke-width", "2");
    svg.appendChild(ln);
  }
  const lab = document.createElementNS(SVG_NS, "text");
  lab.setAttribute("x", x + 10); lab.setAttribute("y", y - 6);
  lab.setAttribute("fill", "#4caf50");
  lab.setAttribute("font-family", "monospace");
  lab.setAttribute("font-size", "12");
  lab.textContent = "GPS";
  svg.appendChild(lab);
}

function drawErrorLine(svg, fix, truth, project) {
  const [x1, y1] = project(fix.lat_deg, fix.lon_deg);
  const [x2, y2] = project(truth.lat, truth.lon);
  const ln = document.createElementNS(SVG_NS, "line");
  ln.setAttribute("x1", x1); ln.setAttribute("y1", y1);
  ln.setAttribute("x2", x2); ln.setAttribute("y2", y2);
  ln.setAttribute("stroke", "#ffb840");
  ln.setAttribute("stroke-width", "1.5");
  ln.setAttribute("stroke-dasharray", "3 3");
  svg.appendChild(ln);
}

// Compute (lat,lon) at `nm` nautical miles on bearing
// `brgDeg` from a starting fix. Small-displacement
// equirectangular inverse (consistent with the projector).
function projectFromFix(fix, nm, brgDeg) {
  const brg = (brgDeg * Math.PI) / 180;
  const dN = nm * Math.cos(brg);
  const dE = nm * Math.sin(brg);
  const cosLat = Math.cos((fix.lat_deg * Math.PI) / 180);
  const dLat = dN / 60;
  const dLon = dE / (60 * Math.max(cosLat, 1e-6));
  return { lat: fix.lat_deg + dLat, lon: fix.lon_deg + dLon };
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
