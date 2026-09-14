/* MerkurDB observability console — vanilla JS SPA, no build step.
 * Hash routes: #/dashboard, #/memories, #/memory/:id, #/graph/:id, #/log.
 * All /v1 calls carry the bearer token from localStorage; 401 clears it and shows the gate.
 */
'use strict';

/* ---------------------------------------------------------------- store */

const store = {
  get token() { return localStorage.getItem('merkur_token') || ''; },
  set token(v) { v ? localStorage.setItem('merkur_token', v) : localStorage.removeItem('merkur_token'); },
};

/* ---------------------------------------------------------------- api */

class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

async function api(path, ns) {
  const headers = { Authorization: `Bearer ${store.token}` };
  if (ns) headers['X-Merkur-Namespace'] = ns;
  const resp = await fetch(path, { headers });
  if (resp.status === 401) {
    store.token = '';
    showTokenGate();
    throw new ApiError(401, 'unauthorized');
  }
  if (resp.status === 429) {
    throw new ApiError(429, 'rate limited');
  }
  if (!resp.ok) {
    throw new ApiError(resp.status, `${resp.status} ${await resp.text()}`);
  }
  return resp.json();
}

/* ---------------------------------------------------------------- helpers */

const view = () => document.getElementById('view');

function esc(s) {
  return String(s === null || s === undefined ? '' : s)
    .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}

function truncate(s, n) {
  s = String(s || '');
  return s.length > n ? s.slice(0, n) + '…' : s;
}

function fmtDate(ts) {
  if (!ts) return '—';
  const d = new Date(ts);
  return isNaN(d) ? esc(ts) : d.toLocaleString();
}

function fmtNum(n) {
  return (n === null || n === undefined) ? '—' : Number(n).toLocaleString();
}

function fmtFloat(x, digits) {
  return (x === null || x === undefined || isNaN(Number(x)))
    ? '—' : Number(x).toFixed(digits === undefined ? 3 : digits);
}

function fmtUptime(seconds) {
  let s = Number(seconds) || 0;
  const d = Math.floor(s / 86400); s -= d * 86400;
  const h = Math.floor(s / 3600); s -= h * 3600;
  const m = Math.floor(s / 60); s -= m * 60;
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}

const LEVELS = ['full', 'summary', 'title', 'archived'];
const LEVEL_BY_CODE = { '-1': 'archived', '0': 'title', '1': 'summary', '2': 'full' };

function levelBadge(level) {
  const lv = String(level || '').toLowerCase();
  const cls = LEVELS.includes(lv) ? ` lv-${lv}` : '';
  return `<span class="badge${cls}">${esc(lv || 'unknown')}</span>`;
}

/* ---------------------------------------------------------------- routing */

function parseHash() {
  const raw = location.hash.slice(1) || '/dashboard';
  const qIdx = raw.indexOf('?');
  const path = qIdx === -1 ? raw : raw.slice(0, qIdx);
  const query = new URLSearchParams(qIdx === -1 ? '' : raw.slice(qIdx + 1));
  return { path, query };
}

function currentRoute() {
  const { path, query } = parseHash();
  let m = path.match(/^\/memory\/(.+)$/);
  if (m) return { name: 'memory', id: decodeURIComponent(m[1]), query };
  m = path.match(/^\/graph\/(.+)$/);
  if (m) return { name: 'graph', id: decodeURIComponent(m[1]), query };
  if (path === '/memories') return { name: 'memories', query };
  if (path === '/log') return { name: 'log', query };
  return { name: 'dashboard', query };
}

function setActiveNav(name) {
  const map = { dashboard: '/dashboard', memories: '/memories', memory: '/memories', log: '/log' };
  document.querySelectorAll('nav.nav a').forEach(a => {
    a.classList.toggle('active', a.dataset.nav === map[name]);
  });
}

function render() {
  const route = currentRoute();
  setActiveNav(route.name);
  if (!store.token) { showTokenGate(); return; }
  switch (route.name) {
    case 'memories': return renderMemories(route.query);
    case 'memory': return renderMemoryDetail(route.id);
    case 'graph': return renderGraph(route.id);
    case 'log': return renderLog();
    default: return renderDashboard();
  }
}

/* ---------------------------------------------------------------- chrome */

function showTokenGate() {
  view().innerHTML = `
    <section class="gate panel">
      <h2>Token required</h2>
      <p>Enter a bearer token in the top bar to query the MerkurDB API.
         The token is stored in <code>localStorage</code> and sent as
         <code>Authorization: Bearer …</code> on every <code>/v1</code> call.</p>
      <p class="muted">It is cleared automatically when the server answers 401.</p>
    </section>`;
  document.getElementById('token-input').focus();
}

function showError(err, retry) {
  const is429 = err && err.status === 429;
  const msg = is429
    ? 'Rate limited by the server (429).'
    : `Request failed: ${esc(err && err.message ? err.message : String(err))}`;
  view().innerHTML = `
    <section class="panel">
      <div class="notice${is429 ? ' notice-warn' : ' notice-err'}">${msg}</div>
      <p><button id="retry-btn" type="button">Retry</button></p>
    </section>`;
  document.getElementById('retry-btn').addEventListener('click', retry);
}

function runView(fn) {
  Promise.resolve()
    .then(fn)
    .catch(err => {
      if (err && err.status === 401) return; // gate already shown by api()
      showError(err, render);
    });
}

/* ---------------------------------------------------------------- dashboard */

function renderDashboard() {
  runView(async () => {
    const s = await api('/v1/status');
    const levelRows = Object.keys(LEVEL_BY_CODE).map(code => {
      const name = LEVEL_BY_CODE[code];
      const n = (s.by_level && s.by_level[code] !== undefined) ? s.by_level[code] : 0;
      return `<tr><td>${levelBadge(name)}</td><td class="muted">${esc(code)}</td><td class="num">${fmtNum(n)}</td></tr>`;
    }).join('');
    const nsEntries = Object.entries(s.by_namespace || {}).sort((a, b) => b[1] - a[1]);
    const nsRows = nsEntries.length
      ? nsEntries.map(([ns, n]) => `
          <tr class="clickable" data-ns="${esc(ns)}">
            <td>${esc(ns)}</td><td class="num">${fmtNum(n)}</td>
          </tr>`).join('')
      : '<tr><td colspan="2" class="muted">No namespaces.</td></tr>';

    view().innerHTML = `
      <section class="cards">
        <div class="card"><div class="card-label">Memories</div><div class="card-value">${fmtNum(s.total_memories)}</div></div>
        <div class="card"><div class="card-label">Edges</div><div class="card-value">${fmtNum(s.total_edges)}</div></div>
        <div class="card"><div class="card-label">Pending consolidation</div><div class="card-value">${fmtNum(s.pending_consolidation)}</div></div>
        <div class="card"><div class="card-label">Uptime</div><div class="card-value">${esc(fmtUptime(s.uptime_seconds))}</div></div>
      </section>
      <section class="grid-2">
        <div class="panel">
          <h3>By level</h3>
          <table class="tbl">
            <thead><tr><th>Level</th><th>Code</th><th class="num">Count</th></tr></thead>
            <tbody>${levelRows}</tbody>
          </table>
        </div>
        <div class="panel">
          <h3>By namespace</h3>
          <table class="tbl">
            <thead><tr><th>Namespace</th><th class="num">Count</th></tr></thead>
            <tbody>${nsRows}</tbody>
          </table>
        </div>
      </section>`;

    view().querySelectorAll('tr.clickable[data-ns]').forEach(tr => {
      tr.addEventListener('click', () => {
        location.hash = `#/memories?ns=${encodeURIComponent(tr.dataset.ns)}`;
      });
    });
  });
}

/* ---------------------------------------------------------------- memories browse */

const PAGE_SIZE = 20;

function renderMemories(query) {
  runView(async () => {
    const ns = query.get('ns') || '';
    const level = query.get('level') || '';
    const category = query.get('category') || '';
    const offset = Math.max(0, parseInt(query.get('offset') || '0', 10) || 0);

    const params = new URLSearchParams();
    if (ns) params.set('namespace', ns);
    if (level && level !== 'all') params.set('level', level);
    if (category) params.set('category', category);
    params.set('offset', String(offset));
    params.set('limit', String(PAGE_SIZE));

    const data = await api(`/v1/memories?${params.toString()}`);
    const items = data.items || [];
    const total = data.total || 0;

    const levelOpts = ['all'].concat(LEVELS).map(lv =>
      `<option value="${lv}"${lv === (level || 'all') ? ' selected' : ''}>${lv}</option>`).join('');

    const rows = items.length
      ? items.map(m => `
          <tr class="clickable" data-id="${esc(m.id)}">
            <td class="cell-content">${esc(truncate(m.content, 120))}</td>
            <td>${levelBadge(m.level)}</td>
            <td class="num">${fmtFloat(m.importance, 2)}</td>
            <td class="num">${fmtFloat(m.weight, 2)}</td>
            <td class="cell-date">${fmtDate(m.created_at)}</td>
          </tr>`).join('')
      : '<tr><td colspan="5" class="muted">No memories match these filters.</td></tr>';

    const rangeLabel = items.length
      ? `${data.offset}–${data.offset + items.length} / ${total}`
      : `0–0 / ${total}`;
    const canPrev = offset > 0;
    const canNext = offset + items.length < total;

    view().innerHTML = `
      <section class="panel">
        <form id="filter-form" class="filterbar">
          <label>Namespace <input name="ns" type="text" value="${esc(ns)}" placeholder="default"></label>
          <label>Level <select name="level">${levelOpts}</select></label>
          <label>Category <input name="category" type="text" value="${esc(category)}"></label>
          <button type="submit">Apply</button>
        </form>
        <table class="tbl">
          <thead><tr>
            <th>Content</th><th>Level</th><th class="num">Importance</th>
            <th class="num">Weight</th><th>Created</th>
          </tr></thead>
          <tbody>${rows}</tbody>
        </table>
        <div class="pager">
          <button id="pg-prev" type="button" ${canPrev ? '' : 'disabled'}>‹ Prev</button>
          <span class="pager-label">${esc(rangeLabel)}</span>
          <button id="pg-next" type="button" ${canNext ? '' : 'disabled'}>Next ›</button>
        </div>
      </section>`;

    const gotoFilters = (newOffset) => {
      const p = new URLSearchParams();
      const f = document.getElementById('filter-form');
      const fns = f.elements.ns.value.trim();
      const flv = f.elements.level.value;
      const fcat = f.elements.category.value.trim();
      if (fns) p.set('ns', fns);
      if (flv && flv !== 'all') p.set('level', flv);
      if (fcat) p.set('category', fcat);
      if (newOffset > 0) p.set('offset', String(newOffset));
      const qs = p.toString();
      location.hash = `#/memories${qs ? '?' + qs : ''}`;
    };

    document.getElementById('filter-form').addEventListener('submit', ev => {
      ev.preventDefault();
      gotoFilters(0);
    });
    document.getElementById('pg-prev').addEventListener('click', () => {
      if (canPrev) gotoFilters(Math.max(0, offset - PAGE_SIZE));
    });
    document.getElementById('pg-next').addEventListener('click', () => {
      if (canNext) gotoFilters(offset + PAGE_SIZE);
    });
    view().querySelectorAll('tr.clickable[data-id]').forEach(tr => {
      tr.addEventListener('click', () => {
        location.hash = `#/memory/${encodeURIComponent(tr.dataset.id)}`;
      });
    });
  });
}

/* ---------------------------------------------------------------- memory detail */

function kvTable(obj, renderVal) {
  const entries = Object.entries(obj || {});
  if (!entries.length) return '<p class="muted">None.</p>';
  return `<table class="tbl tbl-kv"><tbody>${entries.map(([k, v]) =>
    `<tr><td class="kv-key">${esc(k)}</td><td>${renderVal(v)}</td></tr>`).join('')}</tbody></table>`;
}

function renderMemoryDetail(id) {
  runView(async () => {
    // Graph BFS is scoped by the X-Merkur-Namespace header, so the memory must
    // be fetched first to learn its namespace (sequential, not Promise.all).
    const m = await api(`/v1/memory/${encodeURIComponent(id)}`);
    const g = await api(`/v1/graph/${encodeURIComponent(id)}`, m.namespace).catch(err => {
      if (err && err.status === 401) throw err; // keep the gate; don't render past a cleared token
      return null; // other graph failures degrade to an empty edge list
    });

    const banner = m.invalid_at
      ? `<div class="banner-invalid">This memory was invalidated at ${fmtDate(m.invalid_at)} — it is hidden from all retrieval channels.</div>`
      : '';

    // Node labels for edge endpoints: center + neighborhood.
    const labels = {};
    labels[id] = truncate(m.abstract || m.content, 60);
    (g && g.neighborhood ? g.neighborhood : []).forEach(n => {
      labels[n.id] = truncate(n.abstract || n.content, 60);
    });

    const edges = g && g.edges ? g.edges : [];
    const edgeRows = edges.length
      ? edges.map(e => {
          const ep = (eid) => eid === id
            ? `<span class="this-mem">this memory</span>`
            : `<a href="#/memory/${encodeURIComponent(eid)}" title="${esc(eid)}">${esc(labels[eid] || eid)}</a>`;
          return `<tr>
            <td>${ep(e.source_id)}</td>
            <td class="edge-rel">${esc(e.relation)} <span class="muted">(${fmtFloat(e.weight, 2)}${e.edge_type ? ', ' + esc(e.edge_type) : ''})</span></td>
            <td>${ep(e.target_id)}</td>
          </tr>`;
        }).join('')
      : '<tr><td colspan="3" class="muted">No edges in this neighborhood.</td></tr>';

    view().innerHTML = `
      ${banner}
      <section class="panel">
        <div class="detail-head">
          <h2 class="detail-title">Memory <code>${esc(m.id)}</code></h2>
          <a class="btn" href="#/graph/${encodeURIComponent(m.id)}">Open graph</a>
        </div>
        <dl class="fields">
          <dt>Content</dt><dd class="content-full">${esc(m.content)}</dd>
          <dt>Abstract</dt><dd>${m.abstract ? esc(m.abstract) : '<span class="muted">—</span>'}</dd>
          <dt>Level</dt><dd>${levelBadge(m.level)}${m.pending_consolidation ? ' <span class="badge lv-pending">pending consolidation</span>' : ''}</dd>
          <dt>Category</dt><dd>${esc(m.category || '—')}</dd>
          <dt>Namespace</dt><dd>${esc(m.namespace)}</dd>
          <dt>Importance</dt><dd>${fmtFloat(m.importance, 3)}</dd>
          <dt>Weight</dt><dd>${fmtFloat(m.weight, 3)}</dd>
          <dt>Access count</dt><dd>${fmtNum(m.access_count)}</dd>
          <dt>Created</dt><dd>${fmtDate(m.created_at)}</dd>
          <dt>Updated</dt><dd>${fmtDate(m.updated_at)}</dd>
          <dt>Last accessed</dt><dd>${fmtDate(m.accessed_at)}</dd>
          <dt>Valid at</dt><dd>${fmtDate(m.valid_at)}</dd>
          <dt>Invalid at</dt><dd>${m.invalid_at ? `<span class="invalid-text">${fmtDate(m.invalid_at)}</span>` : '<span class="muted">—</span>'}</dd>
        </dl>
        <h3>Context</h3>
        ${kvTable(m.context, v => esc(v))}
        <h3>Metadata</h3>
        ${kvTable(m.metadata, v => `<code>${esc(JSON.stringify(v))}</code>`)}
      </section>
      <section class="panel">
        <h3>Edges (${edges.length})</h3>
        <table class="tbl">
          <thead><tr><th>Source</th><th>Relation</th><th>Target</th></tr></thead>
          <tbody>${edgeRows}</tbody>
        </table>
      </section>`;
  });
}

/* ---------------------------------------------------------------- graph view */

// Force layout (reference implementation from the Task 6 brief): O(n²) charge
// repulsion + weighted spring attraction, velocity damping. Runs in world
// coordinates around the origin; the draw step fits the resulting bounding
// box into the canvas, so the layout itself is resolution-independent.
function forceLayout(nodes, edges, centerId, iterations = 300) {
  const pos = new Map(nodes.map((n, i) => [n.id, {
    x: 250 * Math.cos(i * 2.399), y: 250 * Math.sin(i * 2.399), vx: 0, vy: 0,
  }]));
  for (let k = 0; k < iterations; k++) {
    for (const [id, p] of pos) {                     // charge repulsion
      for (const [id2, q] of pos) {
        if (id === id2) continue;
        const dx = p.x - q.x, dy = p.y - q.y, d2 = dx * dx + dy * dy + 1;
        const f = Math.min(4000 / d2, 4);
        p.vx += dx * f / Math.sqrt(d2) * 0.5; p.vy += dy * f / Math.sqrt(d2) * 0.5;
      }
    }
    for (const e of edges) {                         // spring attraction
      const p = pos.get(e.source_id), q = pos.get(e.target_id);
      if (!p || !q) continue;
      const dx = q.x - p.x, dy = q.y - p.y;
      p.vx += dx * 0.005 * e.weight; p.vy += dy * 0.005 * e.weight;
      q.vx -= dx * 0.005 * e.weight; q.vy -= dy * 0.005 * e.weight;
    }
    for (const [id, p] of pos) {
      p.vx *= 0.85; p.vy *= 0.85; p.x += p.vx; p.y += p.vy;
      if (id === centerId) { p.x = 0; p.y = 0; p.vx = 0; p.vy = 0; } // pin center
    }
  }
  return pos;
}

function renderGraph(id) {
  runView(async () => {
    // Memory first: the graph BFS is scoped by the X-Merkur-Namespace header,
    // which must carry this memory's own namespace.
    const m = await api(`/v1/memory/${encodeURIComponent(id)}`).catch(err => {
      if (err && err.status === 401) throw err; // keep the gate
      return null; // center node still renders, labeled by id
    });
    const g = await api(`/v1/graph/${encodeURIComponent(id)}?depth=2`, m ? m.namespace : undefined);

    const neighbors = g.neighborhood || [];
    const edges = g.edges || [];

    const nodes = [];
    const seen = new Set([g.center]);
    nodes.push({
      id: g.center,
      level: m ? m.level : '',
      label: m ? truncate(m.abstract || m.content, 28) : truncate(g.center, 28),
      center: true,
    });
    neighbors.forEach(n => {
      if (seen.has(n.id)) return;
      seen.add(n.id);
      nodes.push({
        id: n.id,
        level: n.level,
        label: truncate(n.abstract || n.content, 28) || n.id,
        center: false,
      });
    });

    const head = `
      <div class="graph-head">
        <h2 class="detail-title">Graph neighborhood of <code>${esc(g.center)}</code></h2>
        <a class="btn" href="#/memory/${encodeURIComponent(g.center)}">← Memory detail</a>
      </div>
      <p class="graph-meta">${nodes.length} node${nodes.length === 1 ? '' : 's'} ·
        ${edges.length} edge${edges.length === 1 ? '' : 's'} ·
        depth ${esc(g.depth)} · degree limit ${esc(g.degree_limit)}</p>`;

    if (!neighbors.length && !edges.length) {
      view().innerHTML = `
        <section class="panel">
          ${head}
          <div class="graph-empty muted">
            <p>This memory has no graph neighborhood yet.</p>
            <p>Edges appear here once consolidation or manual relate calls connect it to other memories.</p>
          </div>
        </section>`;
      return;
    }

    view().innerHTML = `
      <section class="panel">
        ${head}
        <div class="graph-canvas-wrap"><canvas id="graph-canvas" class="graph-canvas"></canvas></div>
        <div class="graph-legend">
          <span><i class="dot dot-center"></i>center</span>
          <span><i class="dot dot-full"></i>full</span>
          <span><i class="dot dot-summary"></i>summary</span>
          <span><i class="dot dot-title"></i>title</span>
          <span><i class="dot dot-archived"></i>archived</span>
          <span class="muted">click a node to open its detail</span>
        </div>
      </section>`;

    const canvas = document.getElementById('graph-canvas');
    const wrap = canvas.parentElement;
    const ctx = canvas.getContext('2d');
    const cssVars = getComputedStyle(document.documentElement);
    const col = name => (cssVars.getPropertyValue(name).trim() || '#8b949e');
    const ACCENT = col('--accent'), MUTED = col('--muted'),
      BORDER = col('--border'), PANEL = col('--panel');
    const LEVEL_STROKE = {
      full: col('--green'), summary: col('--accent'),
      title: col('--amber'), archived: col('--muted'),
    };

    const world = forceLayout(nodes, edges, g.center);
    const NODE_R = 9, CENTER_R = 14;
    let hitNodes = []; // [{id, x, y, r}] in CSS pixels, rebuilt on every draw

    function draw() {
      const dpr = window.devicePixelRatio || 1;
      const w = wrap.clientWidth, h = 600;
      if (!w) return;
      canvas.width = Math.round(w * dpr);
      canvas.height = Math.round(h * dpr);
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, w, h);

      // Fit the world bounding box into the canvas with padding.
      const pad = 40;
      let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
      for (const p of world.values()) {
        minX = Math.min(minX, p.x); maxX = Math.max(maxX, p.x);
        minY = Math.min(minY, p.y); maxY = Math.max(maxY, p.y);
      }
      const bw = Math.max(maxX - minX, 1), bh = Math.max(maxY - minY, 1);
      const scale = Math.min((w - 2 * pad) / bw, (h - 2 * pad) / bh, 1.6);
      const cx = (minX + maxX) / 2, cy = (minY + maxY) / 2;
      const X = wx => (wx - cx) * scale + w / 2;
      const Y = wy => (wy - cy) * scale + h / 2;

      // Edges: gray lines, width follows weight.
      for (const e of edges) {
        const p = world.get(e.source_id), q = world.get(e.target_id);
        if (!p || !q) continue;
        ctx.beginPath();
        ctx.moveTo(X(p.x), Y(p.y));
        ctx.lineTo(X(q.x), Y(q.y));
        ctx.strokeStyle = BORDER;
        ctx.lineWidth = Math.min(0.6 + (Number(e.weight) || 0) * 3, 5);
        ctx.stroke();
      }

      // Nodes + labels.
      hitNodes = [];
      ctx.textAlign = 'center';
      ctx.font = '11px -apple-system, "Segoe UI", Roboto, sans-serif';
      for (const n of nodes) {
        const p = world.get(n.id);
        if (!p) continue;
        const x = X(p.x), y = Y(p.y), r = n.center ? CENTER_R : NODE_R;
        hitNodes.push({ id: n.id, x, y, r });
        ctx.beginPath();
        ctx.arc(x, y, r, 0, Math.PI * 2);
        ctx.fillStyle = n.center ? ACCENT : PANEL;
        ctx.fill();
        ctx.lineWidth = n.center ? 2.5 : 2;
        ctx.strokeStyle = n.center
          ? ACCENT
          : (LEVEL_STROKE[String(n.level || '').toLowerCase()] || MUTED);
        ctx.stroke();
        ctx.fillStyle = MUTED;
        ctx.fillText(n.label || n.id, x, y + r + 14, 160);
      }
    }

    function nearestNode(ev) {
      const rect = canvas.getBoundingClientRect();
      const mx = ev.clientX - rect.left, my = ev.clientY - rect.top;
      let best = null, bestD = Infinity;
      for (const hn of hitNodes) {
        const d = Math.hypot(hn.x - mx, hn.y - my);
        if (d < bestD) { bestD = d; best = hn; }
      }
      return best && bestD <= Math.max(best.r + 6, 16) ? best : null;
    }

    canvas.addEventListener('click', ev => {
      const hit = nearestNode(ev);
      if (hit) location.hash = `#/memory/${encodeURIComponent(hit.id)}`;
    });
    canvas.addEventListener('mousemove', ev => {
      const hit = nearestNode(ev);
      canvas.style.cursor = hit ? 'pointer' : 'default';
      canvas.title = hit ? hit.id : '';
    });
    window.addEventListener('resize', function onResize() {
      if (!canvas.isConnected) { // view navigated away — self-clean
        window.removeEventListener('resize', onResize);
        return;
      }
      draw();
    });

    draw();
  });
}

/* ---------------------------------------------------------------- consolidation log */

function fmtDuration(start, end) {
  const a = new Date(start), b = new Date(end);
  if (isNaN(a) || isNaN(b)) return '—';
  const ms = b - a;
  if (ms < 0) return '—';
  if (ms < 1000) return `${ms} ms`;
  if (ms < 60000) return `${(ms / 1000).toFixed(1)} s`;
  return `${Math.floor(ms / 60000)}m ${Math.round((ms % 60000) / 1000)}s`;
}

function renderLog() {
  runView(async () => {
    const data = await api('/v1/consolidate/log?limit=100');
    const entries = data.entries || [];
    const rows = entries.length
      ? entries.map(e => {
          const errs = Number(e.errors) || 0;
          return `<tr${errs > 0 ? ' class="log-row-err"' : ''}>
            <td class="num">${fmtNum(e.id)}</td>
            <td class="cell-date">${fmtDate(e.started_at)}</td>
            <td class="cell-date">${fmtDate(e.finished_at)}</td>
            <td class="num">${fmtDuration(e.started_at, e.finished_at)}</td>
            <td class="num">${fmtNum(e.memories_processed)}</td>
            <td class="num">${fmtNum(e.edges_created)}</td>
            <td class="num">${fmtNum(e.absorptions)}</td>
            <td class="num">${fmtNum(e.invalidations)}</td>
            <td class="num cell-errors">${fmtNum(errs)}</td>
          </tr>`;
        }).join('')
      : '<tr><td colspan="9" class="muted">No consolidation runs recorded yet.</td></tr>';

    view().innerHTML = `
      <section class="panel">
        <h2>Consolidation log</h2>
        <p class="muted">Latest ${entries.length} run${entries.length === 1 ? '' : 's'} (newest first).
           Rows in red finished with errors.</p>
        <table class="tbl tbl-zebra">
          <thead><tr>
            <th class="num">#</th><th>Started</th><th>Finished</th><th class="num">Duration</th>
            <th class="num">Processed</th><th class="num">Edges created</th>
            <th class="num">Absorbed</th><th class="num">Invalidated</th><th class="num">Errors</th>
          </tr></thead>
          <tbody>${rows}</tbody>
        </table>
      </section>`;
  });
}

/* ---------------------------------------------------------------- boot */

document.getElementById('token-save').addEventListener('click', () => {
  store.token = document.getElementById('token-input').value.trim();
  render();
});
document.getElementById('token-input').addEventListener('keydown', ev => {
  if (ev.key === 'Enter') {
    store.token = ev.target.value.trim();
    render();
  }
});
document.getElementById('token-input').value = store.token;

window.addEventListener('hashchange', render);
render();
