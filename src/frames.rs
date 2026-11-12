//! Exact video frames for `reel peek` and `reel render`, decoded by ffmpeg.
//!
//! A `<video>` element has no "frame at t" call: a seek is answered by events, and none comes for
//! a seek inside the frame already shown, so capture stalled. Here each file keeps one ffmpeg
//! process that reads forward; a render asks for frames in order, so most requests only read on.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Command, Stdio},
    sync::{Arc, LazyLock, Mutex},
    time::SystemTime,
};

use anyhow::{Context as _, Result, anyhow, bail};

#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Top-down RGBA.
    pub rgba: Arc<Vec<u8>>,
}

type Shared = Arc<Mutex<Decoder>>;
static DECODERS: LazyLock<Mutex<HashMap<PathBuf, Shared>>> = LazyLock::new(Default::default);

/// The frame on screen at `t` seconds: the last one that starts at or before `t`.
pub fn frame_at(path: &Path, t: f64) -> Result<Frame> {
    let decoder = DECODERS
        .lock()
        .expect("decoders lock")
        .entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(Decoder::new(path))))
        .clone();
    let mut decoder = decoder.lock().expect("decoder lock");
    decoder.at(t)
}

/// Stop every decoder; called when a capture job ends.
pub fn clear() {
    DECODERS.lock().expect("decoders lock").clear();
}

struct Decoder {
    path: PathBuf,
    /// Modification time and size the state below belongs to; an edited file starts over.
    stamp: Option<(SystemTime, u64)>,
    fps: Option<(u64, u64)>,
    matrix: Option<&'static str>,
    pipe: Option<(Child, BufReader<ChildStdout>)>,
    /// Index of the next frame the pipe will give.
    next: u64,
    current: Option<(u64, Frame)>,
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Decoder {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            stamp: None,
            fps: None,
            matrix: None,
            pipe: None,
            next: 0,
            current: None,
        }
    }

    fn at(&mut self, t: f64) -> Result<Frame> {
        let meta = std::fs::metadata(&self.path)?;
        let stamp = Some((meta.modified()?, meta.len()));
        if self.stamp != stamp {
            self.stop();
            self.current = None;
            self.probe()?;
            self.stamp = stamp;
        }
        let (num, den) = self.fps.expect("probed");
        let rate = num as f64 / den as f64;
        let want = (t.max(0.0) * rate + 1e-6).floor() as u64;
        if let Some((index, frame)) = &self.current
            && *index == want
        {
            return Ok(frame.clone());
        }
        // Reading on is cheaper than a restart for up to about two seconds of frames.
        if self.pipe.is_none() || want < self.next || want > self.next + 2 * rate.ceil() as u64 {
            self.start(want)?;
        }
        while let Some(frame) = self.read()? {
            let index = self.next;
            self.next += 1;
            self.current = Some((index, frame));
            if index >= want {
                break;
            }
        }
        if let Some((_, frame)) = &self.current {
            // Past the last frame this is the last one, as a player shows.
            return Ok(frame.clone());
        }
        // Started past the end: the final frame is in the file's last second.
        self.start_command(&["-sseof".into(), "-1".into()])?;
        let mut last = None;
        while let Some(frame) = self.read()? {
            last = Some(frame);
        }
        let frame =
            last.ok_or_else(|| anyhow!("{} has no video frames", self.path.display()))?;
        self.current = Some((want, frame.clone()));
        Ok(frame)
    }

    fn probe(&mut self) -> Result<()> {
        let output = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
            .arg("stream=r_frame_rate,height,color_space")
            .args(["-of", "default=noprint_wrappers=1"])
            .arg(&self.path)
            .output()
            .context("running ffprobe (install ffmpeg)")?;
        let text = String::from_utf8_lossy(&output.stdout);
        let field = |key: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
                .unwrap_or("")
                .to_string()
        };
        let rate = field("r_frame_rate");
        let fps = rate
            .split_once('/')
            .and_then(|(n, d)| Some((n.parse::<u64>().ok()?, d.parse::<u64>().ok()?)))
            .filter(|(n, d)| *n > 0 && *d > 0)
            .ok_or_else(|| anyhow!("{} has no video stream", self.path.display()))?;
        self.fps = Some(fps);
        // Untagged footage: browsers read HD as BT.709 and SD as BT.601; ffmpeg would pick 601.
        let tagged = !matches!(field("color_space").as_str(), "" | "unknown" | "reserved");
        let height: u32 = field("height").parse().unwrap_or(0);
        self.matrix = match (tagged, height >= 720) {
            (true, _) => None,
            (false, true) => Some("bt709"),
            (false, false) => Some("bt601"),
        };
        Ok(())
    }

    fn start(&mut self, index: u64) -> Result<()> {
        let (num, den) = self.fps.expect("probed");
        let mut seek = Vec::new();
        if index > 0 {
            // A hair early so rounding cannot skip frame `index`; earlier frames are dropped. Not
            // half a frame: that can round to a tie, and `-r` then fills slot 0 with a copy.
            let at = index as f64 * den as f64 / num as f64 - 0.0001;
            seek = vec!["-ss".into(), format!("{at:.6}")];
        }
        self.start_command(&seek)?;
        self.next = index;
        Ok(())
    }

    fn start_command(&mut self, seek: &[String]) -> Result<()> {
        self.stop();
        let (num, den) = self.fps.expect("probed");
        let mut command = Command::new("ffmpeg");
        command.args(["-v", "error", "-nostdin"]).args(seek);
        command.arg("-i").arg(&self.path).arg("-an");
        if let Some(matrix) = self.matrix {
            command.arg("-vf").arg(format!("scale=in_color_matrix={matrix}"));
        }
        // A constant rate at the stream's own rate makes output frame i the frame at i / rate.
        command
            .args(["-r", &format!("{num}/{den}")])
            .args(["-pix_fmt", "rgba", "-c:v", "pam", "-f", "image2pipe", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().context("starting ffmpeg (install ffmpeg)")?;
        let stdout = child.stdout.take().expect("ffmpeg stdout");
        self.pipe = Some((child, BufReader::with_capacity(1 << 20, stdout)));
        self.current = None;
        Ok(())
    }

    fn stop(&mut self) {
        if let Some((mut child, _)) = self.pipe.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// The next PAM image from the pipe, or `None` at the end of the video.
    fn read(&mut self) -> Result<Option<Frame>> {
        let Some((_, reader)) = self.pipe.as_mut() else {
            return Ok(None);
        };
        let (mut width, mut height, mut depth) = (0u32, 0u32, 0u32);
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                self.stop();
                return Ok(None);
            }
            let mut words = line.split_whitespace();
            match (words.next(), words.next()) {
                (Some("WIDTH"), Some(v)) => width = v.parse()?,
                (Some("HEIGHT"), Some(v)) => height = v.parse()?,
                (Some("DEPTH"), Some(v)) => depth = v.parse()?,
                (Some("ENDHDR"), _) => break,
                _ => {}
            }
        }
        if depth != 4 || width == 0 || height == 0 {
            bail!("unexpected frame from ffmpeg: {width}x{height}x{depth}");
        }
        let mut rgba = vec![0; width as usize * height as usize * 4];
        reader.read_exact(&mut rgba)?;
        Ok(Some(Frame {
            width,
            height,
            rgba: Arc::new(rgba),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 24 frames at 24 fps (film's rate and timebase) whose gray level encodes the frame number,
    /// lossless, a keyframe every 4.
    fn numbered_clip(dir: &Path) -> PathBuf {
        let path = dir.join("numbered.mp4");
        let status = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("nullsrc=s=32x32:r=24:d=1,geq=lum=16+N*8:cb=128:cr=128")
            .args(["-c:v", "libx264", "-qp", "0", "-g", "4", "-pix_fmt", "yuv420p"])
            .args(["-color_range", "tv", "-colorspace", "bt709"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        path
    }

    fn number(frame: &Frame) -> u32 {
        // Limited range: luma 16 + 8 N is red ≈ 8 N × 255 / 219.
        (frame.rgba[0] as f64 / (8.0 * 255.0 / 219.0)).round() as u32
    }

    #[test]
    fn frames_are_exact_in_order_backwards_repeated_and_past_the_end() {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("lumireel-frames-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = numbered_clip(&dir);
        let at = |t: f64| number(&frame_at(&clip, t).unwrap());
        let f = |n: u32| n as f64 / 24.0;
        // In order at 30 fps: frames repeat, and a boundary belongs to the new frame.
        for (t, n) in [(0.0, 0), (1.0 / 30.0, 0), (f(1), 1), (2.0 / 30.0, 1), (f(2), 2)] {
            assert_eq!(at(t), n, "t = {t}");
        }
        assert_eq!(at(f(17)), 17); // ahead, past keyframes
        // Back: a restart, then reading on from it must not repeat or skip a frame.
        for n in [3, 4, 5, 6] {
            assert_eq!(at(f(n) + 0.001), n);
        }
        assert_eq!(at(f(23)), 23);
        for n in [13, 14, 15] {
            assert_eq!(at(f(n)), n);
        }
        assert_eq!(at(5.0), 23); // past the end: the last frame
        let frame = frame_at(&clip, 0.2).unwrap();
        assert_eq!((frame.width, frame.height, frame.rgba.len()), (32, 32, 32 * 32 * 4));
        clear();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
