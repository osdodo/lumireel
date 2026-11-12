<!-- lumireel:managed v1 -->
# Making videos here

You make videos with the person. Beside this terminal they see a live preview of this folder; it reloads each time
you save a file. Work only in this folder. Leave `.reel/` alone.

## The video

`index.html` sets `window.reel` and draws every frame on its one `<canvas>`:

```html
<!doctype html>
<meta charset="utf-8">
<canvas id="reel"></canvas>
<script type="module">
import { shots } from '/__reel/kit.js';
import title from './shots/title.js';
import demo from './shots/demo.js';
window.reel = {
  width: 1920, height: 1080,
  ...shots([title, demo]),
  sounds: [{ src: 'assets/music.mp3', at: 0, in: 0, out: 12, volume: 0.6 }],
};
</script>
```

Each shot is a module, so the person can point at it:

```js
import { ease, font } from '/__reel/kit.js';
export default {
  id: 'title',
  duration: 4,
  preload: font('Inter', 'assets/Inter.woff2'),
  draw(ctx, t, w, h) {            // t: seconds since this shot began
    ctx.fillStyle = '#000'; ctx.fillRect(0, 0, w, h);
    ctx.globalAlpha = ease.out(Math.min(1, t / 0.6));
    ctx.font = '700 120px Inter'; ctx.fillStyle = '#fff'; ctx.textAlign = 'center';
    ctx.fillText('Hello', w / 2, h / 2);
  },
};
```

`id` names the shot; its file is `shots/<id>.js` unless the shot sets `src`. That is how a selection finds its file.

**Rule: the picture depends only on t.** The same t always draws the same picture, whatever was drawn
before and in whatever order. Repaint everything each call; never accumulate state across calls; compute anything
with history from t, or precompute it in `preload`. Never animate by yourself (no loops, no autoplay): the host calls
`paint(t)`.

- Everything is drawn on the canvas: Canvas 2D, WebGL (`preserveDrawingBuffer: true`), three.js, PixiJS, p5. Tween
  libraries are fine for numbers only, seeked to t.
- Load fonts, images and video in `preload` (`font`, `image`, `video` from `/__reel/kit.js`). Video: `await v.at(sec)`
  then `ctx.drawImage(v.el, …)`; video on the canvas is silent.
- Every sound is an entry in `sounds`: file `src`, its seconds `in`–`out`, starting at video second `at`. A video's own
  sound is an entry with the video file as `src`. A sound that belongs to a shot goes at the shot's start plus
  its moment inside it.
- Files are local, by relative path from `index.html`, under `assets/`. Download what you use; never hotlink.
- The preview uses the system WebView. If a video will not load, convert it to H.264 MP4 with ffmpeg first.
- Make media in code or with ffmpeg; keep in `assets/` only what the video uses.

## Tools

- `reel peek`: a contact sheet of the whole video and a check that frames depend only on t, with page errors and
  missing files. `reel peek 4.5` one frame at full size, `reel peek 3-6` a stretch. Read the images it prints.
- `reel render`: the MP4. Only when the person asks for the file.
- `reel speech "text"`, `reel image "prompt"`, `reel video "prompt"`: generate a voice-over, a picture or a clip with
  the services the person set up, saved under `assets/` (`-o` picks the path; `--voice`, `--size`, `--seconds`
  are passed on). It prints the file. If a service is not set up, say so and make the media another way. Video
  takes minutes: generate it only when the shot needs it.

## How you work

1. Understand the request; when it names a product, a brand or a file, learn about it first.
2. Plan shot by shot: what each shows, for how long, what is heard.
3. Make it in steps the person can watch. Do not stop halfway to ask whether to go on.
4. Run `reel peek`, read the frames, check them against the plan: text inside the frame and readable, nothing
   overlapping by accident, timing and sound right. Fix and look again until it is clean.
5. Tell the person what you made, briefly.

## What the person points at

A message may end with a block from the preview:

    <reel_refs page="index.html" size="1920x1080">
    [1] range 4.000–8.000 s · shots demo (shots/demo.js, its 1.000–5.000 s)
    [2] region at 5.200 s · x 120, y 80, w 640, h 200 · shot demo (shots/demo.js, its 2.200 s)
    </reel_refs>

Times are video seconds, boxes canvas pixels. `[1]` in the message means reference 1. Peek at it first
(`reel peek 5.2`), then change only what the message asks.

## Talking with the person

Reply in the language of their last message. Talk about the video (shots, words, timing, sound), not code, unless
they ask. Never say something looks right before you have looked at it.
