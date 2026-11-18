(() => {
  'use strict';

  const REFRESH_INTERVAL_SECONDS = 30;
  const FETCH_TIMEOUT_MS = 15000;
  const SVG_NS = 'http://www.w3.org/2000/svg';

  const VIEWBOX_WIDTH = 760;
  const PLOT_LEFT = 16;
  const PLOT_RIGHT = 744;
  const PLOT_WIDTH = 728;
  const PLOT_BOTTOM = 240;
  const PLOT_HEIGHT = 224;
  const CLICK_MAX_DRAG_PX = 6;
  const BRUSH_MIN_DRAG_PX = 8;

  let remaining = REFRESH_INTERVAL_SECONDS;
  let activeRefreshController = null;
  let refreshSequence = 0;
  let dashboardRefreshing = false;
  let accountActionInFlight = false;

  // Persistent & live interactive chart state.
  let smoothWindow = 1;
  let pointerOverCharts = false;
  let pinnedMinute = null;
  let hoverMinute = null;
  let chartState = null;
  let dragState = null;
  let hoverRafId = 0;
  let pendingHover = null;
  let pinHandledOnPointerUp = false;
  let suppressNextClick = false;
  const boundHitAreas = new WeakSet();

  function setHidden(element, isHidden) {
    if (!element) return;
    const hide = Boolean(isHidden);
    element.hidden = hide;
    if (hide) {
      element.setAttribute('hidden', '');
    } else {
      element.removeAttribute('hidden');
    }
  }

  function getMain(root) {
    const scope = root || document;
    return scope.getElementById('admin-stats-main') || scope.querySelector('main.admin-stats');
  }

  function isDashboardPage() {
    const main = getMain(document);
    if (!main) return false;
    const page = main.dataset.page;
    return page === 'dashboard' || page === 'overview';
  }

  function isRefreshPaused() {
    return Boolean(
      pointerOverCharts ||
      pinnedMinute !== null ||
      (chartState && chartState.isZoomed)
    );
  }

  function resolveSessionId(trigger) {
    if (trigger) {
      if (trigger.dataset.session) return trigger.dataset.session;
      if (trigger.dataset.sessionId) return trigger.dataset.sessionId;
      const scoped = trigger.closest('[data-session], [data-session-id]');
      if (scoped) {
        if (scoped.dataset.session) return scoped.dataset.session;
        if (scoped.dataset.sessionId) return scoped.dataset.sessionId;
      }
    }
    const main = getMain(document);
    if (main) {
      if (main.dataset.session) return main.dataset.session;
      if (main.dataset.sessionId) return main.dataset.sessionId;
    }
    const match = window.location.pathname.match(/^\/admin\/stats\/sessions\/([^/?#]+)/);
    return match ? decodeURIComponent(match[1]) : null;
  }

  function applyNoticeTone(element, tone) {
    if (!element) return;
    element.classList.remove('admin-notice--ok', 'admin-notice--warn', 'admin-notice--error');
    element.classList.add('admin-notice');
    if (tone === 'ok') {
      element.classList.add('admin-notice--ok');
    } else if (tone === 'warn') {
      element.classList.add('admin-notice--warn');
    } else if (tone === 'error') {
      element.classList.add('admin-notice--error');
    }
  }

  function setStatusBanner(element, message, tone) {
    if (!element) return;
    if (!message) {
      element.textContent = '';
      setHidden(element, true);
      return;
    }
    element.textContent = message;
    applyNoticeTone(element, tone);
    setHidden(element, false);
  }

  function getErrorBanner() {
    return (
      document.getElementById('stats-status') ||
      document.getElementById('investigation-status') ||
      document.getElementById('account-action-status') ||
      document.getElementById('probe-status')
    );
  }

  function getAccountStatusElement() {
    return document.getElementById('account-action-status') || document.getElementById('probe-status');
  }

  function syncCountdownText() {
    const countdown = document.getElementById('stats-countdown');
    if (countdown) {
      countdown.textContent = isRefreshPaused() ? 'Paused' : `${remaining}s`;
    }
  }

  // ── Formatting & Time-Series Math (matching src/views/admin_stats.rs) ──────

  function pad2(num) {
    return String(num).padStart(2, '0');
  }

  function formatPanelVal(value, unit) {
    const safe = Number.isFinite(value) ? value : 0;
    if (unit === '%') {
      return `${safe.toFixed(1)}%`;
    }
    if (unit === 'ms') {
      return `${Math.round(safe)} ms`;
    }
    if (safe >= 100 || Math.abs(safe - Math.round(safe)) < 0.05) {
      return `${Math.round(safe)}`;
    }
    return safe.toFixed(1);
  }

  function formatHoverBadge(value, unit) {
    const formatted = formatPanelVal(value, unit);
    if (!unit || unit === '%' || unit === 'ms') {
      return formatted;
    }
    return `${formatted} ${unit}`;
  }

  function formatMetricCount(value) {
    const safe = Number.isFinite(value) ? value : 0;
    if (Math.abs(safe - Math.round(safe)) < 0.05) {
      return `${Math.round(safe)}`;
    }
    return safe.toFixed(1);
  }

  function axisTimeLabel(unixSeconds, includeDate) {
    if (!Number.isFinite(unixSeconds)) return '';
    const date = new Date(Math.trunc(unixSeconds) * 1000);
    if (Number.isNaN(date.getTime())) return '';
    const hours = pad2(date.getUTCHours());
    const minutes = pad2(date.getUTCMinutes());
    if (includeDate) {
      const month = pad2(date.getUTCMonth() + 1);
      const day = pad2(date.getUTCDate());
      return `${month}-${day} ${hours}:${minutes}`;
    }
    return `${hours}:${minutes} UTC`;
  }

  function formatInspectorUtc(unixSeconds) {
    if (!Number.isFinite(unixSeconds)) return '—';
    const date = new Date(Math.trunc(unixSeconds) * 1000);
    if (Number.isNaN(date.getTime())) return '—';
    const year = date.getUTCFullYear();
    const month = pad2(date.getUTCMonth() + 1);
    const day = pad2(date.getUTCDate());
    const hours = pad2(date.getUTCHours());
    const minutes = pad2(date.getUTCMinutes());
    return `${year}-${month}-${day} ${hours}:${minutes} UTC`;
  }

  function getSampleMetric(sample, key) {
    switch (key) {
      case 'incoming':
        return sample.incoming;
      case 'answered':
        return sample.answered;
      case 'empty':
        return sample.empty;
      case 'rejected':
        return sample.rejected;
      case 'rate_limited':
        return sample.rate_limited;
      case 'failed':
        return sample.failed;
      case 'other':
        return sample.other;
      case 'challenged':
        return sample.challenged;
      case 'error_rate':
        return sample.error_rate;
      case 'latency':
        return sample.latency;
      default:
        return 0;
    }
  }

  function parseSeriesRows(rawJson) {
    if (!rawJson) return [];
    let parsed;
    try {
      parsed = JSON.parse(rawJson);
    } catch {
      return [];
    }
    if (!Array.isArray(parsed)) return [];
    const samples = [];
    for (const row of parsed) {
      if (!Array.isArray(row) || row.length < 11) continue;
      samples.push({
        minute: Number(row[0]) || 0,
        incoming: Number(row[1]) || 0,
        answered: Number(row[2]) || 0,
        empty: Number(row[3]) || 0,
        rejected: Number(row[4]) || 0,
        rate_limited: Number(row[5]) || 0,
        failed: Number(row[6]) || 0,
        other: Number(row[7]) || 0,
        challenged: Number(row[8]) || 0,
        error_rate: Number(row[9]) || 0,
        latency: Number(row[10]) || 0
      });
    }
    samples.sort((a, b) => a.minute - b.minute);
    return samples;
  }

  function computeSmoothedSamples(rawSamples, windowSize, maxGap) {
    if (windowSize <= 1 || rawSamples.length === 0) {
      return rawSamples.slice();
    }

    // Split into contiguous maxGap runs so missing intervals are never averaged across gaps.
    const runs = [];
    for (const sample of rawSamples) {
      const currentRun = runs.length > 0 ? runs[runs.length - 1] : null;
      if (
        currentRun &&
        sample.minute - currentRun[currentRun.length - 1].minute <= maxGap
      ) {
        currentRun.push(sample);
      } else {
        runs.push([sample]);
      }
    }

    const half = Math.floor(windowSize / 2);
    const out = [];

    for (const run of runs) {
      const len = run.length;
      for (let i = 0; i < len; i += 1) {
        const lo = Math.max(0, i - half);
        const hi = Math.min(len - 1, i + half);
        const count = hi - lo + 1;
        if (count <= 1) {
          out.push({ ...run[i] });
          continue;
        }
        let incoming = 0;
        let answered = 0;
        let empty = 0;
        let rejected = 0;
        let rateLimited = 0;
        let failed = 0;
        let other = 0;
        let challenged = 0;
        let errorRate = 0;
        let latency = 0;
        for (let j = lo; j <= hi; j += 1) {
          const item = run[j];
          incoming += item.incoming;
          answered += item.answered;
          empty += item.empty;
          rejected += item.rejected;
          rateLimited += item.rate_limited;
          failed += item.failed;
          other += item.other;
          challenged += item.challenged;
          errorRate += item.error_rate;
          latency += item.latency;
        }
        out.push({
          minute: run[i].minute,
          incoming: incoming / count,
          answered: answered / count,
          empty: empty / count,
          rejected: rejected / count,
          rate_limited: rateLimited / count,
          failed: failed / count,
          other: other / count,
          challenged: challenged / count,
          error_rate: errorRate / count,
          latency: latency / count
        });
      }
    }
    return out;
  }

  function lowerBoundMinute(samples, targetMinute) {
    let lo = 0;
    let hi = samples.length;
    while (lo < hi) {
      const mid = (lo + hi) >>> 1;
      if (samples[mid].minute < targetMinute) {
        lo = mid + 1;
      } else {
        hi = mid;
      }
    }
    return lo;
  }

  function upperBoundMinute(samples, targetMinute) {
    let lo = 0;
    let hi = samples.length;
    while (lo < hi) {
      const mid = (lo + hi) >>> 1;
      if (samples[mid].minute <= targetMinute) {
        lo = mid + 1;
      } else {
        hi = mid;
      }
    }
    return lo;
  }

  function findSampleByMinute(samples, minute) {
    if (minute === null || minute === undefined) return null;
    const idx = lowerBoundMinute(samples, minute);
    if (idx < samples.length && samples[idx].minute === minute) {
      return samples[idx];
    }
    return null;
  }

  function getVisibleIndexBounds(state) {
    if (!state || state.displaySamples.length === 0) {
      return [0, 0];
    }
    if (!state.isZoomed) {
      return [0, state.displaySamples.length];
    }
    const startIdx = lowerBoundMinute(state.displaySamples, state.zoomStart);
    const endIdx = upperBoundMinute(state.displaySamples, state.zoomEnd);
    return [startIdx, Math.max(startIdx, endIdx)];
  }

  function findNearestVisibleSample(state, targetUnix) {
    const [startIdx, endIdx] = getVisibleIndexBounds(state);
    if (startIdx >= endIdx) return null;
    const samples = state.displaySamples;
    let lo = startIdx;
    let hi = endIdx;
    while (lo < hi) {
      const mid = (lo + hi) >>> 1;
      if (samples[mid].minute < targetUnix) {
        lo = mid + 1;
      } else {
        hi = mid;
      }
    }
    if (lo <= startIdx) return samples[startIdx];
    if (lo >= endIdx) return samples[endIdx - 1];
    const prev = samples[lo - 1];
    const curr = samples[lo];
    return targetUnix - prev.minute <= curr.minute - targetUnix ? prev : curr;
  }

  function xAtMinute(state, minute) {
    const span = Math.max(0, state.zoomEnd - state.zoomStart);
    const spanDenom = Math.max(60, span);
    const clamped = Math.min(Math.max(minute - state.zoomStart, 0), span);
    return PLOT_LEFT + PLOT_WIDTH * (clamped / spanDenom);
  }

  function yAtValue(value, maximum) {
    const maxSafe = Math.max(1, maximum);
    const clamped = Math.min(Math.max(value, 0), maxSafe);
    return PLOT_BOTTOM - PLOT_HEIGHT * (clamped / maxSafe);
  }

  function clientXToViewX(svg, clientX) {
    const rect = svg.getBoundingClientRect();
    let rawViewX;
    if (rect.width > 0) {
      rawViewX = ((clientX - rect.left) / rect.width) * VIEWBOX_WIDTH;
    } else {
      rawViewX = clientX;
    }
    return Math.min(Math.max(rawViewX, PLOT_LEFT), PLOT_RIGHT);
  }

  function viewXToUnix(state, viewX) {
    const span = Math.max(0, state.zoomEnd - state.zoomStart);
    const spanDenom = Math.max(60, span);
    const ratio = (viewX - PLOT_LEFT) / PLOT_WIDTH;
    return state.zoomStart + ratio * spanDenom;
  }

  // ── Synchronized Inspector, Crosshair, Pinning, Brush Zoom & Redraw ────────

  function isPanelVisible(panel) {
    return Boolean(panel && panel.figure && panel.figure.isConnected && !panel.figure.closest('[hidden]'));
  }

  function buildInspectorItem(label, valueText) {
    const item = document.createElement('div');
    item.className = 'admin-inspector-item';
    const dt = document.createElement('span');
    dt.className = 'admin-inspector-label';
    dt.textContent = label;
    const dd = document.createElement('strong');
    dd.className = 'admin-inspector-value';
    dd.textContent = valueText;
    item.replaceChildren(dt, dd);
    return item;
  }

  function renderInspector(sample, locked) {
    if (!chartState) return;
    const { inspectorHint, inspectorValues } = chartState;
    if (!inspectorValues) return;

    if (!sample) {
      inspectorValues.replaceChildren();
      setHidden(inspectorValues, true);
      if (inspectorHint) {
        setHidden(inspectorHint, false);
      }
      return;
    }

    const nodes = [];
    if (locked) {
      const badge = document.createElement('span');
      badge.className = 'admin-badge admin-badge--search';
      badge.textContent = 'Pinned';
      nodes.push(badge);
    }

    nodes.push(
      buildInspectorItem('Time UTC', formatInspectorUtc(sample.minute)),
      buildInspectorItem('Incoming', formatMetricCount(sample.incoming)),
      buildInspectorItem(
        'Answered (+Empty)',
        `${formatMetricCount(sample.answered)} (+${formatMetricCount(sample.empty)})`
      ),
      buildInspectorItem(
        'Denied / 429 / Failed',
        `${formatMetricCount(sample.rejected)} / ${formatMetricCount(sample.rate_limited)} / ${formatMetricCount(sample.failed)}`
      ),
      buildInspectorItem('Challenged', formatMetricCount(sample.challenged)),
      buildInspectorItem('Fault %', `${sample.error_rate.toFixed(1)}%`),
      buildInspectorItem('Latency', `${Math.round(sample.latency)} ms`)
    );

    if (inspectorHint) {
      setHidden(inspectorHint, true);
    }
    inspectorValues.replaceChildren(...nodes);
    setHidden(inspectorValues, false);
  }

  function syncPinnedRowHighlight() {
    const tbody = document.getElementById('stats-rows');
    if (!tbody) return;
    for (const row of tbody.querySelectorAll('tr.is-highlighted')) {
      row.classList.remove('is-highlighted');
    }
    if (pinnedMinute === null) return;
    const targetKey = String(pinnedMinute);
    let matchedExact = false;
    const rows = tbody.querySelectorAll('tr[data-minute]');
    for (const row of rows) {
      if (row.dataset.minute === targetKey) {
        row.classList.add('is-highlighted');
        matchedExact = true;
      }
    }
    if (!matchedExact && chartState && chartState.hourly) {
      for (const row of rows) {
        const rowMin = Number.parseInt(row.dataset.minute || '', 10);
        if (Number.isFinite(rowMin) && rowMin >= pinnedMinute && rowMin < pinnedMinute + 3600) {
          row.classList.add('is-highlighted');
        }
      }
    }
  }

  function syncToolbarButtons() {
    const rawBtn = document.getElementById('chart-mode-raw');
    const smoothBtn = document.getElementById('chart-mode-smooth');
    const unpinBtn = document.getElementById('chart-unpin');
    const resetZoomBtn = document.getElementById('chart-reset-zoom');

    if (rawBtn) {
      rawBtn.classList.toggle('is-active', smoothWindow === 1);
    }
    if (smoothBtn) {
      smoothBtn.classList.toggle('is-active', smoothWindow === 3);
    }
    if (unpinBtn) {
      setHidden(unpinBtn, pinnedMinute === null);
    }
    if (resetZoomBtn) {
      setHidden(resetZoomBtn, !(chartState && chartState.isZoomed));
    }
  }

  function hideCrosshairAndHover() {
    if (!chartState) return;
    for (const panel of chartState.panels) {
      setHidden(panel.crosshair, true);
      setHidden(panel.hoverRing, true);
      setHidden(panel.hoverDot, true);
      if (panel.hoverVal) {
        panel.hoverVal.textContent = '';
        setHidden(panel.hoverVal, true);
      }
    }
    renderInspector(null, false);
  }

  function renderSynchronizedSample(sample, locked) {
    if (!chartState || !sample) {
      hideCrosshairAndHover();
      return;
    }

    // If zoomed and the sample lies outside [zoomStart, zoomEnd], hide hover markers.
    if (
      chartState.isZoomed &&
      (sample.minute < chartState.zoomStart || sample.minute > chartState.zoomEnd)
    ) {
      hideCrosshairAndHover();
      return;
    }

    const cx = xAtMinute(chartState, sample.minute);
    const cxStr = cx.toFixed(2);

    for (const panel of chartState.panels) {
      if (!isPanelVisible(panel)) continue;
      const value = getSampleMetric(sample, panel.key);
      const cy = yAtValue(value, panel.maximum);
      const cyStr = cy.toFixed(2);

      if (panel.crosshair) {
        panel.crosshair.setAttribute('x1', cxStr);
        panel.crosshair.setAttribute('x2', cxStr);
        setHidden(panel.crosshair, false);
      }
      if (panel.hoverRing) {
        panel.hoverRing.setAttribute('cx', cxStr);
        panel.hoverRing.setAttribute('cy', cyStr);
        setHidden(panel.hoverRing, false);
      }
      if (panel.hoverDot) {
        panel.hoverDot.setAttribute('cx', cxStr);
        panel.hoverDot.setAttribute('cy', cyStr);
        setHidden(panel.hoverDot, false);
      }
      if (panel.hoverVal) {
        panel.hoverVal.textContent = formatHoverBadge(value, panel.unit);
        setHidden(panel.hoverVal, false);
      }
    }

    renderInspector(sample, locked);
  }

  function syncActiveInspection() {
    if (!chartState) return;
    if (pinnedMinute !== null) {
      const pinnedSample = findSampleByMinute(chartState.displaySamples, pinnedMinute);
      if (pinnedSample) {
        renderSynchronizedSample(pinnedSample, true);
        return;
      }
    }
    if (pointerOverCharts && hoverMinute !== null) {
      const hoveredSample = findSampleByMinute(chartState.displaySamples, hoverMinute);
      if (hoveredSample) {
        renderSynchronizedSample(hoveredSample, false);
        return;
      }
    }
    hideCrosshairAndHover();
  }

  function hideAllBrushes() {
    if (!chartState) return;
    for (const panel of chartState.panels) {
      if (panel.brush) {
        panel.brush.setAttribute('x', String(PLOT_LEFT));
        panel.brush.setAttribute('width', '0');
        setHidden(panel.brush, true);
      }
    }
  }

  function renderBrushAcrossPanels(startViewX, currentViewX) {
    if (!chartState) return;
    const x = Math.min(startViewX, currentViewX);
    const width = Math.max(1, Math.abs(currentViewX - startViewX));
    const xStr = x.toFixed(2);
    const widthStr = width.toFixed(2);
    for (const panel of chartState.panels) {
      if (!isPanelVisible(panel) || !panel.brush) continue;
      panel.brush.setAttribute('x', xStr);
      panel.brush.setAttribute('width', widthStr);
      setHidden(panel.brush, false);
    }
  }

  function computePanelMaximum(panel, visibleSamples) {
    let peak = 0;
    for (const sample of visibleSamples) {
      const val = getSampleMetric(sample, panel.key);
      if (val > peak) peak = val;
    }
    const thresholdFloor = panel.threshold !== null ? panel.threshold : 0;
    return Math.max(peak, thresholdFloor, 1);
  }

  function redrawPanel(panel, state, visibleSamples) {
    const unit = panel.unit;
    let peak = 0;
    let sum = 0;
    const count = visibleSamples.length;
    const latest = count > 0 ? getSampleMetric(visibleSamples[count - 1], panel.key) : 0;

    for (let i = 0; i < count; i += 1) {
      const val = getSampleMetric(visibleSamples[i], panel.key);
      if (val > peak) peak = val;
      sum += val;
    }

    const mean = count > 0 ? sum / count : 0;
    const thresholdFloor = panel.threshold !== null ? panel.threshold : 0;
    const maximum = Math.max(peak, thresholdFloor, 1);
    panel.maximum = maximum;

    if (!isPanelVisible(panel)) return;

    const segments = [];
    let prevMinute = null;
    for (let i = 0; i < count; i += 1) {
      const sample = visibleSamples[i];
      const val = getSampleMetric(sample, panel.key);
      const pt = {
        x: xAtMinute(state, sample.minute),
        y: yAtValue(val, maximum)
      };
      if (
        prevMinute !== null &&
        sample.minute - prevMinute <= state.maxGap &&
        segments.length > 0
      ) {
        segments[segments.length - 1].push(pt);
      } else {
        segments.push([pt]);
      }
      prevMinute = sample.minute;
    }

    const lineParts = [];
    const areaParts = [];
    const dotNodes = [];

    for (const seg of segments) {
      if (seg.length === 1) {
        const circle = document.createElementNS(SVG_NS, 'circle');
        circle.setAttribute('class', 'stats-dot');
        circle.setAttribute('cx', seg[0].x.toFixed(2));
        circle.setAttribute('cy', seg[0].y.toFixed(2));
        circle.setAttribute('r', '3.5');
        dotNodes.push(circle);
        continue;
      }
      if (seg.length > 1) {
        const first = seg[0];
        const last = seg[seg.length - 1];
        let lineSeg = `M${first.x.toFixed(2)} ${first.y.toFixed(2)}`;
        for (let i = 1; i < seg.length; i += 1) {
          lineSeg += ` L${seg[i].x.toFixed(2)} ${seg[i].y.toFixed(2)}`;
        }
        lineParts.push(lineSeg);
        areaParts.push(
          `${lineSeg} L${last.x.toFixed(2)} ${PLOT_BOTTOM} L${first.x.toFixed(2)} ${PLOT_BOTTOM} Z`
        );
      }
    }

    if (panel.line) {
      panel.line.setAttribute('d', lineParts.join(' '));
    }
    if (panel.area) {
      panel.area.setAttribute('d', areaParts.join(' '));
    }
    if (panel.dots) {
      panel.dots.replaceChildren(...dotNodes);
    }
    if (panel.thresholdEl && panel.threshold !== null) {
      const thY = yAtValue(panel.threshold, maximum);
      panel.thresholdEl.setAttribute('d', `M${PLOT_LEFT} ${thY.toFixed(2)} H${PLOT_RIGHT}`);
    }

    if (panel.axisMax) {
      panel.axisMax.textContent = formatPanelVal(maximum, unit);
    }
    if (panel.axisMid) {
      panel.axisMid.textContent = formatPanelVal(maximum / 2, unit);
    }

    const span = Math.max(0, state.zoomEnd - state.zoomStart);
    const midUnix = state.zoomStart + Math.trunc(span / 2);
    if (panel.xaxisStart) {
      panel.xaxisStart.textContent = axisTimeLabel(state.zoomStart, state.includeDate);
    }
    if (panel.xaxisMid) {
      panel.xaxisMid.textContent = axisTimeLabel(midUnix, state.includeDate);
    }
    if (panel.xaxisEnd) {
      panel.xaxisEnd.textContent = axisTimeLabel(state.zoomEnd, state.includeDate);
    }

    if (panel.statLast) {
      panel.statLast.textContent = formatPanelVal(latest, unit);
    }
    if (panel.statAvg) {
      panel.statAvg.textContent = formatPanelVal(mean, unit);
    }
    if (panel.statPeak) {
      panel.statPeak.textContent = formatPanelVal(peak, unit);
    }
  }

  function redrawAllPanels() {
    if (!chartState) return;
    const [startIdx, endIdx] = getVisibleIndexBounds(chartState);
    const visibleSamples = chartState.displaySamples.slice(startIdx, endIdx);
    for (const panel of chartState.panels) {
      redrawPanel(panel, chartState, visibleSamples);
    }
    syncToolbarButtons();
    syncActiveInspection();
  }

  function flushPendingHover() {
    if (hoverRafId) {
      window.cancelAnimationFrame(hoverRafId);
      hoverRafId = 0;
    }
    const next = pendingHover;
    pendingHover = null;
    if (!next || !chartState) return;

    const viewX = clientXToViewX(next.svg, next.clientX);
    if (dragState && dragState.brushing) {
      renderBrushAcrossPanels(dragState.startViewX, viewX);
    }

    const targetUnix = viewXToUnix(chartState, viewX);
    const nearest = findNearestVisibleSample(chartState, targetUnix);
    hoverMinute = nearest ? nearest.minute : null;

    if (pinnedMinute === null) {
      renderSynchronizedSample(nearest, false);
    }
  }

  function scheduleHoverUpdate(svg, clientX) {
    pendingHover = { svg, clientX };
    if (hoverRafId) return;
    hoverRafId = window.requestAnimationFrame(() => {
      hoverRafId = 0;
      flushPendingHover();
    });
  }

  function setPinnedMinute(nextMinute) {
    pinnedMinute = nextMinute;
    syncPinnedRowHighlight();
    syncToolbarButtons();
    syncActiveInspection();
    syncCountdownText();
  }

  function unpinSample() {
    if (pinnedMinute === null) return;
    setPinnedMinute(null);
  }

  function togglePinAtClientX(svg, clientX) {
    if (!chartState || chartState.displaySamples.length === 0) return;
    const viewX = clientXToViewX(svg, clientX);
    const targetUnix = viewXToUnix(chartState, viewX);
    const nearest = findNearestVisibleSample(chartState, targetUnix);
    if (!nearest) return;

    hoverMinute = nearest.minute;
    if (pinnedMinute === nearest.minute) {
      setPinnedMinute(null);
    } else {
      setPinnedMinute(nearest.minute);
    }
  }

  function applyZoomRange(minViewX, maxViewX) {
    if (!chartState) return;
    const rawStart = Math.round(viewXToUnix(chartState, minViewX));
    const rawEnd = Math.round(viewXToUnix(chartState, maxViewX));
    const minStep = chartState.hourly ? 3600 : 60;

    let nextStart = Math.max(chartState.windowStart, Math.min(rawStart, chartState.windowEnd));
    let nextEnd = Math.max(chartState.windowStart, Math.min(rawEnd, chartState.windowEnd));
    if (nextEnd < nextStart) {
      const swap = nextStart;
      nextStart = nextEnd;
      nextEnd = swap;
    }

    if (nextEnd - nextStart < minStep) {
      const center = Math.round((nextStart + nextEnd) / 2);
      nextStart = Math.max(chartState.windowStart, center - Math.ceil(minStep / 2));
      nextEnd = Math.min(chartState.windowEnd, nextStart + minStep);
      nextStart = Math.max(chartState.windowStart, nextEnd - minStep);
    }

    chartState.zoomStart = nextStart;
    chartState.zoomEnd = nextEnd;
    chartState.isZoomed =
      nextStart > chartState.windowStart || nextEnd < chartState.windowEnd;

    if (
      pinnedMinute !== null &&
      (pinnedMinute < chartState.zoomStart || pinnedMinute > chartState.zoomEnd)
    ) {
      pinnedMinute = null;
      syncPinnedRowHighlight();
    }

    redrawAllPanels();
    syncCountdownText();
  }

  function resetZoom() {
    if (!chartState || !chartState.isZoomed) return;
    chartState.zoomStart = chartState.windowStart;
    chartState.zoomEnd = chartState.windowEnd;
    chartState.isZoomed = false;
    hideAllBrushes();
    redrawAllPanels();
    syncCountdownText();
  }

  function setSmoothMode(nextWindow) {
    const normalized = nextWindow === 3 ? 3 : 1;
    smoothWindow = normalized;
    if (!chartState) {
      syncToolbarButtons();
      return;
    }
    chartState.displaySamples = computeSmoothedSamples(
      chartState.rawSamples,
      smoothWindow,
      chartState.maxGap
    );
    redrawAllPanels();
  }

  function finishPointerGesture(event, cancelled) {
    if (!dragState || dragState.pointerId !== event.pointerId) return;
    const active = dragState;
    dragState = null;

    try {
      active.hitArea.releasePointerCapture(event.pointerId);
    } catch {
      // Synthetic pointer events may not hold capture.
    }

    flushPendingHover();
    hideAllBrushes();

    if (cancelled) {
      return;
    }

    const dxPx = Math.abs(event.clientX - active.startClientX);
    if (active.brushing || dxPx >= BRUSH_MIN_DRAG_PX) {
      const endViewX = clientXToViewX(active.svg, event.clientX);
      const minViewX = Math.min(active.startViewX, endViewX);
      const maxViewX = Math.max(active.startViewX, endViewX);
      suppressNextClick = true;
      window.setTimeout(() => {
        suppressNextClick = false;
      }, 0);
      applyZoomRange(minViewX, maxViewX);
      if (pointerOverCharts) {
        scheduleHoverUpdate(active.svg, event.clientX);
        flushPendingHover();
      }
      return;
    }

    if (dxPx < CLICK_MAX_DRAG_PX) {
      pinHandledOnPointerUp = true;
      window.setTimeout(() => {
        pinHandledOnPointerUp = false;
      }, 0);
      togglePinAtClientX(active.svg, event.clientX);
    }
  }

  function bindHitAreaListeners(hitArea, svg) {
    if (!hitArea || !svg || boundHitAreas.has(hitArea)) return;
    boundHitAreas.add(hitArea);

    hitArea.addEventListener('pointerenter', () => {
      if (!pointerOverCharts) {
        pointerOverCharts = true;
        syncCountdownText();
      }
    });

    hitArea.addEventListener('pointerdown', (event) => {
      if (typeof event.button === 'number' && event.button !== 0) return;
      pinHandledOnPointerUp = false;
      suppressNextClick = false;
      pointerOverCharts = true;
      syncCountdownText();

      const startViewX = clientXToViewX(svg, event.clientX);
      dragState = {
        pointerId: event.pointerId,
        hitArea,
        svg,
        startClientX: event.clientX,
        startViewX,
        brushing: false
      };

      try {
        hitArea.setPointerCapture(event.pointerId);
      } catch {
        // Ignore when dispatched synthetically without active browser pointer id.
      }
    });

    hitArea.addEventListener('pointermove', (event) => {
      if (!pointerOverCharts) {
        pointerOverCharts = true;
        syncCountdownText();
      }
      if (dragState && dragState.pointerId === event.pointerId) {
        const dxPx = Math.abs(event.clientX - dragState.startClientX);
        if (dxPx >= BRUSH_MIN_DRAG_PX) {
          dragState.brushing = true;
          renderBrushAcrossPanels(dragState.startViewX, clientXToViewX(svg, event.clientX));
        }
      }
      scheduleHoverUpdate(svg, event.clientX);
    });

    hitArea.addEventListener('pointerup', (event) => {
      finishPointerGesture(event, false);
    });

    hitArea.addEventListener('pointercancel', (event) => {
      finishPointerGesture(event, true);
    });

    hitArea.addEventListener('pointerleave', () => {
      if (dragState && dragState.hitArea === hitArea) return;
      flushPendingHover();
      pointerOverCharts = false;
      hoverMinute = null;
      if (pinnedMinute === null) {
        hideCrosshairAndHover();
      }
      syncCountdownText();
    });

    hitArea.addEventListener('click', (event) => {
      if (suppressNextClick) {
        suppressNextClick = false;
        return;
      }
      if (pinHandledOnPointerUp) {
        pinHandledOnPointerUp = false;
        return;
      }
      togglePinAtClientX(svg, event.clientX);
    });

    hitArea.addEventListener('dblclick', (event) => {
      event.preventDefault();
      resetZoom();
    });
  }

  function initCharts() {
    if (hoverRafId) {
      window.cancelAnimationFrame(hoverRafId);
      hoverRafId = 0;
    }
    pendingHover = null;
    dragState = null;
    pointerOverCharts = false;
    hoverMinute = null;
    pinnedMinute = null;

    const main = getMain(document);
    const content = main ? main.querySelector('.admin-stats-content[data-series]') : null;
    if (!content) {
      chartState = null;
      return;
    }

    const rawSamples = parseSeriesRows(content.dataset.series);
    const hourly = content.dataset.hourly === 'true';
    const includeDate = content.dataset.includeDate === 'true';
    const maxGap = hourly ? 7200 : 180;

    let windowStart = Number.parseInt(content.dataset.windowStart || '', 10);
    let windowEnd = Number.parseInt(content.dataset.windowEnd || '', 10);
    if (!Number.isFinite(windowStart) || !Number.isFinite(windowEnd)) {
      windowStart = rawSamples.length > 0 ? rawSamples[0].minute : 0;
      windowEnd = rawSamples.length > 0 ? rawSamples[rawSamples.length - 1].minute : 0;
    }

    const displaySamples = computeSmoothedSamples(rawSamples, smoothWindow, maxGap);
    const panels = [];

    for (const figure of content.querySelectorAll('figure.admin-panel')) {
      const svg = figure.querySelector('svg.admin-panel-svg');
      if (!svg) continue;
      const key = figure.dataset.panelKey || '';
      const unit = figure.dataset.unit || '';
      const rawThreshold = figure.dataset.threshold;
      const parsedThreshold =
        rawThreshold !== undefined && rawThreshold !== '' ? Number(rawThreshold) : Number.NaN;
      const threshold = Number.isFinite(parsedThreshold) ? parsedThreshold : null;
      const hitArea = svg.querySelector('.stats-hit-area');

      const panelObj = {
        figure,
        svg,
        key,
        unit,
        threshold,
        maximum: 1,
        hoverVal: figure.querySelector('[data-hover-val]'),
        statLast: figure.querySelector('dd[data-stat="last"]'),
        statAvg: figure.querySelector('dd[data-stat="avg"]'),
        statPeak: figure.querySelector('dd[data-stat="peak"]'),
        axisMax: figure.querySelector('[data-axis="max"]'),
        axisMid: figure.querySelector('[data-axis="mid"]'),
        xaxisStart: figure.querySelector('[data-xaxis="start"]'),
        xaxisMid: figure.querySelector('[data-xaxis="mid"]'),
        xaxisEnd: figure.querySelector('[data-xaxis="end"]'),
        thresholdEl: svg.querySelector('path.stats-threshold'),
        area: svg.querySelector('path.stats-area'),
        line: svg.querySelector('path.stats-line'),
        dots: svg.querySelector('g.stats-dots'),
        brush: svg.querySelector('rect.stats-brush'),
        crosshair: svg.querySelector('line.stats-crosshair'),
        hoverRing: svg.querySelector('circle.stats-hover-ring'),
        hoverDot: svg.querySelector('circle.stats-hover-dot'),
        hitArea
      };

      panelObj.maximum = computePanelMaximum(panelObj, displaySamples);
      panels.push(panelObj);

      if (hitArea) {
        bindHitAreaListeners(hitArea, svg);
      }
    }

    chartState = {
      content,
      rawSamples,
      displaySamples,
      windowStart,
      windowEnd,
      zoomStart: windowStart,
      zoomEnd: windowEnd,
      isZoomed: false,
      hourly,
      includeDate,
      maxGap,
      panels,
      inspector: content.querySelector('#stats-inspector'),
      inspectorHint: content.querySelector('#inspector-hint'),
      inspectorValues: content.querySelector('#inspector-values')
    };

    syncToolbarButtons();
    syncPinnedRowHighlight();

    if (smoothWindow > 1 && rawSamples.length > 0) {
      redrawAllPanels();
    }
  }

  // ── Live Refresh & Existing Account / Probe / Cookie Handlers ──────────────

  function preserveOpenDetails(currentMain, incomingMain) {
    const states = new Map();
    for (const details of currentMain.querySelectorAll('details[id]')) {
      states.set(details.id, details.open);
    }
    if (states.size === 0) return;
    for (const nextDetails of incomingMain.querySelectorAll('details[id]')) {
      if (states.has(nextDetails.id)) {
        nextDetails.open = states.get(nextDetails.id);
      }
    }
  }

  function syncDataset(target, source) {
    for (const key of Object.keys(target.dataset)) {
      if (!(key in source.dataset)) {
        delete target.dataset[key];
      }
    }
    for (const [key, value] of Object.entries(source.dataset)) {
      target.dataset[key] = value;
    }
  }

  async function refreshMain(url) {
    const targetUrl = url || window.location.href;
    const seq = ++refreshSequence;

    if (activeRefreshController) {
      activeRefreshController.abort();
    }
    const controller = new AbortController();
    activeRefreshController = controller;
    const timeoutId = window.setTimeout(() => controller.abort(), FETCH_TIMEOUT_MS);

    try {
      const response = await fetch(targetUrl, {
        credentials: 'same-origin',
        cache: 'no-store',
        headers: {
          'Accept': 'text/html'
        },
        signal: controller.signal
      });

      if (seq !== refreshSequence) return false;

      if (response.status === 401 || response.status === 403) {
        throw new Error(
          'Administrator authorization is required to refresh this view. Sign in again if your session expired.'
        );
      }
      if (!response.ok) {
        throw new Error(
          `Could not refresh view (HTTP ${response.status}). Displayed figures are from the last successful load.`
        );
      }

      const html = await response.text();
      if (seq !== refreshSequence) return false;

      const parsed = new DOMParser().parseFromString(html, 'text/html');
      const currentMain = getMain(document);
      const incomingMain = getMain(parsed);

      if (!currentMain || !incomingMain) {
        throw new Error('Could not locate main content container in refreshed response.');
      }

      preserveOpenDetails(currentMain, incomingMain);
      syncDataset(currentMain, incomingMain);
      currentMain.replaceChildren(...incomingMain.childNodes);

      const outerStatus = document.getElementById('stats-status');
      if (outerStatus && !currentMain.contains(outerStatus)) {
        outerStatus.textContent = '';
        setHidden(outerStatus, true);
      }

      if (parsed.title) {
        document.title = parsed.title;
      }

      initCharts();

      if (isDashboardPage()) {
        syncCountdownText();
      }

      return true;
    } catch (error) {
      if (controller.signal.aborted && seq !== refreshSequence) {
        return false;
      }
      const message =
        error instanceof Error && error.name !== 'AbortError'
          ? error.message
          : 'Refresh timed out before the server responded. Displayed figures are from the last successful load.';
      setStatusBanner(getErrorBanner(), message, 'error');
      return false;
    } finally {
      window.clearTimeout(timeoutId);
      if (activeRefreshController === controller) {
        activeRefreshController = null;
      }
    }
  }

  async function runDashboardRefresh() {
    if (dashboardRefreshing) return;
    dashboardRefreshing = true;
    remaining = REFRESH_INTERVAL_SECONDS;
    syncCountdownText();

    const refreshBtn = document.getElementById('stats-refresh-now');
    if (refreshBtn) {
      refreshBtn.disabled = true;
    }

    try {
      await refreshMain(window.location.href);
    } finally {
      dashboardRefreshing = false;
      const latestBtn = document.getElementById('stats-refresh-now');
      if (latestBtn) {
        latestBtn.disabled = false;
      }
      syncCountdownText();
    }
  }

  function snapshotAccountButtons() {
    const buttons = document.querySelectorAll(
      '#run-probe, #move-pool-search, #move-pool-view, [data-page="account"] button[data-pool], .admin-actions-card button[data-pool]'
    );
    const priorDisabled = new Map();
    for (const btn of buttons) {
      priorDisabled.set(btn, btn.disabled);
      btn.disabled = true;
    }
    return priorDisabled;
  }

  function restoreAccountButtons(priorDisabled) {
    for (const [btn, wasDisabled] of priorDisabled.entries()) {
      if (btn.isConnected) {
        btn.disabled = wasDisabled;
      }
    }
  }

  function formatProbeSummary(result) {
    const verdict = result && typeof result.verdict === 'string' ? result.verdict : 'error';
    const beforePool = result && result.before && result.before.pool ? result.before.pool : 'unknown';
    const afterPool = result && result.after && result.after.pool ? result.after.pool : beforePool;
    const reinstatement =
      result && result.reinstated
        ? 'reinstated — stored verdict cleared'
        : 'stored verdict unchanged';
    let summary = `Probe verdict: ${verdict} (${beforePool} -> ${afterPool}; ${reinstatement}).`;
    if (result && typeof result.detail === 'string' && result.detail.trim() !== '') {
      summary += ` ${result.detail.trim()}`;
    }
    return summary;
  }

  function probeTone(verdict) {
    if (verdict === 'alive') return 'ok';
    if (verdict === 'settings refused') return 'warn';
    return 'error';
  }

  async function readErrorMessage(response, fallbackMessage) {
    try {
      const text = (await response.text()).trim();
      if (text && !text.startsWith('<!DOCTYPE') && !text.startsWith('<html')) {
        return text;
      }
    } catch {
      // Fall back to status-derived message below.
    }
    if (response.status === 401 || response.status === 403) {
      return 'Administrator authorization is required for this action.';
    }
    if (response.status === 404) {
      return 'This account is not in the active runtime pool.';
    }
    return fallbackMessage;
  }

  async function handleRunProbe(button) {
    if (accountActionInFlight) return;
    const id = resolveSessionId(button);
    if (!id) {
      setStatusBanner(getAccountStatusElement(), 'Could not determine account session ID.', 'error');
      return;
    }

    accountActionInFlight = true;
    const priorDisabled = snapshotAccountButtons();
    setStatusBanner(getAccountStatusElement(), 'Running live probe against X...', 'warn');

    try {
      const response = await fetch(`/admin/stats/sessions/${encodeURIComponent(id)}/probe`, {
        method: 'POST',
        credentials: 'same-origin',
        cache: 'no-store',
        headers: {
          'Accept': 'application/json'
        }
      });

      if (!response.ok) {
        const errMsg = await readErrorMessage(response, `Probe failed (HTTP ${response.status}).`);
        throw new Error(errMsg);
      }

      const result = await response.json();
      const message = formatProbeSummary(result);
      const tone = probeTone(result.verdict);

      setStatusBanner(getAccountStatusElement(), message, tone);
      await refreshMain(window.location.href);
      setStatusBanner(getAccountStatusElement(), message, tone);
    } catch (error) {
      const message = error instanceof Error ? error.message : 'Live probe could not run.';
      setStatusBanner(getAccountStatusElement(), message, 'error');
    } finally {
      restoreAccountButtons(priorDisabled);
      accountActionInFlight = false;
    }
  }

  async function handleMovePool(button) {
    if (accountActionInFlight) return;
    const id = resolveSessionId(button);
    const targetPool =
      button.dataset.pool ||
      (button.id === 'move-pool-search' ? 'search' : button.id === 'move-pool-view' ? 'view' : '');

    if (!id || (targetPool !== 'search' && targetPool !== 'view')) {
      setStatusBanner(getAccountStatusElement(), 'Invalid target pool or session ID.', 'error');
      return;
    }

    accountActionInFlight = true;
    const priorDisabled = snapshotAccountButtons();
    setStatusBanner(getAccountStatusElement(), `Moving account to the ${targetPool} pool...`, 'warn');

    try {
      const response = await fetch(`/admin/stats/sessions/${encodeURIComponent(id)}/pool`, {
        method: 'POST',
        credentials: 'same-origin',
        cache: 'no-store',
        headers: {
          'Content-Type': 'application/json',
          'Accept': 'application/json'
        },
        body: JSON.stringify({ pool: targetPool })
      });

      if (!response.ok) {
        const errMsg = await readErrorMessage(response, `Could not move account pool (HTTP ${response.status}).`);
        throw new Error(errMsg);
      }

      const result = await response.json();
      const finalPool = result && result.pool ? result.pool : targetPool;
      const message = `Account moved to the ${finalPool} pool.`;

      await refreshMain(window.location.href);
      setStatusBanner(getAccountStatusElement(), message, 'ok');
    } catch (error) {
      const message = error instanceof Error ? error.message : 'Could not move account pool.';
      setStatusBanner(getAccountStatusElement(), message, 'error');
    } finally {
      restoreAccountButtons(priorDisabled);
      accountActionInFlight = false;
    }
  }

  async function handleInvestigationRefresh(button) {
    button.disabled = true;
    const uri = button.dataset.uri || window.location.href;
    try {
      await refreshMain(uri);
    } finally {
      if (button.isConnected) {
        button.disabled = false;
      }
      const freshBtn = document.getElementById('investigation-refresh');
      if (freshBtn) {
        freshBtn.disabled = false;
      }
    }
  }

  async function handleConfirmCookies(button) {
    const id = resolveSessionId(button);
    const confirmation = document.getElementById('cookie-confirmation');
    const output = document.getElementById('cookie-export');
    const text = document.getElementById('cookie-json');
    const status = document.getElementById('cookie-status');
    const reveal = document.getElementById('show-cookies');
    if (!id || !output || !text || !status) return;

    button.disabled = true;
    status.textContent = 'Loading current cookies...';
    setHidden(status, false);

    try {
      const response = await fetch(`/admin/stats/sessions/${encodeURIComponent(id)}/cookies`, {
        credentials: 'same-origin',
        cache: 'no-store',
        headers: {
          'Accept': 'application/json'
        }
      });
      if (!response.ok) {
        const errMsg = await readErrorMessage(
          response,
          'Cookie export is unavailable for this account.'
        );
        throw new Error(errMsg);
      }
      const cookies = await response.json();
      text.value = JSON.stringify(cookies, null, 2);
      if (confirmation) setHidden(confirmation, true);
      if (reveal) setHidden(reveal, true);
      setHidden(output, false);
      status.textContent = 'Credentials revealed. Keep them private.';
    } catch (error) {
      status.textContent = error instanceof Error ? error.message : 'Cookie export failed.';
    } finally {
      if (button.isConnected) {
        button.disabled = false;
      }
    }
  }

  async function handleCopyCookies() {
    const text = document.getElementById('cookie-json');
    const status = document.getElementById('cookie-status');
    if (!text || !status) return;
    setHidden(status, false);
    try {
      await navigator.clipboard.writeText(text.value);
      status.textContent = 'Cookie JSON copied.';
    } catch {
      text.focus();
      text.select();
      status.textContent = 'Clipboard unavailable. The JSON is selected; copy it manually.';
    }
  }

  // Fallback window listeners if a brush drag continues outside hitArea without pointer capture.
  window.addEventListener('pointermove', (event) => {
    if (!dragState || dragState.pointerId !== event.pointerId) return;
    const dxPx = Math.abs(event.clientX - dragState.startClientX);
    if (dxPx >= BRUSH_MIN_DRAG_PX) {
      dragState.brushing = true;
    }
    scheduleHoverUpdate(dragState.svg, event.clientX);
  });

  window.addEventListener('pointerup', (event) => {
    if (!dragState || dragState.pointerId !== event.pointerId) return;
    finishPointerGesture(event, false);
    if (!event.target || !(event.target instanceof Element) || !event.target.closest('.stats-hit-area')) {
      pointerOverCharts = false;
      hoverMinute = null;
      if (pinnedMinute === null) {
        hideCrosshairAndHover();
      }
      syncCountdownText();
    }
  });

  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape' && pinnedMinute !== null) {
      unpinSample();
    }
  });

  // Event delegation on document so all controls survive live HTML swaps.
  document.addEventListener('click', (event) => {
    const target = event.target instanceof Element ? event.target : null;
    if (!target) return;

    const smoothBtn = target.closest('#chart-mode-raw, #chart-mode-smooth');
    if (smoothBtn) {
      event.preventDefault();
      const requested = Number.parseInt(smoothBtn.dataset.smooth || '1', 10);
      setSmoothMode(requested === 3 ? 3 : 1);
      return;
    }

    const unpinBtn = target.closest('#chart-unpin');
    if (unpinBtn) {
      event.preventDefault();
      unpinSample();
      return;
    }

    const resetZoomBtn = target.closest('#chart-reset-zoom');
    if (resetZoomBtn) {
      event.preventDefault();
      resetZoom();
      return;
    }

    const refreshNowBtn = target.closest('#stats-refresh-now');
    if (refreshNowBtn) {
      event.preventDefault();
      void runDashboardRefresh();
      return;
    }

    const investigationBtn = target.closest('#investigation-refresh');
    if (investigationBtn) {
      event.preventDefault();
      void handleInvestigationRefresh(investigationBtn);
      return;
    }

    const probeBtn = target.closest('#run-probe');
    if (probeBtn) {
      event.preventDefault();
      void handleRunProbe(probeBtn);
      return;
    }

    const poolBtn = target.closest('#move-pool-search, #move-pool-view, button[data-pool]');
    if (poolBtn) {
      event.preventDefault();
      void handleMovePool(poolBtn);
      return;
    }

    const showCookiesBtn = target.closest('#show-cookies');
    if (showCookiesBtn) {
      event.preventDefault();
      const confirmation = document.getElementById('cookie-confirmation');
      const confirmBtn = document.getElementById('confirm-cookies');
      if (confirmation) {
        setHidden(confirmation, false);
      }
      if (confirmBtn) {
        confirmBtn.focus();
      }
      return;
    }

    const confirmCookiesBtn = target.closest('#confirm-cookies');
    if (confirmCookiesBtn) {
      event.preventDefault();
      void handleConfirmCookies(confirmCookiesBtn);
      return;
    }

    const copyCookiesBtn = target.closest('#copy-cookies');
    if (copyCookiesBtn) {
      event.preventDefault();
      void handleCopyCookies();
    }
  });

  // Dashboard 30s countdown & visibility-aware auto-refresh.
  window.setInterval(() => {
    if (!isDashboardPage() || document.hidden || dashboardRefreshing) return;
    if (isRefreshPaused()) {
      syncCountdownText();
      return;
    }
    remaining -= 1;
    if (remaining <= 0) {
      void runDashboardRefresh();
    } else {
      syncCountdownText();
    }
  }, 1000);

  document.addEventListener('visibilitychange', () => {
    if (!isDashboardPage() || document.hidden) return;
    if (isRefreshPaused()) {
      syncCountdownText();
      return;
    }
    if (remaining <= 5) {
      void runDashboardRefresh();
    } else {
      syncCountdownText();
    }
  });

  initCharts();

  if (isDashboardPage()) {
    syncCountdownText();
  }
})();
