'use strict';

const $ = id => document.getElementById(id);
const labels = {containers: 'Containers', images: 'Images', volumes: 'Volumes', networks: 'Networks'};
const headings = {
  containers: ['Name', 'Image', 'State', 'Actions'],
  images: ['Image', 'Size', 'ID'],
  volumes: ['Name', 'Driver', 'Scope'],
  networks: ['Name', 'Driver', 'Subnets', 'IPv6'],
};
let view = 'containers';
let resources = [];
let total = 0;
let truncated = false;
let allowChanges = false;
let busy = false;

function node(tag, text, className) {
  const element = document.createElement(tag);
  if (text !== undefined) element.textContent = String(text);
  if (className) element.className = className;
  return element;
}

async function request(path, value) {
  const options = {credentials: 'same-origin', cache: 'no-store', signal: AbortSignal.timeout(50000)};
  if (value !== undefined) Object.assign(options, {
    method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify(value),
  });
  const response = await fetch(path, options);
  if (response.status === 401) throw new Error('Browser access was disabled or the password changed. Reload this page to sign in.');
  const result = await response.json();
  if (!response.ok || result.error) throw new Error(result.error || 'The engine request failed.');
  return result;
}

async function tool(name, args = {}) {
  return (await request('/api/call', {name, arguments: args})).result;
}

function controls() {
  for (const button of document.querySelectorAll('[data-request]')) button.disabled = busy;
  $('refresh').textContent = busy ? 'Working…' : 'Refresh';
}

async function work(operation) {
  if (busy) return;
  busy = true;
  $('error').hidden = true;
  controls();
  try {
    await operation();
    $('connection').textContent = 'Connected';
    $('connection').className = 'ready';
  } catch (error) {
    $('error').textContent = error.message || 'Could not reach the local engine.';
    $('error').hidden = false;
    $('connection').textContent = 'Check connection';
    $('connection').className = '';
  } finally {
    busy = false;
    controls();
  }
}

function size(bytes) {
  if (!Number.isFinite(bytes)) return '—';
  if (bytes < 1024) return bytes + ' B';
  const units = ['KiB', 'MiB', 'GiB', 'TiB'];
  let value = bytes / 1024, index = 0;
  while (value >= 1024 && index < units.length - 1) { value /= 1024; index++; }
  return value.toFixed(value < 10 ? 1 : 0) + ' ' + units[index];
}

async function permissions() {
  const result = await request('/api/tools');
  allowChanges = result.permissions.allowContainerChanges === true;
  $('access').textContent = allowChanges ? 'Container controls enabled · Start, stop, and restart' : 'Read access · Container controls are off';
}

async function overview() {
  const result = await tool('asagiri_status');
  const engine = result.engine;
  $('running').textContent = engine.ContainersRunning ?? '—';
  $('container-count').textContent = (engine.Containers ?? 0) + ' containers in total';
  $('image-count').textContent = engine.Images ?? '—';
  $('memory').textContent = size(engine.MemTotal);
  $('processors').textContent = engine.NCPU ? engine.NCPU + ' processors available' : 'Available to containers';
  const version = result.version.AsagiriVersion;
  $('version').textContent = version === '0.0.0' ? 'Development' : version || '—';
  $('platform').textContent = [result.version.Os, result.version.Arch].filter(Boolean).join(' · ');
}

async function loadResources() {
  const result = await tool('asagiri_list_' + view, {limit: 100});
  resources = result[view];
  total = result.total;
  truncated = result.truncated;
  render();
}

function button(label, click, changes = false) {
  const element = node('button', label, changes ? 'change' : '');
  element.dataset.request = '';
  element.disabled = busy;
  element.addEventListener('click', click);
  return element;
}

function resourceName(item) {
  if (view === 'containers') return (item.Names?.[0] || item.Id || '').replace(/^\//, '');
  if (view === 'images') return item.RepoTags?.join(', ') || '<untagged>';
  return item.Name || '';
}

function showContainer(item, logs) {
  work(async () => {
    const result = await tool(logs ? 'asagiri_container_logs' : 'asagiri_inspect_container', {container: item.Id});
    $('detail-title').textContent = (logs ? 'Logs · ' : 'Details · ') + resourceName(item);
    $('detail-note').textContent = logs
      ? 'Recent stdout and stderr. Application logs may contain secrets. ' + (result.truncated ? 'Output was truncated at 32 KiB.' : '')
      : 'Container state and resource settings. Environment values, commands, labels, and host mount paths are excluded.';
    $('detail-content').textContent = logs ? result.logs || 'No logs yet.' : JSON.stringify(result, null, 2);
    if (!$('detail').open) $('detail').showModal();
  });
}

function changeContainer(item, action) {
  work(async () => {
    await permissions();
    if (!allowChanges) throw new Error('Container changes are now disabled.');
    await tool('asagiri_' + action + '_container', {container: item.Id});
    await loadResources();
    await overview();
  });
}

function render() {
  const header = node('tr');
  for (const label of headings[view]) {
    const cell = node('th', label);
    cell.scope = 'col';
    header.append(cell);
  }
  $('columns').replaceChildren(header);
  $('rows').replaceChildren();
  const filter = $('filter').value.toLocaleLowerCase();
  const visible = resources.filter(item => JSON.stringify(item).toLocaleLowerCase().includes(filter));
  for (const item of visible) {
    const row = node('tr');
    const first = node('td');
    const name = node('span', resourceName(item), 'name');
    name.title = resourceName(item);
    first.append(name);
    row.append(first);
    if (view === 'containers') {
      row.append(node('td', item.Image || '—'));
      const state = node('td');
      state.append(node('span', item.State || 'unknown', 'badge' + (item.State === 'running' ? ' running' : '')));
      row.append(state);
      const actions = node('td');
      const group = node('div', undefined, 'actions');
      group.append(button('Details', () => showContainer(item, false)), button('Logs', () => showContainer(item, true)));
      if (allowChanges) {
        const running = item.State === 'running' || item.State === 'paused';
        group.append(button(running ? 'Stop' : 'Start', () => changeContainer(item, running ? 'stop' : 'start'), true));
        if (running) group.append(button('Restart', () => changeContainer(item, 'restart'), true));
      }
      actions.append(group);
      row.append(actions);
    } else if (view === 'images') {
      row.append(node('td', size(item.Size)));
      const id = node('td');
      id.append(node('code', (item.Id || '').replace(/^sha256:/, '').slice(0, 12)));
      row.append(id);
    } else if (view === 'volumes') {
      row.append(node('td', item.Driver || '—'), node('td', item.Scope || '—'));
    } else {
      row.append(node('td', item.Driver || '—'), node('td', item.IPAM?.Config?.map(entry => entry.Subnet).filter(Boolean).join(', ') || '—'),
        node('td', item.EnableIPv6 ? 'Enabled' : 'Off'));
    }
    $('rows').append(row);
  }
  $('empty').hidden = visible.length !== 0;
  $('empty').textContent = filter ? 'No displayed resources match this filter.' : 'No ' + view + ' yet.';
  $('count').textContent = visible.length + ' shown · ' + total + ' ' + view + (truncated ? ' · Only the first 100 were loaded.' : '');
}

async function refresh() {
  await permissions();
  await overview();
  await loadResources();
}

for (const tab of document.querySelectorAll('[data-view]')) {
  tab.addEventListener('click', () => work(async () => {
    view = tab.dataset.view;
    $('filter').value = '';
    for (const sibling of document.querySelectorAll('[data-view]')) sibling.removeAttribute('aria-current');
    tab.setAttribute('aria-current', 'page');
    $('resources-title').textContent = labels[view];
    await permissions();
    await loadResources();
  }));
}
$('refresh').addEventListener('click', () => work(refresh));
$('filter').addEventListener('input', render);
$('close-detail').addEventListener('click', () => $('detail').close());
work(refresh);
