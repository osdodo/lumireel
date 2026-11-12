// Run with: node --test tests/host-audio.test.cjs
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const vm = require('node:vm');

const host = readFileSync(join(__dirname, '../assets/host.js'), 'utf8');
const flush = async () => { for (let i = 0; i < 40; i++) await Promise.resolve(); };
function deferred() {
  let resolve;
  const promise = new Promise(r => { resolve = r; });
  return { promise, resolve };
}

async function preview(options = {}) {
  let now = 0;
  let frames = [];
  const contexts = [];
  const messages = [];
  const element = () => ({ style: {}, dataset: {}, addEventListener() {}, append() {}, appendChild() {} });
  class Canvas { getContext() {} addEventListener() {} }
  class AudioContext {
    constructor() {
      this.state = options.suspended ? 'suspended' : 'running';
      this.frozen = !!options.frozen;
      this.clockStart = now;
      this.sources = [];
      this.destination = {};
      this.closeGate = options.closeGate;
      contexts.push(this);
    }
    get currentTime() { return this.frozen || this.state !== 'running' ? 0 : (now - this.clockStart) / 1000; }
    resume() { return this.state === 'suspended' ? new Promise(() => {}) : Promise.resolve(); }
    async close() {
      this.closeStarted = true;
      if (this.closeGate) await this.closeGate.promise;
      this.state = 'closed';
    }
    createGain() { return { gain: {}, connect() {}, disconnect() {} }; }
    createBufferSource() {
      const source = {
        connect(gain) { return gain; },
        start(when, offset, duration) { Object.assign(this, { when, offset, duration }); },
        stop() { this.stopped = true; },
      };
      this.sources.push(source);
      return source;
    }
  }
  const sandbox = {
    URL, URLSearchParams, console, setTimeout, clearTimeout,
    location: { search: '', href: 'http://localhost/index.html' },
    performance: { now: () => now },
    requestAnimationFrame: callback => { frames.push(callback); },
    cancelAnimationFrame() {}, addEventListener() {},
    HTMLCanvasElement: Canvas,
    document: {
      readyState: 'complete', head: element(), body: element(),
      createElement: element, getAnimations: () => [],
      querySelectorAll: () => [new Canvas()], addEventListener() {},
    },
    AudioContext,
    OfflineAudioContext: class { decodeAudioData() { return options.decodeGate?.promise || Promise.resolve({ duration: 30 }); } },
    fetch: async () => ({ ok: true, arrayBuffer: async () => new ArrayBuffer(1) }),
    ipc: { postMessage: message => messages.push(JSON.parse(message)) },
    reel: { duration: 30, paint: () => {}, sounds: [{ src: 'music.wav', at: 0, in: 0, out: 30 }] },
  };
  sandbox.window = sandbox;
  vm.runInNewContext(host, sandbox, { filename: 'host.js' });
  await flush();
  assert.ok(messages.some(m => m.type === 'ready'));
  return {
    api: sandbox.__fd, contexts, messages,
    async play() { void sandbox.__fd.play(); await flush(); },
    async tick(ms) {
      now += ms;
      const pending = frames;
      frames = [];
      pending.forEach(f => f(now));
      await flush();
    },
  };
}

test('restarts audio at the current picture after its clock stops while running', async () => {
  const p = await preview();
  await p.play();
  await p.tick(100);
  const first = p.contexts[0];
  first.frozen = true; // WebKit can report running while its device clock no longer moves.
  await p.tick(100);
  await p.tick(1200);
  assert.equal(p.contexts.length, 2);
  assert.equal(first.state, 'closed');
  assert.ok(first.sources[0].stopped);
  assert.ok(p.contexts[1].sources[0].offset >= 1.4);
});

test('recovers a resume that never settles after an idle period', async () => {
  const options = {};
  const p = await preview(options);
  await p.play();
  p.api.pause();
  await flush();
  await p.tick(60000);
  options.suspended = true;
  await p.play();
  options.suspended = false;
  await p.tick(1200);
  assert.equal(p.contexts.length, 3);
  assert.ok(p.contexts[2].sources[0].offset >= 1.2);
});

test('waits for device release and ignores obsolete rapid play/seek requests', async () => {
  const gate = deferred();
  const p = await preview({ closeGate: gate });
  await p.play();
  p.api.pause();
  await p.play();
  void p.api.seek(5, 1);
  await flush();
  assert.equal(p.contexts.length, 1, 'do not open another device while close is pending');
  gate.resolve();
  await flush();
  assert.equal(p.contexts.length, 2);
  assert.equal(p.contexts[1].sources.length, 1);
  assert.equal(p.contexts[1].sources[0].offset, 5);
});

test('late decoding cannot attach voices to a replaced context', async () => {
  const gate = deferred();
  const p = await preview({ decodeGate: gate });
  await p.play();
  p.contexts[0].frozen = true;
  await p.tick(1200);
  gate.resolve({ duration: 30 });
  await flush();
  assert.equal(p.contexts.length, 2);
  assert.equal(p.contexts[0].sources.length, 0);
  assert.equal(p.contexts[1].sources.length, 1);
});

test('persistent device failure reports an error and stops retrying', async () => {
  const p = await preview({ suspended: true });
  await p.play();
  for (let i = 0; i < 8; i++) await p.tick(1200);
  assert.equal(p.contexts.length, 3);
  assert.ok(p.messages.some(m => m.type === 'error' && m.source === 'sound'));
});

test('pausing during device recovery prevents an asynchronous restart', async () => {
  const gate = deferred();
  const p = await preview({ closeGate: gate });
  await p.play();
  p.contexts[0].frozen = true;
  await p.tick(1200);
  p.api.pause();
  gate.resolve();
  await flush();
  await p.tick(2000);
  assert.equal(p.contexts.length, 1);
  assert.equal(p.contexts[0].state, 'closed');
  assert.equal(p.api.playing, false);
});

test('reaching the end releases audio and replay starts at the beginning', async () => {
  const p = await preview();
  await p.play();
  await p.tick(30000);
  assert.equal(p.api.playing, false);
  assert.equal(p.contexts[0].state, 'closed');
  assert.ok(p.messages.some(m => m.type === 'ended'));
  await p.play();
  assert.equal(p.contexts.length, 2);
  assert.equal(p.contexts[1].sources[0].offset, 0);
});

test('healthy playback, pause, and replay do not trigger recovery', async () => {
  const p = await preview();
  await p.play();
  for (let i = 0; i < 10; i++) await p.tick(1000);
  assert.equal(p.contexts.length, 1);
  p.api.pause();
  await flush();
  await p.tick(60000);
  assert.equal(p.contexts.length, 1);
  await p.play();
  assert.equal(p.contexts.length, 2);
  assert.equal(p.contexts[1].sources[0].offset, 10);
  assert.equal(p.messages.filter(m => m.type === 'error').length, 0);
});
