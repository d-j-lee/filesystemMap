(() => {
'use strict';

// ---------------------------------------------------------------- data
const D = JSON.parse(document.getElementById('fsmap-data').textContent);
const N = D.nodes;
const COUNT = N.length;
const NOW = D.generatedAt || Math.floor(Date.now() / 1000);
const kids = Array.from({ length: COUNT }, () => []);
const depth = new Int32Array(COUNT);
const end = new Int32Array(COUNT);
for (let i = 0; i < COUNT; i++) end[i] = i + 1;
for (let i = 1; i < COUNT; i++) {
  const p = N[i].parent;
  kids[p].push(i);
  depth[i] = depth[p] + 1;
}
for (let i = COUNT - 1; i > 0; i--) {
  const p = N[i].parent;
  if (end[i] > end[p]) end[p] = end[i];
}
const isDir = (i) => N[i].kind === 'dir';
const isFile = (i) => N[i].kind === 'file';
const within = (anc, i) => i >= anc && i < end[anc];
const v = (i, k) => N[i][k] || 0;
const filesOf = (i) => (isFile(i) ? 1 : v(i, 'files'));
const flagBit = (name) => 1 << D.flagNames.indexOf(name);
const hasFlag = (i, name) => (v(i, 'flags') & flagBit(name)) !== 0;
const analysed = (i) => (N[i].zone === undefined || N[i].zone === 'vendored') && N[i].git !== 'ignored';
const tokens = new Float64Array(COUNT);
for (let i = COUNT - 1; i >= 0; i--) {
  if (isFile(i) && analysed(i) && v(i, 'lines') > 0) tokens[i] += v(i, 'bytes') / 4;
  if (i > 0) tokens[N[i].parent] += tokens[i];
}
const pathCache = new Array(COUNT);
function path(i) {
  if (i === 0) return '.';
  if (pathCache[i] !== undefined) return pathCache[i];
  const p = N[i].parent;
  const s = p === 0 ? N[i].name : path(p) + '/' + N[i].name;
  pathCache[i] = s;
  return s;
}
const rootName = N[0].name;
const langName = (i) => (N[i].lang !== undefined ? D.languages[N[i].lang].name : '');
const authorName = (a) => (D.authors && D.authors[a] !== undefined ? D.authors[a] : null);
const findingsByNode = new Map();
(D.findings || []).forEach((f, fi) => f.nodes.forEach((nd) => {
  if (!findingsByNode.has(nd)) findingsByNode.set(nd, []);
  findingsByNode.get(nd).push(fi);
}));

// ---------------------------------------------------------------- format
const UNITS = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
function fmtBytes(b) {
  let x = b, u = 0;
  while (x >= 1024 && u < UNITS.length - 1) { x /= 1024; u++; }
  return u === 0 ? `${b} B` : `${x >= 100 ? x.toFixed(0) : x.toFixed(1)} ${UNITS[u]}`;
}
function fmtCount(c) {
  if (c >= 1e6) return (c / 1e6).toFixed(1) + 'M';
  if (c >= 1e4) return Math.round(c / 1e3) + 'k';
  if (c >= 1e3) return (c / 1e3).toFixed(1) + 'k';
  return String(Math.round(c));
}
const fmtInt = (c) => Math.round(c).toLocaleString('en-US');
function fmtDate(ts) {
  if (!ts) return '—';
  return new Date(ts * 1000).toISOString().slice(0, 10);
}
function fmtAgo(ts) {
  if (!ts) return '—';
  const d = Math.max(0, (NOW - ts) / 86400);
  if (d < 1) return 'today';
  if (d < 60) return `${Math.round(d)} days ago`;
  if (d < 730) return `${Math.round(d / 30)} months ago`;
  return `${(d / 365).toFixed(1)} years ago`;
}
const pct = (x) => `${(x * 100).toFixed(x < 0.1 ? 1 : 0)}%`;

// ---------------------------------------------------------------- dom
function h(tag, props, ...children) {
  const e = document.createElement(tag);
  if (props) {
    for (const [k, val] of Object.entries(props)) {
      if (val === null || val === undefined || val === false) continue;
      if (k === 'class') e.className = val;
      else if (k === 'text') e.textContent = val;
      else if (k.startsWith('on')) e.addEventListener(k.slice(2), val);
      else if (k === 'style') Object.assign(e.style, val);
      else e.setAttribute(k, val === true ? '' : val);
    }
  }
  for (const c of children.flat(Infinity)) {
    if (c === null || c === undefined || c === false) continue;
    e.append(c instanceof Node ? c : String(c));
  }
  return e;
}
const svgNS = 'http://www.w3.org/2000/svg';
function s(tag, attrs, ...children) {
  const e = document.createElementNS(svgNS, tag);
  for (const [k, val] of Object.entries(attrs || {})) if (val !== null && val !== undefined) e.setAttribute(k, val);
  for (const c of children.flat()) if (c) e.append(c instanceof Node ? c : document.createTextNode(String(c)));
  return e;
}
function copyButton(text, target) {
  const b = h('button', { class: 'btn', type: 'button', text: 'Copy' });
  b.addEventListener('click', () => {
    const done = () => { b.textContent = 'Copied'; setTimeout(() => { b.textContent = 'Copy'; }, 1400); };
    const fallback = () => {
      const r = document.createRange();
      r.selectNodeContents(target);
      const sel = window.getSelection();
      sel.removeAllRanges();
      sel.addRange(r);
      b.textContent = 'Selected — press Ctrl/⌘+C';
    };
    try {
      navigator.clipboard.writeText(text).then(done, fallback);
    } catch (_) { fallback(); }
  });
  return b;
}

// ---------------------------------------------------------------- colour
let C = {};
function readColors() {
  const cs = getComputedStyle(document.documentElement);
  const get = (n) => cs.getPropertyValue(n).trim();
  C = {
    series: ['--s1', '--s2', '--s3', '--s4', '--s5', '--s6', '--s7', '--s8'].map(get),
    other: get('--other'), none: get('--none'), bg: get('--bg'), surface: get('--surface'),
    surface2: get('--surface-2'), surface3: get('--surface-3'), line: get('--line'), fg: get('--fg'),
    fg2: get('--fg-2'), muted: get('--muted'), accent: get('--accent'),
    seqLo: get('--seq-lo'), seqHi: get('--seq-hi'), heatLo: get('--heat-lo'), heatHi: get('--heat-hi'),
    critical: get('--critical'),
  };
}
function hexRgb(hex) {
  const m = /^#?([0-9a-f]{6})$/i.exec(hex);
  if (!m) return [128, 128, 128];
  const n = parseInt(m[1], 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}
function mix(a, b, t) {
  const x = hexRgb(a), y = hexRgb(b);
  const c = x.map((xv, k) => Math.round(xv + (y[k] - xv) * Math.min(1, Math.max(0, t))));
  return `rgb(${c[0]},${c[1]},${c[2]})`;
}

// Categorical groupings (fixed slot order; anything beyond folds into "other").
const ZONES = [['source', 'Source'], ['vendored', 'Vendored'], ['dependency', 'Dependency'], ['artifact', 'Build output'], ['vcs', 'Git internals']];
const CATS = [['code', 'Code', ['code']], ['docs', 'Docs', ['docs']], ['config', 'Config', ['config']], ['data', 'Data', ['data']],
  ['media', 'Images & media', ['image', 'media', 'font']], ['binary', 'Binaries & archives', ['binary', 'archive']],
  ['generated', 'Generated', ['generated']]];
const GITS = [['tracked', 'Tracked'], ['untracked', 'Untracked'], ['ignored', 'Ignored']];
const catSlot = {};
CATS.forEach(([, , cs], k) => cs.forEach((c) => { catSlot[c] = k; }));
const zoneSlot = {};
ZONES.forEach(([z], k) => { zoneSlot[z] = k; });
const gitSlot = { tracked: 0, untracked: 1, ignored: 7 };

const topLangs = (() => {
  const by = {};
  for (let i = 0; i < COUNT; i++) {
    if (isFile(i) && N[i].lang !== undefined && N[i].cat === 'code') by[N[i].lang] = (by[N[i].lang] || 0) + v(i, 'code');
  }
  return Object.entries(by).sort((a, b) => b[1] - a[1]).slice(0, 7).map((e) => +e[0]);
})();
const topOwners = (() => {
  const by = {};
  for (let i = 0; i < COUNT; i++) if (isFile(i) && N[i].owner !== undefined) by[N[i].owner] = (by[N[i].owner] || 0) + 1;
  return Object.entries(by).sort((a, b) => b[1] - a[1]).slice(0, 7).map((e) => +e[0]);
})();

const COLOR_MODES = {
  zone: { label: 'Origin', key: (i) => zoneSlot[N[i].zone || 'source'], legend: () => ZONES.map(([, l], k) => [l, slotColor(k)]) },
  category: { label: 'File kind', key: (i) => (isFile(i) ? (catSlot[N[i].cat || 'other'] ?? -1) : -2), legend: () => CATS.map(([, l], k) => [l, slotColor(k)]).concat([['Other', C.other]]) },
  language: { label: 'Language', key: (i) => (isFile(i) ? (N[i].lang !== undefined ? topLangs.indexOf(N[i].lang) : -1) : -2), legend: () => topLangs.map((l, k) => [D.languages[l].name, slotColor(k)]).concat([['Other / not code', C.other]]) },
  git: { label: 'Git state', key: (i) => (N[i].git ? gitSlot[N[i].git] : -1), legend: () => GITS.map(([g, l]) => [l, slotColor(gitSlot[g])]).concat([['Not in a repository', C.other]]) },
  age: { label: 'Last change', seq: true },
  hotspot: { label: 'Hotspot score', seq: true },
  owner: { label: 'Main author', key: (i) => (isFile(i) ? (N[i].owner !== undefined ? topOwners.indexOf(N[i].owner) : -1) : -2), legend: () => topOwners.map((a, k) => [authorName(a) || `author ${a + 1}`, slotColor(k)]).concat([['Other / no history', C.other]]) },
};
function slotColor(k) { return k >= 0 && k < 8 ? C.series[k] : C.other; }

const dominantCache = {};
/** For directories drawn as leaves: the colour group holding most of the size metric. */
function dominant(mode, metric) {
  const key = mode + '|' + metric;
  if (dominantCache[key]) return dominantCache[key];
  const acc = new Float64Array(COUNT * 9);
  const keyOf = COLOR_MODES[mode].key;
  for (let i = COUNT - 1; i >= 0; i--) {
    if (!isDir(i)) {
      const k = keyOf(i);
      acc[i * 9 + (k >= 0 ? k : 8)] += sizeOf(i, metric);
    }
    if (i > 0) { const p = N[i].parent; for (let k = 0; k < 9; k++) acc[p * 9 + k] += acc[i * 9 + k]; }
  }
  const out = new Int8Array(COUNT);
  for (let i = 0; i < COUNT; i++) {
    let best = -1, bv = 0;
    for (let k = 0; k < 9; k++) if (acc[i * 9 + k] > bv) { bv = acc[i * 9 + k]; best = k; }
    out[i] = best === 8 ? -1 : best;
  }
  dominantCache[key] = out;
  return out;
}
const hotMax = new Float32Array(COUNT);
for (let i = COUNT - 1; i >= 0; i--) {
  hotMax[i] = Math.max(hotMax[i], v(i, 'hotspot'));
  if (i > 0 && hotMax[i] > hotMax[N[i].parent]) hotMax[N[i].parent] = hotMax[i];
}
function ageOf(i) { return v(i, 'lastCommit') || v(i, 'mtime'); }
function colorOf(i, mode, metric) {
  const m = COLOR_MODES[mode];
  if (mode === 'age') {
    const t = ageOf(i);
    if (!t) return C.none;
    const days = Math.max(0, (NOW - t) / 86400);
    return mix(C.seqHi, C.seqLo, Math.log1p(days) / Math.log1p(1460));
  }
  if (mode === 'hotspot') {
    const x = hotMax[i];
    return x > 0 ? mix(C.heatLo, C.heatHi, x) : C.none;
  }
  let k = m.key(i);
  if (k === -2) k = dominant(mode, metric)[i];
  return k >= 0 ? slotColor(k) : C.other;
}

// ---------------------------------------------------------------- metrics
const METRICS = {
  disk: { label: 'Disk usage', fmt: fmtBytes },
  bytes: { label: 'Apparent size', fmt: fmtBytes },
  files: { label: 'File count', fmt: (x) => fmtInt(x) + ' files' },
  code: { label: 'Code lines', fmt: (x) => fmtInt(x) + ' lines' },
  tokens: { label: 'LLM tokens (≈ bytes/4)', fmt: (x) => '≈' + fmtCount(x) + ' tokens' },
};
function sizeOf(i, metric) {
  switch (metric) {
    case 'files': return filesOf(i);
    case 'tokens': return tokens[i];
    default: return v(i, metric);
  }
}

// ---------------------------------------------------------------- shell
const T = D.totals;
const repos = D.repos || [];
const findings = D.findings || [];
const hotspots = (D.hotspots || []);
const deps = D.deps || null;
const coupling = D.coupling || [];
const sevCount = (sv) => findings.filter((f) => f.severity === sv).length;
const hist = repos.map((r) => r.history).filter(Boolean);

document.getElementById('root-path').textContent = D.root;
document.getElementById('stamp').textContent = `scanned ${new Date(D.generatedAt * 1000).toISOString().replace('T', ' ').slice(0, 16)} UTC`;

(function ribbon() {
  const box = document.getElementById('ribbon');
  const add = (k, val, sub) => box.append(h('div', { class: 'stat' }, h('div', { class: 'k', text: k }), h('div', { class: 'v num', text: val }), sub ? h('div', { class: 's', text: sub }) : null));
  add('On disk', fmtBytes(T.disk), `${fmtBytes(T.bytes)} apparent`);
  add('Files', fmtInt(T.files), `${fmtInt(T.dirs)} directories`);
  add('Code lines', fmtCount(T.code), `${fmtCount(T.comments)} comment lines`);
  const langs = Object.entries(T.byLanguage).filter(([, b]) => b.code > 0).sort((a, b) => b[1].code - a[1].code);
  if (langs.length) add('Main language', langs[0][0], `${langs.length} languages with code`);
  if (hist.length) {
    const commits = hist.reduce((a, x) => a + x.commits, 0);
    const authors = hist.reduce((a, x) => a + x.authors, 0);
    add('Commits read', fmtInt(commits), `${repos.length} repo${repos.length === 1 ? '' : 's'}, ${authors} authors`);
  }
  add('Findings', String(findings.length), `${sevCount('high')} high · ${sevCount('medium')} medium`);
  add('Agent read cost', '≈' + fmtCount(T.tokens), 'tokens of analysed text');
})();

const VIEWS = [
  ['overview', 'Overview', findings.length],
  ['map', 'Map', null],
  ['hotspots', 'Hotspots', hotspots.length ? hotspots.filter(flagged).length : -1],
  ['architecture', 'Architecture', deps ? deps.cycles.length : -1],
  ['coupling', 'Change coupling', coupling.length ? coupling.length : -1],
  ['repos', 'Repositories', repos.length ? repos.length : -1],
  ['files', 'Files', null],
];
const tabs = document.getElementById('tabs');
for (const [id, label, count] of VIEWS) {
  if (count === -1) continue;
  tabs.append(h('a', { href: '#' + id, 'data-view': id }, label, count !== null ? h('span', { class: 'count num', text: String(count) }) : null));
}
const rendered = {};
const renderers = {};
function show(view) {
  if (!VIEWS.some(([id, , c]) => id === view && c !== -1)) view = 'overview';
  for (const [id] of VIEWS) {
    const sec = document.getElementById('view-' + id);
    sec.hidden = id !== view;
  }
  for (const a of tabs.querySelectorAll('a')) a.setAttribute('aria-current', a.dataset.view === view ? 'page' : 'false');
  if (!rendered[view]) { rendered[view] = true; renderers[view](document.getElementById('view-' + view)); }
  if (view === 'map') mapApi.resize();
}
window.addEventListener('hashchange', () => show(location.hash.slice(1)));

function flagged(hs) {
  const n = N[hs.node];
  return Boolean(n) && hs.changePct >= 0.8 && hs.complexityPct >= 0.8 && v(hs.node, 'commits') >= 5 && v(hs.node, 'recent') >= 1 && !(v(hs.node, 'flags') & flagBit('test'));
}

function goToNode(i) {
  mapApi.pending = i;
  if (location.hash === '#map') { mapApi.focus(i); } else { location.hash = 'map'; }
}

// ---------------------------------------------------------------- overview
renderers.overview = (root) => {
  const left = h('div', { class: 'panel' });
  const right = h('div', { class: 'grid', style: { display: 'grid', gap: '18px', minWidth: '0' } });
  root.append(h('div', { class: 'cols' }, left, right));

  // Findings.
  const sevs = ['high', 'medium', 'low', 'info'];
  let active = new Set(sevs);
  const list = h('div', { class: 'findings' });
  const chips = h('div', { class: 'filters' });
  const card = (f) => {
    const code = f.command ? h('code', { text: f.command }) : null;
    const target = f.nodes.find((nd) => nd !== 0);
    return h('article', { class: 'finding', 'data-sev': f.severity },
      h('div', { class: 'head' },
        h('span', { class: 'sev', 'data-sev': f.severity, text: f.severity }),
        h('span', { class: 'title', text: f.title }),
        f.new ? h('span', { class: 'tag', text: 'new' }) : null),
      h('div', { class: 'detail', text: f.detail }),
      h('div', { class: 'action', text: f.action }),
      h('div', { class: 'row' },
        code ? h('div', { class: 'cmd' }, code, copyButton(f.command, code)) : null,
        target !== undefined ? h('button', { class: 'linkish', type: 'button', text: 'Show on map', onclick: () => goToNode(target) }) : null,
        h('span', { class: 'rule', text: f.id })));
  };
  const draw = () => {
    list.replaceChildren();
    const shown = findings.filter((f) => active.has(f.severity));
    if (!shown.length) list.append(h('div', { class: 'empty', text: findings.length ? 'No findings at the selected severities.' : 'No findings. Nothing here needs attention.' }));
    // Group by severity + rule, keeping first-appearance order; show two, fold the rest.
    const groups = [];
    for (const f of shown) {
      const g = groups.find((x) => x.sev === f.severity && x.rule === f.rule);
      if (g) g.items.push(f); else groups.push({ sev: f.severity, rule: f.rule, items: [f] });
    }
    for (const g of groups) {
      g.items.slice(0, 2).forEach((f) => list.append(card(f)));
      if (g.items.length > 2) {
        const rest = g.items.slice(2);
        list.append(h('details', { class: 'more' },
          h('summary', null, `${rest.length} more ${g.rule} finding${rest.length === 1 ? '' : 's'}: `, h('span', { class: 'muted', text: rest.map((f) => f.title.replace(/^[^:]+:\s*/, '')).slice(0, 4).join(' · ') + (rest.length > 4 ? ' …' : '') })),
          h('div', { class: 'findings', style: { marginTop: '10px' } }, rest.map(card))));
      }
    }
  };
  for (const sv of sevs) {
    const n = sevCount(sv);
    if (!n) continue;
    const b = h('button', { class: 'chip', type: 'button', 'aria-pressed': 'true' }, `${sv} ${n}`);
    b.addEventListener('click', () => {
      if (active.has(sv)) active.delete(sv); else active.add(sv);
      b.setAttribute('aria-pressed', String(active.has(sv)));
      draw();
    });
    chips.append(b);
  }
  left.append(h('h2', null, 'Findings', h('span', { class: 'sub', text: 'ranked by severity, with evidence and a next step' })), chips, h('div', { style: { height: '10px' } }), list);
  draw();

  // Composition: disk by origin.
  const zonePanel = h('div', { class: 'panel' }, h('h2', null, 'Where the bytes are', h('span', { class: 'sub', text: 'disk usage by origin' })));
  const zs = ZONES.map(([z, l], k) => [l, (T.byZone[z] || {}).disk || 0, slotColor(k)]).filter((x) => x[1] > 0);
  const zTotal = zs.reduce((a, x) => a + x[1], 0) || 1;
  zonePanel.append(h('div', { class: 'stack', role: 'img', 'aria-label': 'Disk usage by origin' }, zs.map(([l, b, c]) => h('span', { title: `${l}: ${fmtBytes(b)}`, style: { width: (b / zTotal * 100) + '%', background: c } }))));
  zonePanel.append(h('div', { class: 'legend', style: { marginTop: '8px' } }, zs.map(([l, b, c]) => h('span', null, h('i', { class: 'swatch', style: { background: c } }), `${l} ${fmtBytes(b)} (${pct(b / zTotal)})`))));
  // Top-level directories.
  const top = kids[0].slice().sort((a, b) => v(b, 'disk') - v(a, 'disk')).slice(0, 8);
  const maxTop = top.length ? v(top[0], 'disk') || 1 : 1;
  zonePanel.append(h('div', { class: 'bars', style: { marginTop: '14px' } }, top.map((i) => h('div', { class: 'bar' },
    h('span', { class: 'lbl mono', title: path(i) }, h('i', { class: 'swatch', style: { background: slotColor(zoneSlot[N[i].zone || 'source']) } }), N[i].name + (isDir(i) ? '/' : '')),
    h('span', { class: 'track' }, h('span', { class: 'fill', style: { width: (v(i, 'disk') / maxTop * 100) + '%', background: slotColor(zoneSlot[N[i].zone || 'source']) } })),
    h('span', { class: 'val', text: fmtBytes(v(i, 'disk')) })))));
  right.append(zonePanel);

  // Code by language.
  const langs = Object.entries(T.byLanguage).filter(([, b]) => b.code > 0).sort((a, b) => b[1].code - a[1].code).slice(0, 8);
  if (langs.length) {
    const maxL = langs[0][1].code;
    const color = (name) => { const idx = topLangs.findIndex((l) => D.languages[l].name === name); return slotColor(idx); };
    right.append(h('div', { class: 'panel' }, h('h2', null, 'Code by language', h('span', { class: 'sub', text: 'non-blank, non-comment lines' })),
      h('div', { class: 'bars' }, langs.map(([name, b]) => h('div', { class: 'bar' },
        h('span', { class: 'lbl' }, h('i', { class: 'swatch', style: { background: color(name) } }), name),
        h('span', { class: 'track' }, h('span', { class: 'fill', style: { width: (b.code / maxL * 100) + '%', background: color(name) } })),
        h('span', { class: 'val', text: `${fmtCount(b.code)} · ${fmtInt(b.files)} files` }))))));
  }

  // Git history at a glance.
  const r0 = repos.find((r) => r.history && r.history.commits > 0);
  if (r0) {
    const hi = r0.history;
    const act = (D.activity || []).find((a) => a[0] === r0.node);
    const panel = h('div', { class: 'panel' }, h('h2', null, 'History', h('span', { class: 'sub', text: repos.length > 1 ? `${r0.path} · ${repos.length} repositories` : `${fmtDate(hi.first)} → ${fmtDate(hi.last)}` })));
    if (act) panel.append(sparkline(act[1], 'Commits per month, oldest left'));
    panel.append(h('dl', { class: 'details', style: { display: 'grid', gridTemplateColumns: 'auto 1fr', gap: '4px 10px', margin: '10px 0 0' } },
      h('dt', { class: 'muted', text: 'Commits read' }), h('dd', { style: { margin: 0 }, text: `${fmtInt(hi.commits)} (${fmtInt(hi.counted)} counted as revisions)` }),
      h('dt', { class: 'muted', text: 'Authors' }), h('dd', { style: { margin: 0 }, text: `${hi.authors} total, ${hi.activeAuthors} active in the last 6 months of history` }),
      hi.truckFactor !== undefined && hi.truckFactor !== null ? [h('dt', { class: 'muted', text: 'Truck factor' }), h('dd', { style: { margin: 0 }, text: `${hi.truckFactor}${hi.truckAuthors && hi.truckAuthors.length && D.authors ? ' — ' + hi.truckAuthors.map(authorName).filter(Boolean).join(', ') : ''}` })] : null,
      r0.shallow ? [h('dt', { class: 'muted', text: 'Note' }), h('dd', { style: { margin: 0 }, text: 'Shallow clone: history is partial.' })] : null));
    right.append(panel);
  }
};

function sparkline(series, label) {
  // series[0] = most recent month; draw oldest → newest left to right.
  const pts = series.slice().reverse();
  const W = 300, H = 44, max = Math.max(1, ...pts);
  const x = (k) => (k / (pts.length - 1)) * (W - 4) + 2;
  const y = (val) => H - 4 - (val / max) * (H - 10);
  const line = pts.map((val, k) => `${k ? 'L' : 'M'}${x(k).toFixed(1)},${y(val).toFixed(1)}`).join('');
  const area = `${line}L${x(pts.length - 1)},${H - 4}L${x(0)},${H - 4}Z`;
  const svg = s('svg', { class: 'spark', viewBox: `0 0 ${W} ${H}`, preserveAspectRatio: 'none', role: 'img', 'aria-label': label },
    s('path', { d: area, fill: C.accent, 'fill-opacity': '0.15' }),
    s('path', { d: line, fill: 'none', stroke: C.accent, 'stroke-width': '2', 'vector-effect': 'non-scaling-stroke' }),
    s('circle', { cx: x(pts.length - 1), cy: y(pts[pts.length - 1]), r: '3', fill: C.accent }));
  return h('div', null, svg, h('div', { class: 'note', text: `${label}; peak ${max} commits` }));
}

// ---------------------------------------------------------------- map
const mapApi = { pending: null, resize() {}, focus() {} };
renderers.map = (root) => {
  let zoom = 0, selected = 0, hover = -1;
  // A repository opens as a code map; anything else as a disk map.
  const repoAtRoot = repos.some((r) => r.node === 0) && T.code > 0;
  let metric = repoAtRoot ? 'code' : 'disk';
  let mode = repoAtRoot ? 'language' : 'zone';
  let query = '', matches = null;
  const sizeSel = h('select', { id: 'map-size' }, Object.entries(METRICS).map(([k, m]) => h('option', { value: k, text: m.label })));
  const colorSel = h('select', { id: 'map-color' }, Object.entries(COLOR_MODES).filter(([k]) => !((k === 'git' || k === 'hotspot' || k === 'owner') && !repos.length) && !(k === 'owner' && !topOwners.length)).map(([k, m]) => h('option', { value: k, text: m.label })));
  colorSel.value = mode;
  sizeSel.value = metric;
  const search = h('input', { id: 'map-search', type: 'search', placeholder: 'Find: name or *.ext', autocomplete: 'off' });
  const hits = h('span', { class: 'note' });
  const crumbs = h('div', { class: 'crumbs', 'aria-label': 'Current directory' });
  const canvas = h('canvas', { 'aria-label': 'Treemap of the scanned tree. Click a directory to zoom in.', role: 'img', tabindex: '0' });
  const tip = h('div', { class: 'tip', hidden: true });
  const box = h('div', { class: 'canvas-box' }, canvas, tip);
  const legend = h('div', { class: 'legend' });
  const scale = h('div', { class: 'scalebar' });
  const details = h('div', { class: 'panel details' });
  root.append(
    h('div', { class: 'map-controls' },
      h('label', { class: 'field', for: 'map-size' }, 'Size', sizeSel),
      h('label', { class: 'field', for: 'map-color' }, 'Colour', colorSel),
      h('label', { class: 'field', for: 'map-search' }, 'Find', search), hits),
    crumbs,
    h('div', { class: 'map-layout' }, h('div', { style: { display: 'grid', gap: '8px', minWidth: '0' } }, box, h('div', { style: { display: 'flex', flexWrap: 'wrap', gap: '8px 18px', justifyContent: 'space-between' } }, legend, scale)), details));

  let rects = [];
  let W = 0, H = 0, dpr = 1;
  const fontFamily = getComputedStyle(document.body).fontFamily;
  const ctx = canvas.getContext('2d');

  function squarify(items, vals, x, y, w, hgt, out) {
    let total = 0;
    for (const k of items) total += vals[k];
    if (total <= 0) return;
    const scaleF = (w * hgt) / total;
    let i = 0;
    while (i < items.length) {
      const short = Math.min(w, hgt);
      if (short <= 0) return;
      let sum = 0, mn = Infinity, mx = 0, worst = Infinity, j = i;
      while (j < items.length) {
        const a = vals[items[j]] * scaleF;
        const nsum = sum + a, nmn = Math.min(mn, a), nmx = Math.max(mx, a);
        const nworst = Math.max((short * short * nmx) / (nsum * nsum), (nsum * nsum) / (short * short * nmn));
        if (j > i && nworst > worst) break;
        sum = nsum; mn = nmn; mx = nmx; worst = nworst; j++;
      }
      if (w >= hgt) {
        const cw = sum / hgt;
        let cy = y;
        for (let k = i; k < j; k++) { const ah = (vals[items[k]] * scaleF) / cw; out.push([items[k], x, cy, cw, ah]); cy += ah; }
        x += cw; w -= cw;
      } else {
        const rh = sum / w;
        let cx = x;
        for (let k = i; k < j; k++) { const aw = (vals[items[k]] * scaleF) / rh; out.push([items[k], cx, y, aw, rh]); cx += aw; }
        y += rh; hgt -= rh;
      }
      i = j;
    }
  }

  function layout() {
    rects = [];
    const vals = {};
    const rec = (i, x, y, w, hgt, d, framed) => {
      const ch = kids[i].filter((c) => sizeOf(c, metric) > 0);
      const nest = ch.length > 0 && (d === 0 || (w > 26 && hgt > 26 && d < 7));
      if (d > 0) rects.push({ i, x, y, w, h: hgt, d, nest });
      if (!nest) return;
      const head = d === 0 ? 0 : (hgt > 40 && w > 60 ? 16 : 0);
      const pad = d === 0 ? 0 : 2;
      const ix = x + pad, iy = y + head + pad, iw = w - 2 * pad, ih = hgt - head - 2 * pad;
      if (iw < 3 || ih < 3) { if (d > 0) rects[rects.length - 1].nest = false; return; }
      ch.sort((a, b) => sizeOf(b, metric) - sizeOf(a, metric));
      for (const c of ch) vals[c] = sizeOf(c, metric);
      const out = [];
      squarify(ch, vals, ix, iy, iw, ih, out);
      for (const [c, cx, cy, cw, chh] of out) {
        if (cw * chh < 2) continue;
        rec(c, cx, cy, cw, chh, d + 1, framed);
      }
    };
    rec(zoom, 0, 0, W, H, 0, false);
  }

  function matchesNode(i) { return matches && matches.has(i); }
  function subtreeMatches(i) {
    if (!matches) return true;
    for (const m of matches) if (within(i, m)) return true;
    return false;
  }

  function draw() {
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.fillStyle = C.surface2;
    ctx.fillRect(0, 0, W, H);
    ctx.textBaseline = 'top';
    for (const r of rects) {
      const { i, x, y, w, h: hh, nest } = r;
      const dim = matches && !subtreeMatches(i);
      ctx.globalAlpha = dim ? 0.25 : 1;
      if (nest) {
        ctx.fillStyle = r.d % 2 ? C.surface3 : C.surface2;
        ctx.fillRect(x, y, w, hh);
        ctx.strokeStyle = C.line;
        ctx.lineWidth = 1;
        ctx.strokeRect(x + 0.5, y + 0.5, w - 1, hh - 1);
        if (hh > 40 && w > 60) {
          ctx.fillStyle = C.fg;
          ctx.font = `600 11px ${fontFamily}`;
          const name = N[i].name + '/';
          const size = METRICS[metric].fmt(sizeOf(i, metric));
          const full = `${name}  ${size}`;
          label(ctx.measureText(full).width <= w - 8 ? full : name, x + 4, y + 3, w - 8, null);
        }
      } else {
        ctx.fillStyle = colorOf(i, mode, metric);
        ctx.fillRect(x, y, w, hh);
        ctx.strokeStyle = C.surface;
        ctx.lineWidth = 1;
        ctx.strokeRect(x + 0.5, y + 0.5, Math.max(0, w - 1), Math.max(0, hh - 1));
        if (w > 46 && hh > 16) {
          ctx.fillStyle = textOn(ctx.fillStyle);
          ctx.font = `500 11px ${fontFamily}`;
          label(N[i].name + (isDir(i) ? '/' : ''), x + 4, y + 3, w - 8, hh > 30 ? METRICS[metric].fmt(sizeOf(i, metric)) : null);
        }
      }
      if (matchesNode(i) || i === selected || i === hover) {
        ctx.globalAlpha = 1;
        ctx.strokeStyle = i === hover ? C.fg : C.accent;
        ctx.lineWidth = 2;
        ctx.strokeRect(x + 1, y + 1, Math.max(0, w - 2), Math.max(0, hh - 2));
      }
    }
    ctx.globalAlpha = 1;
  }
  function textOn(color) {
    const m = /rgb\((\d+),(\d+),(\d+)\)/.exec(color);
    const [r, g, b] = m ? [+m[1], +m[2], +m[3]] : hexRgb(color);
    return (0.299 * r + 0.587 * g + 0.114 * b) > 150 ? '#0b0b0b' : '#ffffff';
  }
  function label(text, x, y, maxW, sub) {
    let t = text;
    if (ctx.measureText(t).width > maxW) {
      while (t.length > 1 && ctx.measureText(t + '…').width > maxW) t = t.slice(0, -1);
      t += '…';
    }
    if (t.length > 1) ctx.fillText(t, x, y);
    if (sub && ctx.measureText(sub).width < maxW) {
      const prev = ctx.font;
      ctx.font = prev.replace(/^\d+ /, '400 ');
      ctx.globalAlpha = ctx.globalAlpha * 0.85;
      ctx.fillText(sub, x, y + 13);
      ctx.font = prev;
    }
  }

  function resize() {
    const rect = canvas.getBoundingClientRect();
    if (!rect.width) return;
    dpr = window.devicePixelRatio || 1;
    W = Math.floor(rect.width); H = Math.floor(rect.height);
    canvas.width = W * dpr; canvas.height = H * dpr;
    layout(); draw(); updateScale();
  }
  mapApi.resize = () => requestAnimationFrame(() => {
    resize();
    if (mapApi.pending !== null) { const p = mapApi.pending; mapApi.pending = null; mapApi.focus(p); }
  });

  function hitTest(px, py) {
    for (let k = rects.length - 1; k >= 0; k--) {
      const r = rects[k];
      if (px >= r.x && px < r.x + r.w && py >= r.y && py < r.y + r.h) return r.i;
    }
    return -1;
  }

  function updateCrumbs() {
    crumbs.replaceChildren();
    const chain = [];
    for (let i = zoom; ; i = N[i].parent) { chain.unshift(i); if (i === 0) break; }
    chain.forEach((i, k) => {
      if (k) crumbs.append(h('span', { class: 'sep', text: '/' }));
      crumbs.append(h('button', { type: 'button', text: i === 0 ? rootName : N[i].name, onclick: () => setZoom(i) }));
    });
  }
  function updateLegend() {
    legend.replaceChildren();
    const m = COLOR_MODES[mode];
    if (m.seq) {
      const lo = mode === 'age' ? C.seqHi : C.heatLo, hi = mode === 'age' ? C.seqLo : C.heatHi;
      const grad = h('span', { style: { display: 'inline-block', width: '120px', height: '10px', borderRadius: '2px', background: `linear-gradient(90deg, ${lo}, ${hi})` } });
      legend.append(h('span', null, mode === 'age' ? 'recent' : 'low', grad, mode === 'age' ? '4+ years' : 'high'), h('span', null, h('i', { class: 'swatch', style: { background: C.none } }), mode === 'age' ? 'unknown' : 'no history'));
      return;
    }
    for (const [l, c] of m.legend()) legend.append(h('span', null, h('i', { class: 'swatch', style: { background: c } }), l));
  }
  function updateScale() {
    const total = sizeOf(zoom, metric);
    if (!W || !H || !total) { scale.textContent = ''; return; }
    const side = 24;
    const per = total / (W * H) * side * side;
    scale.replaceChildren(h('i', { style: { width: side + 'px' } }), `${side}×${side} px ≈ ${METRICS[metric].fmt(per)}`, h('span', { class: 'muted', text: ` · view total ${METRICS[metric].fmt(total)}` }));
  }

  function row(dt, dd) { return dd === null || dd === undefined || dd === '' ? null : [h('dt', { text: dt }), h('dd', { text: dd })]; }
  function updateDetails() {
    const i = selected;
    const n = N[i];
    const zr = (() => {
      for (let a = i; ; a = N[a].parent) {
        const z = (D.zoneRoots || []).find((x) => x.node === a);
        if (z) return z.reason;
        if (a === 0) return null;
      }
    })();
    const flags = D.flagNames.filter((f) => v(i, 'flags') & flagBit(f));
    const hs = hotspots.find((x) => x.node === i);
    const owner = n.owner !== undefined ? authorName(n.owner) : null;
    const ch = kids[i].slice().sort((a, b) => sizeOf(b, metric) - sizeOf(a, metric)).slice(0, 8);
    const total = sizeOf(i, metric) || 1;
    const fs = (findingsByNode.get(i) || []).map((fi) => findings[fi]);
    details.replaceChildren(
      h('div', { class: 'path', text: i === 0 ? rootName + '/' : path(i) + (isDir(i) ? '/' : '') }),
      h('div', { class: 'tags' }, h('span', { class: 'tag', text: n.kind }), h('span', { class: 'tag', text: (n.zone || 'source') + (zr ? ` · ${zr}` : '') }), n.git ? h('span', { class: 'tag', text: 'git: ' + n.git }) : null, flags.map((f) => h('span', { class: 'tag', text: f }))),
      h('dl', null,
        row('Disk', fmtBytes(v(i, 'disk'))),
        row('Apparent', fmtBytes(v(i, 'bytes'))),
        isDir(i) ? row('Files', fmtInt(filesOf(i))) : null,
        v(i, 'lines') ? row('Lines', `${fmtInt(v(i, 'code'))} code · ${fmtInt(v(i, 'comments'))} comment · ${fmtInt(v(i, 'lines'))} total`) : null,
        v(i, 'cx') ? row('Complexity', fmtInt(v(i, 'cx')) + ' (indentation)') : null,
        langName(i) ? row('Language', langName(i)) : null,
        n.cat ? row('Kind', n.cat) : null,
        v(i, 'commits') ? row('Commits', `${fmtInt(v(i, 'commits'))} in window · ${v(i, 'recent').toFixed(1)} recency-weighted`) : null,
        v(i, 'lastCommit') ? row('Last commit', `${fmtDate(v(i, 'lastCommit'))} (${fmtAgo(v(i, 'lastCommit'))})`) : row('Modified', v(i, 'mtime') ? fmtDate(v(i, 'mtime')) : null),
        owner ? row('Main author', owner + (v(i, 'authors') > 1 ? ` (+${v(i, 'authors') - 1} co-authors)` : '')) : null,
        hs ? row('Hotspot', `change top ${Math.max(1, Math.ceil((1 - hs.changePct) * 100))}% × complexity top ${Math.max(1, Math.ceil((1 - hs.complexityPct) * 100))}%`) : null,
        isFile(i) && (v(i, 'fanIn') || v(i, 'fanOut')) ? row('Imports', `imported by ${v(i, 'fanIn')} · imports ${v(i, 'fanOut')}`) : null,
        tokens[i] ? row('Agent cost', '≈' + fmtCount(tokens[i]) + ' tokens') : null),
      fs.length ? h('div', { style: { marginTop: '10px' } }, h('div', { class: 'note', text: 'Findings here' }), fs.map((f) => h('div', null, h('span', { class: 'sev', 'data-sev': f.severity, text: f.severity }), ' ', f.title))) : null,
      ch.length ? h('div', { style: { marginTop: '12px' } }, h('div', { class: 'note', text: `Largest inside by ${METRICS[metric].label.toLowerCase()}` }),
        h('div', { class: 'bars', style: { marginTop: '6px' } }, ch.map((c) => h('div', { class: 'bar' },
          h('button', { class: 'linkish lbl mono', type: 'button', title: path(c), text: N[c].name + (isDir(c) ? '/' : ''), onclick: () => (isDir(c) ? setZoom(c) : select(c)) }),
          h('span', { class: 'track' }, h('span', { class: 'fill', style: { width: (sizeOf(c, metric) / total * 100) + '%', background: colorOf(c, mode, metric) } })),
          h('span', { class: 'val', text: METRICS[metric].fmt(sizeOf(c, metric)) }))))) : null);
  }

  function setZoom(i) {
    zoom = isDir(i) ? i : N[i].parent;
    selected = i;
    updateCrumbs(); layout(); draw(); updateScale(); updateDetails();
  }
  function select(i) { selected = i; draw(); updateDetails(); }
  mapApi.focus = (i) => { setZoom(i === 0 ? 0 : N[i].parent); select(i); };

  canvas.addEventListener('mousemove', (e) => {
    const r = canvas.getBoundingClientRect();
    const px = e.clientX - r.left, py = e.clientY - r.top;
    const i = hitTest(px, py);
    if (i !== hover) { hover = i; draw(); }
    if (i < 0) { tip.hidden = true; return; }
    const total = sizeOf(zoom, metric) || 1;
    tip.replaceChildren(
      h('div', { class: 'p', text: path(i) + (isDir(i) ? '/' : '') }),
      h('div', { text: `${METRICS[metric].fmt(sizeOf(i, metric))} · ${pct(sizeOf(i, metric) / total)} of view` }),
      langName(i) ? h('div', { class: 'muted', text: langName(i) + (v(i, 'code') ? ` · ${fmtInt(v(i, 'code'))} code lines` : '') }) : null,
      v(i, 'lastCommit') ? h('div', { class: 'muted', text: `last commit ${fmtAgo(v(i, 'lastCommit'))}` }) : null,
      isDir(i) ? h('div', { class: 'muted', text: 'click to zoom in' }) : null);
    tip.hidden = false;
    const tw = tip.offsetWidth, th = tip.offsetHeight;
    tip.style.left = Math.min(px + 14, W - tw - 4) + 'px';
    tip.style.top = (py + 14 + th > H ? py - th - 10 : py + 14) + 'px';
  });
  canvas.addEventListener('mouseleave', () => { hover = -1; tip.hidden = true; draw(); });
  canvas.addEventListener('click', (e) => {
    const r = canvas.getBoundingClientRect();
    const i = hitTest(e.clientX - r.left, e.clientY - r.top);
    if (i < 0) return;
    if (isDir(i) && kids[i].length) setZoom(i); else select(i);
  });
  canvas.addEventListener('keydown', (e) => {
    if ((e.key === 'Backspace' || e.key === 'Escape') && zoom !== 0) { e.preventDefault(); setZoom(N[zoom].parent); }
  });
  sizeSel.addEventListener('change', () => { metric = sizeSel.value; layout(); draw(); updateScale(); updateDetails(); });
  colorSel.addEventListener('change', () => { mode = colorSel.value; updateLegend(); draw(); updateDetails(); });
  search.addEventListener('input', () => {
    query = search.value.trim();
    if (!query) { matches = null; hits.textContent = ''; draw(); return; }
    let re;
    try {
      re = query.includes('*') || query.includes('?')
        ? new RegExp('^' + query.replace(/[.+^${}()|[\]\\]/g, '\\$&').replace(/\*/g, '.*').replace(/\?/g, '.') + '$', 'i')
        : new RegExp(query.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'i');
    } catch (_) { return; }
    matches = new Set();
    for (let i = 1; i < COUNT; i++) if (re.test(N[i].name)) matches.add(i);
    hits.textContent = `${fmtInt(matches.size)} match${matches.size === 1 ? '' : 'es'}`;
    draw();
  });
  search.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && matches && matches.size) { const first = matches.values().next().value; mapApi.focus(first); }
  });
  window.addEventListener('resize', () => mapApi.resize());
  mapApi.redraw = () => { draw(); updateLegend(); updateDetails(); };

  updateCrumbs(); updateLegend(); updateDetails();
};

// ---------------------------------------------------------------- hotspots
renderers.hotspots = (root) => {
  const pts = hotspots.filter((x) => v(x.node, 'cx') > 0 && v(x.node, 'recent') > 0);
  const panel = h('div', { class: 'panel scatter' }, h('h2', null, 'Change × complexity', h('span', { class: 'sub', text: 'each dot is a source file; the shaded corner is the top 20% on both axes' })));
  root.append(panel);
  if (!pts.length) { panel.append(h('div', { class: 'empty', text: 'No files with both git history and measurable complexity.' })); return; }
  const W = 900, H = 380, L = 56, R = 16, Tp = 14, B = 40;
  const xs = pts.map((p) => v(p.node, 'cx')), ys = pts.map((p) => v(p.node, 'recent'));
  const lx = (x) => Math.log10(Math.max(1, x));
  const ly = (y) => Math.log10(Math.max(0.01, y));
  const [x0, x1] = [Math.floor(lx(Math.min(...xs))), Math.ceil(lx(Math.max(...xs)) + 0.001)];
  const [y0, y1] = [Math.floor(ly(Math.min(...ys))), Math.ceil(ly(Math.max(...ys)) + 0.001)];
  const X = (x) => L + ((lx(x) - x0) / Math.max(0.001, x1 - x0)) * (W - L - R);
  const Y = (y) => H - B - ((ly(y) - y0) / Math.max(0.001, y1 - y0)) * (H - B - Tp);
  const cxT = Math.min(...pts.filter((p) => p.complexityPct >= 0.8).map((p) => v(p.node, 'cx')));
  const chT = Math.min(...pts.filter((p) => p.changePct >= 0.8).map((p) => v(p.node, 'recent')));
  const svg = s('svg', { viewBox: `0 0 ${W} ${H}`, role: 'img', 'aria-label': 'Scatter of complexity against recent change rate' });
  if (isFinite(cxT) && isFinite(chT)) svg.append(s('rect', { class: 'quad', x: X(cxT), y: Tp, width: W - R - X(cxT), height: Y(chT) - Tp }));
  for (let e = x0; e <= x1; e++) {
    svg.append(s('line', { class: 'grid', x1: X(10 ** e), x2: X(10 ** e), y1: Tp, y2: H - B }));
    svg.append(s('text', { x: X(10 ** e), y: H - B + 16, 'text-anchor': 'middle' }, fmtCount(10 ** e)));
  }
  for (let e = y0; e <= y1; e++) {
    svg.append(s('line', { class: 'grid', x1: L, x2: W - R, y1: Y(10 ** e), y2: Y(10 ** e) }));
    svg.append(s('text', { x: L - 8, y: Y(10 ** e) + 4, 'text-anchor': 'end' }, String(+(10 ** e).toFixed(2))));
  }
  svg.append(s('text', { x: (L + W - R) / 2, y: H - 6, 'text-anchor': 'middle' }, 'complexity (indentation, log scale) →'));
  svg.append(s('text', { x: 14, y: (Tp + H - B) / 2, 'text-anchor': 'middle', transform: `rotate(-90 14 ${(Tp + H - B) / 2})` }, 'recency-weighted commits (log) →'));
  const maxCode = Math.max(...pts.map((p) => v(p.node, 'code')), 1);
  const tip = h('div', { class: 'tip', hidden: true });
  const wrapBox = h('div', { style: { position: 'relative' } }, svg, tip);
  const ordered = pts.slice().sort((a, b) => Number(flagged(a)) - Number(flagged(b)));
  for (const p of ordered) {
    const i = p.node;
    const f = !!flagged(p);
    const dot = s('circle', { class: 'dot', cx: X(v(i, 'cx')), cy: Y(v(i, 'recent')), r: (3 + 9 * Math.sqrt(v(i, 'code') / maxCode)).toFixed(1), fill: f ? C.series[1] : C.series[0], 'fill-opacity': f ? '0.95' : '0.55' });
    dot.addEventListener('mouseenter', (e) => {
      tip.replaceChildren(h('div', { class: 'p', text: path(i) }), h('div', { text: `${v(i, 'commits')} commits · ${v(i, 'recent').toFixed(1)} weighted · complexity ${fmtInt(v(i, 'cx'))} · ${fmtInt(v(i, 'code'))} code lines` }), h('div', { class: 'muted', text: 'click to show on the map' }));
      tip.hidden = false;
      const r = wrapBox.getBoundingClientRect();
      tip.style.left = Math.min(e.clientX - r.left + 12, r.width - 240) + 'px';
      tip.style.top = (e.clientY - r.top + 12) + 'px';
    });
    dot.addEventListener('mouseleave', () => { tip.hidden = true; });
    dot.addEventListener('click', () => goToNode(i));
    svg.append(dot);
  }
  const placed = [];
  pts.filter(flagged).slice(0, 8).forEach((p) => {
    const i = p.node;
    const text = shortLabel(i);
    const w = text.length * 6.6, hgt = 13;
    const cx = X(v(i, 'cx')), cy = Y(v(i, 'recent'));
    const spots = [[8, -8], [8, 14], [-8 - w, -8], [-8 - w, 14]];
    for (const [dx, dy] of spots) {
      const box = { x: cx + dx, y: cy + dy - hgt, w, h: hgt };
      if (box.x < L || box.x + w > W - R || box.y < 0) continue;
      if (placed.some((q) => box.x < q.x + q.w && box.x + box.w > q.x && box.y < q.y + q.h && box.y + box.h > q.y)) continue;
      placed.push(box);
      svg.append(s('text', { x: box.x, y: cy + dy, style: 'fill: var(--fg); font-weight: 600' }, text));
      break;
    }
  });
  panel.append(wrapBox, h('div', { class: 'legend', style: { marginTop: '6px' } },
    h('span', null, h('i', { class: 'swatch', style: { background: C.series[1] } }), 'Flagged hotspot (non-test, top 20% on both axes, active recently)'),
    h('span', null, h('i', { class: 'swatch', style: { background: C.series[0], opacity: 0.55 } }), 'Other files'),
    h('span', null, 'dot size = code lines')));

  const table = sortableTable([
    { k: 'path', label: 'File', p: true, get: (x) => path(x.node) },
    { k: 'commits', label: 'Commits', r: true, get: (x) => v(x.node, 'commits') },
    { k: 'recent', label: 'Weighted', r: true, get: (x) => v(x.node, 'recent'), fmt: (val) => val.toFixed(1) },
    { k: 'code', label: 'Code lines', r: true, get: (x) => v(x.node, 'code'), fmt: fmtInt },
    { k: 'cx', label: 'Complexity', r: true, get: (x) => v(x.node, 'cx'), fmt: fmtInt },
    { k: 'score', label: 'Score', r: true, get: (x) => x.score, cell: (x) => h('td', { class: 'r' }, h('span', { class: 'mini', style: { width: Math.round(x.score * 60) + 'px', background: flagged(x) ? C.series[1] : C.series[0] } }), ' ', x.score.toFixed(2)) },
    { k: 'owner', label: 'Main author', get: (x) => (N[x.node].owner !== undefined ? authorName(N[x.node].owner) || '' : '') },
    { k: 'last', label: 'Last commit', r: true, get: (x) => v(x.node, 'lastCommit'), fmt: fmtAgo },
  ], hotspots, 'score', (x) => goToNode(x.node));
  root.append(h('div', { class: 'panel' }, h('h2', null, 'Ranking', h('span', { class: 'sub', text: 'percentile(recent change) × percentile(complexity)' })), table));
};

/** `lib.rs` → `globset/…/lib.rs`: generic file names get their nearest meaningful directory. */
function shortLabel(i) {
  const name = N[i].name;
  if (!/^(lib|mod|main|index|__init__|util|utils|types|app|config)\./.test(name)) return name;
  let a = N[i].parent, hops = 0;
  while (a > 0 && /^(src|lib|source|app|pkg|internal)$/.test(N[a].name)) { a = N[a].parent; hops++; }
  return a > 0 ? `${N[a].name}/${hops ? '…/' : ''}${name}` : name;
}

function sortableTable(cols, rows, sortKey, onRow, limit = 300) {
  let key = sortKey, desc = true;
  const thead = h('thead');
  const tbody = h('tbody');
  const box = h('div', { class: 'table-box' }, h('table', null, thead, tbody));
  const render = () => {
    thead.replaceChildren(h('tr', null, cols.map((c) => {
      const th = h('th', { class: 'sortable' + (c.r ? ' r' : ''), scope: 'col', 'aria-sort': key === c.k ? (desc ? 'descending' : 'ascending') : null }, c.label + (key === c.k ? (desc ? ' ↓' : ' ↑') : ''));
      th.addEventListener('click', () => { if (key === c.k) desc = !desc; else { key = c.k; desc = !c.p; } render(); });
      return th;
    })));
    const col = cols.find((c) => c.k === key);
    const sorted = rows.slice().sort((a, b) => {
      const x = col.get(a), y = col.get(b);
      const r = typeof x === 'string' ? x.localeCompare(y) : x - y;
      return desc ? -r : r;
    });
    tbody.replaceChildren(...sorted.slice(0, limit).map((rw) => {
      const tr = h('tr', null, cols.map((c) => (c.cell ? c.cell(rw) : h('td', { class: (c.r ? 'r' : '') + (c.p ? ' p' : ''), text: c.fmt ? c.fmt(c.get(rw)) : String(c.get(rw)) }))));
      if (onRow) { tr.style.cursor = 'pointer'; tr.addEventListener('click', () => onRow(rw)); }
      return tr;
    }));
  };
  render();
  return box;
}

// ---------------------------------------------------------------- architecture
renderers.architecture = (root) => {
  // Same edges as cycle detection: no idiomatic parent-module references, no deferred imports.
  const edges = deps.edges.filter((e) => !e.up && !e.weak);
  // Start where the structure is: descend while one child directory holds ≥ 85% of edges.
  let dir = 0;
  for (;;) {
    const inside = edges.filter((e) => within(dir, e.from) && within(dir, e.to));
    if (!inside.length) break;
    const byChild = new Map();
    for (const e of inside) {
      const a = childToward(dir, e.from), b = childToward(dir, e.to);
      if (a === b) byChild.set(a, (byChild.get(a) || 0) + 1);
    }
    const best = [...byChild.entries()].sort((x, y) => y[1] - x[1])[0];
    if (best && isDir(best[0]) && best[1] >= inside.length * 0.85) dir = best[0]; else break;
  }
  const crumbs = h('div', { class: 'crumbs' });
  const matrixBox = h('div', { style: { overflowX: 'auto' } });
  const tip = h('div', { class: 'tip', hidden: true });
  const panel = h('div', { class: 'panel', style: { position: 'relative' } }, h('h2', null, 'Dependency matrix', h('span', { class: 'sub', text: 'row imports column · red outline = both directions (a cycle) · click a directory to open it' })), crumbs, h('div', { style: { height: '8px' } }), matrixBox, tip);
  root.append(panel);

  function childToward(anc, i) {
    let c = i;
    while (N[c].parent !== anc && c !== 0) c = N[c].parent;
    return c;
  }
  function drawMatrix() {
    crumbs.replaceChildren();
    const chain = [];
    for (let i = dir; ; i = N[i].parent) { chain.unshift(i); if (i === 0) break; }
    chain.forEach((i, k) => { if (k) crumbs.append(h('span', { class: 'sep', text: '/' })); crumbs.append(h('button', { type: 'button', text: i === 0 ? rootName : N[i].name, onclick: () => { dir = i; drawMatrix(); } })); });
    const cell = new Map();
    const files = new Map();
    for (const e of edges) {
      if (!within(dir, e.from) || !within(dir, e.to) || e.from === dir || e.to === dir) continue;
      const a = childToward(dir, e.from), b = childToward(dir, e.to);
      if (a === b) continue;
      const key = a + ':' + b;
      cell.set(key, (cell.get(key) || 0) + e.weight);
      if (!files.has(key)) files.set(key, []);
      if (files.get(key).length < 6) files.get(key).push([e.from, e.to, e.weight]);
    }
    let members = [...new Set([...cell.keys()].flatMap((k) => k.split(':').map(Number)))];
    if (!members.length) { matrixBox.replaceChildren(h('div', { class: 'empty', text: 'No imports between the entries of this directory.' })); return; }
    members.sort((a, b) => v(b, 'code') - v(a, 'code'));
    members = members.slice(0, 32);
    const out = (m) => members.reduce((acc, o) => acc + (cell.get(m + ':' + o) ? 1 : 0), 0);
    members.sort((a, b) => out(a) - out(b) || N[a].name.localeCompare(N[b].name));
    const maxW = Math.max(...cell.values());
    const table = h('table', { class: 'dsm' });
    table.append(h('tr', null, h('th'), members.map((m, k) => h('th', { class: 'colh', title: N[m].name, text: String(k + 1) }))));
    members.forEach((a, ra) => {
      const label = isDir(a)
        ? h('button', { class: 'drill', type: 'button', title: 'Open ' + path(a), onclick: () => { dir = a; drawMatrix(); } }, `${ra + 1}. ${N[a].name}/`)
        : `${ra + 1}. ${N[a].name}`;
      table.append(h('tr', null, h('th', { class: 'rowh', scope: 'row' }, label), members.map((b) => {
        if (a === b) return h('td', { class: 'diag' });
        const w = cell.get(a + ':' + b) || 0;
        const back = cell.get(b + ':' + a) || 0;
        const td = h('td', { class: (w ? 'has' : '') + (w && back ? ' cyc' : ''), text: w ? fmtCount(w) : '' });
        if (w) {
          td.style.background = mix(C.seqLo, C.seqHi, 0.15 + 0.85 * Math.log1p(w) / Math.log1p(maxW));
          td.style.color = Math.log1p(w) / Math.log1p(maxW) > 0.5 ? '#ffffff' : C.fg;
          td.addEventListener('mouseenter', () => {
            tip.replaceChildren(h('div', { text: `${N[a].name} → ${N[b].name}: ${w} import${w === 1 ? '' : 's'}${back ? ` (and ${back} back)` : ''}` }), (files.get(a + ':' + b) || []).map(([f, t, wt]) => h('div', { class: 'p muted', text: `${path(f)} → ${path(t)}${wt > 1 ? ` ×${wt}` : ''}` })));
            tip.hidden = false;
            const pr = panel.getBoundingClientRect(), r = td.getBoundingClientRect();
            tip.style.left = Math.min(r.left - pr.left + 30, pr.width - 330) + 'px';
            tip.style.top = (r.top - pr.top + 30) + 'px';
          });
          td.addEventListener('mouseleave', () => { tip.hidden = true; });
        }
        return td;
      })));
    });
    matrixBox.replaceChildren(table);
  }
  drawMatrix();

  // Cycles.
  const cyc = h('div', { class: 'panel' }, h('h2', null, 'Cycles', h('span', { class: 'sub', text: 'between siblings, at the level where they occur; deferred, type-only and test imports excluded' })));
  if (!deps.cycles.length) cyc.append(h('div', { class: 'empty', text: 'No import cycles. Every directory level is layered.' }));
  for (const c of deps.cycles) {
    cyc.append(h('div', { class: 'finding', 'data-sev': c.members.some(isDir) ? 'medium' : 'low', style: { marginTop: '8px' } },
      h('div', { class: 'title', text: `${path(c.parent)}: ${c.members.map((m) => N[m].name + (isDir(m) ? '/' : '')).join(' ↔ ')}` }),
      h('div', { class: 'detail', text: `${c.weight} imports in the cycle. Removing ${c.cuts.reduce((a, x) => a + x[2], 0)} breaks it: ${c.cuts.slice(0, 8).map((x) => `${N[x[0]].name} → ${N[x[1]].name} (${x[2]})`).join(', ')}${c.cuts.length > 8 ? ` and ${c.cuts.length - 8} more` : ''}.` }),
      h('div', { class: 'detail mono', style: { fontSize: '12px' } }, c.cutFiles.map(([a, b]) => h('div', { text: `${path(a)} → ${path(b)}` }))),
      h('div', { class: 'row' }, h('button', { class: 'linkish', type: 'button', text: 'Open in matrix', onclick: () => { dir = c.parent; drawMatrix(); panel.scrollIntoView({ behavior: 'smooth' }); } }))));
  }
  // Most imported + resolution stats.
  const fanIn = [];
  for (let i = 0; i < COUNT; i++) if (isFile(i) && v(i, 'fanIn') > 0) fanIn.push(i);
  fanIn.sort((a, b) => v(b, 'fanIn') - v(a, 'fanIn'));
  const right = h('div', { class: 'panel' }, h('h2', null, 'Most imported files', h('span', { class: 'sub', text: 'change these with care' })),
    sortableTable([
      { k: 'path', label: 'File', p: true, get: (i) => path(i) },
      { k: 'in', label: 'Imported by', r: true, get: (i) => v(i, 'fanIn') },
      { k: 'out', label: 'Imports', r: true, get: (i) => v(i, 'fanOut') },
      { k: 'code', label: 'Code lines', r: true, get: (i) => v(i, 'code'), fmt: fmtInt },
    ], fanIn.slice(0, 40), 'in', goToNode),
    h('div', { class: 'note', style: { marginTop: '10px' } }, 'Import resolution: ', deps.stats.map((st) => `${st.lang} ${fmtInt(st.internal)} internal / ${fmtInt(st.external)} external or unresolved`).join(' · ')));
  root.append(h('div', { class: 'cols' }, cyc, right));
};

// ---------------------------------------------------------------- coupling
renderers.coupling = (root) => {
  const isCodeFile = (i) => N[i].cat === 'code';
  let filter = coupling.some((p) => isCodeFile(p.a) && isCodeFile(p.b)) ? 'code' : 'all';
  const holder = h('div');
  const chips = h('div', { class: 'filters' });
  const opts = [['code', 'Code ⇄ code'], ['hidden', 'No import link'], ['all', 'All pairs']];
  const isCode = isCodeFile;
  const draw = () => {
    const rows = coupling.filter((p) => filter === 'all' || (filter === 'hidden' && p.linked === false) || (filter === 'code' && isCode(p.a) && isCode(p.b)));
    holder.replaceChildren(rows.length ? sortableTable([
      { k: 'a', label: 'File A', p: true, get: (p) => path(p.a) },
      { k: 'b', label: 'File B', p: true, get: (p) => path(p.b) },
      { k: 'shared', label: 'Shared commits', r: true, get: (p) => p.shared },
      { k: 'ab', label: 'P(B | A)', r: true, get: (p) => p.confAb, fmt: pct },
      { k: 'ba', label: 'P(A | B)', r: true, get: (p) => p.confBa, fmt: pct },
      { k: 'lift', label: 'Lift', r: true, get: (p) => p.lift, fmt: (x) => x.toFixed(1) },
      { k: 'link', label: 'Import link', get: (p) => (p.linked === true ? 'yes' : p.linked === false ? 'none' : 'n/a') + (p.testPair ? ' · test pair' : '') },
    ], rows, 'shared', (p) => goToNode(p.a)) : h('div', { class: 'empty', text: 'No pairs match this filter.' }));
  };
  for (const [k, l] of opts) {
    const b = h('button', { class: 'chip', type: 'button', 'aria-pressed': String(k === filter), text: l });
    b.addEventListener('click', () => { filter = k; chips.querySelectorAll('button').forEach((x) => x.setAttribute('aria-pressed', String(x === b))); draw(); });
    chips.append(b);
  }
  root.append(h('div', { class: 'panel' }, h('h2', null, 'Files that change together', h('span', { class: 'sub', text: 'commits touching ≤ 30 files; shown when lift ≥ 2 and confidence ≥ 30%' })), chips, h('div', { style: { height: '10px' } }), holder));
  draw();
};

// ---------------------------------------------------------------- repos
renderers.repos = (root) => {
  const act = new Map((D.activity || []).map((a) => [a[0], a[1]]));
  const rows = repos.map((r) => {
    const hy = r.hygiene || {};
    const hi = r.history || {};
    const local = [];
    if (hy.unpushedBranches && hy.unpushedBranches.length) local.push(`${hy.unpushedBranches.length} unpushed branch${hy.unpushedBranches.length === 1 ? '' : 'es'}`);
    if (hy.stashes) local.push(`${hy.stashes} stash${hy.stashes === 1 ? '' : 'es'}`);
    if (hy.dirty) local.push(`${hy.dirty} modified`);
    if (hy.ahead) local.push(`${hy.ahead} ahead`);
    if (hy.behind) local.push(`${hy.behind} behind`);
    return { r, hy, hi, local: local.join(', ') || 'clean', series: act.get(r.node) };
  });
  root.append(h('div', { class: 'panel' }, h('h2', null, 'Repositories', h('span', { class: 'sub', text: D.reposNotAnalysed ? `${D.reposNotAnalysed} more not analysed (--max-repos)` : 'every git work tree found in the scan' })),
    sortableTable([
      { k: 'path', label: 'Work tree', p: true, get: (x) => (x.r.path === '.' ? rootName : x.r.path) },
      { k: 'branch', label: 'Branch', get: (x) => (x.hy.branch || '(detached)') + (x.r.head ? ` @ ${x.r.head}` : '') },
      { k: 'local', label: 'Local-only work', get: (x) => x.local },
      { k: 'tracked', label: 'Tracked files', r: true, get: (x) => x.r.tracked, fmt: fmtInt },
      { k: 'ignored', label: 'Ignored on disk', r: true, get: (x) => x.r.ignoredBytes, fmt: fmtBytes },
      { k: 'last', label: 'Last commit', r: true, get: (x) => x.hi.last || 0, fmt: fmtAgo },
      { k: 'commits', label: 'Commits read', r: true, get: (x) => x.hi.commits || 0, fmt: fmtInt },
      { k: 'authors', label: 'Authors (active)', r: true, get: (x) => x.hi.authors || 0, fmt: (val) => String(val) },
      { k: 'tf', label: 'Truck factor', r: true, get: (x) => (x.hi.truckFactor === undefined || x.hi.truckFactor === null ? -1 : x.hi.truckFactor), fmt: (val) => (val < 0 ? '—' : String(val)) },
      { k: 'err', label: 'Notes', get: (x) => [x.r.shallow ? 'shallow' : '', x.r.partial ? 'partial clone' : '', x.r.error || ''].filter(Boolean).join(', ') },
    ], rows, 'last', (x) => goToNode(x.r.node))));
};

// ---------------------------------------------------------------- files
renderers.files = (root) => {
  const all = [];
  for (let i = 0; i < COUNT; i++) if (isFile(i)) all.push(i);
  const input = h('input', { id: 'files-search', type: 'search', placeholder: 'Filter by path', autocomplete: 'off' });
  const count = h('span', { class: 'note' });
  const holder = h('div');
  const draw = () => {
    const q = input.value.trim().toLowerCase();
    const rows = q ? all.filter((i) => path(i).toLowerCase().includes(q)) : all;
    count.textContent = `${fmtInt(rows.length)} files${rows.length > 300 ? ' · showing the top 300 by the sorted column' : ''}`;
    holder.replaceChildren(sortableTable([
      { k: 'path', label: 'Path', p: true, get: (i) => path(i) },
      { k: 'disk', label: 'Disk', r: true, get: (i) => v(i, 'disk'), fmt: fmtBytes },
      { k: 'code', label: 'Code lines', r: true, get: (i) => v(i, 'code'), fmt: fmtInt },
      { k: 'lang', label: 'Language', get: (i) => langName(i) || N[i].cat || '' },
      { k: 'git', label: 'Git', get: (i) => N[i].git || '' },
      { k: 'commits', label: 'Commits', r: true, get: (i) => v(i, 'commits') },
      { k: 'last', label: 'Changed', r: true, get: (i) => ageOf(i), fmt: fmtAgo },
      { k: 'tokens', label: 'Tokens', r: true, get: (i) => tokens[i], fmt: (x) => (x ? fmtCount(x) : '') },
    ], rows, 'disk', goToNode));
  };
  input.addEventListener('input', draw);
  root.append(h('div', { class: 'panel' }, h('h2', null, 'Files', h('span', { class: 'sub', text: 'the map as a table' })), h('div', { class: 'map-controls' }, h('label', { class: 'field', for: 'files-search' }, 'Filter', input), count), h('div', { style: { height: '10px' } }), holder));
  draw();
};

// ---------------------------------------------------------------- footer, theme, boot
(function footer() {
  const f = document.getElementById('footer');
  const t = D.timingsMs || {};
  const total = Object.values(t).reduce((a, x) => a + x, 0);
  f.append(h('span', { text: `${D.tool} · schema ${D.schema}` }), h('span', { text: `analysis ${fmtInt(total)} ms` }));
  if (D.pruned) f.append(h('span', { text: `map shows ${fmtInt(D.pruned.keptNodes)} of ${fmtInt(D.pruned.originalNodes)} entries; smaller items are folded, totals are exact` }));
  if (D.options && D.options.authors !== 'names') f.append(h('span', { text: `authors: ${D.options.authors}` }));
  if (D.scanIssues && D.scanIssues.count) f.append(h('span', { text: `${fmtInt(D.scanIssues.count)} entries unreadable` }));
})();

function applyTheme(mode) {
  if (mode === 'auto') document.documentElement.removeAttribute('data-theme');
  else document.documentElement.setAttribute('data-theme', mode);
  document.querySelectorAll('[data-theme-set]').forEach((b) => b.setAttribute('aria-pressed', String(b.dataset.themeSet === mode)));
  readColors();
  for (const k of Object.keys(dominantCache)) delete dominantCache[k];
  if (mapApi.redraw) mapApi.redraw();
  for (const view of ['overview', 'hotspots', 'architecture']) {
    if (rendered[view]) {
      const sec = document.getElementById('view-' + view);
      sec.replaceChildren();
      renderers[view](sec);
    }
  }
}
let savedTheme = 'auto';
try { savedTheme = localStorage.getItem('fsmap-theme') || 'auto'; } catch (_) { /* storage blocked */ }
document.querySelectorAll('[data-theme-set]').forEach((b) => b.addEventListener('click', () => {
  try { localStorage.setItem('fsmap-theme', b.dataset.themeSet); } catch (_) { /* storage blocked */ }
  applyTheme(b.dataset.themeSet);
}));
if (window.matchMedia) window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => applyTheme(document.documentElement.getAttribute('data-theme') || 'auto'));
if (savedTheme !== 'auto') document.documentElement.setAttribute('data-theme', savedTheme);
document.querySelectorAll('[data-theme-set]').forEach((b) => b.setAttribute('aria-pressed', String(b.dataset.themeSet === savedTheme)));
readColors();
show(location.hash.slice(1) || 'overview');
})();
