// lumireel kit — helpers for films. Served from /__reel/kit.js.

export function ease(t) {
  return t < 0.5 ? 4 * t * t * t : 1 - Math.pow(-2 * t + 2, 3) / 2;
}
ease.in = (t) => t * t * t;
ease.out = (t) => 1 - Math.pow(1 - t, 3);
ease.inOut = (t) =>
  t < 0.5 ? 4 * t * t * t : 1 - Math.pow(-2 * t + 2, 3) / 2;

export function lerp(a, b, t) {
  return a + (b - a) * t;
}

export function clamp(x, min, max) {
  return Math.max(min, Math.min(max, x));
}

// Run one of several numbered segments inside a shot: seq(t, [[0, 1, fn], [1, 2, fn]]).
export function seq(t, segments) {
  let current = segments[0];
  for (const segment of segments) {
    if (t >= segment[0]) current = segment;
    else break;
  }
  if (!current) return;
  return current[2](clamp(t - current[0], 0, current[1] - current[0]));
}

// Deterministic randomness for precomputation: same seed, same sequence.
export function rng(seed = 1) {
  let a = seed >>> 0;
  return function random() {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

export function image(src) {
  return new Promise((resolve, reject) => {
    const img = new Image();
    img.onload = () => resolve(img);
    img.onerror = () => reject(new Error(`could not load image ${src}`));
    img.src = src;
  });
}

// `reel peek` and `reel render` load the page with this parameter.
const capturing = new URLSearchParams(location.search).has("reel-capture");

export async function video(src) {
  const el = document.createElement("video");
  el.muted = true;
  el.playsInline = true;
  el.preload = capturing ? "metadata" : "auto";
  el.src = src;
  await new Promise((resolve, reject) => {
    if (el.readyState >= 1) return resolve();
    el.onloadedmetadata = () => resolve();
    el.onerror = () =>
      reject(new Error(`could not load video ${src}: ${el.error && el.error.message}`));
    setTimeout(() => reject(new Error(`timed out loading video ${src}`)), 15000);
  });
  // Peek and render take exact frames decoded by ffmpeg on the server: the element has no
  // "frame at t" call, and its seek events stall or come a frame late.
  let frame = null;
  async function exact(time) {
    const url = `${src}${src.includes("?") ? "&" : "?"}frame=${time}`;
    const response = await fetch(url, {
      headers: { Authorization: `Bearer ${window.__LUMIREEL_TOKEN__}` },
    });
    if (!response.ok) {
      throw new Error(`could not decode ${src} at ${time}s: ${await response.text()}`);
    }
    const w = Number(response.headers.get("X-Width"));
    const h = Number(response.headers.get("X-Height"));
    const pixels = new Uint8ClampedArray(await response.arrayBuffer());
    if (!frame) frame = document.createElement("canvas");
    if (frame.width !== w || frame.height !== h) {
      frame.width = w;
      frame.height = h;
    }
    frame.getContext("2d").putImageData(new ImageData(pixels, w, h), 0, 0);
    // Code written for the element reads its size from these.
    frame.videoWidth = w;
    frame.videoHeight = h;
    return frame;
  }

  // Preview playback lets the element play and draws whatever frame it shows: a seek per frame
  // cannot keep up. A paused preview seeks the element to the frame.
  let last = null;
  let idle = 0;
  async function follow(time) {
    // The host freezes Date at the reel time being painted, so the speed is exact, not measured.
    const reelT = Date.now() / 1000;
    const rate = last && reelT > last.reelT
      ? Math.round(((time - last.time) / (reelT - last.reelT)) * 1000) / 1000
      : 1;
    last = { time, reelT };
    clearTimeout(idle);
    idle = setTimeout(() => el.pause(), 300); // the preview paused or moved on
    if (rate < 0.05 || rate > 4) {
      // A still frame or a jump: show the exact frame.
      if (!el.paused) el.pause();
      return seek(time);
    }
    if (el.playbackRate !== rate) el.playbackRate = rate;
    if (!el.paused && el.readyState >= 2 && Math.abs(el.currentTime - time) < 0.25) return el;
    await seek(time);
    el.play().catch(() => {});
    return el;
  }

  async function seek(time) {
    if (Math.abs(el.currentTime - time) < 0.0005 && el.readyState >= 2) return el;
    el.currentTime = time;
    await new Promise((resolve, reject) => {
      let settled = false;
      const done = () => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        resolve(el);
      };
      const timer = setTimeout(done, 5000);
      if ("requestVideoFrameCallback" in el) {
        el.requestVideoFrameCallback(done);
        // A seek inside the frame on screen presents no new frame, so no callback would come.
        // Video frame callbacks run before animation frames, so one frame after `seeked` is enough.
        el.addEventListener("seeked", () => requestAnimationFrame(done), { once: true });
      } else {
        el.addEventListener("seeked", done, { once: true });
      }
      el.onerror = () => reject(new Error(`could not seek video ${src}`));
    });
    return el;
  }

  return {
    // What to draw: the decoded frame while capturing, else the element.
    get el() {
      return frame || el;
    },
    get duration() {
      return el.duration;
    },
    at(seconds) {
      const time = clamp(seconds, 0, Math.max(0, el.duration - 0.001));
      if (capturing) return exact(time);
      if (window.__fd && window.__fd.playing) return follow(time);
      last = null;
      if (!el.paused) el.pause();
      return seek(time);
    },
  };
}

export async function font(family, url, options = {}) {
  const face = new FontFace(family, `url(${url})`, options);
  await face.load();
  document.fonts.add(face);
  return family;
}

export function shots(modules) {
  const canvas = () => document.querySelector("canvas");
  const list = [];
  let at = 0;
  // Shots draw here and the finished frame is copied over, so a slow video seek mid-draw
  // keeps the last frame on screen instead of a cleared, black canvas.
  const back = document.createElement("canvas");

  for (let i = 0; i < modules.length; i++) {
    const module = modules[i];
    const shot = typeof module === "function" ? module() : module;
    if (!shot || typeof shot.draw !== "function") {
      throw new Error(`shot ${i + 1} has no draw(ctx, t, w, h)`);
    }
    const duration = Number(shot.duration) || 3;
    const id = shot.id || `shot-${i + 1}`;
    const src = shot.src || (shot.id ? `shots/${shot.id}.js` : undefined);
    list.push({ id, src, at, duration, shot });
    at += duration;
  }

  const reel = {
    duration: at,
    shots: list.map(({ id, src, at, duration }) => ({ id, src, at, duration })),
    async preload() {
      await Promise.all(list.map(({ shot }) => shot.preload));
    },
    async paint(t) {
      const el = canvas();
      if (!el) throw new Error("index.html needs one <canvas>");
      const w = Number(el.width) || 1920;
      const h = Number(el.height) || 1080;
      const time = clamp(Number(t) || 0, 0, at);
      let entry = list[0];
      for (const item of list) {
        if (time >= item.at) entry = item;
        else break;
      }
      const ctx = el.getContext("2d");
      if (!ctx) {
        throw new Error(
          "shots() draws with Canvas 2D; a WebGL reel sets its own paint(t) instead",
        );
      }
      if (back.width !== w || back.height !== h) {
        back.width = w;
        back.height = h;
      }
      const draft = back.getContext("2d");
      draft.save();
      draft.setTransform(1, 0, 0, 1, 0, 0);
      draft.globalAlpha = 1;
      draft.globalCompositeOperation = "source-over";
      draft.clearRect(0, 0, w, h);
      draft.restore();
      if (entry) await entry.shot.draw(draft, time - entry.at, w, h, time);
      ctx.save();
      ctx.setTransform(1, 0, 0, 1, 0, 0);
      ctx.globalAlpha = 1;
      ctx.globalCompositeOperation = "copy";
      ctx.filter = "none";
      ctx.drawImage(back, 0, 0);
      ctx.restore();
    },
  };
  return reel;
}
