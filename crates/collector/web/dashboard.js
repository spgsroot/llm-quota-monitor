'use strict';
const byId = id => document.getElementById(id);
const statusNames = {ok: 'OK', stale: 'STALE', unavailable: 'UNAVAILABLE', auth_required: 'AUTH REQUIRED', error: 'ERROR'};
let cached = null;
let receivedAt = 0;
let online = false;
let inFlight = false;

function validate(data) {
  if (!data || typeof data.updated_at !== 'string' || !Number.isFinite(Date.parse(data.updated_at)) || !Number.isSafeInteger(data.age_seconds) || data.age_seconds < 0 || !Number.isSafeInteger(data.stale_after_seconds) || data.stale_after_seconds <= 0 || !data.providers || Array.isArray(data.providers) || typeof data.providers !== 'object' || Object.keys(data.providers).length > 8) throw new Error('Некорректный ответ коллектора');
  for (const [name, provider] of Object.entries(data.providers)) {
    if (!name || name.length > 32 || !provider || !Object.hasOwn(statusNames, provider.status) || typeof provider.source !== 'string' || !provider.source || provider.source.length > 64 || (provider.message != null && (typeof provider.message !== 'string' || provider.message.length > 256)) || (provider.source_age_seconds != null && (!Number.isSafeInteger(provider.source_age_seconds) || provider.source_age_seconds < 0)) || !Array.isArray(provider.windows) || provider.windows.length > 8) throw new Error('Некорректный провайдер');
    for (const window of provider.windows) {
      if (!window || typeof window.name !== 'string' || !window.name || window.name.length > 32 || (window.remaining_percent != null && (!Number.isInteger(window.remaining_percent) || window.remaining_percent < 0 || window.remaining_percent > 100)) || (window.resets_at != null && (typeof window.resets_at !== 'string' || !Number.isFinite(Date.parse(window.resets_at))))) throw new Error('Некорректное окно квоты');
    }
  }
  return data;
}
function element(tag, text, className) {
  const node = document.createElement(tag);
  if (text != null) node.textContent = text;
  if (className) node.className = className;
  return node;
}
function renderData() {
  byId('updated').textContent = 'Обновление коллектора: ' + new Date(cached.updated_at).toLocaleString();
  const cards = [];
  for (const [name, provider] of Object.entries(cached.providers)) {
    const card = element('article', null, 'card');
    card.dataset.provider = name;
    const head = element('div', null, 'cardhead');
    head.append(element('h2', name === 'codex' ? 'GPT / Codex' : name), element('span', statusNames[provider.status], 'badge provider-status'));
    card.append(head);
    card.append(element('p', provider.source, 'source reset'));
    if (provider.message) card.append(element('p', provider.message, 'reset'));
    if (provider.windows.length === 0) card.append(element('p', 'Данные о лимитах недоступны.'));
    for (const window of provider.windows) {
      const row = element('div', null, 'window');
      const numbers = element('div', null, 'numbers');
      // Do not present cached windows from a failing provider as current quota.
      const percent = window.remaining_percent;
      numbers.append(element('span', window.name), element('strong', percent == null ? '--' : percent + '%', 'percent'));
      row.append(numbers);
      if (percent != null) {
        const bar = element('progress');
        bar.max = 100;
        bar.value = percent;
        bar.setAttribute('aria-label', name + ' ' + window.name + ': осталось ' + percent + '%');
        row.append(bar);
      }
      row.append(element('p', window.resets_at ? 'Сброс: ' + new Date(window.resets_at).toLocaleString() : 'Время сброса неизвестно', 'reset'));
      if (provider.status !== 'ok') row.append(element('p', 'Последнее полученное значение · ' + statusNames[provider.status], 'warn'));
      card.append(row);
    }
    cards.push(card);
  }
  byId('providers').replaceChildren(...(cards.length ? cards : [element('p', 'Провайдеры ещё не настроены.')]));
}
function providerStale(provider, elapsed) {
  return provider.status === 'stale' ||
    (['error', 'auth_required', 'unavailable'].includes(provider.status) && provider.windows.some(window => window.remaining_percent != null)) ||
    (provider.source_age_seconds != null && provider.source_age_seconds + elapsed >= cached.stale_after_seconds);
}
function renderHealth() {
  const connection = byId('connection');
  connection.textContent = online ? 'ONLINE' : 'PC OFFLINE';
  connection.className = 'badge ' + (online ? 'good' : 'bad');
  if (!cached) return;
  const age = cached.age_seconds + Math.floor(Math.max(0, performance.now() - receivedAt) / 1000);
  byId('age').textContent = 'Возраст кеша: ' + Math.floor(age / 60) + ' мин ' + age % 60 + ' с';
  const elapsed = age;
  let stale = elapsed >= cached.stale_after_seconds;
  for (const card of document.querySelectorAll('.card')) {
    const provider = cached.providers[card.dataset.provider];
    const old = providerStale(provider, elapsed);
    stale ||= old;
    const badge = card.querySelector('.provider-status');
    badge.textContent = provider.status === 'ok' && old ? 'STALE' : statusNames[provider.status];
    badge.className = 'badge provider-status ' + (provider.status === 'ok' && !old ? 'good' : 'warn');
    const sourceAge = provider.source_age_seconds == null ? 'возраст источника неизвестен' : 'возраст источника: ' + Math.floor((provider.source_age_seconds + elapsed) / 60) + ' мин';
    card.querySelector('.source').textContent = provider.source + ' · ' + sourceAge;
  }
  byId('freshness').hidden = !stale;
}
async function refresh() {
  if (inFlight) return;
  inFlight = true;
  byId('refresh').disabled = true;
  const abort = new AbortController();
  const timeout = setTimeout(() => abort.abort(), 5000);
  try {
    const response = await fetch('/api/quotas', {cache: 'no-store', signal: abort.signal});
    if (!response.ok) throw new Error('HTTP ' + response.status);
    const text = await response.text();
    if (new TextEncoder().encode(text).length > 16384) throw new Error('Ответ слишком большой');
    const data = validate(JSON.parse(text));
    cached = data;
    receivedAt = performance.now();
    online = true;
    byId('checked').textContent = 'Последний успешный запрос: ' + new Date().toLocaleTimeString();
    byId('error').textContent = '';
    renderData();
  } catch (error) {
    online = false;
    byId('error').textContent = cached ? 'Коллектор недоступен или вернул ошибку. Показаны последние полученные данные.' : 'Нет данных. Проверьте, что коллектор запущен.';
  } finally {
    clearTimeout(timeout);
    inFlight = false;
    byId('refresh').disabled = false;
    renderHealth();
  }
}
byId('refresh').addEventListener('click', refresh);
window.addEventListener('online', refresh);
window.addEventListener('offline', () => { online = false; renderHealth(); });
document.addEventListener('visibilitychange', () => { if (!document.hidden) refresh(); });
setInterval(refresh, 30000);
setInterval(renderHealth, 1000);
refresh();
