"use strict";

// Voluntary Castle debugging collector.
// Loaded under a neutral filename (/js/m7k4q9v2.js) and fetches the live SDK
// from /admin/assets/r8x2n5p1.js so content blockers do not strip it by name.
// Nothing runs or transmits until the visitor presses the button.

(function () {
  const PK = "pk_AvRa79bHyJSYSQHnRpcVtzyxetSvFerx";
  const SDK_URL = "/admin/assets/r8x2n5p1.js";
  const SUBMIT_URL = "/admin/assets/k3w9v4t6";
  const MAX_EVENTS = 5000;
  const MAX_URL = 750000;

  const tape = { events: [] };
  // The session-activity channel: the exact pointer and key events Castle
  // folds into the token while the signup phone screen is open. A page-load
  // mint has none of this, which is why its token is ~400 bytes short of the
  // real begin_signup_phone token. Recording it here lets the VM minter replay
  // the same stream and reach the same token size.
  const interaction = { events: [], durationMs: 0, fires: [] };
  const MAX_INTERACTION = 6000;
  const MAX_SAMPLES = 16;
  let interactionActive = false;
  let interactionStart = 0;
  const wrapped = new WeakSet();
  const rawGetContext = HTMLCanvasElement.prototype.getContext;
  const rawGetParameter = window.WebGLRenderingContext
    ? WebGLRenderingContext.prototype.getParameter
    : null;
  let hooked = false;
  let entropyHooked = false;
  let sdkLoaded = false;

  function push(op, fields) {
    if (tape.events.length >= MAX_EVENTS) return;
    tape.events.push(Object.assign({ op: op, t: Math.round(performance.now()) }, fields));
  }

  function fnv(bytes) {
    let hash = 2166136261;
    for (let i = 0; i < bytes.length; i += 1) {
      hash ^= bytes[i];
      hash = Math.imul(hash, 16777619);
    }
    return (hash >>> 0).toString(16).padStart(8, "0");
  }

  function installHooks() {
    if (hooked) return;
    hooked = true;

    const originalContext = HTMLCanvasElement.prototype.getContext;
    HTMLCanvasElement.prototype.getContext = function getContext(kind, attrs) {
      const ctx = originalContext.call(this, kind, attrs);
      if (!ctx || wrapped.has(ctx)) return ctx;
      wrapped.add(ctx);
      const name = String(kind);
      if (name === "2d") wrap2d(ctx);
      if (name.indexOf("webgl") === 0) wrapGl(ctx);
      return ctx;
    };

    const originalUrl = HTMLCanvasElement.prototype.toDataURL;
    HTMLCanvasElement.prototype.toDataURL = function toDataURL() {
      const value = originalUrl.apply(this, arguments);
      const stored = value.length > MAX_URL ? value.slice(0, 64) : value;
      push("toDataURL", {
        width: this.width,
        height: this.height,
        length: value.length,
        truncated: value.length > MAX_URL,
        value: stored,
      });
      return value;
    };

    if (typeof window.matchMedia === "function") {
      const originalMatch = window.matchMedia.bind(window);
      window.matchMedia = function matchMedia(query) {
        const res = originalMatch(query);
        push("matchMedia", { query: String(query), matches: Boolean(res && res.matches) });
        return res;
      };
    }

    wrapAudio("AudioContext");
    wrapAudio("webkitAudioContext");
    wrapAudio("OfflineAudioContext");
    wrapAudio("webkitOfflineAudioContext");
  }

  function wrap2d(ctx) {
    const measure = ctx.measureText.bind(ctx);
    ctx.measureText = function measureText(text) {
      const result = measure(text);
      const row = { font: String(ctx.font), text: String(text), width: result.width };
      const fields = [
        "actualBoundingBoxLeft",
        "actualBoundingBoxRight",
        "actualBoundingBoxAscent",
        "actualBoundingBoxDescent",
        "fontBoundingBoxAscent",
        "fontBoundingBoxDescent",
        "emHeightAscent",
        "emHeightDescent",
        "hangingBaseline",
        "alphabeticBaseline",
        "ideographicBaseline",
      ];
      for (let i = 0; i < fields.length; i += 1) {
        const value = result[fields[i]];
        if (typeof value === "number") row[fields[i]] = value;
      }
      push("measureText", row);
      return result;
    };

    const fill = ctx.fillText.bind(ctx);
    ctx.fillText = function fillText(text, x, y) {
      push("fillText", {
        font: String(ctx.font),
        text: String(text),
        x: x,
        y: y,
        fillStyle: String(ctx.fillStyle),
      });
      return fill.apply(ctx, arguments);
    };

    if (typeof ctx.strokeText === "function") {
      const stroke = ctx.strokeText.bind(ctx);
      ctx.strokeText = function strokeText(text, x, y) {
        push("strokeText", {
          font: String(ctx.font),
          text: String(text),
          x: x,
          y: y,
          strokeStyle: String(ctx.strokeStyle),
        });
        return stroke.apply(ctx, arguments);
      };
    }

    const image = ctx.getImageData.bind(ctx);
    ctx.getImageData = function getImageData(x, y, w, h) {
      const data = image(x, y, w, h);
      let nonzero = 0;
      const pixels = data.data;
      for (let i = 3; i < pixels.length; i += 4) {
        if (pixels[i] !== 0) nonzero += 1;
      }
      push("getImageData", {
        x: x,
        y: y,
        w: w,
        h: h,
        nonzero: nonzero,
        fnv: fnv(pixels),
      });
      return data;
    };
  }

  function wrapGl(ctx) {
    const original = ctx.getParameter.bind(ctx);
    ctx.getParameter = function getParameter(name) {
      const value = original(name);
      const printable =
        typeof value === "string" || typeof value === "number" || typeof value === "boolean";
      if (printable) push("getParameter", { name: name, value: value });
      return value;
    };
  }

  function wrapAudio(name) {
    const Original = window[name];
    if (typeof Original !== "function") return;
    const Wrapped = function AudioHook() {
      const args = Array.prototype.slice.call(arguments);
      const logged = args.map(function (arg) {
        return typeof arg === "number" ? arg : String(arg);
      });
      const instance = new Original(...args);
      push("audio", {
        kind: name,
        args: logged,
        sampleRate: instance && typeof instance.sampleRate === "number" ? instance.sampleRate : null,
      });
      return instance;
    };
    Wrapped.prototype = Original.prototype;
    window[name] = Wrapped;
  }

  function hookEntropy() {
    if (entropyHooked) return;
    entropyHooked = true;
    const data = navigator.userAgentData;
    if (!data || typeof data.getHighEntropyValues !== "function") return;
    const original = data.getHighEntropyValues.bind(data);
    data.getHighEntropyValues = function getHighEntropyValues(hints) {
      return original(hints).then(function (result) {
        push("highEntropy", { hints: hints, result: result });
        return result;
      });
    };
  }

  async function browserSnapshot() {
    const data = navigator.userAgentData;
    let highEntropy = null;
    if (data && typeof data.getHighEntropyValues === "function") {
      try {
        highEntropy = await data.getHighEntropyValues([
          "architecture",
          "bitness",
          "model",
          "platformVersion",
          "uaFullVersion",
          "fullVersionList",
          "wow64",
          "formFactors",
        ]);
      } catch (error) {
        highEntropy = { error: String(error) };
      }
    }
    let webgl = null;
    try {
      const canvas = document.createElement("canvas");
      const gl =
        rawGetContext.call(canvas, "webgl") ||
        rawGetContext.call(canvas, "experimental-webgl");
      if (gl && rawGetParameter) {
        const ext = gl.getExtension("WEBGL_debug_renderer_info");
        const exts = gl.getSupportedExtensions ? gl.getSupportedExtensions() : null;
        webgl = {
          vendor: rawGetParameter.call(gl, gl.VENDOR),
          renderer: rawGetParameter.call(gl, gl.RENDERER),
          version: rawGetParameter.call(gl, gl.VERSION),
          shadingLanguageVersion: rawGetParameter.call(gl, gl.SHADING_LANGUAGE_VERSION),
          unmaskedVendor: ext ? rawGetParameter.call(gl, ext.UNMASKED_VENDOR_WEBGL) : null,
          unmaskedRenderer: ext ? rawGetParameter.call(gl, ext.UNMASKED_RENDERER_WEBGL) : null,
          extensionsCount: Array.isArray(exts) ? exts.length : null,
        };
      }
    } catch (error) {
      webgl = { error: String(error) };
    }
    let intlOpts = null;
    try {
      const resolved = Intl.DateTimeFormat().resolvedOptions();
      intlOpts = {
        locale: resolved.locale || null,
        timeZone: resolved.timeZone || null,
        calendar: resolved.calendar || null,
        numberingSystem: resolved.numberingSystem || null,
      };
    } catch (_) {
      intlOpts = null;
    }
    let plugins = [];
    try {
      if (navigator.plugins) {
        plugins = Array.from(navigator.plugins, function (p) {
          return { name: p.name || "", filename: p.filename || "" };
        });
      }
    } catch (_) {
      plugins = [];
    }
    const conn = navigator.connection || navigator.mozConnection || navigator.webkitConnection;
    return {
      userAgent: navigator.userAgent,
      platform: navigator.platform,
      vendor: navigator.vendor,
      language: navigator.language,
      languages: Array.isArray(navigator.languages) ? navigator.languages.slice() : [],
      hardwareConcurrency: navigator.hardwareConcurrency,
      deviceMemory: navigator.deviceMemory,
      maxTouchPoints: navigator.maxTouchPoints,
      pdfViewerEnabled: navigator.pdfViewerEnabled,
      webdriver: navigator.webdriver,
      cookieEnabled: navigator.cookieEnabled,
      doNotTrack: navigator.doNotTrack ?? null,
      globalPrivacyControl: navigator.globalPrivacyControl ?? null,
      hasWebRTC: typeof window.RTCPeerConnection !== "undefined",
      hasChromeObject: typeof window.chrome === "object" && window.chrome !== null,
      chromeKeys:
        typeof window.chrome === "object" && window.chrome !== null
          ? Object.keys(window.chrome).slice(0, 20)
          : [],
      plugins: plugins,
      connection: conn
        ? {
            effectiveType: conn.effectiveType || null,
            rtt: typeof conn.rtt === "number" ? conn.rtt : null,
            downlink: typeof conn.downlink === "number" ? conn.downlink : null,
            saveData: Boolean(conn.saveData),
          }
        : null,
      userAgentData: data
        ? {
            brands: data.brands,
            mobile: data.mobile,
            platform: data.platform,
            highEntropy: highEntropy,
          }
        : null,
      screen: {
        width: screen.width,
        height: screen.height,
        availWidth: screen.availWidth,
        availHeight: screen.availHeight,
        colorDepth: screen.colorDepth,
        pixelDepth: screen.pixelDepth,
        orientation: screen.orientation
          ? { type: screen.orientation.type || null, angle: screen.orientation.angle ?? null }
          : null,
      },
      window: {
        innerWidth: window.innerWidth,
        innerHeight: window.innerHeight,
        outerWidth: window.outerWidth,
        outerHeight: window.outerHeight,
        devicePixelRatio: window.devicePixelRatio,
      },
      timeZone: intlOpts ? intlOpts.timeZone : null,
      intl: intlOpts,
      timezoneOffset: new Date().getTimezoneOffset(),
      webgl: webgl,
    };
  }

  function cuid() {
    const match = document.cookie.match(/(?:^|; )__cuid=([^;]*)/);
    return match ? decodeURIComponent(match[1]) : null;
  }

  // --- interaction recorder: the session-activity channel ---------------------
  function pushInteraction(entry) {
    if (interaction.events.length >= MAX_INTERACTION) return;
    entry.t = Math.round((performance.now() - interactionStart) * 100) / 100;
    interaction.events.push(entry);
  }

  function recordMove(ev) {
    if (!interactionActive) return;
    let samples = null;
    if (typeof ev.getCoalescedEvents === "function") {
      try {
        const list = ev.getCoalescedEvents();
        if (list && list.length) {
          samples = [];
          const step = Math.max(1, Math.ceil(list.length / MAX_SAMPLES));
          for (let i = 0; i < list.length; i += step) {
            const c = list[i];
            samples.push([
              Math.round(c.clientX),
              Math.round(c.clientY),
              Math.round((c.timeStamp - interactionStart) * 100) / 100,
            ]);
          }
        }
      } catch (_) {
        samples = null;
      }
    }
    pushInteraction({
      e: ev.type === "pointermove" ? "pm" : "mm",
      x: Math.round(ev.clientX),
      y: Math.round(ev.clientY),
      n: typeof ev.getCoalescedEvents === "function" ? (ev.getCoalescedEvents() || [ev]).length : 1,
      s: samples,
    });
  }

  function recordButton(ev) {
    if (!interactionActive) return;
    const map = { pointerdown: "pd", pointerup: "pu", mousedown: "md", mouseup: "mu", click: "ck" };
    pushInteraction({ e: map[ev.type] || ev.type, x: Math.round(ev.clientX), y: Math.round(ev.clientY), b: ev.buttons });
  }

  function recordKey(ev) {
    if (!interactionActive) return;
    // The phone field holds a throwaway number; only the key category matters
    // for the token, so a letter is masked and digits are kept verbatim.
    const key = /^[0-9+]$/.test(ev.key) ? ev.key : ev.key && ev.key.length === 1 ? "x" : ev.key;
    pushInteraction({
      e: ev.type,
      k: key,
      c: ev.code,
      r: ev.repeat ? 1 : 0,
      alt: ev.altKey ? 1 : 0,
      ctrl: ev.ctrlKey ? 1 : 0,
      shift: ev.shiftKey ? 1 : 0,
      meta: ev.metaKey ? 1 : 0,
    });
  }

  function recordFocus(ev) {
    if (!interactionActive) return;
    pushInteraction({ e: ev.type });
  }

  const interactionBindings = [
    ["pointermove", recordMove, window],
    ["mousemove", recordMove, window],
    ["pointerdown", recordButton, window],
    ["pointerup", recordButton, window],
    ["mousedown", recordButton, window],
    ["mouseup", recordButton, window],
    ["click", recordButton, window],
    ["keydown", recordKey, window],
    ["keyup", recordKey, window],
    ["beforeinput", recordKey, window],
    ["focusin", recordFocus, window],
    ["focusout", recordFocus, window],
  ];

  function startInteraction() {
    if (interactionActive) return;
    interaction.events.length = 0;
    interaction.fires.length = 0;
    interactionStart = performance.now();
    interactionActive = true;
    for (let i = 0; i < interactionBindings.length; i += 1) {
      const [type, fn, target] = interactionBindings[i];
      target.addEventListener(type, fn, { capture: true, passive: true });
    }
  }

  function stopInteraction() {
    if (!interactionActive) return;
    interactionActive = false;
    interaction.durationMs = Math.round(performance.now() - interactionStart);
    for (let i = 0; i < interactionBindings.length; i += 1) {
      const [type, fn, target] = interactionBindings[i];
      target.removeEventListener(type, fn, { capture: true });
    }
  }

  function loadBundle() {
    if (sdkLoaded && (globalThis.__r8x2 || globalThis.__castle)) {
      return Promise.resolve();
    }
    return new Promise(function (resolve, reject) {
      const script = document.createElement("script");
      script.src = SDK_URL;
      script.onload = function () {
        sdkLoaded = true;
        resolve();
      };
      script.onerror = function () {
        reject(new Error("Could not load the capture bundle (" + SDK_URL + ")"));
      };
      document.head.appendChild(script);
    });
  }

  function setStatus(el, text, state) {
    el.hidden = false;
    el.textContent = text;
    if (state) el.dataset.state = state;
    else delete el.dataset.state;
  }

  function countDetectedFonts(events) {
    const bases = {};
    for (let i = 0; i < events.length; i += 1) {
      const ev = events[i];
      if (ev.op !== "measureText" || ev.text !== "mmmmmmmmmmlli") continue;
      const m = String(ev.font || "").match(/^100px ([^,]+)$/);
      if (m && bases[m[1]] === undefined) bases[m[1]] = ev.width;
    }
    const seen = new Map();
    for (let i = 0; i < events.length; i += 1) {
      const ev = events[i];
      if (ev.op !== "measureText" || ev.text !== "mmmmmmmmmmlli") continue;
      const m = String(ev.font || "").match(/^100px (.+), (monospace|sans-serif|serif)$/);
      if (!m) continue;
      const fam = m[1].replace(/^['"]|['"]$/g, "");
      const base = bases[m[2]];
      const hit = typeof base === "number" && Math.abs(ev.width - base) >= 0.05;
      seen.set(fam, Boolean(seen.get(fam) || hit));
    }
    let installed = 0;
    seen.forEach(function (val) {
      if (val) installed += 1;
    });
    return { installed: installed, total: seen.size };
  }

  function renderMetrics(container, rows) {
    container.textContent = "";
    for (let i = 0; i < rows.length; i += 1) {
      const card = document.createElement("div");
      card.className = "diag-metric";
      const label = document.createElement("span");
      label.className = "diag-metric-label";
      label.textContent = rows[i][0];
      const value = document.createElement("span");
      value.className = "diag-metric-value";
      value.textContent = rows[i][1];
      card.appendChild(label);
      card.appendChild(value);
      container.appendChild(card);
    }
    container.hidden = false;
  }

  function compactForPreview(payload) {
    const clone = JSON.parse(JSON.stringify(payload));
    if (clone.token && clone.token.length > 96) {
      clone.token = clone.token.slice(0, 48) + "…(" + clone.token.length + " chars)";
    }
    if (clone.tape && Array.isArray(clone.tape.events)) {
      clone.tape.events = clone.tape.events.map(function (ev) {
        if (ev.op === "toDataURL" && typeof ev.value === "string" && ev.value.length > 64) {
          return Object.assign({}, ev, {
            value: ev.value.slice(0, 48) + "…(" + ev.value.length + " chars)",
          });
        }
        return ev;
      });
    }
    if (clone.interaction && Array.isArray(clone.interaction.events)) {
      const count = clone.interaction.events.length;
      clone.interaction.events = clone.interaction.events.slice(0, 6);
      if (count > 6) clone.interaction.events.push("…(" + count + " events total)");
    }
    return clone;
  }

  // Castle API + the final mint, resolved once Castle is loaded.
  const session = { create: null, started: 0, browser: null, phase: "idle" };

  function els() {
    return {
      button: document.getElementById("diag-submit"),
      copyBtn: document.getElementById("diag-copy-id"),
      status: document.getElementById("diag-status"),
      noteEl: document.getElementById("diag-note"),
      metricsEl: document.getElementById("diag-metrics"),
      detailsEl: document.getElementById("diag-details"),
      previewEl: document.getElementById("diag-preview"),
      dwell: document.getElementById("diag-dwell"),
      phone: document.getElementById("diag-phone"),
    };
  }

  // Phase 1 — mimic the signup page: load Castle, let it do its page-load fire,
  // then open the phone field and start recording the session-activity channel.
  async function startCapture() {
    const ui = els();
    ui.button.disabled = true;
    if (ui.copyBtn) ui.copyBtn.hidden = true;
    if (ui.metricsEl) ui.metricsEl.hidden = true;
    if (ui.detailsEl) ui.detailsEl.hidden = true;
    tape.events.length = 0;

    setStatus(ui.status, "1/3 Reading browser properties…");
    try {
      session.browser = await browserSnapshot();
      installHooks();
      hookEntropy();

      setStatus(ui.status, "2/3 Loading live Castle script…");
      await loadBundle();
      const api = globalThis.__r8x2 || globalThis.__castle;
      if (!api || typeof api.configure !== "function") {
        throw new Error("Capture bundle loaded, but did not initialize.");
      }
      session.started = performance.now();
      const configured = await api.configure({ pk: PK });
      session.create =
        configured && typeof configured.createRequestToken === "function"
          ? configured.createRequestToken.bind(configured)
          : api.createRequestToken.bind(api);

      // Begin recording BEFORE the page-load fire so the pointer trail that
      // leads into the phone field is in the token, exactly as on x.com.
      startInteraction();
      // The page-load fire: Castle's heavy first collection. We discard the
      // token but note its size, the same fire the signup page runs on load.
      try {
        const warm = await session.create();
        interaction.fires.push({ at: "load", len: String(warm || "").length });
      } catch (_) {
        /* non-fatal: the final mint is what matters */
      }

      session.phase = "recording";
      if (ui.dwell) ui.dwell.hidden = false;
      if (ui.phone) {
        ui.phone.disabled = false;
        ui.phone.focus();
      }
      ui.button.disabled = false;
      ui.button.textContent = "Continue & submit";
      setStatus(
        ui.status,
        "3/3 Move your mouse and type a throwaway phone number above for ~10 seconds, then press Continue & submit.",
      );
    } catch (error) {
      stopInteraction();
      session.phase = "idle";
      setStatus(ui.status, String(error && error.message ? error.message : error), "error");
      ui.button.disabled = false;
      ui.button.textContent = "Run & submit this browser";
    }
  }

  // Phase 2 — the begin_signup_phone moment: mint the token that carries the
  // recorded interaction, then submit the capture.
  async function finishCapture() {
    const ui = els();
    ui.button.disabled = true;
    stopInteraction();
    setStatus(ui.status, "Minting the token with your interaction…");
    try {
      const token = await session.create();
      const elapsedMs = Math.round(performance.now() - session.started);
      if (!token || String(token).length < 40) {
        throw new Error("Castle returned an empty token.");
      }
      interaction.fires.push({ at: "continue", len: String(token).length });

      const label = ui.noteEl ? ui.noteEl.value.trim().slice(0, 160) : "";
      const payload = {
        consent: true,
        label: label || null,
        sdk: globalThis.__r8x2h || globalThis.__castleSdk || null,
        token: String(token),
        cuid: cuid(),
        elapsedMs: elapsedMs,
        browser: session.browser,
        tape: tape,
        interaction: interaction,
      };

      setStatus(ui.status, "Submitting capture…");
      const response = await fetch(SUBMIT_URL, {
        method: "POST",
        headers: { "content-type": "application/json" },
        credentials: "same-origin",
        body: JSON.stringify(payload),
      });
      const saved = await response.json();
      if (!response.ok) {
        throw new Error(saved.error || "Save failed (HTTP " + response.status + ")");
      }

      const measures = tape.events.filter(function (e) { return e.op === "measureText"; }).length;
      const urls = tape.events.filter(function (e) { return e.op === "toDataURL"; }).length;
      const fonts = countDetectedFonts(tape.events);
      const keyid = String(token).indexOf("|") > 0 ? String(token).split("|")[0] : "—";

      setStatus(ui.status, "Saved capture " + saved.id + ". Thank you — you can close this tab now.", "ok");

      if (ui.metricsEl) {
        renderMetrics(ui.metricsEl, [
          ["Submission ID", saved.id],
          ["Token length", String(token).length + " chars (" + keyid + ")"],
          ["Interaction", interaction.events.length + " events · " + interaction.durationMs + " ms"],
          ["Fonts detected", fonts.installed + " / " + fonts.total],
          ["Canvas & text", measures + " measures · " + urls + " PNGs"],
          ["Mint time", elapsedMs + " ms"],
        ]);
      }
      if (ui.copyBtn) {
        ui.copyBtn.dataset.captureId = saved.id;
        ui.copyBtn.textContent = "Copy ID";
        ui.copyBtn.hidden = false;
      }
      if (ui.detailsEl && ui.previewEl) {
        ui.previewEl.textContent = JSON.stringify(compactForPreview(payload), null, 2);
        ui.detailsEl.hidden = false;
      }
      if (ui.phone) {
        ui.phone.disabled = true;
        ui.phone.value = "";
      }
      if (ui.dwell) ui.dwell.hidden = true;
      session.phase = "idle";
      ui.button.disabled = false;
      ui.button.textContent = "Run & submit another";
    } catch (error) {
      setStatus(ui.status, String(error && error.message ? error.message : error), "error");
      session.phase = "idle";
      ui.button.disabled = false;
      ui.button.textContent = "Run & submit this browser";
    }
  }

  const submitBtn = document.getElementById("diag-submit");
  if (submitBtn) {
    submitBtn.addEventListener("click", function () {
      if (session.phase === "recording") void finishCapture();
      else void startCapture();
    });
  }

  const copyBtn = document.getElementById("diag-copy-id");
  if (copyBtn) {
    copyBtn.addEventListener("click", function () {
      const id = copyBtn.dataset.captureId;
      if (!id || !navigator.clipboard) return;
      navigator.clipboard.writeText(id).then(function () {
        copyBtn.textContent = "Copied!";
        setTimeout(function () { copyBtn.textContent = "Copy ID"; }, 1800);
      }).catch(function () {});
    });
  }
})();
