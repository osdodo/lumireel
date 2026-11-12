//! Preview audio lives outside WebKit: a new native output for each playback, backed by
//! the same ffmpeg mix as export. Mixing and device preparation never block the UI thread.

use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use objc2::{
    AllocAnyThread,
    rc::{Retained, autoreleasepool},
};
use objc2_avf_audio::AVAudioPlayer;
use objc2_foundation::{NSString, NSURL};

use crate::mix::{self, Sound};

enum Request {
    Prepare {
        root: PathBuf,
        sounds: Vec<Sound>,
        duration: f64,
    },
    Play {
        t: f64,
        sent_at_ms: f64,
        received: Instant,
    },
    Stop,
    Clear,
    Shutdown,
}

pub struct NativeAudio {
    tx: flume::Sender<Request>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl NativeAudio {
    pub fn new() -> (Self, flume::Receiver<String>) {
        let (tx, rx) = flume::unbounded();
        let (errors, error_rx) = flume::unbounded();
        let worker = std::thread::spawn(move || {
            let dir = std::env::temp_dir().join(format!(
                "lumireel-audio-{}-{:x}",
                std::process::id(),
                rand::random::<u64>()
            ));
            let result = fs::create_dir(&dir)
                .map_err(anyhow::Error::from)
                .and_then(|()| run(&rx, &errors, &dir));
            if let Err(error) = result {
                let _ = errors.send(format!("preview audio: {error:#}"));
            }
            let _ = fs::remove_dir_all(dir);
        });
        (
            Self {
                tx,
                worker: Some(worker),
            },
            error_rx,
        )
    }

    pub fn prepare(&self, root: PathBuf, sounds: Vec<Sound>, duration: f64) {
        let _ = self.tx.send(Request::Prepare {
            root,
            sounds,
            duration,
        });
    }

    pub fn play(&self, t: f64, sent_at_ms: f64) {
        let _ = self.tx.send(Request::Play {
            t,
            sent_at_ms,
            received: Instant::now(),
        });
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Request::Stop);
    }
    pub fn clear(&self) {
        let _ = self.tx.send(Request::Clear);
    }
}

impl Drop for NativeAudio {
    fn drop(&mut self) {
        let _ = self.tx.send(Request::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Render(Child);
impl Drop for Render {
    fn drop(&mut self) {
        // Cancelling a project or closing the app must also cancel ffmpeg and reap it.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Player(Retained<AVAudioPlayer>);
impl Drop for Player {
    fn drop(&mut self) {
        // All AVAudioPlayer access, including destruction, is confined to this worker.
        unsafe { self.0.stop() };
    }
}

struct Play {
    t: f64,
    received: Instant,
}

impl Play {
    fn new(t: f64, sent_at_ms: f64, received: Instant) -> Self {
        // Account for IPC delivery once using wall time, then use the monotonic clock.
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
            * 1000.0;
        let delivery = if sent_at_ms.is_finite() && sent_at_ms > 0.0 {
            ((now_ms - sent_at_ms) / 1000.0 - received.elapsed().as_secs_f64()).max(0.0)
        } else {
            0.0
        };
        Self {
            t: t.max(0.0) + delivery,
            received,
        }
    }

    fn time(&self) -> f64 {
        self.t + self.received.elapsed().as_secs_f64()
    }
}

fn run(rx: &flume::Receiver<Request>, errors: &flume::Sender<String>, dir: &Path) -> Result<()> {
    let wave = dir.join("mix.wav");
    let error_log = dir.join("ffmpeg.log");
    let mut render: Option<Render> = None;
    let mut ready = false;
    let mut wanted: Option<Play> = None;
    let mut player: Option<Player> = None;
    loop {
        let command = match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(Request::Shutdown) => break,
            Ok(command) => Some(command),
            Err(flume::RecvTimeoutError::Timeout) => None,
            Err(flume::RecvTimeoutError::Disconnected) => break,
        };
        let result: Result<()> = autoreleasepool(|_| {
            if let Some(command) = command {
                // Every transport change stops the previous output, including seeks.
                player = None;
                wanted = None;
                match command {
                    Request::Prepare {
                        root,
                        sounds,
                        duration,
                    } => {
                        render = None;
                        ready = false;
                        if !sounds.is_empty() && duration > 0.0 {
                            render = Some(Render(render_mix(
                                &root, &sounds, duration, &wave, &error_log,
                            )?));
                        }
                    }
                    Request::Play {
                        t,
                        sent_at_ms,
                        received,
                    } if t.is_finite() => {
                        wanted = Some(Play::new(t, sent_at_ms, received));
                    }
                    Request::Clear => {
                        render = None;
                        ready = false;
                    }
                    Request::Stop | Request::Play { .. } | Request::Shutdown => {}
                }
            }
            if let Some(job) = &mut render {
                if let Some(status) = job.0.try_wait()? {
                    render = None;
                    if !status.success() {
                        bail!(
                            "could not mix preview audio: {}",
                            fs::read_to_string(&error_log).unwrap_or_default().trim()
                        );
                    }
                    ready = true;
                    log::info!("native preview audio ready");
                }
            }
            // Drain transport commands before opening an output. A pause/seek arriving
            // during mixing or device preparation must never resurrect an obsolete play.
            if ready && wanted.is_some() && rx.is_empty() {
                let request = wanted.take().unwrap();
                let next = open_player(&wave)?;
                if rx.is_empty() && start_player(&next, &request)? {
                    log::info!("native preview audio playing at {:.3}s", unsafe {
                        next.0.currentTime()
                    });
                    player = Some(next);
                }
            }
            if player
                .as_ref()
                .is_some_and(|player| !unsafe { player.0.isPlaying() })
            {
                player = None;
                log::info!("native preview audio ended");
            }
            Ok(())
        });
        if let Err(error) = result {
            player = None;
            wanted = None;
            render = None;
            let _ = errors.send(format!("preview audio: {error:#}"));
        }
    }
    Ok(())
}

fn render_mix(
    root: &Path,
    sounds: &[Sound],
    duration: f64,
    wave: &Path,
    error_log: &Path,
) -> Result<Child> {
    let root = root.canonicalize().context("opening the audio project")?;
    let plan = mix::plan(sounds, duration);
    let filter = plan.filter.ok_or_else(|| anyhow!("no sounds to mix"))?;
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-nostdin",
        "-y",
        "-v",
        "error",
        "-f",
        "lavfi",
        "-i",
        "anullsrc=r=48000:cl=stereo",
    ]);
    // Input zero reserves the same slot the video occupies in the export mix.
    for src in plan.inputs {
        let file = root
            .join(&src)
            .canonicalize()
            .with_context(|| format!("opening sound {src}"))?;
        if !file.starts_with(&root) {
            bail!("sound must be inside the project: {src}");
        }
        cmd.arg("-i").arg(file);
    }
    cmd.args([
        "-filter_complex",
        &filter,
        "-map",
        "[audio]",
        "-ar",
        "48000",
        "-ac",
        "2",
        "-c:a",
        "pcm_s16le",
    ])
    .arg(wave)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(File::create(error_log)?);
    cmd.spawn().context("starting ffmpeg for preview audio")
}

fn open_player(wave: &Path) -> Result<Player> {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&wave.to_string_lossy()));
    // The URL and object remain owned for the call; no delegate or shared mutable state.
    let player =
        unsafe { AVAudioPlayer::initWithContentsOfURL_error(AVAudioPlayer::alloc(), &url) }
            .map_err(|error| anyhow!("opening native audio: {error}"))?;
    if !unsafe { player.prepareToPlay() } {
        bail!("could not prepare the audio output device");
    }
    Ok(Player(player))
}

fn start_player(player: &Player, request: &Play) -> Result<bool> {
    // Preparing the device or mixing may take time. Join the picture at its current
    // position, never replay the beginning after a late load.
    let t = request.time();
    if t >= unsafe { player.0.duration() } {
        return Ok(false);
    }
    unsafe {
        player.0.setCurrentTime(t);
        if !player.0.play() {
            bail!("could not start the audio output device");
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir()
                .join(format!("lumireel-audio-test-{:x}", rand::random::<u64>()));
            fs::create_dir(&dir).unwrap();
            Self(dir)
        }
        fn wave(&self, seconds: f64, sample: impl Fn(usize) -> i16) -> PathBuf {
            let path = self.0.join("source.wav");
            let frames = (48000.0 * seconds) as u32;
            let mut file = File::create(&path).unwrap();
            file.write_all(b"RIFF").unwrap();
            file.write_all(&(36 + frames * 4).to_le_bytes()).unwrap();
            file.write_all(b"WAVEfmt \x10\0\0\0\x01\0\x02\0").unwrap();
            file.write_all(&48000_u32.to_le_bytes()).unwrap();
            file.write_all(&192000_u32.to_le_bytes()).unwrap();
            file.write_all(b"\x04\0\x10\0data").unwrap();
            file.write_all(&(frames * 4).to_le_bytes()).unwrap();
            for frame in 0..frames {
                let value = sample(frame as usize).to_le_bytes();
                file.write_all(&value).unwrap();
                file.write_all(&value).unwrap();
            }
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn preview_mix_has_export_timing_trim_gain_and_overlap() {
        let fixture = Fixture::new();
        fixture.wave(0.2, |_| 3000);
        let output = fixture.0.join("mix.wav");
        let sounds = vec![
            Sound {
                src: "source.wav".into(),
                at: 0.02,
                in_point: 0.02,
                out_point: 0.06,
                volume: 0.5,
            },
            Sound {
                src: "source.wav".into(),
                at: 0.04,
                in_point: 0.0,
                out_point: 0.08,
                volume: 1.0,
            },
        ];
        let status = render_mix(
            &fixture.0,
            &sounds,
            0.16,
            &output,
            &fixture.0.join("error.log"),
        )
        .unwrap()
        .wait()
        .unwrap();
        assert!(status.success());
        let pcm = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(output)
            .args(["-f", "s16le", "-"])
            .output()
            .unwrap();
        assert!(pcm.status.success());
        assert_eq!(pcm.stdout.len(), 7680 * 4);
        for (t, expected) in [
            (0.01, 0_i16),
            (0.03, 1500),
            (0.05, 4500),
            (0.08, 3000),
            (0.14, 0),
        ] {
            let offset = (t * 48000.0) as usize * 4;
            let sample = i16::from_le_bytes(pcm.stdout[offset..offset + 2].try_into().unwrap());
            assert!(
                (sample - expected).abs() <= 1,
                "at {t}: {sample} != {expected}"
            );
        }
    }

    #[test]
    fn ipc_delay_is_not_counted_twice_after_waiting_for_the_mix() {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
            * 1000.0;
        let request = Play::new(
            5.0,
            now_ms - 2000.0,
            Instant::now() - Duration::from_secs(1),
        );
        assert!((request.time() - 7.0).abs() < 0.05);
    }

    #[test]
    #[ignore = "uses the macOS output device and waits 90 seconds; run explicitly with --ignored --nocapture"]
    fn native_output_replays_after_natural_end_and_idle() {
        let fixture = Fixture::new();
        // Quiet, known nonzero signal. Metering is on the native output, not a Web Audio node.
        let wave = fixture.wave(2.0, |i| {
            (100.0 * (i as f64 * std::f64::consts::TAU * 440.0 / 48000.0).sin()) as i16
        });
        let check = || {
            autoreleasepool(|_| {
                let player = open_player(&wave).unwrap();
                unsafe {
                    player.0.setMeteringEnabled(true);
                }
                assert!(
                    start_player(
                        &player,
                        &Play {
                            t: 0.0,
                            received: Instant::now()
                        }
                    )
                    .unwrap()
                );
                std::thread::sleep(Duration::from_millis(700));
                unsafe {
                    player.0.updateMeters();
                    let power = player.0.averagePowerForChannel(0);
                    eprintln!(
                        "native output: time={:.3}s, power={power:.1}dB",
                        player.0.currentTime()
                    );
                    assert!(player.0.isPlaying());
                    assert!(player.0.currentTime() > 0.1);
                    assert!(power > -65.0);
                }
                std::thread::sleep(Duration::from_secs(2));
                assert!(!unsafe { player.0.isPlaying() });
            })
        };
        check();
        eprintln!("natural playback ended; idling for 90 seconds");
        std::thread::sleep(Duration::from_secs(90));
        check();
    }
}
