(() => {
  'use strict';
  const refresh = document.getElementById('investigation-refresh');
  if (!refresh) return;
  const rows = document.getElementById('investigation-rows');
  const status = document.getElementById('investigation-status');
  const esc = (value) => String(value).replace(/[&<>"']/g, (char) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[char]);
  const time = (unix) => new Date(unix * 1000).toISOString().replace('T', ' ').slice(0, 19) + ' UTC';
  refresh.addEventListener('click', async () => {
    refresh.disabled = true;
    status.hidden = false;
    status.textContent = 'Refreshing observations…';
    try {
      const url = new URL(refresh.dataset.uri, location.origin);
      url.pathname = '/admin/stats/restrictions/data';
      const response = await fetch(url, { credentials: 'same-origin', cache: 'no-store', redirect: 'error', headers: { Accept: 'application/json' } });
      if (!response.ok) throw new Error('Observations could not be refreshed.');
      const data = await response.json();
      rows.innerHTML = data.events.map(({ event, identity, active }) => `<tr>
        <td>${time(event.observed_unix)}</td>
        <td><a href="/admin/stats/sessions/${event.session_id}"><strong>${esc(identity?.username || 'Unknown account')}</strong><small>#${event.session_id}</small></a></td>
        <td><span class="admin-badge">${esc(event.kind)}</span></td>
        <td>${active === true ? 'Active' : active === false ? 'Historical' : 'Unknown'}</td>
        <td><strong>${esc(event.endpoint)}</strong><small>${esc(event.evidence)}</small></td>
      </tr>`).join('');
      status.textContent = 'Observations refreshed.';
    } catch (error) { status.textContent = error.message; }
    finally { refresh.disabled = false; }
  });
})();


(() => {
  'use strict';
  const page = document.querySelector('.admin-stats-content');
  if (!page) {
    const reveal = document.getElementById('show-cookies');
    const confirm = document.getElementById('confirm-cookies');
    const confirmation = document.getElementById('cookie-confirmation');
    const output = document.getElementById('cookie-export');
    const text = document.getElementById('cookie-json');
    const status = document.getElementById('cookie-status');
    const copy = document.getElementById('copy-cookies');
    const id = document.querySelector('.admin-cookie-panel')?.dataset.session;
    if (!reveal || !confirm || !confirmation || !output || !text || !status || !copy || !id) return;
    reveal.addEventListener('click', () => { confirmation.hidden = false; confirm.focus(); });
    confirm.addEventListener('click', async () => {
      confirm.disabled = true;
      status.textContent = 'Loading current cookies…';
      try {
        const response = await fetch(`/admin/stats/sessions/${encodeURIComponent(id)}/cookies`, {
          credentials: 'same-origin', cache: 'no-store', redirect: 'error', headers: { Accept: 'application/json' },
        });
        if (!response.ok) throw new Error(response.status === 403 ? 'Administrator authorization is required.' : 'Cookie export is unavailable for this account.');
        const cookies = await response.json();
        text.value = JSON.stringify(cookies, null, 2);
        confirmation.hidden = true;
        output.hidden = false;
        status.textContent = 'Credentials revealed. Keep them private.';
        reveal.hidden = true;
      } catch (error) {
        status.textContent = error.message;
      } finally { confirm.disabled = false; }
    });
    copy.addEventListener('click', async () => {
      try { await navigator.clipboard.writeText(text.value); status.textContent = 'Cookie JSON copied.'; }
      catch { text.focus(); text.select(); status.textContent = 'Clipboard unavailable. The JSON is selected; copy it manually.'; }
    });
    return;
  }
  const minutes = Number(page.dataset.minutes);
  if (!Number.isSafeInteger(minutes) || minutes < 1) return;

  const byId = (id) => document.getElementById(`stats-${id}`);
  const status = byId('status');
  const delay = 30_000;
  const tableLimit = 120;
  const hourly = minutes === 7 * 24 * 60;
  let timer;
  let active;
  let refreshAgain = false;

  const fields = ['incoming', 'finished', 'success', 'empty', 'rejected', 'rate_limited', 'failed', 'other'];
  const bucketFields = [...fields, 'latency_ms'];
  const integer = Number.isSafeInteger;
  const formatMetric = (number, unit) => `${number.toFixed(1)} ${unit}`;
  const utc = (date) => {
    const time = document.createElement('time');
    time.dateTime = date.toISOString();
    time.textContent = `${time.dateTime.slice(0, 10)} ${time.dateTime.slice(11, 19)} UTC`;
    return time;
  };
  const minute = (seconds) => utc(new Date(seconds * 1000));
  const validMinute = (row) => row && integer(row.minute) && Math.abs(row.minute) <= 8_640_000_000_000;

  function render(data) {
    if (!data || data.minutes !== minutes || !Array.isArray(data.buckets) || !Array.isArray(data.restrictions) ||
        !data.buckets.every((row) => validMinute(row) && bucketFields.every((field) => integer(row[field]))) ||
        !Number.isInteger(data.dropped_events) || data.dropped_events < 0 ||
        !data.restrictions.every((row) => validMinute(row) && integer(row.search_only) && integer(row.suspended)) ||
        !(data.pool === null || (Array.isArray(data.pool) && data.pool.length === 2 && data.pool.every(integer))) ||
        !(data.capacity === null || Number.isFinite(data.capacity))) {
      throw new Error('Invalid statistics');
    }

    const buckets = [...data.buckets].sort((a, b) => a.minute - b.minute);
    const restrictions = data.restrictions.filter((row) => row.search_only > 0 || row.suspended > 0)
      .sort((a, b) => b.minute - a.minute).slice(0, 10);
    const points = [];
    const currentMinute = Math.floor(Date.now() / 60_000) * 60;
    let totalIncoming = 0;
    let totalSuccessful = 0;
    let totalCompleted = 0;
    let weightedLatency = 0;
    let closedSuccesses = 0;
    let closedMinutes = 0;
    let peak = buckets[0]?.incoming ?? 0;
    let maximum = 1;
    for (const row of buckets) {
      totalIncoming += row.incoming;
      totalSuccessful += row.success;
      totalCompleted += row.finished;
      weightedLatency += row.latency_ms * row.finished;
      if (row.minute < currentMinute) {
        closedSuccesses += row.success;
        closedMinutes++;
      }
      peak = Math.max(peak, row.incoming);
      maximum = Math.max(maximum, row.incoming, row.success);

      const minute = hourly ? Math.floor(row.minute / 3600) * 3600 : row.minute;
      const point = points.at(-1);
      if (hourly && point?.minute === minute) {
        point.incoming = Math.max(point.incoming, row.incoming);
        point.success = Math.max(point.success, row.success);
      } else {
        points.push({ minute, incoming: row.incoming, success: row.success });
      }
    }
    const first = buckets[0]?.minute;
    const last = buckets.at(-1)?.minute;
    const plotStart = points[0]?.minute;
    const span = points.at(-1)?.minute - plotStart;
    const path = (field, offset) => {
    if (!points.length) return '';
    const line = points.map((row) => {
      const x = (span ? 18 + 604 * ((row.minute - plotStart) / span) : 320) + offset;
      const y = 152 - 136 * (Math.max(0, row[field]) / maximum);
      return { x, y };
    });
    return line.map((point, index) => `${index ? 'L' : 'M'}${point.x.toFixed(2)} ${point.y.toFixed(2)}`).join(' ');
  };
  const area = (field, offset) => {
    const line = path(field, offset);
    if (!line) return '';
    const end = points.at(-1);
    const endX = (span ? 18 + 604 * ((end.minute - plotStart) / span) : 320) + offset;
    const startX = (span ? 18 : 320) + offset;
    return `${line} L${endX.toFixed(2)} 153 L${startX.toFixed(2)} 153 Z`;
  };

    const trafficRows = document.createDocumentFragment();
    for (const bucket of buckets.slice(-tableLimit).reverse()) {
      const row = document.createElement('tr');
      const header = document.createElement('th');
      header.scope = 'row';
      header.append(minute(bucket.minute));
      row.append(header);
      for (const key of fields) row.insertCell().textContent = bucket[key];
      trafficRows.append(row);
    }
    const restrictionRows = document.createDocumentFragment();
    for (const row of restrictions) {
      const item = document.createElement('li');
      item.append(minute(row.minute), `: ${row.search_only} search-only, ${row.suspended} suspended`);
      restrictionRows.append(item);
    }

    byId('total-incoming').textContent = totalIncoming;
    byId('total-successful').textContent = totalSuccessful;
    byId('total-completed').textContent = totalCompleted;
    byId('latency').textContent = totalCompleted > 0 ? formatMetric(weightedLatency / totalCompleted, 'ms') : 'Unavailable';
    byId('peak').textContent = buckets.length ? `${peak} req/min` : 'Unavailable';
    byId('observed').textContent = closedMinutes ? formatMetric(closedSuccesses / closedMinutes, 'successes/min') : 'Unavailable';
    byId('estimate').textContent = data.capacity === null ? 'Unavailable' : formatMetric(data.capacity, 'calls/min');
    byId('pool').textContent = data.pool ? `${data.pool[0]} search · ${data.pool[1]} view` : 'Unavailable';
    byId('dropped').textContent = data.dropped_events;
    byId('maximum').textContent = maximum;
    byId('incoming').setAttribute('d', path('incoming', 0));
    byId('successful').setAttribute('d', path('success', 0));
    document.querySelector('.stats-incoming-area')?.setAttribute('d', area('incoming', 0));
    document.querySelector('.stats-finished-area')?.setAttribute('d', area('success', 0));
    byId('span').replaceChildren(...(buckets.length ? [minute(first), ' – ', minute(last)] : []));
    byId('rows').replaceChildren(trafficRows);
    byId('table-limit').hidden = buckets.length <= tableLimit;
    byId('traffic').hidden = byId('traffic-table').hidden = buckets.length === 0;
    byId('traffic-empty').hidden = buckets.length > 0;
    byId('traffic-empty').textContent = 'No traffic samples were recorded in this range.';
    byId('restrictions-list').replaceChildren(restrictionRows);
    byId('restrictions-list').hidden = restrictions.length === 0;
    byId('restrictions-empty').hidden = restrictions.length > 0;
    byId('restrictions-empty').textContent = 'No restriction events were recorded in this range.';
    byId('updated').replaceChildren(utc(new Date()));
  }

  async function refresh() {
    if (document.visibilityState !== 'visible') return;
    if (active) {
      refreshAgain = active.signal.aborted;
      return;
    }
    const controller = new AbortController();
    active = controller;
    const deadline = setTimeout(() => controller.abort(), 15_000);
    let authFailed = false;
    try {
      const response = await fetch(`/admin/stats/data?minutes=${minutes}`, {
        credentials: 'same-origin', cache: 'no-store', redirect: 'error', signal: controller.signal,
        headers: { Accept: 'application/json' },
      });
      authFailed = response.status === 401 || response.status === 403;
      if (!response.ok) throw new Error('Statistics unavailable');
      const data = await response.json();
      if (document.visibilityState !== 'visible' || controller.signal.aborted) return;
      render(data);
      if (!status.hidden) {
        status.textContent = 'Statistics refreshed.';
        status.hidden = false;
      }
    } catch {
      if (document.visibilityState === 'visible' && !refreshAgain) {
        status.hidden = false;
        status.textContent = authFailed
          ? 'Administrator authorization is required to refresh statistics. Sign in again if your session expired. Any displayed values are from the last successful refresh.'
          : 'Could not refresh statistics. Any displayed values are from the last successful refresh.';
      }
    } finally {
      clearTimeout(deadline);
      active = undefined;
      if (refreshAgain) {
        refreshAgain = false;
        void refresh();
      }
    }
  }

  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') {
      void refresh();
      schedule();
    } else {
      clearTimeout(timer);
      active?.abort();
    }
  });
  // Schedule from the start of each poll rather than waiting for the request to finish.
  function tick() {
    void refresh();
    schedule();
  }
  function schedule() {
    clearTimeout(timer);
    if (document.visibilityState === 'visible') timer = setTimeout(tick, delay);
  }
  const countdown = document.getElementById('stats-countdown');
  if (countdown) {
    let secsLeft = delay / 1000;
    countdown.textContent = secsLeft;
    setInterval(() => {
      secsLeft = Math.max(0, secsLeft - 1);
      countdown.textContent = secsLeft;
    }, 1000);
    const _origTick = tick;
    tick = () => { secsLeft = delay / 1000; _origTick(); };
  }
  schedule();
})();
// Manual health probe. The only call the panel makes that reaches X on behalf
// of a benched account, and it only runs when the button is pressed.
(() => {
  'use strict';
  const button = document.getElementById('run-probe');
  const panel = document.querySelector('.admin-probe-panel');
  const status = document.getElementById('probe-status');
  const result = document.getElementById('probe-result');
  const verdictEl = document.getElementById('probe-verdict');
  const suspendedEl = document.getElementById('probe-suspended');
  const retiredEl = document.getElementById('probe-retired');
  const poolEl = document.getElementById('probe-pool');
  const detailEl = document.getElementById('probe-detail');
  if (!button || !panel || !status || !result) return;
  const id = panel.dataset.session;

  const show = (flags) => {
    suspendedEl.textContent = flags.suspended ? 'yes' : 'no';
    retiredEl.textContent = flags.retired ? 'yes' : 'no';
    poolEl.textContent = flags.pool;
  };

  button.addEventListener('click', async () => {
    button.disabled = true;
    status.textContent = 'Asking X about this account…';
    result.hidden = true;
    try {
      const response = await fetch(`/admin/stats/sessions/${encodeURIComponent(id)}/probe`, {
        method: 'POST',
        credentials: 'same-origin',
        cache: 'no-store',
        redirect: 'error',
        headers: { Accept: 'application/json' },
      });
      if (!response.ok) throw new Error(response.status === 404 ? 'That account is not in the runtime pool.' : 'The probe could not run.');
      const data = await response.json();
      verdictEl.textContent = data.reinstated ? `${data.verdict} — verdict cleared` : data.verdict;
      show(data.after);
      detailEl.textContent = data.detail
        ? data.detail
        : data.reinstated
          ? 'The stored suspension was contradicted and has been cleared.'
          : 'The stored verdict stands.';
      result.hidden = false;
      status.textContent = '';
    } catch (error) {
      status.textContent = error.message;
    } finally {
      button.disabled = false;
    }
  });
})();
