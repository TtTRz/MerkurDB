/* MerkurDB observability console — vanilla JS SPA, no build step.
 * Hash routes: #/dashboard, #/memories, #/memory/:id, (#/graph/:id, #/log land in Task 6).
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

async function api(path) {
  const resp = await fetch(path, { headers: { Authorization: `Bearer ${store.token}` } });
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
    case 'graph': return renderGraphStub(route.id);
    case 'log': return renderLogStub();
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
    const [m, g] = await Promise.all([
      api(`/v1/memory/${encodeURIComponent(id)}`),
      api(`/v1/graph/${encodeURIComponent(id)}`).catch(err => {
        if (err && err.status === 401) throw err; // keep the gate; don't render past a cleared token
        return null; // other graph failures degrade to an empty edge list
      }),
    ]);

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

/* ---------------------------------------------------------------- stubs (Task 6) */

function renderGraphStub(id) {
  view().innerHTML = `
    <section class="panel">
      <h2>Graph neighborhood</h2>
      <p class="muted">The canvas graph view for <code>${esc(id)}</code> is delivered in Task 6.</p>
      <p><a href="#/memory/${encodeURIComponent(id)}">← Back to memory detail</a></p>
    </section>`;
}

function renderLogStub() {
  view().innerHTML = `
    <section class="panel">
      <h2>Consolidation log</h2>
      <p class="muted">The consolidation audit log is delivered in Task 6.</p>
    </section>`;
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
