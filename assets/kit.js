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

export async function video(src) {
  const el = document.createElement("video");
  el.muted = true;
  el.playsInline = true;
  el.preload = "auto";
  el.src = src;
  await new Promise((resolve, reject) => {
    if (el.readyState >= 1) return resolve();
    el.onloadedmetadata = () => resolve();
    el.onerror = () =>
      reject(new Error(`could not load video ${src}: ${el.error && el.error.message}`));
    setTimeout(() => reject(new Error(`timed out loading video ${src}`)), 15000);
  });
  return {
    el,
    get duration() {
      return el.duration;
    },
    async at(seconds) {
      const time = clamp(seconds, 0, Math.max(0, el.duration - 0.001));
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
        } else {
          el.addEventListener("seeked", done, { once: true });
        }
        el.onerror = () => reject(new Error(`could not seek video ${src}`));
      });
      return el;
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
      ctx.save();
      ctx.setTransform(1, 0, 0, 1, 0, 0);
      ctx.globalAlpha = 1;
      ctx.globalCompositeOperation = "source-over";
      ctx.clearRect(0, 0, w, h);
      ctx.restore();
      if (entry) await entry.shot.draw(ctx, time - entry.at, w, h, time);
    },
  };
  return reel;
}
