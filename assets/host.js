// lumireel host — injected before every index.html.
// Freezes the clock while paint(t) runs, plays the preview, schedules sounds,
// handles region selection, and captures frames for `reel peek` / `reel render`.
(() => {
  const params = new URLSearchParams(location.search);
  // UI strings are localized by the host app; the fallbacks only cover a missing injection.
  const strings = window.__LUMIREEL_STRINGS__ || {};
  const sendLabel = () => strings.sendToAgent || "Send to Agent";
  const sendHint = () =>
    strings.sendTooltip || "Pastes into the terminal on the right without submitting";
  const clearHint = () => strings.clearRegion || "Clear selection";
  let sendButton = null;
  let closeButton = null;

  function applyOverlayStrings() {
    if (sendButton) {
      sendButton.textContent = sendLabel();
      sendButton.title = sendHint();
    }
    if (closeButton) closeButton.title = clearHint();
  }

  // The app calls this when the interface language changes; the page itself is not reloaded.
  window.__lumireelSetStrings = (next) => {
    Object.assign(strings, next || {});
    applyOverlayStrings();
  };
  const captureJob = params.get("reel-capture");
  const initialT = Number(params.get("t")) || 0;
  // The app numbers its seeks; every time report says which seek it follows, so the app can drop
  // reports that were already in flight when a newer seek was made (the thumb bounced back on them).
  let seekSeq = Number(params.get("seq")) || 0;

  const realNow = performance.now.bind(performance);
  const realRAF = window.requestAnimationFrame.bind(window);
  const realRandom = Math.random;
  const RealDate = Date;

  let frozenT = null;
  let canvas = null;
  let reel = null;
  let width = 1920;
  let height = 1080;
  let duration = 0;
  let currentT = initialT;
  let playing = false;
  // The stage shows the cover (the middle frame) until the person plays or seeks. The clock stays
  // where it was: this is a poster, not a jump to the middle of the film.
  let showingCover = false;

  const problems = [];
  const contextTypes = new WeakMap();

  function send(type, data = {}) {
    try {
      if (window.ipc && window.ipc.postMessage) {
        window.ipc.postMessage(JSON.stringify({ type, ...data }));
      }
    } catch (error) {
      console.error("lumireel ipc failed", error);
    }
  }

  function problem(message, source) {
    const text = String(message || "unknown error");
    if (problems.some((p) => p.message === text)) return;
    problems.push({ message: text, source: source || null });
    send("error", { message: text, source: source || null });
  }

  // Installed before reel modules run, so parse errors are reported too.
  window.addEventListener("error", (event) => {
    if (event.target && event.target !== window && event.target.tagName) {
      const src = event.target.currentSrc || event.target.src || "";
      problem(`could not load ${event.target.tagName.toLowerCase()} ${src}`, src);
    } else {
      problem(event.message, event.filename);
    }
  });
  window.addEventListener("unhandledrejection", (event) => {
    problem(
      event.reason && event.reason.stack ? event.reason.stack : event.reason,
      "promise",
    );
  });
  document.addEventListener(
    "error",
    (event) => {
      if (event.target && event.target !== document && event.target.tagName) {
        const src = event.target.currentSrc || event.target.src || "";
        problem(`could not load ${event.target.tagName.toLowerCase()} ${src}`, src);
      }
    },
    true,
  );

  // Record which context a reel asks for; reading pixels must not create a new one.
  const originalGetContext = HTMLCanvasElement.prototype.getContext;
  HTMLCanvasElement.prototype.getContext = function (type, ...args) {
    const ctx = originalGetContext.call(this, type, ...args);
    if (ctx && (type === "2d" || type === "webgl" || type === "webgl2")) {
      contextTypes.set(this, { type, ctx });
    }
    return ctx;
  };

  // --- Clock ---------------------------------------------------------------
  function seedFor(t) {
    const x = Math.floor(Math.max(0, Number(t) || 0) * 1e6) | 0;
    return (x ^ 0x9e3779b9) >>> 0;
  }

  function seededRandom(seed) {
    let a = seed >>> 0;
    return function () {
      a |= 0;
      a = (a + 0x6d2b79f5) | 0;
      let r = Math.imul(a ^ (a >>> 15), 1 | a);
      r = (r + Math.imul(r ^ (r >>> 7), 61 | r)) ^ r;
      return ((r ^ (r >>> 14)) >>> 0) / 4294967296;
    };
  }

  Object.defineProperty(performance, "now", {
    configurable: true,
    value() {
      return frozenT === null ? realNow() : frozenT * 1000;
    },
  });

  window.requestAnimationFrame = function (callback) {
    return realRAF((timestamp) =>
      callback(frozenT === null ? timestamp : frozenT * 1000),
    );
  };
  window.cancelAnimationFrame = window.cancelAnimationFrame.bind(window);

  function FrozenDate(...args) {
    if (new.target) {
      return args.length === 0 && frozenT !== null
        ? new RealDate(frozenT * 1000)
        : new RealDate(...args);
    }
    return args.length === 0 && frozenT !== null
      ? new RealDate(frozenT * 1000).toString()
      : new RealDate(...args).toString();
  }
  FrozenDate.now = () => (frozenT === null ? RealDate.now() : frozenT * 1000);
  FrozenDate.parse = RealDate.parse;
  FrozenDate.UTC = RealDate.UTC;
  FrozenDate.prototype = RealDate.prototype;
  window.Date = FrozenDate;

  // One frame at a time: paint(t) is async and shares the frozen clock, so two overlapping draws
  // (playback's last frame and a seek's) would corrupt each other and land out of order.
  let drawing = Promise.resolve();
  function drawFrame(t) {
    const run = drawing.then(() => drawFrameNow(t));
    drawing = run.catch(() => {});
    return run;
  }

  async function drawFrameNow(t) {
    if (!reel) return;
    frozenT = t;
    Math.random = seededRandom(seedFor(t));
    try {
      for (const animation of document.getAnimations()) {
        animation.pause();
        animation.currentTime = t * 1000;
      }
      await reel.paint(t);
    } catch (error) {
      problem(error && error.stack ? error.stack : error, "frame");
      throw error;
    } finally {
      Math.random = realRandom;
      frozenT = null;
    }
  }

  function nextFrame() {
    return new Promise((resolve) => realRAF(() => resolve()));
  }

  // --- Sounds --------------------------------------------------------------
  // Each sound is decoded once and scheduled on the audio clock when playback starts. The old
  // <audio> elements started late and were re-seeked every 250 ms to catch up, which stuttered.
  let sounds = [];
  let audioCtx = null;
  let audioClosing = Promise.resolve();
  let audioWatch = null;
  const buffers = new Map();
  let voices = [];
  // Decoding gets its own offline context: it is never suspended, and closing the playback
  // context cannot fail a decode in flight. Buffers play in any context.
  let decodeCtx = null;

  function closeAudioContext() {
    const ctx = audioCtx;
    audioCtx = null;
    audioWatch = null;
    // close() releases the device asynchronously. Do not open its replacement until that
    // finishes, including when several play/seek requests arrive during the same close.
    if (ctx) audioClosing = audioClosing.then(() => ctx.close()).catch(() => {});
  }

  function decode(data) {
    if (!decodeCtx) {
      decodeCtx = new (window.OfflineAudioContext || window.webkitOfflineAudioContext)(2, 1, 48000);
    }
    return decodeCtx.decodeAudioData(data);
  }

  function loadBuffer(src) {
    if (!buffers.has(src)) {
      buffers.set(
        src,
        fetch(src)
          .then((response) => {
            if (!response.ok) throw new Error(`HTTP ${response.status}`);
            return response.arrayBuffer();
          })
          .then(decode)
          .catch((error) => {
            problem(`could not decode sound ${src}: ${error}`, src);
            return null;
          }),
      );
    }
    return buffers.get(src);
  }

  function stopSounds() {
    for (const { source, gain } of voices) {
      try {
        source.stop();
      } catch {}
      gain.disconnect();
    }
    voices = [];
    closeAudioContext();
  }

  // Start every sound overlapping playback that began at reel time `fromT`, real time `realStart`.
  // Same in/out/volume rules as the exported mix (src/mix.rs), so preview and file sound alike.
  async function scheduleSounds(fromT, realStart, gen, retries = 0) {
    stopSounds();
    if (!playing || !sounds.length) return;
    await audioClosing;
    if (!playing || gen !== generation) return;
    const ctx = new (window.AudioContext || window.webkitAudioContext)();
    audioCtx = ctx;
    audioWatch = { time: ctx.currentTime, advancedAt: realNow(), retries };
    const resumed = ctx.state === "running" ? Promise.resolve() : ctx.resume();
    for (const sound of sounds) {
      if (!sound || !sound.src) continue;
      const at = Math.max(0, Number(sound.at) || 0);
      const inPoint = Math.max(0, Number(sound.in) || 0);
      const out = Number(sound.out) || 0;
      const outPoint = out > inPoint ? out : Math.max(duration, inPoint);
      const volume = Number.isFinite(Number(sound.volume)) && Number(sound.volume) >= 0
        ? Number(sound.volume)
        : 1;
      Promise.all([loadBuffer(sound.src), resumed])
        .then(([buffer]) => {
          if (!buffer || !playing || gen !== generation || ctx !== audioCtx) return;
          const endAt = Math.min(duration, at + Math.min(outPoint, buffer.duration) - inPoint);
          // Where the picture is now, on its own clock; the sound joins it there.
          const playNow = fromT + (realNow() - realStart) / 1000;
          const startT = Math.max(playNow, at);
          // Heard `latency` after it is scheduled, so start that much early.
          const latency = ctx.outputLatency || ctx.baseLatency || 0;
          const delay = startT - playNow - latency;
          const skip = Math.max(0, -delay);
          const length = endAt - startT - skip;
          if (length <= 0) return;
          const source = ctx.createBufferSource();
          source.buffer = buffer;
          const gain = ctx.createGain();
          gain.gain.value = volume;
          source.connect(gain).connect(ctx.destination);
          source.start(ctx.currentTime + Math.max(0, delay), inPoint + (startT - at) + skip, length);
          voices.push({ source, gain });
        })
        // A pause closes the context mid-resume; that rejection is expected, not a problem.
        .catch((error) => {
          if (playing && gen === generation && ctx === audioCtx) problem(error, "sound");
        });
    }
  }

  function startSounds(fromT, realStart, gen, retries = 0) {
    scheduleSounds(fromT, realStart, gen, retries).catch((error) => {
      if (playing && gen === generation) problem(error, "sound");
    });
  }

  function checkAudioClock(fromT, realStart, gen) {
    if (!audioCtx || !audioWatch) return;
    const now = realNow();
    const time = audioCtx.currentTime;
    if (audioCtx.state === "running" && time > audioWatch.time) {
      audioWatch.time = time;
      audioWatch.advancedAt = now;
      return;
    }
    // WebKit can suspend/interrupt audio after hiding or idling, leave resume() pending,
    // or report "running" with a stalled device clock. A successful resume alone is not
    // enough. The picture's clock keeps moving, so rebuild and join it at its current time.
    if (now - audioWatch.advancedAt < 1000) return;
    const retries = audioWatch.retries;
    if (retries >= 2) {
      stopSounds();
      problem("Audio output did not start. Check the output device and try playing again.", "sound");
      return;
    }
    startSounds(fromT, realStart, gen, retries + 1);
  }

  // --- Preview -------------------------------------------------------------
  let lastTimeSent = 0;

  function sendTime(force = false) {
    if (!force && realNow() - lastTimeSent < 100) return;
    lastTimeSent = realNow();
    send("time", { t: currentT, seq: seekSeq });
  }

  let generation = 0;

  async function playbackLoop(gen, playStart, realStart) {
    while (playing && gen === generation) {
      const target = playStart + (realNow() - realStart) / 1000;
      if (target >= duration) {
        try {
          await drawFrame(duration);
        } catch {}
        currentT = duration;
        playing = false;
        generation++;
        stopSounds();
        sendTime(true);
        send("ended", { seq: seekSeq });
        break;
      }
      checkAudioClock(playStart, realStart, gen);
      try {
        await drawFrame(target);
      } catch {
        playing = false;
        generation++;
        stopSounds();
        break;
      }
      if (!playing || gen !== generation) break;
      currentT = target;
      sendTime();
      await nextFrame();
    }
  }

  async function play() {
    if (playing || !reel) return;
    if (currentT >= duration - 0.001) currentT = 0;
    showingCover = false;
    playing = true;
    const gen = ++generation;
    const playStart = currentT;
    const realStart = realNow();
    startSounds(playStart, realStart, gen);
    await playbackLoop(gen, playStart, realStart);
  }

  function pause() {
    if (!playing) return;
    playing = false;
    generation++;
    stopSounds();
    sendTime(true);
  }

  // A drag on the progress bar sends a seek per pixel. Drawing each one queued up frames faster
  // than they could be drawn and left the page busy for seconds; only the latest target matters.
  let seekWanted = null;
  let seekBusy = null;
  let resumeAfterSeek = false;

  function seek(t, seq) {
    if (seq != null) seekSeq = Number(seq) || 0;
    if (!reel) return Promise.resolve();
    showingCover = false;
    currentT = Math.max(0, Math.min(duration, Number(t) || 0));
    if (playing) {
      resumeAfterSeek = true;
      playing = false;
      generation++;
      stopSounds();
    }
    seekWanted = currentT;
    if (!seekBusy) seekBusy = drainSeeks();
    return seekBusy;
  }

  async function drainSeeks() {
    try {
      while (seekWanted !== null) {
        const t = seekWanted;
        seekWanted = null;
        try {
          await drawFrame(t);
        } catch {}
      }
    } finally {
      seekBusy = null;
    }
    sendTime(true);
    if (resumeAfterSeek) {
      resumeAfterSeek = false;
      playing = true;
      const gen = ++generation;
      const realStart = realNow();
      startSounds(currentT, realStart, gen);
      playbackLoop(gen, currentT, realStart);
    }
  }

  // --- Region selection ----------------------------------------------------
  // Dragging on the stage draws a box; dragging inside it moves it, its edges resize it.
  let overlay = null;
  let box = null;
  let actions = null;
  let selectionRect = null; // in canvas pixels
  let drag = null;
  let handles = [];
  let hovered = null;
  const SIDES = ["nw", "n", "ne", "e", "se", "s", "sw", "w"];
  const CURSORS = {
    n: "ns-resize", s: "ns-resize", e: "ew-resize", w: "ew-resize",
    nw: "nwse-resize", se: "nwse-resize", ne: "nesw-resize", sw: "nesw-resize",
    move: "move",
  };

  function displayedRect() {
    return canvas.getBoundingClientRect();
  }

  function canvasPoint(event) {
    const rect = displayedRect();
    const x = ((event.clientX - rect.left) / rect.width) * width;
    const y = ((event.clientY - rect.top) / rect.height) * height;
    return {
      x: Math.max(0, Math.min(width, x)),
      y: Math.max(0, Math.min(height, y)),
    };
  }

  function shotAt(t) {
    const list = (reel && reel.shots) || [];
    let found = null;
    for (const shot of list) {
      if (t >= shot.at && t < shot.at + shot.duration) found = shot;
    }
    return found;
  }

  // Which part of the box is under the pointer: an edge/corner ("nw", "e", ...), "move", or null.
  function hit(event) {
    if (!selectionRect) return null;
    const r = box.getBoundingClientRect();
    // A generous edge band: resizing should not need pixel-perfect aim.
    const m = 8;
    const { clientX: x, clientY: y } = event;
    if (x < r.left - m || x > r.right + m || y < r.top - m || y > r.bottom + m) return null;
    // The actions bar hangs just below the box; it must not read as the bottom edge.
    if (actions && actions.style.display !== "none") {
      const a = actions.getBoundingClientRect();
      if (x >= a.left && x <= a.right && y >= a.top && y <= a.bottom) return null;
    }
    const v = Math.abs(y - r.top) <= m ? "n" : Math.abs(y - r.bottom) <= m ? "s" : "";
    const h = Math.abs(x - r.left) <= m ? "w" : Math.abs(x - r.right) <= m ? "e" : "";
    return v + h || "move";
  }

  // Pointer feedback inside the page itself, so the box feels live even before the native cursor
  // changes: the middle tints, the grip under the pointer widens.
  function setHover(mode) {
    if (hovered === mode) return;
    hovered = mode;
    if (box) {
      box.style.background = mode === "move" ? "rgba(77,163,255,.26)" : "rgba(77,163,255,.18)";
    }
    for (const handle of handles) {
      handle.classList.toggle("reel-active", !!mode && handle.dataset.side === mode);
    }
  }

  function draw() {
    if (!overlay) return;
    if (!selectionRect) {
      box.style.display = "none";
      actions.style.display = "none";
      for (const handle of handles) handle.style.display = "none";
      return;
    }
    const rect = displayedRect();
    const sx = rect.width / width;
    const sy = rect.height / height;
    const left = rect.left + selectionRect.x * sx;
    const top = rect.top + selectionRect.y * sy;
    const w = selectionRect.w * sx;
    const h = selectionRect.h * sy;
    Object.assign(box.style, {
      display: "block", left: `${left}px`, top: `${top}px`, width: `${w}px`, height: `${h}px`,
    });
    // Eight grips on the edges and corners: resize lives here. While a fresh box is being drawn
    // they would only clutter the drag, so they appear once the hand lifts.
    const showGrips = !(drag && drag.mode === "new") && w >= 16 && h >= 16;
    for (const handle of handles) {
      const side = handle.dataset.side;
      const hx = side.includes("w") ? left : side.includes("e") ? left + w : left + w / 2;
      const hy = side.includes("n") ? top : side.includes("s") ? top + h : top + h / 2;
      handle.style.display = showGrips ? "block" : "none";
      handle.style.left = `${hx}px`;
      handle.style.top = `${hy}px`;
    }
    if (drag) {
      actions.style.display = "none";
      return;
    }
    actions.style.display = "flex";
    const aw = actions.offsetWidth;
    const ah = actions.offsetHeight;
    // Below the box, else above it, else inside its bottom edge.
    let y = top + h + 6;
    if (y + ah > window.innerHeight) y = top - ah - 6;
    if (y < 0) y = top + h - ah - 6;
    const x = Math.max(4, Math.min(window.innerWidth - aw - 4, left + w - aw));
    actions.style.left = `${x}px`;
    actions.style.top = `${y}px`;
  }

  // A short flourish when a box is born: the grips pop out one after another and the border
  // glows twice. It says the box is an object — drag its middle to move it, its edges to resize.
  function pulseHint() {
    if (!selectionRect || !handles.length) return;
    for (const el of [box, ...handles]) el.classList.remove("reel-hint");
    handles.forEach((handle, i) => {
      handle.style.animationDelay = `${i * 28}ms`;
    });
    void box.offsetWidth; // reflow, so the animation starts again if it was already played
    box.classList.add("reel-hint");
    for (const handle of handles) handle.classList.add("reel-hint");
  }

  function report() {
    const shot = shotAt(currentT);
    send("region", {
      t: currentT,
      box: {
        x: Math.round(selectionRect.x),
        y: Math.round(selectionRect.y),
        w: Math.round(selectionRect.w),
        h: Math.round(selectionRect.h),
      },
      shot: shot
        ? {
            id: shot.id,
            src: shot.src,
            at: shot.at,
            duration: shot.duration,
            local: currentT - shot.at,
          }
        : null,
    });
  }

  function clearRegion() {
    selectionRect = null;
    drag = null;
    if (overlay) overlay.style.cursor = "crosshair";
    setHover(null);
    draw();
  }

  function button(label, title, onClick) {
    const el = document.createElement("button");
    el.textContent = label;
    el.title = title;
    el.style.cssText =
      "border:0;border-radius:6px;padding:4px 10px;font:500 12px system-ui,sans-serif;cursor:pointer;";
    // mousedown, not click: WKWebView does not always turn a press here into a click.
    el.addEventListener("mousedown", (event) => {
      if (event.button !== 0) return;
      event.preventDefault();
      event.stopPropagation();
      onClick();
    });
    return el;
  }

  function makeOverlay() {
    if (overlay) return overlay;
    overlay = document.createElement("div");
    overlay.style.cssText =
      "position:fixed;inset:0;z-index:2147483647;cursor:crosshair;background:transparent;";
    box = document.createElement("div");
    box.className = "reel-box";
    box.style.cssText =
      "position:absolute;display:none;box-sizing:border-box;border:2px solid #4da3ff;background:rgba(77,163,255,.18);pointer-events:none;";
    actions = document.createElement("div");
    actions.style.cssText =
      "position:absolute;display:none;gap:4px;padding:3px;border-radius:8px;background:rgba(20,20,24,.92);box-shadow:0 2px 8px rgba(0,0,0,.4);cursor:default;";
    sendButton = button(sendLabel(), sendHint(), () => send("region-send"));
    sendButton.style.background = "#fff";
    sendButton.style.color = "#111";
    closeButton = button("✕", clearHint(), () => {
      clearRegion();
      send("region-clear");
    });
    closeButton.style.background = "transparent";
    closeButton.style.color = "#ccc";
    actions.append(sendButton, closeButton);
    actions.addEventListener("mousedown", (event) => event.stopPropagation());
    const hintStyle = document.createElement("style");
    hintStyle.textContent = `
      @keyframes reel-grip-pop {
        from { opacity: 0; transform: translate(-50%, -50%) scale(.4); }
        60%  { opacity: 1; transform: translate(-50%, -50%) scale(1.25); }
        to   { opacity: 1; transform: translate(-50%, -50%) scale(1); }
      }
      @keyframes reel-region-glow {
        0%, 100% { box-shadow: 0 0 0 0 rgba(77, 163, 255, 0); }
        50%      { box-shadow: 0 0 0 6px rgba(77, 163, 255, .22); }
      }
      .reel-handle {
        position: absolute; display: none; width: 9px; height: 9px; border-radius: 2px;
        background: #fff; box-shadow: 0 0 0 1.5px #4da3ff, 0 1px 2px rgba(0, 0, 0, .35);
        pointer-events: none; transform: translate(-50%, -50%);
      }
      .reel-handle.reel-active {
        box-shadow: 0 0 0 2px #4da3ff, 0 0 0 5px rgba(77, 163, 255, .3), 0 1px 2px rgba(0, 0, 0, .35);
      }
      .reel-handle.reel-hint { animation: reel-grip-pop .38s ease-out 2; }
      .reel-box.reel-hint { animation: reel-region-glow 1s ease-in-out 2; }
    `;
    document.head.appendChild(hintStyle);
    handles = SIDES.map((side) => {
      const handle = document.createElement("div");
      handle.className = "reel-handle";
      handle.dataset.side = side;
      return handle;
    });
    overlay.append(box, actions, ...handles);
    document.body.appendChild(overlay);

    overlay.addEventListener("mousedown", (event) => {
      if (event.button !== 0) return;
      event.preventDefault();
      pause();
      // A selection on the poster must still name the time that is actually on screen.
      if (showingCover) seek(currentT);
      const p = canvasPoint(event);
      const mode = hit(event);
      if (mode) {
        drag = { mode, start: p, orig: { ...selectionRect } };
      } else {
        drag = { mode: "new", start: p };
        selectionRect = { x: p.x, y: p.y, w: 0, h: 0 };
      }
      overlay.style.cursor = drag.mode === "new" ? "crosshair" : CURSORS[drag.mode];
      setHover(mode);
      draw();
    });
    window.addEventListener("mousemove", (event) => {
      if (!drag) {
        const mode = hit(event);
        overlay.style.cursor = CURSORS[mode] || "crosshair";
        setHover(mode);
        return;
      }
      const p = canvasPoint(event);
      const { mode, start, orig } = drag;
      // Hold the cursor that matches the gesture, even when the pointer slips off the edge.
      if (mode !== "new") overlay.style.cursor = CURSORS[mode];
      if (mode === "new") {
        selectionRect = {
          x: Math.min(start.x, p.x),
          y: Math.min(start.y, p.y),
          w: Math.abs(start.x - p.x),
          h: Math.abs(start.y - p.y),
        };
      } else if (mode === "move") {
        selectionRect = {
          ...orig,
          x: Math.max(0, Math.min(width - orig.w, orig.x + p.x - start.x)),
          y: Math.max(0, Math.min(height - orig.h, orig.y + p.y - start.y)),
        };
      } else {
        let [l, t, r, b] = [orig.x, orig.y, orig.x + orig.w, orig.y + orig.h];
        if (mode.includes("w")) l = p.x;
        if (mode.includes("e")) r = p.x;
        if (mode.includes("n")) t = p.y;
        if (mode.includes("s")) b = p.y;
        selectionRect = {
          x: Math.min(l, r),
          y: Math.min(t, b),
          w: Math.abs(r - l),
          h: Math.abs(b - t),
        };
      }
      draw();
    });
    window.addEventListener("mouseup", (event) => {
      if (!drag) return;
      const { mode } = drag;
      drag = null;
      // A click (no drag) on empty stage drops the selection.
      if (selectionRect.w < 2 || selectionRect.h < 2) {
        clearRegion();
        send("region-clear");
        return;
      }
      report();
      draw();
      setHover(hit(event));
      // The first moment a box exists is when its affordances are worth pointing out.
      if (mode === "new") pulseHint();
    });
    window.addEventListener("resize", draw);
    return overlay;
  }

  // --- Format & readiness --------------------------------------------------
  function styleDocument() {
    const style = document.createElement("style");
    style.textContent = `
      html, body { margin: 0; height: 100%; background: #000; overflow: hidden; }
      body { display: flex; align-items: center; justify-content: center; }
      canvas#reel, body > canvas {
        display: block; max-width: 100vw; max-height: 100vh;
        width: auto; height: auto; object-fit: contain;
      }
    `;
    document.head.appendChild(style);
  }

  function findCanvas() {
    const canvases = document.querySelectorAll("canvas");
    if (canvases.length !== 1) {
      problem(`the page must contain exactly one <canvas> (found ${canvases.length})`, "format");
    }
    return canvases[0] || null;
  }

  async function waitForReel(timeoutMs) {
    const started = realNow();
    while (!window.reel) {
      if (realNow() - started > timeoutMs) {
        throw new Error("index.html did not set window.reel within 10 seconds");
      }
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    return window.reel;
  }

  async function boot() {
    styleDocument();
    try {
      reel = await waitForReel(10000);
      if (typeof reel.paint !== "function") {
        throw new Error("window.reel.paint(t) is required");
      }
      if (reel.preload) {
        await (typeof reel.preload === "function" ? reel.preload() : reel.preload);
      }
      canvas = findCanvas();
      if (!canvas) throw new Error("index.html needs one <canvas>");
      width = Number(reel.width) || 1920;
      height = Number(reel.height) || 1080;
      canvas.width = width;
      canvas.height = height;
      duration = Number(reel.duration) || 0;
      sounds = Array.isArray(reel.sounds) ? reel.sounds : [];
      // Decode now, so pressing play does not wait on it. Capture never plays sound.
      if (!captureJob) {
        for (const sound of sounds) if (sound && sound.src) loadBuffer(sound.src);
      }
    } catch (error) {
      problem(error && error.stack ? error.stack : error, "boot");
      if (captureJob) {
        try {
          await fetch(`/__ctl/done?job=${encodeURIComponent(captureJob)}`, {
            method: "POST",
            headers: authHeaders({ "Content-Type": "application/json" }),
            body: JSON.stringify({ problems, duration, width, height, frames: 0 }),
          });
        } catch {}
      }
      return;
    }

    if (captureJob) {
      await runCapture(captureJob);
      return;
    }

    send("ready", {
      duration,
      width,
      height,
      shots: reel.shots || [],
      sounds,
    });

    canvas.addEventListener("contextmenu", (event) => event.preventDefault());
    makeOverlay();
    // Films often open on black. Show the cover — the middle frame, past any fade-in and before
    // any fade-out, as in `.reel/cover.png` — until the person plays or seeks.
    if (initialT <= 0 && duration > 0) {
      showingCover = true;
      drawFrame(duration / 2).catch(() => {});
    } else {
      await seek(initialT);
    }
  }

  // --- Capture -------------------------------------------------------------
  const token = window.__LUMIREEL_TOKEN__ || "";

  function authHeaders(extra = {}) {
    return { Authorization: `Bearer ${token}`, ...extra };
  }

  function readPixels() {
    const recorded = contextTypes.get(canvas);
    if (recorded && recorded.type === "2d") {
      return recorded.ctx.getImageData(0, 0, width, height).data;
    }
    const gl =
      (recorded && recorded.ctx) ||
      canvas.getContext("webgl2") ||
      canvas.getContext("webgl");
    if (gl) {
      const pixels = new Uint8Array(width * height * 4);
      gl.readPixels(0, 0, width, height, gl.RGBA, gl.UNSIGNED_BYTE, pixels);
      // WebGL reads bottom-up; the server expects top-down RGBA.
      const flipped = new Uint8Array(pixels.length);
      const row = width * 4;
      for (let y = 0; y < height; y++) {
        flipped.set(pixels.subarray((height - 1 - y) * row, (height - y) * row), y * row);
      }
      return flipped;
    }
    return canvas.getContext("2d").getImageData(0, 0, width, height).data;
  }

  async function postPixels(job, index, pass, t, pixels) {
    const url = `/__ctl/frame?job=${encodeURIComponent(job)}&i=${index}&pass=${pass}&t=${encodeURIComponent(t)}`;
    const response = await fetch(url, {
      method: "POST",
      headers: authHeaders({ "Content-Type": "application/octet-stream" }),
      body: pixels,
    });
    if (!response.ok) throw new Error(`frame POST failed: ${response.status}`);
  }

  async function postPlan(job, plan) {
    const response = await fetch(`/__ctl/plan?job=${encodeURIComponent(job)}`, {
      method: "POST",
      headers: authHeaders({ "Content-Type": "application/json" }),
      body: JSON.stringify(plan),
    });
    if (!response.ok) {
      throw new Error(`capture plan failed: ${response.status} ${await response.text()}`);
    }
  }

  async function checkSounds() {
    for (const sound of sounds) {
      if (!sound || !sound.src) continue;
      try {
        const response = await fetch(sound.src, {
          headers: { Range: "bytes=0-0" },
        });
        if (!response.ok && response.status !== 206) {
          problem(`sound file is missing: ${sound.src}`, sound.src);
        }
      } catch (error) {
        problem(`could not load sound ${sound.src}: ${error}`, sound.src);
      }
    }
  }

  async function runCapture(job) {
    let index = 0;
    try {
      const infoResponse = await fetch(`/__ctl/job?id=${encodeURIComponent(job)}`, {
        headers: authHeaders(),
      });
      if (!infoResponse.ok) throw new Error(`could not read capture job: ${infoResponse.status}`);
      const info = await infoResponse.json();
      const passes = Number(info.passes) || 1;
      let times = [];

      await checkSounds();

      if (info.kind === "render") {
        const fps = Number(info.fps) || 30;
        const frames = Math.max(1, Math.ceil(duration * fps));
        await postPlan(job, {
          width,
          height,
          frames,
          fps,
          sounds,
        });
        for (let i = 0; i < frames; i++) {
          await drawFrame(i / fps);
          const pixels = readPixels();
          await postPixels(job, index, 0, i / fps, pixels);
          index++;
        }
      } else {
        const spec = info.spec || { type: "all" };
        if (spec.type === "single") {
          times = [Number(spec.t) || 0];
        } else if (spec.type === "cover") {
          // The middle frame: past any fade-in, before any fade-out.
          times = [duration / 2];
        } else {
          const start = spec.type === "range" ? Number(spec.start) || 0 : 0;
          const end =
            spec.type === "range" ? Number(spec.end) || duration : duration;
          const count = 12;
          times = Array.from(
            { length: count },
            (_, i) => start + ((end - start) * (i + 0.5)) / count,
          );
        }
        await postPlan(job, { width, height, times, passes });

        for (let i = 0; i < times.length; i++) {
          const t = Number(times[i]);
          await drawFrame(t);
          const pixels = readPixels();
          await postPixels(job, i, 0, t, pixels);
          index++;
        }
        for (let pass = 1; pass < passes; pass++) {
          const order = times.map((_, i) => i);
          for (let i = order.length - 1; i > 0; i--) {
            const j = Math.floor(
              seededRandom(seedFor(pass * 1000 + i))() * (i + 1),
            );
            [order[i], order[j]] = [order[j], order[i]];
          }
          // Keep the original index so the two passes line up frame by frame.
          for (const i of order) {
            const t = Number(times[i]);
            await drawFrame(t);
            const pixels = readPixels();
            await postPixels(job, i, pass, t, pixels);
            index++;
          }
        }
      }

      await fetch(`/__ctl/done?job=${encodeURIComponent(job)}`, {
        method: "POST",
        headers: authHeaders({ "Content-Type": "application/json" }),
        body: JSON.stringify({
          problems,
          duration,
          width,
          height,
          frames: index,
        }),
      });
    } catch (error) {
      problem(error && error.stack ? error.stack : error, "capture");
      try {
        await fetch(`/__ctl/done?job=${encodeURIComponent(job)}`, {
          method: "POST",
          headers: authHeaders({ "Content-Type": "application/json" }),
          body: JSON.stringify({ problems, duration, width, height, frames: index }),
        });
      } catch {}
    }
  }

  window.__fd = {
    play,
    pause,
    seek,
    clearRegion,
    reload(t, seq) {
      const url = new URL(location.href);
      url.searchParams.delete("reel-capture");
      url.searchParams.set("t", String(Number(t) || 0));
      url.searchParams.set("seq", String(Number(seq) || 0));
      location.replace(url.toString());
    },
    get time() {
      return currentT;
    },
    get playing() {
      return playing;
    },
  };

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", boot, { once: true });
  } else {
    boot();
  }
})();
