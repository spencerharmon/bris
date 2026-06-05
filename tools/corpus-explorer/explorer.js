// Bris corpus explorer — pure-vanilla single-page app.
//
// Loads <corpus-root>/index.json (relative to the page),
// renders a session list, fetches the per-session
// bris-replay-report.json on demand, and renders capture and
// per-frame thumbnails.

const CORPUS_ROOT = "../../"; // tools/corpus-explorer/ → corpus root

const statusEl = document.getElementById("status");
const sessionListEl = document.getElementById("session-list");
const mainViewEl = document.getElementById("main-view");
const lightbox = document.getElementById("lightbox");
const lightboxImg = document.getElementById("lightbox-img");
const lightboxClose = document.getElementById("lightbox-close");

lightboxClose.addEventListener("click", () => {
  lightbox.hidden = true;
  lightboxImg.src = "";
});
lightbox.addEventListener("click", (e) => {
  if (e.target === lightbox) {
    lightbox.hidden = true;
    lightboxImg.src = "";
  }
});

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
  const html = [];
  html.push(`<h2>${escapeHtml(report.session_title || "(untitled)")}</h2>`);
  html.push(
    `<p class="meta">id <code>${escapeHtml(report.session_id)}</code>` +
      ` · generated ${fmtTs(report.generated_unix_ms)}` +
      ` · build ${escapeHtml(report.engine_build?.git_describe || report.engine_build?.crate_version || "?")}` +
      `</p>`,
  );
  if (!report.captures || report.captures.length === 0) {
    html.push("<p>(no captures in this session report)</p>");
    mainViewEl.innerHTML = html.join("");
    return;
  }
  for (const cap of report.captures) {
    html.push(`<section class="capture">`);
    html.push(`<header>`);
    html.push(`<h3>capture ${escapeHtml(cap.capture_id)}</h3>`);
    html.push(
      `<p class="meta">` +
        `app <code>${escapeHtml(cap.app_version || "?")}</code>` +
        ` · frames ${cap.frame_count}` +
        ` · pushed ${cap.frames_pushed}` +
        ` · fixes ${cap.fixes_published}` +
        ` · sights ${cap.sights_inserted_total}` +
        `</p>`,
    );
    html.push(renderHistogram(cap.stage_e_rejection_counts || {}));
    html.push(`</header>`);
    html.push(`<div class="thumbgrid">`);
    for (const f of cap.frames || []) {
      if (!f.render_path) continue;
      const fullUrl = CORPUS_ROOT + sessionDir + f.render_path;
      const stage =
        f.stage_e_outcomes && f.stage_e_outcomes.length > 0
          ? summarizeStageE(f.stage_e_outcomes)
          : "no stage E attempt";
      html.push(
        `<a class="thumb" href="#" data-full="${escapeAttr(fullUrl)}" title="${escapeAttr(
          `frame ${f.seq} · ${f.classification} · ${stage}`,
        )}">` +
          `<img loading="lazy" src="${escapeAttr(fullUrl)}" alt="frame ${f.seq}" />` +
          `<span class="seq">${f.seq}</span>` +
          (f.sight_emitted ? `<span class="ok">✓</span>` : "") +
          `</a>`,
      );
    }
    html.push(`</div>`);
    html.push(`</section>`);
  }
  mainViewEl.innerHTML = html.join("");
  for (const a of mainViewEl.querySelectorAll("a.thumb")) {
    a.addEventListener("click", (e) => {
      e.preventDefault();
      lightboxImg.src = a.dataset.full;
      lightbox.hidden = false;
    });
  }
}

function renderHistogram(hist) {
  const keys = Object.keys(hist).filter((k) => hist[k] > 0);
  if (keys.length === 0) return '<p class="meta">no Stage E rejections</p>';
  keys.sort((a, b) => hist[b] - hist[a]);
  const parts = keys
    .map((k) => `<li><code>${escapeHtml(k)}</code>: ${hist[k]}</li>`)
    .join("");
  return `<ul class="hist">${parts}</ul>`;
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
function escapeAttr(s) {
  return escapeHtml(s);
}
function fmtTs(ms) {
  if (!ms) return "?";
  return new Date(ms).toISOString().replace("T", " ").slice(0, 19) + " UTC";
}

loadIndex();
