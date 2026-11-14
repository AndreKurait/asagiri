const repository = 'https://github.com/AndreKurait/asagiri';
const dataUrl = 'https://raw.githubusercontent.com/AndreKurait/asagiri/performance-history/benchmarks/v1/index.json';
const fields = ['run_ms', 'run_p95_ms', 'ping_ms', 'ping_p95_ms', 'list_ms', 'list_p95_ms',
  'rss_idle_kib', 'rss_after_kib', 'syscalls', 'engine_bytes', 'init_bytes'];
const byId = id => document.getElementById(id);
let entries = [];

export function validateHistory(data) {
  if (![1, 2].includes(data?.schema) || data.repository !== 'AndreKurait/asagiri' ||
      !Array.isArray(data.entries) || data.entries.length > 500) throw new Error('Invalid history index');
  const ids = new Set();
  for (const row of data.entries) {
    if (!/^[a-f0-9]{40}$/.test(row.commit) || !/^[a-f0-9]{40}$/.test(row.baseline) ||
        !Number.isSafeInteger(row.run_id) || row.run_id <= 0 ||
        !Number.isSafeInteger(row.run_attempt) || row.run_attempt <= 0 ||
        !/^[a-f0-9]{16}$/.test(row.series) || !Number.isFinite(Date.parse(row.measured_at)) ||
        !['within-limits', 'regression'].includes(row.status) ||
        !['linux-engine-v1', 'linux-engine-v2'].includes(row.workload) ||
        row.path !== `benchmarks/v1/runs/${row.run_id}-${row.run_attempt}.json` ||
        ids.has(row.path)) throw new Error('Invalid measurement identity');
    ids.add(row.path);
    for (const field of ['architecture', 'cpu_model', 'runner_image']) {
      if (typeof row[field] !== 'string' || row[field].length > 256) throw new Error('Invalid runner metadata');
    }
    if (data.schema === 2 && (!Number.isSafeInteger(row.cpus) || row.cpus <= 0 || row.cpus >= 1e12 ||
        !Number.isSafeInteger(row.memory_mib) || row.memory_mib <= 0 || row.memory_mib >= 1e12 ||
        row.memory_mib % 64 !== 0)) throw new Error('Invalid runner capacity');
    for (const field of fields) {
      const pair = row.metrics?.[field];
      if (!Array.isArray(pair) || pair.length !== 2 ||
          pair.some(value => !Number.isFinite(value) || value < 0 || value > 1e15)) {
        throw new Error('Invalid measurement value');
      }
    }
  }
  return [...data.entries].sort((a, b) => Date.parse(a.measured_at) - Date.parse(b.measured_at) ||
    a.run_id - b.run_id || a.run_attempt - b.run_attempt);
}

function unit(metric) {
  return metric.endsWith('_ms') ? 'ms' : metric.endsWith('_kib') ? 'MiB' : metric.endsWith('_bytes') ? 'MiB' : 'calls';
}

function converted(value, metric) {
  return value / (metric.endsWith('_kib') ? 1024 : metric.endsWith('_bytes') ? 1048576 : 1);
}

function formatted(value, metric) {
  return `${converted(value, metric).toLocaleString(undefined, {maximumFractionDigits: metric.endsWith('_ms') ? 3 : 2})} ${unit(metric)}`;
}

function change(pair) {
  return pair[0] === 0 ? null : (pair[1] / pair[0] - 1) * 100;
}

function percent(value) {
  return value === null ? 'No nonzero baseline' : `${value >= 0 ? '+' : ''}${value.toFixed(1)}%`;
}

function link(label, url) {
  const element = document.createElement('a');
  element.textContent = label;
  element.href = url;
  return element;
}

function svg(tag, attributes = {}, label) {
  const element = document.createElementNS('http://www.w3.org/2000/svg', tag);
  for (const [name, value] of Object.entries(attributes)) element.setAttribute(name, String(value));
  if (label !== undefined) element.textContent = label;
  return element;
}

function draw() {
  const metric = byId('metric').value;
  const relative = byId('measurement').value === 'change';
  const rows = entries.filter(row => row.series === byId('series').value).slice(-Number(byId('range').value));
  if (!rows.length) return;
  const latest = rows.at(-1);
  byId('latest-commit').textContent = latest.commit.slice(0, 10);
  byId('latest-commit').href = `${repository}/commit/${latest.commit}`;
  byId('latest-time').textContent = new Date(latest.measured_at).toLocaleString();
  byId('latest-result').textContent = latest.status === 'regression' ? 'Investigate regression' : 'Within limits';
  byId('latest-result').className = latest.status === 'regression' ? 'regression' : 'pass';
  byId('sample-count').textContent = rows.length.toLocaleString();
  const capacity = latest.cpus ? ` · ${latest.cpus} CPUs · ~${(latest.memory_mib / 1024).toFixed(1)} GiB` : '';
  byId('sample-host').textContent = `${latest.architecture}${capacity} · ${latest.runner_image} · ${latest.workload}`;
  byId('plot-title').textContent = byId('metric').selectedOptions[0].textContent;
  byId('plot-unit').textContent = relative ? '% against paired baseline' : unit(metric);
  const tbody = byId('measurements');
  tbody.replaceChildren();
  const tableRows = new Map();
  for (const row of [...rows].reverse()) {
    const tr = document.createElement('tr');
    const pair = row.metrics[metric];
    const badge = document.createElement('span');
    badge.className = 'result-badge' + (row.status === 'regression' ? ' regression' : '');
    badge.textContent = row.status === 'regression' ? 'Regression' : 'Within limits';
    const contents = [link(row.commit.slice(0, 10), `${repository}/commit/${row.commit}`),
      new Date(row.measured_at).toISOString().slice(0, 16).replace('T', ' '),
      link(formatted(pair[0], metric), `${repository}/commit/${row.baseline}`),
      formatted(pair[1], metric), percent(change(pair)), badge,
      link('Run ↗', `${repository}/actions/runs/${row.run_id}/attempts/${row.run_attempt}`)];
    for (const content of contents) {
      const td = document.createElement('td');
      if (typeof content === 'string') td.textContent = content;
      else td.append(content);
      tr.append(td);
    }
    tbody.append(tr);
    tableRows.set(row.path, tr);
  }
  function select(row) {
    for (const tr of tableRows.values()) tr.classList.remove('selected');
    tableRows.get(row.path).classList.add('selected');
    const pair = row.metrics[metric];
    byId('selected-point').textContent = `${row.commit.slice(0, 10)} · ${formatted(pair[1], metric)} · ${percent(change(pair))} against ${row.baseline.slice(0, 10)}`;
  }
  const plot = byId('plot');
  const width = Math.max(320, plot.getBoundingClientRect().width);
  const left = 58, right = width - 14;
  plot.setAttribute('viewBox', `0 0 ${width} 310`);
  plot.replaceChildren(svg('desc', {id: 'plot-description'},
    'Runs from oldest to newest. Select a point for its commit and comparison. Values also appear in the table.'));
  const points = rows.map(row => ({row, value: relative ? change(row.metrics[metric]) : converted(row.metrics[metric][1], metric)}))
    .filter(point => point.value !== null);
  const values = points.map(point => point.value);
  let low = Math.min(0, ...values);
  let high = Math.max(0, ...values);
  const padding = (high - low || 1) * .15;
  low -= relative ? padding : 0;
  high += padding;
  const x = index => points.length === 1 ? (left + right) / 2 : left + index / (points.length - 1) * (right - left);
  const y = value => 260 - (value - low) / (high - low) * 225;
  for (let i = 0; i <= 4; i++) {
    const value = low + (high - low) * i / 4;
    plot.append(svg('line', {x1: left, x2: right, y1: y(value), y2: y(value), class: 'grid'}));
    plot.append(svg('text', {x: left - 12, y: y(value) + 4, 'text-anchor': 'end', class: 'axis'},
      value.toLocaleString(undefined, {maximumFractionDigits: 2})));
  }
  if (points.length) plot.append(svg('polyline', {class: 'trend',
    points: points.map((point, index) => `${x(index)},${y(point.value)}`).join(' ')}));
  points.forEach((point, index) => {
    const circle = svg('circle', {cx: x(index), cy: y(point.value), r: points.length > 100 ? 3 : 5,
      class: 'point' + (point.row.status === 'regression' ? ' regression' : ''), tabindex: 0, role: 'button',
      'aria-label': `${point.row.commit.slice(0, 10)}: ${formatted(point.row.metrics[metric][1], metric)}`});
    circle.append(svg('title', {}, `${point.row.commit.slice(0, 10)} · ${percent(change(point.row.metrics[metric]))}`));
    circle.addEventListener('focus', () => select(point.row));
    circle.addEventListener('click', () => select(point.row));
    circle.addEventListener('keydown', event => {
      if (event.key === 'Enter' || event.key === ' ') { event.preventDefault(); select(point.row); }
    });
    plot.append(circle);
  });
  if (rows.length) {
    plot.append(svg('text', {x: left, y: 295, class: 'axis'}, rows[0].commit.slice(0, 8)));
    plot.append(svg('text', {x: right, y: 295, class: 'axis', 'text-anchor': 'end'}, latest.commit.slice(0, 8)));
  }
  select(latest);
}

export function renderHistory(data) {
  entries = validateHistory(data);
  byId('performance').hidden = !entries.length;
  byId('load-state').hidden = Boolean(entries.length);
  if (!entries.length) {
    byId('load-state').textContent = 'No measurements archived yet. Completed main Speed runs will appear here.';
    return;
  }
  const choices = new Map();
  for (const row of [...entries].reverse()) if (!choices.has(row.series)) choices.set(row.series, row);
  byId('series').replaceChildren();
  for (const [id, row] of choices) {
    const option = document.createElement('option');
    option.value = id;
    const capacity = row.cpus ? ` · ${row.cpus} CPUs · ~${(row.memory_mib / 1024).toFixed(1)} GiB` : '';
    option.textContent = `${row.cpu_model} · ${row.architecture}${capacity} · ${row.workload} · ${id.slice(0, 6)}`;
    byId('series').append(option);
  }
  draw();
}

for (const id of ['metric', 'measurement', 'series', 'range']) byId(id).addEventListener('change', draw);
let resizeQueued = false;
window.addEventListener('resize', () => {
  if (resizeQueued) return;
  resizeQueued = true;
  requestAnimationFrame(() => { resizeQueued = false; if (entries.length) draw(); });
});

export const loaded = (async () => {
  try {
    const response = await fetch(dataUrl, {credentials: 'omit', signal: AbortSignal.timeout(20000)});
    if (response.status === 404) { renderHistory({schema: 1, repository: 'AndreKurait/asagiri', entries: []}); return; }
    if (!response.ok) throw new Error('History is temporarily unavailable');
    const reader = response.body.getReader();
    const chunks = [];
    let size = 0;
    while (true) {
      const {done, value} = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > 4 * 1024 * 1024) { await reader.cancel(); throw new Error('History exceeds the supported size'); }
      chunks.push(value);
    }
    renderHistory(JSON.parse(await new Blob(chunks).text()));
  } catch {
    byId('load-state').textContent = 'Performance history could not be loaded. The Speed runs and raw archive remain available above.';
  }
})();
