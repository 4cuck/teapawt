(() => {
  'use strict';

  const main = document.getElementById('admin-stats-main');
  if (!main || main.dataset.page !== 'fingerprints') return;

  const LIST_URL = '/admin/stats/captures/data';
  const ITEM_URL = (id) => '/admin/stats/captures/' + encodeURIComponent(id);

  const state = {
    summaries: [],
    filtered: [],
    cache: new Map(),
    selectedId: null,
    activeTab: 'overview',
    compareOpen: false,
    compareA: '',
    compareB: '',
    diffOnly: true,
    deleteArmedId: null,
    fontFilter: 'all',
    fontSearch: '',
    tapeOp: 'all',
    tapeSearch: '',
    rawFullBase64: false,
  };

  const els = {
    status: document.getElementById('fp-status'),
    kpiTotal: document.getElementById('fp-kpi-total'),
    kpiShown: document.getElementById('fp-kpi-shown'),
    kpiBrowsers: document.getElementById('fp-kpi-browsers'),
    kpiPlatforms: document.getElementById('fp-kpi-platforms'),
    kpiSdks: document.getElementById('fp-kpi-sdks'),
    kpiKeyids: document.getElementById('fp-kpi-keyids'),
    kpiToken: document.getElementById('fp-kpi-token'),
    kpiLatest: document.getElementById('fp-kpi-latest'),
    copyCaptureUrl: document.getElementById('fp-copy-capture-url'),
    compareToggle: document.getElementById('fp-compare-toggle'),
    refresh: document.getElementById('fp-refresh'),
    comparePanel: document.getElementById('fp-compare-panel'),
    compareBody: document.getElementById('fp-compare-body'),
    compareSwap: document.getElementById('fp-compare-swap'),
    compareClose: document.getElementById('fp-compare-close'),
    diffOnly: document.getElementById('fp-diff-only'),
    selectA: document.getElementById('fp-select-a'),
    selectB: document.getElementById('fp-select-b'),
    filterForm: document.getElementById('fp-filter-form'),
    filterQ: document.getElementById('fp-filter-q'),
    filterBrowser: document.getElementById('fp-filter-browser'),
    filterOs: document.getElementById('fp-filter-os'),
    filterSdk: document.getElementById('fp-filter-sdk'),
    filterSort: document.getElementById('fp-filter-sort'),
    filterClear: document.getElementById('fp-filter-clear'),
    tableCount: document.getElementById('fp-table-count'),
    listRows: document.getElementById('fp-list-rows'),
    inspector: document.getElementById('fp-inspector'),
    inspectorTitle: document.getElementById('fp-inspector-title'),
    inspectorSub: document.getElementById('fp-inspector-sub'),
    copyToken: document.getElementById('fp-copy-token'),
    copyJson: document.getElementById('fp-copy-json'),
    downloadJson: document.getElementById('fp-download-json'),
    setA: document.getElementById('fp-set-a'),
    setB: document.getElementById('fp-set-b'),
    delBtn: document.getElementById('fp-delete'),
    tabCountFonts: document.getElementById('fp-tab-count-fonts'),
    tabCountCanvas: document.getElementById('fp-tab-count-canvas'),
    tabCountTape: document.getElementById('fp-tab-count-tape'),
    panels: {
      overview: document.getElementById('fp-tab-overview'),
      fonts: document.getElementById('fp-tab-fonts'),
      canvas: document.getElementById('fp-tab-canvas'),
      headers: document.getElementById('fp-tab-headers'),
      tape: document.getElementById('fp-tab-tape'),
      raw: document.getElementById('fp-tab-raw'),
    },
  };

  function el(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = String(text);
    return node;
  }

  function showStatus(msg, kind) {
    if (!els.status) return;
    if (!msg) {
      els.status.hidden = true;
      els.status.textContent = '';
      return;
    }
    els.status.hidden = false;
    els.status.className = 'admin-notice' + (kind ? ' admin-notice--' + kind : '');
    els.status.textContent = msg;
  }

  function copyText(text, btn, doneLabel) {
    if (!navigator.clipboard || !text) return;
    navigator.clipboard.writeText(text).then(() => {
      if (!btn) return;
      const prev = btn.textContent;
      btn.textContent = doneLabel || 'Copied!';
      window.setTimeout(() => {
        btn.textContent = prev;
      }, 1500);
    }).catch(() => {
      showStatus('Clipboard write was blocked by the browser.', 'warn');
    });
  }

  function fnvString(str) {
    let hash = 2166136261;
    const s = String(str || '');
    for (let i = 0; i < s.length; i += 1) {
      hash ^= s.charCodeAt(i) & 0xff;
      hash = Math.imul(hash, 16777619);
    }
    return (hash >>> 0).toString(16).padStart(8, '0');
  }

  function decodeCastleEnvelope(token) {
    const text = String(token || '');
    const bar = text.indexOf('|');
    const keyid = bar < 0 ? null : text.slice(0, bar);
    const payload = bar < 0 ? text : text.slice(bar + 1);
    let bytes = new Uint8Array(0);
    let decodeError = null;
    try {
      const normalized = payload.replace(/-/g, '+').replace(/_/g, '/');
      const pad = normalized.length % 4 ? '='.repeat(4 - (normalized.length % 4)) : '';
      const raw = atob(normalized + pad);
      bytes = new Uint8Array(raw.length);
      for (let i = 0; i < raw.length; i += 1) bytes[i] = raw.charCodeAt(i);
    } catch (err) {
      decodeError = String(err && err.message ? err.message : err);
    }
    const fields = [];
    let run = '';
    let start = 0;
    const flush = () => {
      if (run.length >= 8) fields.push({ offset: start, text: run });
      run = '';
    };
    for (let i = 0; i < bytes.length; i += 1) {
      const b = bytes[i];
      const isB64 =
        (b >= 48 && b <= 57) ||
        (b >= 65 && b <= 90) ||
        (b >= 97 && b <= 122) ||
        b === 43 ||
        b === 47 ||
        b === 61;
      if (isB64) {
        if (!run) start = i;
        run += String.fromCharCode(b);
      } else {
        flush();
      }
    }
    flush();
    return {
      keyid,
      rawLength: text.length,
      bytes: bytes.length,
      tag: bytes.length ? bytes[0] : null,
      fields,
      decodeError,
    };
  }

  function analyzeFonts(events) {
    const bases = {};
    const families = new Map();
    const otherMeasures = [];
    for (let i = 0; i < events.length; i += 1) {
      const ev = events[i];
      if (ev.op !== 'measureText') continue;
      if (ev.text !== 'mmmmmmmmmmlli') {
        otherMeasures.push(ev);
        continue;
      }
      const font = String(ev.font || '');
      const baseMatch = font.match(/^100px ([^,]+)$/);
      if (baseMatch) {
        if (bases[baseMatch[1]] === undefined) bases[baseMatch[1]] = ev.width;
        continue;
      }
      const probeMatch = font.match(/^100px (.+), (monospace|sans-serif|serif)$/);
      if (!probeMatch) {
        otherMeasures.push(ev);
        continue;
      }
      const fam = probeMatch[1].replace(/^['"]|['"]$/g, '');
      const fallback = probeMatch[2];
      let entry = families.get(fam);
      if (!entry) {
        entry = {
          family: fam,
          widths: {},
          diffs: {},
          installed: false,
          metrics: null,
        };
        families.set(fam, entry);
      }
      entry.widths[fallback] = ev.width;
      const base = bases[fallback];
      const differs = typeof base === 'number' && Math.abs(ev.width - base) >= 0.05;
      entry.diffs[fallback] = differs;
      if (differs) {
        entry.installed = true;
        if (!entry.metrics) entry.metrics = ev;
      }
      if (!entry.metrics) entry.metrics = ev;
    }
    const list = Array.from(families.values());
    const installedCount = list.filter((f) => f.installed).length;
    return {
      bases,
      list,
      installedCount,
      missingCount: list.length - installedCount,
      otherMeasures,
    };
  }

  function formatUtc(value) {
    if (!value) return '—';
    const text = String(value);
    const m = text.match(/^(\d{4}-\d{2}-\d{2})\s+(\d{1,2}):(\d{2}):(\d{2})/);
    if (m) {
      return m[1] + ' ' + m[2].padStart(2, '0') + ':' + m[3] + ':' + m[4] + 'Z';
    }
    return text.slice(0, 19);
  }

  function shortGpu(gpu) {
    if (!gpu) return '—';
    const m = String(gpu).match(/ANGLE\s*\([^,]+,\s*([^(,]+)/);
    if (m) return m[1].trim();
    return String(gpu).length > 38 ? String(gpu).slice(0, 37) + '…' : String(gpu);
  }

  function browserLabel(row) {
    const parts = [row.browser || 'Unknown'];
    if (row.fullVersion) {
      parts.push(row.fullVersion);
    } else if (row.userAgent) {
      const m = row.userAgent.match(/(?:Firefox|Chrome|Edg|Version)\/([0-9.]+)/);
      if (m) parts.push(m[1]);
    }
    return parts.join(' ');
  }

  function osLabel(row) {
    const base = row.os || row.platform || 'Unknown';
    if (row.platformVersion) return base + ' (' + row.platformVersion + ')';
    return base;
  }

  function optionCaption(row) {
    const parts = [row.id.slice(0, 16), browserLabel(row), osLabel(row)];
    if (row.label) parts.push('"' + row.label + '"');
    return parts.join(' · ');
  }

  function populateSdkFilter() {
    const prev = els.filterSdk.value;
    const counts = new Map();
    for (const row of state.summaries) {
      const sdk = row.sdk || 'unknown';
      counts.set(sdk, (counts.get(sdk) || 0) + 1);
    }
    els.filterSdk.textContent = '';
    els.filterSdk.appendChild(el('option', '', 'All SDK builds'));
    els.filterSdk.firstChild.value = '';
    for (const [sdk, count] of counts.entries()) {
      const opt = el('option', '', (sdk.length > 12 ? sdk.slice(0, 12) : sdk) + ' (' + count + ')');
      opt.value = sdk;
      els.filterSdk.appendChild(opt);
    }
    els.filterSdk.value = counts.has(prev) ? prev : '';
  }

  function populateComparePickers() {
    for (const [select, key] of [[els.selectA, 'compareA'], [els.selectB, 'compareB']]) {
      const prev = state[key];
      select.textContent = '';
      const blank = el('option', '', key === 'compareA' ? 'Choose capture A…' : 'Choose capture B…');
      blank.value = '';
      select.appendChild(blank);
      for (const row of state.summaries) {
        const opt = el('option', '', optionCaption(row));
        opt.value = row.id;
        select.appendChild(opt);
      }
      select.value = state.summaries.some((r) => r.id === prev) ? prev : '';
      state[key] = select.value;
    }
  }

  function updateKpis() {
    const all = state.summaries;
    const shown = state.filtered;
    els.kpiTotal.textContent = String(all.length);
    els.kpiShown.textContent =
      shown.length === all.length
        ? 'Showing all ' + all.length
        : 'Filtered: ' + shown.length + ' of ' + all.length;

    const browsers = new Map();
    const platforms = new Map();
    const sdks = new Set();
    const keyids = new Set();
    let tokenSum = 0;

    for (const row of all) {
      const b = row.browser || 'Other';
      browsers.set(b, (browsers.get(b) || 0) + 1);
      const os = row.os || 'Other';
      platforms.set(os, (platforms.get(os) || 0) + 1);
      if (row.sdk) sdks.add(row.sdk);
      if (row.keyid) keyids.add(row.keyid);
      tokenSum += Number(row.tokenLength || 0);
    }

    const fmtTop = (map) =>
      Array.from(map.entries())
        .sort((a, b) => b[1] - a[1])
        .slice(0, 3)
        .map(([k, v]) => k + ' ' + v)
        .join(' · ') || '—';

    els.kpiBrowsers.textContent = String(browsers.size);
    els.kpiPlatforms.textContent = fmtTop(browsers) + ' / ' + fmtTop(platforms);
    els.kpiSdks.textContent = String(sdks.size);
    els.kpiKeyids.textContent =
      keyids.size > 0 ? 'Keys: ' + Array.from(keyids).slice(0, 3).join(', ') : 'No keyid yet';
    els.kpiToken.textContent = all.length ? Math.round(tokenSum / all.length).toLocaleString() + ' B' : '—';
    els.kpiLatest.textContent = all.length ? 'Latest: ' + formatUtc(all[0].receivedAt) : 'No submissions yet';
  }

  function applyFilters() {
    const q = els.filterQ.value.trim().toLowerCase();
    const browser = els.filterBrowser.value;
    const os = els.filterOs.value;
    const sdk = els.filterSdk.value;
    const sort = els.filterSort.value;

    const out = state.summaries.filter((row) => {
      if (browser && row.browser !== browser) return false;
      if (os && row.os !== os) return false;
      if (sdk && (row.sdk || 'unknown') !== sdk) return false;
      if (q) {
        const hay = [
          row.id,
          row.label,
          row.ip,
          row.userAgent,
          row.browser,
          row.os,
          row.fullVersion,
          row.platformVersion,
          row.webglRenderer,
          row.timeZone,
          row.sdk,
          row.keyid,
          row.cuid,
          row.screen,
        ]
          .filter(Boolean)
          .join(' ')
          .toLowerCase();
        if (!hay.includes(q)) return false;
      }
      return true;
    });

    out.sort((a, b) => {
      if (sort === 'oldest') return String(a.id).localeCompare(String(b.id));
      if (sort === 'token_desc') return (b.tokenLength || 0) - (a.tokenLength || 0);
      if (sort === 'token_asc') return (a.tokenLength || 0) - (b.tokenLength || 0);
      if (sort === 'fonts_desc') return (b.fontsInstalled || 0) - (a.fontsInstalled || 0);
      return String(b.id).localeCompare(String(a.id));
    });

    state.filtered = out;
    updateKpis();
    renderTable();
  }

  function renderTable() {
    els.tableCount.textContent =
      state.filtered.length + ' capture' + (state.filtered.length === 1 ? '' : 's');
    els.listRows.textContent = '';

    if (state.filtered.length === 0) {
      const tr = el('tr');
      const td = el(
        'td',
        'admin-empty',
        state.summaries.length === 0
          ? 'No submissions yet. Share /admin/fingerprint with a browser you want to record.'
          : 'No submissions match these filters.'
      );
      td.colSpan = 7;
      tr.appendChild(td);
      els.listRows.appendChild(tr);
      return;
    }

    for (const row of state.filtered) {
      const tr = el('tr', 'fp-row' + (row.id === state.selectedId ? ' is-selected' : ''));
      tr.dataset.captureId = row.id;

      // Time + ID + label
      const tdTime = el('td', 'admin-col-time');
      tdTime.appendChild(el('div', 'fp-time-main', formatUtc(row.receivedAt)));
      tdTime.appendChild(el('code', 'fp-id-code', row.id));
      if (row.label) {
        const note = el('div', 'fp-note-tag', row.label);
        tdTime.appendChild(note);
      }
      tr.appendChild(tdTime);

      // Browser & OS
      const tdBrowser = el('td');
      const bWrap = el('div', 'fp-badge-row');
      const bClass =
        row.browser === 'Chrome'
          ? 'admin-badge admin-badge--search'
          : row.browser === 'Firefox'
            ? 'admin-badge admin-badge--warn'
            : row.browser === 'Safari'
              ? 'admin-badge admin-badge--view'
              : 'admin-badge admin-badge--neutral';
      bWrap.appendChild(el('span', bClass, browserLabel(row)));
      bWrap.appendChild(el('span', 'admin-badge admin-badge--neutral', osLabel(row)));
      if (row.hasHighEntropy) {
        bWrap.appendChild(el('span', 'admin-badge admin-badge--ok', 'CH-HE'));
      }
      tdBrowser.appendChild(bWrap);
      tr.appendChild(tdBrowser);

      // Display & GPU
      const tdHw = el('td');
      const hwBits = [];
      if (row.screen) hwBits.push(row.screen);
      if (row.cores) hwBits.push(row.cores + 'c');
      if (row.memory) hwBits.push(row.memory + 'GB');
      tdHw.appendChild(el('div', 'fp-hw-top', hwBits.join(' · ') || '—'));
      tdHw.appendChild(el('div', 'admin-muted fp-gpu-sub', shortGpu(row.webglRenderer)));
      tr.appendChild(tdHw);

      // Token
      const tdTok = el('td', 'admin-nowrap');
      tdTok.appendChild(el('strong', '', Number(row.tokenLength || 0).toLocaleString() + ' B'));
      const tokMeta = el('div', 'admin-muted fp-mono-sub');
      tokMeta.textContent = (row.keyid || 'no-key') + (row.elapsedMs ? ' · ' + row.elapsedMs + 'ms' : '');
      tdTok.appendChild(tokMeta);
      if (row.interactionCount) {
        const inter = el('div', 'admin-muted fp-mono-sub');
        inter.textContent =
          '⟲ ' + row.interactionCount + ' interaction' + (row.interactionMs ? ' · ' + (row.interactionMs / 1000).toFixed(1) + 's' : '');
        tdTok.appendChild(inter);
      }
      tr.appendChild(tdTok);

      // Fonts & canvas
      const tdFonts = el('td', 'admin-nowrap');
      tdFonts.appendChild(
        el(
          'div',
          '',
          row.fontsTotal > 0 ? row.fontsInstalled + ' / ' + row.fontsTotal + ' fonts' : 'No font probe'
        )
      );
      tdFonts.appendChild(
        el(
          'div',
          'admin-muted fp-mono-sub',
          (row.canvasCount || 0) + ' PNG · ' + (row.imageDataCount || 0) + ' imgData'
        )
      );
      tr.appendChild(tdFonts);

      // Timezone & IP
      const tdNet = el('td');
      tdNet.appendChild(el('div', '', row.timeZone || '—'));
      tdNet.appendChild(el('div', 'admin-muted fp-mono-sub', row.ip || '—'));
      tr.appendChild(tdNet);

      // Compare A/B slots
      const tdDiff = el('td', 'admin-nowrap');
      const diffActions = el('div', 'fp-diff-slot-btns');
      const btnA = el('button', 'admin-btn admin-btn--sm' + (state.compareA === row.id ? ' fp-slot-active' : ''), 'A');
      btnA.type = 'button';
      btnA.dataset.diffSlot = 'A';
      btnA.dataset.captureId = row.id;
      const btnB = el('button', 'admin-btn admin-btn--sm' + (state.compareB === row.id ? ' fp-slot-active' : ''), 'B');
      btnB.type = 'button';
      btnB.dataset.diffSlot = 'B';
      btnB.dataset.captureId = row.id;
      diffActions.appendChild(btnA);
      diffActions.appendChild(btnB);
      tdDiff.appendChild(diffActions);
      tr.appendChild(tdDiff);

      els.listRows.appendChild(tr);
    }
  }

  async function fetchRecord(id) {
    if (state.cache.has(id)) return state.cache.get(id);
    const response = await fetch(ITEM_URL(id), { credentials: 'same-origin' });
    if (!response.ok) {
      throw new Error('Could not load capture ' + id + ' (HTTP ' + response.status + ')');
    }
    const record = await response.json();
    state.cache.set(id, record);
    return record;
  }

  async function selectCapture(id) {
    if (!id) return;
    state.selectedId = id;
    state.deleteArmedId = null;
    els.delBtn.textContent = 'Delete';
    renderTable();

    els.inspector.hidden = false;
    els.inspectorTitle.textContent = id;
    els.inspectorSub.textContent = 'Loading full capture…';
    try {
      const record = await fetchRecord(id);
      if (state.selectedId !== id) return;
      renderInspector(record);
      try {
        window.history.replaceState(null, '', '#id=' + encodeURIComponent(id));
      } catch (_) {}
    } catch (err) {
      showStatus(String(err && err.message ? err.message : err), 'bad');
    }
  }

  function makeKvGrid(pairs) {
    const dl = el('dl', 'fp-kv-grid');
    for (const [k, v] of pairs) {
      const row = el('div', 'fp-kv-item');
      row.appendChild(el('dt', '', k));
      const dd = el('dd');
      if (v instanceof Node) dd.appendChild(v);
      else dd.textContent = v === undefined || v === null || v === '' ? '—' : String(v);
      row.appendChild(dd);
      dl.appendChild(row);
    }
    return dl;
  }

  function renderOverviewTab(record, env, fontStats) {
    const panel = els.panels.overview;
    panel.textContent = '';

    const sub = record.submission || {};
    const b = sub.browser || {};
    const scr = b.screen || {};
    const win = b.window || {};
    const gl = b.webgl || {};

    // Castle outer envelope card
    const cardToken = el('div', 'fp-card');
    cardToken.appendChild(el('h3', '', 'Castle outer envelope'));
    cardToken.appendChild(
      makeKvGrid([
        ['Key ID', env.keyid || '—'],
        ['Token length', env.rawLength.toLocaleString() + ' chars'],
        ['Decoded payload', env.bytes.toLocaleString() + ' bytes'],
        ['Tag byte', env.tag !== null ? env.tag + ' (0x' + env.tag.toString(16).padStart(2, '0') + ')' : '—'],
        ['CUID cookie', sub.cuid || 'none'],
        ['SDK SHA-256', record.sdk || 'unknown'],
        ['Mint elapsed', typeof sub.elapsedMs === 'number' ? sub.elapsedMs + ' ms' : '—'],
        ['Font probe verdict', fontStats.installedCount + ' installed / ' + fontStats.list.length + ' probed'],
      ])
    );

    if (env.fields.length > 0) {
      const fieldsHeading = el('h4', 'fp-subhead', 'Readable base64 runs in decoded payload');
      cardToken.appendChild(fieldsHeading);
      const tblWrap = el('div', 'admin-table-wrap');
      const tbl = el('table', 'admin-table fp-compact-table');
      const thead = el('thead');
      const htr = el('tr');
      htr.appendChild(el('th', '', 'Offset'));
      htr.appendChild(el('th', '', 'Length'));
      htr.appendChild(el('th', '', 'Text'));
      thead.appendChild(htr);
      tbl.appendChild(thead);
      const tbody = el('tbody');
      for (const f of env.fields) {
        const tr = el('tr');
        tr.appendChild(el('td', 'fp-mono', String(f.offset)));
        tr.appendChild(el('td', 'fp-mono', String(f.text.length)));
        const td = el('td');
        td.appendChild(el('code', 'fp-break', f.text));
        tr.appendChild(td);
        tbody.appendChild(tr);
      }
      tbl.appendChild(tbody);
      tblWrap.appendChild(tbl);
      cardToken.appendChild(tblWrap);
    }

    const tokDetails = el('details', 'fp-details');
    tokDetails.appendChild(el('summary', '', 'Raw $castle_token (' + env.rawLength + ' chars)'));
    tokDetails.appendChild(el('pre', 'fp-pre', sub.token || ''));
    cardToken.appendChild(tokDetails);
    panel.appendChild(cardToken);

    // Environment grid (2 columns)
    const grid = el('div', 'fp-two-col');

    const navCard = el('div', 'fp-card');
    navCard.appendChild(el('h3', '', 'Navigator & runtime'));
    navCard.appendChild(
      makeKvGrid([
        ['User-Agent', b.userAgent],
        ['Platform / Vendor', (b.platform || '—') + ' · ' + (b.vendor || '—')],
        ['Languages', Array.isArray(b.languages) ? b.languages.join(', ') : b.language],
        ['Cores / Memory', (b.hardwareConcurrency ?? '—') + ' threads · ' + (b.deviceMemory ?? '—') + ' GB'],
        ['Touch / PDF / WebDriver', [
          'touch=' + (b.maxTouchPoints ?? '—'),
          'pdf=' + (b.pdfViewerEnabled ?? '—'),
          'webdriver=' + (b.webdriver ?? '—'),
        ].join(' · ')],
        ['WebRTC / Chrome obj', [
          'RTC=' + (b.hasWebRTC === undefined ? '—' : String(b.hasWebRTC)),
          'window.chrome=' + (b.hasChromeObject === undefined ? '—' : String(b.hasChromeObject)),
        ].join(' · ')],
        ['DNT / GPC', (b.doNotTrack ?? 'null') + ' / ' + (b.globalPrivacyControl ?? 'null')],
        ['Plugins', Array.isArray(b.plugins) ? (b.plugins.map((p) => p.name).join(', ') || '0') : '—'],
      ])
    );
    grid.appendChild(navCard);

    const hwCard = el('div', 'fp-card');
    hwCard.appendChild(el('h3', '', 'Display, locale & WebGL'));
    hwCard.appendChild(
      makeKvGrid([
        ['Screen', scr.width ? scr.width + '×' + scr.height + ' (avail ' + scr.availWidth + '×' + scr.availHeight + ')' : '—'],
        ['Window inner / outer', win.innerWidth ? win.innerWidth + '×' + win.innerHeight + ' / ' + win.outerWidth + '×' + win.outerHeight : '—'],
        ['DPR / Color depth', (win.devicePixelRatio ?? '—') + 'x · ' + (scr.colorDepth ?? '—') + '-bit'],
        ['Timezone / Offset', (b.timeZone || '—') + ' (UTC offset ' + (b.timezoneOffset ?? '—') + 'm)'],
        ['Intl locale / calendar', b.intl ? [b.intl.locale, b.intl.calendar, b.intl.numberingSystem].filter(Boolean).join(' · ') : '—'],
        ['WebGL vendor / renderer', (gl.vendor || '—') + ' · ' + (gl.renderer || '—')],
        ['Unmasked vendor', gl.unmaskedVendor || '—'],
        ['Unmasked renderer', gl.unmaskedRenderer || '—'],
      ])
    );
    grid.appendChild(hwCard);

    panel.appendChild(grid);
  }

  function renderFontsTab(fontStats) {
    const panel = els.panels.fonts;
    panel.textContent = '';

    // Baselines bar
    const baseCard = el('div', 'fp-card');
    const baseHead = el('div', 'fp-card-head');
    baseHead.appendChild(el('h3', '', '100px "mmmmmmmmmmlli" fallback baselines'));
    const pills = el('div', 'fp-badge-row');
    pills.appendChild(el('span', 'admin-badge admin-badge--ok', 'Installed: ' + fontStats.installedCount));
    pills.appendChild(el('span', 'admin-badge admin-badge--neutral', 'Missing: ' + fontStats.missingCount));
    pills.appendChild(el('span', 'admin-badge admin-badge--search', 'Total: ' + fontStats.list.length));
    baseHead.appendChild(pills);
    baseCard.appendChild(baseHead);

    baseCard.appendChild(
      makeKvGrid([
        ['100px monospace', typeof fontStats.bases.monospace === 'number' ? fontStats.bases.monospace.toFixed(4) + ' px' : '—'],
        ['100px sans-serif', typeof fontStats.bases['sans-serif'] === 'number' ? fontStats.bases['sans-serif'].toFixed(4) + ' px' : '—'],
        ['100px serif', typeof fontStats.bases.serif === 'number' ? fontStats.bases.serif.toFixed(4) + ' px' : '—'],
      ])
    );
    panel.appendChild(baseCard);

    // Filter controls
    const bar = el('div', 'fp-subtoolbar');
    const chips = el('div', 'fp-chip-group');
    for (const [val, label] of [
      ['all', 'All (' + fontStats.list.length + ')'],
      ['installed', 'Installed (' + fontStats.installedCount + ')'],
      ['missing', 'Missing (' + fontStats.missingCount + ')'],
    ]) {
      const btn = el('button', 'admin-chip' + (state.fontFilter === val ? ' is-active' : ''), label);
      btn.type = 'button';
      btn.addEventListener('click', () => {
        state.fontFilter = val;
        renderFontsTab(fontStats);
      });
      chips.appendChild(btn);
    }
    bar.appendChild(chips);

    const search = el('input', 'fp-inline-search');
    search.type = 'search';
    search.placeholder = 'Filter family name…';
    search.value = state.fontSearch;
    search.addEventListener('input', () => {
      state.fontSearch = search.value;
      renderFontsTableBody(tbody, fontStats);
    });
    bar.appendChild(search);
    panel.appendChild(bar);

    const wrap = el('div', 'admin-table-wrap');
    const table = el('table', 'admin-table fp-compact-table');
    const thead = el('thead');
    const htr = el('tr');
    for (const h of ['Status', 'Family', 'vs monospace', 'vs sans-serif', 'vs serif', 'Bounding box (L / R / Asc / Desc)']) {
      htr.appendChild(el('th', '', h));
    }
    thead.appendChild(htr);
    table.appendChild(thead);
    const tbody = el('tbody');
    renderFontsTableBody(tbody, fontStats);
    table.appendChild(tbody);
    wrap.appendChild(table);
    panel.appendChild(wrap);

    if (fontStats.otherMeasures.length > 0) {
      const det = el('details', 'fp-details');
      det.appendChild(el('summary', '', 'Other measureText calls (' + fontStats.otherMeasures.length + ')'));
      const pre = el(
        'pre',
        'fp-pre',
        fontStats.otherMeasures
          .map((m) => m.font + '  ::  ' + JSON.stringify(m.text) + '  =>  ' + Number(m.width).toFixed(3))
          .join('\n')
      );
      det.appendChild(pre);
      panel.appendChild(det);
    }
  }

  function renderFontsTableBody(tbody, fontStats) {
    tbody.textContent = '';
    const q = state.fontSearch.trim().toLowerCase();
    const rows = fontStats.list.filter((f) => {
      if (state.fontFilter === 'installed' && !f.installed) return false;
      if (state.fontFilter === 'missing' && f.installed) return false;
      if (q && !f.family.toLowerCase().includes(q)) return false;
      return true;
    });
    if (rows.length === 0) {
      const tr = el('tr');
      const td = el('td', 'admin-empty', 'No probed font families match.');
      td.colSpan = 6;
      tr.appendChild(td);
      tbody.appendChild(tr);
      return;
    }
    for (const f of rows) {
      const tr = el('tr');
      const tdStat = el('td', 'admin-nowrap');
      tdStat.appendChild(
        el(
          'span',
          f.installed ? 'admin-badge admin-badge--ok' : 'admin-badge admin-badge--neutral',
          f.installed ? 'HAVE' : 'MISS'
        )
      );
      tr.appendChild(tdStat);
      tr.appendChild(el('td', 'fp-font-name', f.family));
      for (const fb of ['monospace', 'sans-serif', 'serif']) {
        const w = f.widths[fb];
        const td = el('td', 'fp-mono' + (f.diffs[fb] ? ' fp-hit-cell' : ' admin-muted'));
        td.textContent = typeof w === 'number' ? w.toFixed(3) : '—';
        tr.appendChild(td);
      }
      const m = f.metrics || {};
      const bb =
        typeof m.actualBoundingBoxRight === 'number'
          ? [
              Number(m.actualBoundingBoxLeft || 0).toFixed(1),
              Number(m.actualBoundingBoxRight || 0).toFixed(1),
              Number(m.actualBoundingBoxAscent || 0).toFixed(1),
              Number(m.actualBoundingBoxDescent || 0).toFixed(1),
            ].join(' / ')
          : '—';
      tr.appendChild(el('td', 'fp-mono admin-muted', bb));
      tbody.appendChild(tr);
    }
  }

  function renderCanvasTab(events) {
    const panel = els.panels.canvas;
    panel.textContent = '';

    const urls = events.filter((e) => e.op === 'toDataURL');
    const reads = events.filter((e) => e.op === 'getImageData');
    const fills = events.filter((e) => e.op === 'fillText' || e.op === 'strokeText');

    // PNG Gallery
    const galCard = el('div', 'fp-card');
    galCard.appendChild(el('h3', '', 'Captured canvas toDataURL outputs (' + urls.length + ')'));
    if (urls.length === 0) {
      galCard.appendChild(el('p', 'admin-empty', 'No toDataURL calls were recorded.'));
    } else {
      const gallery = el('div', 'fp-canvas-grid');
      urls.forEach((u, idx) => {
        const item = el('figure', 'fp-canvas-card');
        const stage = el('div', 'fp-canvas-stage');
        if (typeof u.value === 'string' && u.value.startsWith('data:image/') && !u.truncated) {
          const img = document.createElement('img');
          img.src = u.value;
          img.alt = 'Captured canvas #' + (idx + 1) + ' (' + u.width + 'x' + u.height + ')';
          stage.appendChild(img);
        } else {
          stage.appendChild(el('span', 'admin-muted', u.truncated ? 'Truncated (>750KB)' : 'Non-image data URL'));
        }
        item.appendChild(stage);
        const cap = el('figcaption', 'fp-canvas-meta');
        cap.appendChild(el('strong', '', '#' + (idx + 1) + ' · ' + u.width + '×' + u.height));
        cap.appendChild(
          el(
            'span',
            'fp-mono admin-muted',
            Number(u.length || 0).toLocaleString() + ' chars · fnv ' + fnvString(u.value) + ' · t=' + u.t + 'ms'
          )
        );
        const copyBtn = el('button', 'admin-btn admin-btn--sm', 'Copy data URL');
        copyBtn.type = 'button';
        copyBtn.addEventListener('click', () => copyText(u.value || '', copyBtn, 'Copied'));
        cap.appendChild(copyBtn);
        item.appendChild(cap);
        gallery.appendChild(item);
      });
      galCard.appendChild(gallery);
    }
    panel.appendChild(galCard);

    // getImageData table
    const imgCard = el('div', 'fp-card');
    imgCard.appendChild(el('h3', '', 'getImageData reads (' + reads.length + ')'));
    if (reads.length === 0) {
      imgCard.appendChild(el('p', 'admin-empty', 'No getImageData calls recorded.'));
    } else {
      const wrap = el('div', 'admin-table-wrap');
      const tbl = el('table', 'admin-table fp-compact-table');
      const thead = el('thead');
      const htr = el('tr');
      for (const h of ['#', 't (ms)', 'Rect (x, y, w×h)', 'Nonzero alpha px', 'FNV-1a hash']) {
        htr.appendChild(el('th', '', h));
      }
      thead.appendChild(htr);
      tbl.appendChild(thead);
      const tbody = el('tbody');
      reads.forEach((r, idx) => {
        const tr = el('tr');
        tr.appendChild(el('td', 'fp-mono admin-muted', String(idx + 1)));
        tr.appendChild(el('td', 'fp-mono', String(r.t ?? '—')));
        tr.appendChild(el('td', 'fp-mono', r.x + ', ' + r.y + ' · ' + r.w + '×' + r.h));
        tr.appendChild(el('td', 'fp-mono', Number(r.nonzero || 0).toLocaleString()));
        const tdHash = el('td');
        tdHash.appendChild(el('code', 'fp-hash-pill', r.fnv || '—'));
        tr.appendChild(tdHash);
        tbody.appendChild(tr);
      });
      tbl.appendChild(tbody);
      wrap.appendChild(tbl);
      imgCard.appendChild(wrap);
    }
    panel.appendChild(imgCard);

    // fillText table
    const fillCard = el('div', 'fp-card');
    fillCard.appendChild(el('h3', '', 'fillText / strokeText draws (' + fills.length + ')'));
    if (fills.length === 0) {
      fillCard.appendChild(el('p', 'admin-empty', 'No text draw calls recorded.'));
    } else {
      const wrap = el('div', 'admin-table-wrap');
      const tbl = el('table', 'admin-table fp-compact-table');
      const thead = el('thead');
      const htr = el('tr');
      for (const h of ['t (ms)', 'Op', 'Font', 'Text', 'Coords', 'Style']) {
        htr.appendChild(el('th', '', h));
      }
      thead.appendChild(htr);
      tbl.appendChild(thead);
      const tbody = el('tbody');
      for (const f of fills) {
        const tr = el('tr');
        tr.appendChild(el('td', 'fp-mono admin-muted', String(f.t ?? '—')));
        tr.appendChild(el('td', 'fp-mono', f.op));
        tr.appendChild(el('td', 'fp-mono', f.font || '—'));
        tr.appendChild(el('td', 'fp-break', f.text || ''));
        tr.appendChild(el('td', 'fp-mono', f.x + ', ' + f.y));
        tr.appendChild(el('td', 'fp-mono admin-muted', f.fillStyle || f.strokeStyle || '—'));
        tbody.appendChild(tr);
      }
      tbl.appendChild(tbody);
      wrap.appendChild(tbl);
      fillCard.appendChild(wrap);
    }
    panel.appendChild(fillCard);
  }

  function renderHeadersTab(record) {
    const panel = els.panels.headers;
    panel.textContent = '';

    const wire = record.wire || {};
    const b = (record.submission && record.submission.browser) || {};
    const uad = b.userAgentData || null;
    const he = (uad && uad.highEntropy) || null;

    // Wire headers vs JS coherence table
    const grid = el('div', 'fp-two-col');

    const wireCard = el('div', 'fp-card');
    wireCard.appendChild(el('h3', '', 'HTTP wire headers (as received)'));
    const wirePairs = Object.keys(wire)
      .sort()
      .map((k) => [k, wire[k]]);
    if (wirePairs.length === 0) {
      wireCard.appendChild(el('p', 'admin-empty', 'No tracked headers recorded.'));
    } else {
      wireCard.appendChild(makeKvGrid(wirePairs));
    }
    grid.appendChild(wireCard);

    const jsCard = el('div', 'fp-card');
    jsCard.appendChild(el('h3', '', 'JS navigator.userAgentData'));
    if (!uad) {
      jsCard.appendChild(el('p', 'admin-empty', 'navigator.userAgentData is absent (expected on Firefox/Safari).'));
    } else {
      const fmtBrands = (arr) =>
        Array.isArray(arr) ? arr.map((x) => '"' + x.brand + '";v="' + x.version + '"').join(', ') : '—';
      jsCard.appendChild(
        makeKvGrid([
          ['brands', fmtBrands(uad.brands)],
          ['mobile', String(uad.mobile)],
          ['platform', uad.platform],
          ['architecture', he ? he.architecture : '—'],
          ['bitness', he ? he.bitness : '—'],
          ['platformVersion', he ? he.platformVersion : '—'],
          ['uaFullVersion', he ? he.uaFullVersion : '—'],
          ['fullVersionList', he ? fmtBrands(he.fullVersionList) : '—'],
          ['model', he ? JSON.stringify(he.model) : '—'],
          ['wow64', he ? String(he.wow64) : '—'],
          ['formFactors', he && Array.isArray(he.formFactors) ? he.formFactors.join(', ') : '—'],
        ])
      );
    }
    grid.appendChild(jsCard);
    panel.appendChild(grid);

    // Consistency checks
    const checks = [];
    const stripQ = (s) => String(s || '').replace(/^"|"$/g, '');
    if (wire['user-agent'] && b.userAgent) {
      checks.push(['User-Agent (wire vs navigator)', wire['user-agent'] === b.userAgent, wire['user-agent'], b.userAgent]);
    }
    if (wire['sec-ch-ua-platform'] && uad && uad.platform) {
      const w = stripQ(wire['sec-ch-ua-platform']);
      checks.push(['Platform (wire vs userAgentData)', w === uad.platform, w, uad.platform]);
    }
    if (wire['sec-ch-ua-platform-version'] && he && he.platformVersion !== undefined) {
      const w = stripQ(wire['sec-ch-ua-platform-version']);
      checks.push(['Platform version', w === he.platformVersion, w, he.platformVersion]);
    }
    if (wire['sec-ch-ua-bitness'] && he && he.bitness !== undefined) {
      const w = stripQ(wire['sec-ch-ua-bitness']);
      checks.push(['Bitness', w === he.bitness, w, he.bitness]);
    }
    if (wire['sec-ch-ua-arch'] && he && he.architecture !== undefined) {
      const w = stripQ(wire['sec-ch-ua-arch']);
      checks.push(['Architecture', w === he.architecture, w, he.architecture]);
    }
    if (wire['sec-ch-ua-full-version'] && he && he.uaFullVersion !== undefined) {
      const w = stripQ(wire['sec-ch-ua-full-version']);
      checks.push(['UA full version', w === he.uaFullVersion, w, he.uaFullVersion]);
    }

    if (checks.length > 0) {
      const checkCard = el('div', 'fp-card');
      checkCard.appendChild(el('h3', '', 'Wire vs JS coherence checks'));
      const wrap = el('div', 'admin-table-wrap');
      const tbl = el('table', 'admin-table fp-compact-table');
      const thead = el('thead');
      const htr = el('tr');
      for (const h of ['Check', 'Match', 'Wire header', 'JS value']) htr.appendChild(el('th', '', h));
      thead.appendChild(htr);
      tbl.appendChild(thead);
      const tbody = el('tbody');
      for (const [name, ok, wv, jv] of checks) {
        const tr = el('tr');
        tr.appendChild(el('td', '', name));
        const tdOk = el('td');
        tdOk.appendChild(
          el('span', ok ? 'admin-badge admin-badge--ok' : 'admin-badge admin-badge--bad', ok ? 'MATCH' : 'MISMATCH')
        );
        tr.appendChild(tdOk);
        tr.appendChild(el('td', 'fp-mono fp-break', wv));
        tr.appendChild(el('td', 'fp-mono fp-break', jv));
        tbody.appendChild(tr);
      }
      tbl.appendChild(tbody);
      wrap.appendChild(tbl);
      checkCard.appendChild(wrap);
      panel.appendChild(checkCard);
    }
  }

  function renderTapeTab(events) {
    const panel = els.panels.tape;
    panel.textContent = '';

    const counts = new Map();
    for (const ev of events) {
      counts.set(ev.op, (counts.get(ev.op) || 0) + 1);
    }

    const bar = el('div', 'fp-subtoolbar');
    const chips = el('div', 'fp-chip-group');
    const allChip = el('button', 'admin-chip' + (state.tapeOp === 'all' ? ' is-active' : ''), 'All (' + events.length + ')');
    allChip.type = 'button';
    allChip.addEventListener('click', () => {
      state.tapeOp = 'all';
      renderTapeTab(events);
    });
    chips.appendChild(allChip);

    for (const [op, count] of counts.entries()) {
      const chip = el('button', 'admin-chip' + (state.tapeOp === op ? ' is-active' : ''), op + ' (' + count + ')');
      chip.type = 'button';
      chip.addEventListener('click', () => {
        state.tapeOp = op;
        renderTapeTab(events);
      });
      chips.appendChild(chip);
    }
    bar.appendChild(chips);

    const search = el('input', 'fp-inline-search');
    search.type = 'search';
    search.placeholder = 'Search tape events…';
    search.value = state.tapeSearch;
    search.addEventListener('input', () => {
      state.tapeSearch = search.value;
      renderTapeRows(tbody, events);
    });
    bar.appendChild(search);
    panel.appendChild(bar);

    const wrap = el('div', 'admin-table-wrap');
    const tbl = el('table', 'admin-table fp-compact-table');
    const thead = el('thead');
    const htr = el('tr');
    for (const h of ['#', 't (ms)', 'Op', 'Details']) htr.appendChild(el('th', '', h));
    thead.appendChild(htr);
    tbl.appendChild(thead);
    const tbody = el('tbody');
    renderTapeRows(tbody, events);
    tbl.appendChild(tbody);
    wrap.appendChild(tbl);
    panel.appendChild(wrap);
  }

  function summarizeEvent(ev) {
    if (ev.op === 'measureText') {
      return ev.font + ' · ' + JSON.stringify(ev.text) + ' => w=' + Number(ev.width).toFixed(3);
    }
    if (ev.op === 'fillText' || ev.op === 'strokeText') {
      return ev.font + ' · ' + JSON.stringify(ev.text) + ' @ (' + ev.x + ',' + ev.y + ') ' + (ev.fillStyle || ev.strokeStyle || '');
    }
    if (ev.op === 'getImageData') {
      return '(' + ev.x + ',' + ev.y + ' ' + ev.w + '×' + ev.h + ') nonzero=' + ev.nonzero + ' fnv=' + ev.fnv;
    }
    if (ev.op === 'toDataURL') {
      return ev.width + '×' + ev.height + ' len=' + ev.length + ' fnv=' + fnvString(ev.value);
    }
    if (ev.op === 'matchMedia') {
      return ev.query + (ev.matches !== undefined ? ' => ' + String(ev.matches) : '');
    }
    if (ev.op === 'getParameter') {
      return 'pname=' + ev.name + ' => ' + JSON.stringify(ev.value);
    }
    return JSON.stringify(ev);
  }

  function renderTapeRows(tbody, events) {
    tbody.textContent = '';
    const q = state.tapeSearch.trim().toLowerCase();
    let shown = 0;
    const cap = 600;
    for (let i = 0; i < events.length; i += 1) {
      const ev = events[i];
      if (state.tapeOp !== 'all' && ev.op !== state.tapeOp) continue;
      const summary = summarizeEvent(ev);
      if (q && !summary.toLowerCase().includes(q) && !String(ev.op).toLowerCase().includes(q)) continue;
      if (shown >= cap) break;
      shown += 1;
      const tr = el('tr');
      tr.appendChild(el('td', 'fp-mono admin-muted', String(i + 1)));
      tr.appendChild(el('td', 'fp-mono', String(ev.t ?? '—')));
      const tdOp = el('td');
      tdOp.appendChild(el('span', 'admin-badge admin-badge--neutral', ev.op));
      tr.appendChild(tdOp);
      tr.appendChild(el('td', 'fp-mono fp-break', summary));
      tbody.appendChild(tr);
    }
    if (shown === 0) {
      const tr = el('tr');
      const td = el('td', 'admin-empty', 'No tape events match.');
      td.colSpan = 4;
      tr.appendChild(td);
      tbody.appendChild(tr);
    }
  }

  function renderRawTab(record) {
    const panel = els.panels.raw;
    panel.textContent = '';

    const bar = el('div', 'fp-subtoolbar');
    const chkWrap = el('label', 'fp-inline-check');
    const chk = document.createElement('input');
    chk.type = 'checkbox';
    chk.checked = state.rawFullBase64;
    chk.addEventListener('change', () => {
      state.rawFullBase64 = chk.checked;
      renderRawTab(record);
    });
    chkWrap.appendChild(chk);
    chkWrap.appendChild(el('span', '', 'Expand full base64 canvas strings & raw token'));
    bar.appendChild(chkWrap);
    panel.appendChild(bar);

    const display = state.rawFullBase64
      ? record
      : JSON.parse(
          JSON.stringify(record, (key, val) => {
            if (typeof val === 'string' && val.length > 200 && (key === 'token' || val.startsWith('data:image/'))) {
              return val.slice(0, 72) + '…[' + val.length + ' chars]';
            }
            return val;
          })
        );
    panel.appendChild(el('pre', 'fp-pre fp-pre--tall', JSON.stringify(display, null, 2)));
  }

  function renderInspector(record) {
    const sub = record.submission || {};
    const events = (sub.tape && Array.isArray(sub.tape.events)) ? sub.tape.events : [];
    const env = decodeCastleEnvelope(sub.token);
    const fontStats = analyzeFonts(events);
    const canvasCount = events.filter((e) => e.op === 'toDataURL').length;

    const titleParts = [record.id];
    if (sub.label) titleParts.push('— ' + sub.label);
    els.inspectorTitle.textContent = titleParts.join(' ');
    els.inspectorSub.textContent = [
      formatUtc(record.receivedAt),
      record.ip || 'unknown IP',
      (sub.browser && sub.browser.userAgent) || '',
    ]
      .filter(Boolean)
      .join(' · ');

    els.tabCountFonts.textContent = fontStats.installedCount + '/' + fontStats.list.length;
    els.tabCountCanvas.textContent = String(canvasCount);
    els.tabCountTape.textContent = String(events.length);

    renderOverviewTab(record, env, fontStats);
    renderFontsTab(fontStats);
    renderCanvasTab(events);
    renderHeadersTab(record);
    renderTapeTab(events);
    renderRawTab(record);
    switchTab(state.activeTab);
  }

  function switchTab(name) {
    state.activeTab = name;
    document.querySelectorAll('.fp-tab').forEach((btn) => {
      const on = btn.dataset.tab === name;
      btn.classList.toggle('is-active', on);
      btn.setAttribute('aria-selected', on ? 'true' : 'false');
    });
    for (const [key, panel] of Object.entries(els.panels)) {
      if (panel) panel.hidden = key !== name;
    }
  }

  function setCompareOpen(open) {
    state.compareOpen = Boolean(open);
    els.comparePanel.hidden = !state.compareOpen;
    els.compareToggle.setAttribute('aria-pressed', state.compareOpen ? 'true' : 'false');
    els.compareToggle.classList.toggle('is-active', state.compareOpen);
    if (state.compareOpen) {
      if (!state.compareA && state.summaries.length >= 1) {
        state.compareA = state.summaries[0].id;
        els.selectA.value = state.compareA;
      }
      if (!state.compareB && state.summaries.length >= 2) {
        state.compareB = state.summaries[1].id;
        els.selectB.value = state.compareB;
      }
      void renderCompare();
    }
  }

  async function renderCompare() {
    const body = els.compareBody;
    body.textContent = '';
    if (!state.compareA || !state.compareB) {
      body.appendChild(el('p', 'admin-empty', 'Select both Capture A and Capture B to diff them.'));
      return;
    }
    if (state.compareA === state.compareB) {
      body.appendChild(el('p', 'admin-empty', 'Choose two different captures to compare.'));
      return;
    }
    body.appendChild(el('p', 'admin-muted', 'Loading both captures for diff…'));
    try {
      const [recA, recB] = await Promise.all([fetchRecord(state.compareA), fetchRecord(state.compareB)]);
      body.textContent = '';

      const subA = recA.submission || {};
      const subB = recB.submission || {};
      const evA = (subA.tape && subA.tape.events) || [];
      const evB = (subB.tape && subB.tape.events) || [];
      const envA = decodeCastleEnvelope(subA.token);
      const envB = decodeCastleEnvelope(subB.token);
      const fontsA = analyzeFonts(evA);
      const fontsB = analyzeFonts(evB);
      const bA = subA.browser || {};
      const bB = subB.browser || {};
      const wA = recA.wire || {};
      const wB = recB.wire || {};

      // 1. Property & Envelope Diff
      const propRows = [
        ['Castle keyid', envA.keyid || '—', envB.keyid || '—'],
        ['Token length', String(envA.rawLength), String(envB.rawLength)],
        ['Decoded payload bytes', String(envA.bytes), String(envB.bytes)],
        ['Envelope tag', String(envA.tag), String(envB.tag)],
        ['SDK SHA-256', (recA.sdk || '').slice(0, 16), (recB.sdk || '').slice(0, 16)],
        ['Readable field #0', (envA.fields[0] && envA.fields[0].text) || '—', (envB.fields[0] && envB.fields[0].text) || '—'],
        ['User-Agent', bA.userAgent || '—', bB.userAgent || '—'],
        ['Platform', bA.platform || '—', bB.platform || '—'],
        ['Hardware (cores / mem)', (bA.hardwareConcurrency ?? '—') + ' / ' + (bA.deviceMemory ?? '—'), (bB.hardwareConcurrency ?? '—') + ' / ' + (bB.deviceMemory ?? '—')],
        ['Screen', (bA.screen ? bA.screen.width + '×' + bA.screen.height : '—'), (bB.screen ? bB.screen.width + '×' + bB.screen.height : '—')],
        ['Timezone', (bA.timeZone || '—') + ' (' + (bA.timezoneOffset ?? '—') + ')', (bB.timeZone || '—') + ' (' + (bB.timezoneOffset ?? '—') + ')'],
        ['WebGL unmasked renderer', (bA.webgl && bA.webgl.unmaskedRenderer) || '—', (bB.webgl && bB.webgl.unmaskedRenderer) || '—'],
        ['Base monospace width', String(fontsA.bases.monospace ?? '—'), String(fontsB.bases.monospace ?? '—')],
        ['Base sans-serif width', String(fontsA.bases['sans-serif'] ?? '—'), String(fontsB.bases['sans-serif'] ?? '—')],
        ['Base serif width', String(fontsA.bases.serif ?? '—'), String(fontsB.bases.serif ?? '—')],
      ];
      const allHeaders = Array.from(new Set([...Object.keys(wA), ...Object.keys(wB)])).sort();
      for (const h of allHeaders) {
        propRows.push(['wire: ' + h, wA[h] || '—', wB[h] || '—']);
      }

      const secEnv = el('div', 'fp-card');
      secEnv.appendChild(el('h3', '', 'Envelope, environment & wire headers'));
      const wrapEnv = el('div', 'admin-table-wrap');
      const tblEnv = el('table', 'admin-table fp-compact-table');
      const thEnv = el('thead');
      const htrEnv = el('tr');
      for (const h of ['Field', 'Status', 'Capture A (' + recA.id.slice(0, 16) + ')', 'Capture B (' + recB.id.slice(0, 16) + ')']) {
        htrEnv.appendChild(el('th', '', h));
      }
      thEnv.appendChild(htrEnv);
      tblEnv.appendChild(thEnv);
      const tbEnv = el('tbody');
      let envShown = 0;
      for (const [label, va, vb] of propRows) {
        const same = va === vb;
        if (state.diffOnly && same) continue;
        envShown += 1;
        const tr = el('tr', same ? '' : 'fp-diff-row');
        tr.appendChild(el('td', '', label));
        const tdS = el('td');
        tdS.appendChild(el('span', same ? 'admin-badge admin-badge--ok' : 'admin-badge admin-badge--warn', same ? 'SAME' : 'DIFF'));
        tr.appendChild(tdS);
        tr.appendChild(el('td', 'fp-mono fp-break', va));
        tr.appendChild(el('td', 'fp-mono fp-break', vb));
        tbEnv.appendChild(tr);
      }
      if (envShown === 0) {
        const tr = el('tr');
        const td = el('td', 'admin-empty', 'Every compared environment and wire field matches.');
        td.colSpan = 4;
        tr.appendChild(td);
        tbEnv.appendChild(tr);
      }
      tblEnv.appendChild(tbEnv);
      wrapEnv.appendChild(tblEnv);
      secEnv.appendChild(wrapEnv);
      body.appendChild(secEnv);

      // 2. Font probe diff
      const mapA = new Map(fontsA.list.map((f) => [f.family, f]));
      const mapB = new Map(fontsB.list.map((f) => [f.family, f]));
      const allFams = Array.from(new Set([...mapA.keys(), ...mapB.keys()]));
      const secFonts = el('div', 'fp-card');
      const onlyA = allFams.filter((k) => (mapA.get(k)?.installed) && !(mapB.get(k)?.installed));
      const onlyB = allFams.filter((k) => !(mapA.get(k)?.installed) && (mapB.get(k)?.installed));
      const bothInstalled = allFams.filter((k) => (mapA.get(k)?.installed) && (mapB.get(k)?.installed));
      const widthDiff = bothInstalled.filter((k) => {
        const wa = mapA.get(k).widths.monospace;
        const wb = mapB.get(k).widths.monospace;
        return typeof wa === 'number' && typeof wb === 'number' && Math.abs(wa - wb) >= 0.05;
      });
      const headF = el('div', 'fp-card-head');
      headF.appendChild(el('h3', '', 'Font probe diff'));
      const fPills = el('div', 'fp-badge-row');
      fPills.appendChild(el('span', 'admin-badge admin-badge--warn', 'Only in A: ' + onlyA.length));
      fPills.appendChild(el('span', 'admin-badge admin-badge--search', 'Only in B: ' + onlyB.length));
      fPills.appendChild(el('span', 'admin-badge admin-badge--bad', 'Width differs: ' + widthDiff.length));
      fPills.appendChild(el('span', 'admin-badge admin-badge--ok', 'Both installed: ' + bothInstalled.length));
      headF.appendChild(fPills);
      secFonts.appendChild(headF);

      const wrapF = el('div', 'admin-table-wrap');
      const tblF = el('table', 'admin-table fp-compact-table');
      const thF = el('thead');
      const htrF = el('tr');
      for (const h of ['Family', 'Verdict', 'A (mono width)', 'B (mono width)', 'Delta']) htrF.appendChild(el('th', '', h));
      thF.appendChild(htrF);
      tblF.appendChild(thF);
      const tbF = el('tbody');
      let fShown = 0;
      for (const fam of allFams) {
        const fa = mapA.get(fam);
        const fb = mapB.get(fam);
        const instA = Boolean(fa && fa.installed);
        const instB = Boolean(fb && fb.installed);
        const wa = fa && typeof fa.widths.monospace === 'number' ? fa.widths.monospace : null;
        const wb = fb && typeof fb.widths.monospace === 'number' ? fb.widths.monospace : null;
        const delta = wa !== null && wb !== null ? wb - wa : null;
        let verdict = 'SAME';
        let badge = 'admin-badge admin-badge--ok';
        let isDiff = false;
        if (instA && !instB) {
          verdict = 'ONLY A';
          badge = 'admin-badge admin-badge--warn';
          isDiff = true;
        } else if (!instA && instB) {
          verdict = 'ONLY B';
          badge = 'admin-badge admin-badge--search';
          isDiff = true;
        } else if (instA && instB && delta !== null && Math.abs(delta) >= 0.05) {
          verdict = 'WIDTH DIFF';
          badge = 'admin-badge admin-badge--bad';
          isDiff = true;
        }
        if (state.diffOnly && !isDiff) continue;
        fShown += 1;
        const tr = el('tr', isDiff ? 'fp-diff-row' : '');
        tr.appendChild(el('td', 'fp-font-name', fam));
        const tdV = el('td');
        tdV.appendChild(el('span', badge, verdict));
        tr.appendChild(tdV);
        tr.appendChild(el('td', 'fp-mono', (instA ? 'HAVE ' : 'MISS ') + (wa !== null ? wa.toFixed(2) : '—')));
        tr.appendChild(el('td', 'fp-mono', (instB ? 'HAVE ' : 'MISS ') + (wb !== null ? wb.toFixed(2) : '—')));
        tr.appendChild(el('td', 'fp-mono', delta !== null ? (delta >= 0 ? '+' : '') + delta.toFixed(2) : '—'));
        tbF.appendChild(tr);
      }
      if (fShown === 0) {
        const tr = el('tr');
        const td = el('td', 'admin-empty', 'No font presence or width differences between A and B.');
        td.colSpan = 5;
        tr.appendChild(td);
        tbF.appendChild(tr);
      }
      tblF.appendChild(tbF);
      wrapF.appendChild(tblF);
      secFonts.appendChild(wrapF);
      body.appendChild(secFonts);

      // 3. Canvas toDataURL & getImageData hash diff
      const urlsA = evA.filter((e) => e.op === 'toDataURL');
      const urlsB = evB.filter((e) => e.op === 'toDataURL');
      const maxU = Math.max(urlsA.length, urlsB.length);
      const secC = el('div', 'fp-card');
      secC.appendChild(el('h3', '', 'Canvas toDataURL diff (' + urlsA.length + ' vs ' + urlsB.length + ')'));
      const wrapC = el('div', 'admin-table-wrap');
      const tblC = el('table', 'admin-table fp-compact-table');
      const thC = el('thead');
      const htrC = el('tr');
      for (const h of ['#', 'Match', 'Capture A', 'Capture B']) htrC.appendChild(el('th', '', h));
      thC.appendChild(htrC);
      tblC.appendChild(thC);
      const tbC = el('tbody');
      for (let i = 0; i < maxU; i += 1) {
        const ua = urlsA[i];
        const ub = urlsB[i];
        const ha = ua ? fnvString(ua.value) : null;
        const hb = ub ? fnvString(ub.value) : null;
        const same = Boolean(ua && ub && ha === hb && ua.length === ub.length);
        if (state.diffOnly && same) continue;
        const tr = el('tr', same ? '' : 'fp-diff-row');
        tr.appendChild(el('td', 'fp-mono', '#' + (i + 1)));
        const tdM = el('td');
        tdM.appendChild(el('span', same ? 'admin-badge admin-badge--ok' : 'admin-badge admin-badge--bad', same ? 'IDENTICAL' : 'DIFF'));
        tr.appendChild(tdM);
        for (const [u, h] of [[ua, ha], [ub, hb]]) {
          const td = el('td');
          if (!u) {
            td.textContent = '—';
          } else {
            td.appendChild(el('div', 'fp-mono', u.width + '×' + u.height + ' · ' + u.length + ' chars · fnv ' + h));
            if (typeof u.value === 'string' && u.value.startsWith('data:image/') && !u.truncated) {
              const stage = el('div', 'fp-canvas-thumb');
              const img = document.createElement('img');
              img.src = u.value;
              img.alt = 'Canvas #' + (i + 1);
              stage.appendChild(img);
              td.appendChild(stage);
            }
          }
          tr.appendChild(td);
        }
        tbC.appendChild(tr);
      }
      if (tbC.childNodes.length === 0) {
        const tr = el('tr');
        const td = el('td', 'admin-empty', 'Every captured canvas PNG is byte-for-byte identical.');
        td.colSpan = 4;
        tr.appendChild(td);
        tbC.appendChild(tr);
      }
      tblC.appendChild(tbC);
      wrapC.appendChild(tblC);
      secC.appendChild(wrapC);
      body.appendChild(secC);
    } catch (err) {
      body.textContent = '';
      body.appendChild(el('p', 'admin-notice admin-notice--bad', String(err && err.message ? err.message : err)));
    }
  }

  async function loadList(keepSelection) {
    showStatus('');
    els.refresh.disabled = true;
    try {
      const response = await fetch(LIST_URL, { credentials: 'same-origin' });
      if (!response.ok) throw new Error('Could not load submissions (HTTP ' + response.status + ')');
      const rows = await response.json();
      state.summaries = Array.isArray(rows) ? rows : [];
      populateSdkFilter();
      populateComparePickers();
      applyFilters();

      const hashMatch = window.location.hash.match(/#id=([^&]+)/);
      const fromHash = hashMatch ? decodeURIComponent(hashMatch[1]) : null;
      const target =
        (keepSelection && state.selectedId && state.summaries.some((r) => r.id === state.selectedId) && state.selectedId) ||
        (fromHash && state.summaries.some((r) => r.id === fromHash) && fromHash) ||
        (state.summaries[0] ? state.summaries[0].id : null);

      if (target) {
        await selectCapture(target);
      } else {
        els.inspector.hidden = true;
      }
    } catch (err) {
      showStatus(String(err && err.message ? err.message : err), 'bad');
    } finally {
      els.refresh.disabled = false;
    }
  }

  async function handleDelete() {
    const id = state.selectedId;
    if (!id) return;
    if (state.deleteArmedId !== id) {
      state.deleteArmedId = id;
      els.delBtn.textContent = 'Confirm delete?';
      window.setTimeout(() => {
        if (state.deleteArmedId === id) {
          state.deleteArmedId = null;
          els.delBtn.textContent = 'Delete';
        }
      }, 4500);
      return;
    }
    state.deleteArmedId = null;
    els.delBtn.disabled = true;
    try {
      const response = await fetch(ITEM_URL(id), {
        method: 'DELETE',
        credentials: 'same-origin',
      });
      if (!response.ok) throw new Error('Delete failed (HTTP ' + response.status + ')');
      state.cache.delete(id);
      state.selectedId = null;
      showStatus('Deleted capture ' + id + '.', 'ok');
      await loadList(false);
    } catch (err) {
      showStatus(String(err && err.message ? err.message : err), 'bad');
    } finally {
      els.delBtn.disabled = false;
      els.delBtn.textContent = 'Delete';
    }
  }

  // Event wiring
  els.refresh.addEventListener('click', () => {
    state.cache.clear();
    void loadList(true);
  });

  els.copyCaptureUrl.addEventListener('click', () => {
    const url = window.location.origin + '/admin/fingerprint';
    copyText(url, els.copyCaptureUrl, 'Copied URL!');
  });

  els.compareToggle.addEventListener('click', () => setCompareOpen(!state.compareOpen));
  els.compareClose.addEventListener('click', () => setCompareOpen(false));
  els.compareSwap.addEventListener('click', () => {
    const tmp = state.compareA;
    state.compareA = state.compareB;
    state.compareB = tmp;
    els.selectA.value = state.compareA;
    els.selectB.value = state.compareB;
    renderTable();
    void renderCompare();
  });
  els.selectA.addEventListener('change', () => {
    state.compareA = els.selectA.value;
    renderTable();
    void renderCompare();
  });
  els.selectB.addEventListener('change', () => {
    state.compareB = els.selectB.value;
    renderTable();
    void renderCompare();
  });
  els.diffOnly.addEventListener('change', () => {
    state.diffOnly = els.diffOnly.checked;
    void renderCompare();
  });

  els.filterForm.addEventListener('submit', (e) => {
    e.preventDefault();
    applyFilters();
  });
  for (const input of [els.filterQ, els.filterBrowser, els.filterOs, els.filterSdk, els.filterSort]) {
    input.addEventListener('input', applyFilters);
    input.addEventListener('change', applyFilters);
  }
  els.filterClear.addEventListener('click', () => {
    els.filterQ.value = '';
    els.filterBrowser.value = '';
    els.filterOs.value = '';
    els.filterSdk.value = '';
    els.filterSort.value = 'newest';
    applyFilters();
  });

  els.listRows.addEventListener('click', (event) => {
    const slotBtn = event.target.closest('button[data-diff-slot]');
    if (slotBtn) {
      event.stopPropagation();
      const id = slotBtn.dataset.captureId;
      if (slotBtn.dataset.diffSlot === 'A') {
        state.compareA = id;
        els.selectA.value = id;
      } else {
        state.compareB = id;
        els.selectB.value = id;
      }
      renderTable();
      setCompareOpen(true);
      return;
    }
    const tr = event.target.closest('tr[data-capture-id]');
    if (tr) {
      void selectCapture(tr.dataset.captureId);
    }
  });

  document.querySelectorAll('.fp-tab').forEach((btn) => {
    btn.addEventListener('click', () => switchTab(btn.dataset.tab));
  });

  els.copyToken.addEventListener('click', () => {
    const rec = state.cache.get(state.selectedId);
    const token = rec && rec.submission && rec.submission.token;
    if (token) copyText(token, els.copyToken, 'Token copied!');
  });

  els.copyJson.addEventListener('click', () => {
    const rec = state.cache.get(state.selectedId);
    if (rec) copyText(JSON.stringify(rec, null, 2), els.copyJson, 'JSON copied!');
  });

  els.downloadJson.addEventListener('click', () => {
    const rec = state.cache.get(state.selectedId);
    if (!rec) return;
    const blob = new Blob([JSON.stringify(rec, null, 2)], { type: 'application/json' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = rec.id + '.json';
    document.body.appendChild(a);
    a.click();
    a.remove();
    URL.revokeObjectURL(url);
  });

  els.setA.addEventListener('click', () => {
    if (!state.selectedId) return;
    state.compareA = state.selectedId;
    els.selectA.value = state.selectedId;
    renderTable();
    setCompareOpen(true);
  });

  els.setB.addEventListener('click', () => {
    if (!state.selectedId) return;
    state.compareB = state.selectedId;
    els.selectB.value = state.selectedId;
    renderTable();
    setCompareOpen(true);
  });

  els.delBtn.addEventListener('click', () => {
    void handleDelete();
  });

  void loadList(false);
})();
