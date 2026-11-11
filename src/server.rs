use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail};
use image::RgbaImage;
use rust_i18n::t;
use serde::{Deserialize, Serialize};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

const HOST_JS: &str = include_str!("../assets/host.js");
const KIT_JS: &str = include_str!("../assets/kit.js");

#[derive(Clone, Debug)]
pub enum PeekSpec {
    All,
    Single(f64),
    Range(f64, f64),
    /// One middle frame, saved small as `.reel/cover.png` for the project list.
    Cover,
}

#[derive(Clone, Debug)]
pub struct CaptureJob {
    pub id: u64,
    pub project: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Problem {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl From<String> for Problem {
    fn from(message: String) -> Self {
        Self {
            message,
            source: None,
        }
    }
}

impl From<&str> for Problem {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

#[derive(Clone, Debug)]
pub struct PeekDone {
    pub sheet: PathBuf,
    pub frames: Vec<PathBuf>,
    pub problems: Vec<Problem>,
}

#[derive(Clone, Debug)]
pub struct RenderDone {
    pub file: PathBuf,
    pub problems: Vec<Problem>,
}

#[derive(Clone, Debug)]
pub enum JobDone {
    Peek(PeekDone),
    Render(RenderDone),
}

#[derive(Clone)]
pub struct ServerHandle {
    pub port: u16,
    pub token: String,
    pub base_url: String,
    control: Arc<Mutex<Control>>,
}

impl ServerHandle {
    pub fn page_url(&self, project: &str) -> String {
        format!("{}/p/{}/index.html", self.base_url, encode_segment(project))
    }

    /// A bare player page for one asset of `project`, for the stage's media webview.
    pub fn player_url(&self, project: &str, relative: &str) -> String {
        format!(
            "{}/__reel/player?project={}&src={}",
            self.base_url,
            encode_segment(project),
            encode_segment(relative)
        )
    }

    /// POST to our own control API, as the `reel` CLI does. Blocks until the job answers.
    fn post(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let mut response = agent
            .post(format!("{}{path}", self.base_url))
            .header("Authorization", format!("Bearer {}", self.token))
            .send_json(body)
            .context("calling the lumireel server")?;
        let ok = response.status().is_success();
        let value: serde_json::Value = response.body_mut().read_json().unwrap_or_default();
        if !ok {
            bail!("{}", value["error"].as_str().unwrap_or("request failed"));
        }
        Ok(value)
    }

    /// Export `project` to `<project>/<project>.mp4`: the same request `reel render` makes, so the
    /// button and the agent share one code path. Blocks until the file is written.
    pub fn render(&self, project: &str, options: &RenderOptions) -> Result<RenderDone> {
        let mut body = serde_json::json!({
            "project": project,
            "fps": options.fps,
            "crop": options.crop,
        });
        if let Some(out) = &options.out {
            body["out"] = serde_json::json!(out);
        }
        if let Some(aspect) = options.aspect {
            body["aspect"] = serde_json::json!(aspect);
        }
        let value = self.post("/__ctl/render", body)?;
        let file = value["file"]
            .as_str()
            .ok_or_else(|| anyhow!("render answered without a file"))?;
        let problems = serde_json::from_value(value["problems"].clone()).unwrap_or_default();
        Ok(RenderDone {
            file: PathBuf::from(file),
            problems,
        })
    }

    /// Draw the project's cover (`.reel/cover.png`). Blocks until it is written.
    pub fn cover(&self, project: &str) -> Result<PathBuf> {
        let value = self.post(
            "/__ctl/peek",
            serde_json::json!({ "project": project, "cover": true }),
        )?;
        let file = value["sheet"]
            .as_str()
            .ok_or_else(|| anyhow!("cover answered without a file"))?;
        Ok(PathBuf::from(file))
    }

    /// Whether capture job `id` is still registered (not yet finished, failed or timed out).
    pub fn job_active(&self, id: u64) -> bool {
        self.control
            .lock()
            .expect("server control lock")
            .jobs
            .contains_key(&id)
    }

    /// Frames written and frames expected by the render in flight, once it has planned them.
    pub fn render_progress(&self) -> Option<(usize, usize)> {
        let control = self.control.lock().expect("server control lock");
        control
            .jobs
            .values()
            .find(|job| matches!(job.kind, JobKind::Render) && job.expected > 0)
            .map(|job| (job.written, job.expected))
    }

    pub fn capture_url(&self, project: &str, job: u64) -> String {
        format!(
            "{}/p/{}/index.html?reel-capture={job}",
            self.base_url,
            encode_segment(project)
        )
    }

    /// Stop work in flight when the application is quitting.
    pub fn shutdown(&self) {
        let mut control = self.control.lock().expect("server control lock");
        for (_, mut job) in control.jobs.drain() {
            if let Some((mut child, _)) = job.ffmpeg.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = job.done_tx.send(Err(anyhow!("lumireel is quitting")));
        }
    }
}

type DoneSender = mpsc::Sender<Result<JobDone>>;

struct Job {
    project_dir: PathBuf,
    kind: JobKind,
    done_tx: DoneSender,
    width: usize,
    height: usize,
    times: Vec<f64>,
    passes: usize,
    expected: usize,
    frames: HashMap<(usize, usize), Vec<u8>>,
    problems: Vec<Problem>,
    ffmpeg: Option<(Child, ChildStdin)>,
    written: usize,
    render_out: PathBuf,
    fps: f64,
    aspect: Option<[u32; 2]>,
    crop: bool,
}

enum JobKind {
    Peek(PeekSpec),
    Render,
}

#[derive(Default)]
struct Control {
    next_id: u64,
    jobs: HashMap<u64, Job>,
}

pub fn start(root: PathBuf, token: String) -> Result<(ServerHandle, flume::Receiver<CaptureJob>)> {
    let server = Server::http("127.0.0.1:0").map_err(|e| anyhow!("starting local server: {e}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| anyhow!("server did not bind to TCP"))?
        .port();
    let control = Arc::new(Mutex::new(Control::default()));
    let (job_tx, job_rx) = flume::unbounded();

    let handle = ServerHandle {
        port,
        token: token.clone(),
        base_url: format!("http://127.0.0.1:{port}"),
        control: control.clone(),
    };

    let root = Arc::new(root);
    thread::spawn(move || {
        for request in server.incoming_requests() {
            let root = root.clone();
            let control = control.clone();
            let token = token.clone();
            let job_tx = job_tx.clone();
            thread::spawn(move || {
                if let Err(error) = handle_request(request, &root, &control, &token, port, &job_tx)
                {
                    log::error!("request failed: {error:#}");
                }
            });
        }
    });

    Ok((handle, job_rx))
}

fn handle_request(
    request: Request,
    root: &Path,
    control: &Arc<Mutex<Control>>,
    token: &str,
    port: u16,
    job_tx: &flume::Sender<CaptureJob>,
) -> Result<()> {
    if !valid_host(&request, port) {
        return respond_json(
            request,
            403,
            &serde_json::json!({"error": "bad Host header"}),
        );
    }

    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (url, String::new()),
    };
    let query = parse_query(&query);

    match (request.method(), path.as_str()) {
        (Method::Get, "/__reel/host.js") => serve_bytes(
            request,
            HOST_JS.as_bytes(),
            "text/javascript; charset=utf-8",
            None,
        ),
        (Method::Get, "/__reel/kit.js") => serve_bytes(
            request,
            KIT_JS.as_bytes(),
            "text/javascript; charset=utf-8",
            None,
        ),
        (Method::Get, "/__reel/player") => serve_player(request, root, &query),
        (Method::Post, "/__ctl/peek") => {
            if !authorized(&request, token) {
                return respond_json(request, 401, &serde_json::json!({"error": "unauthorized"}));
            }
            start_look(request, root, control, job_tx)
        }
        (Method::Post, "/__ctl/render") => {
            if !authorized(&request, token) {
                return respond_json(request, 401, &serde_json::json!({"error": "unauthorized"}));
            }
            start_render(request, root, control, job_tx)
        }
        (Method::Get, "/__ctl/job") => {
            if !authorized(&request, token) {
                return respond_json(request, 401, &serde_json::json!({"error": "unauthorized"}));
            }
            job_info(request, control, &query)
        }
        (Method::Post, "/__ctl/plan") => {
            if !authorized(&request, token) {
                return respond_json(request, 401, &serde_json::json!({"error": "unauthorized"}));
            }
            plan_job(request, control, &query)
        }
        (Method::Post, "/__ctl/frame") => {
            if !authorized(&request, token) {
                return respond_json(request, 401, &serde_json::json!({"error": "unauthorized"}));
            }
            receive_frame(request, control, &query)
        }
        (Method::Post, "/__ctl/done") => {
            if !authorized(&request, token) {
                return respond_json(request, 401, &serde_json::json!({"error": "unauthorized"}));
            }
            finish_job(request, control, &query)
        }
        (Method::Get, _) if path.starts_with("/p/") => {
            serve_project_file(request, root, &path, token)
        }
        _ => respond_json(request, 404, &serde_json::json!({"error": "not found"})),
    }
}

fn valid_host(request: &Request, port: u16) -> bool {
    let Some(host) = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Host"))
        .map(|h| h.value.as_str())
    else {
        return false;
    };
    host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}")
}

fn owned_header(name: &str) -> bool {
    name.eq_ignore_ascii_case("Authorization")
}

fn authorized(request: &Request, token: &str) -> bool {
    let expected = format!("Bearer {token}");
    request
        .headers()
        .iter()
        .find(|h| owned_header(h.field.as_str().as_ref()))
        .map(|h| h.value.as_str() == expected)
        .unwrap_or(false)
}

#[derive(Deserialize)]
struct PeekRequest {
    project: String,
    #[serde(default)]
    t: Option<f64>,
    #[serde(default)]
    range: Option<[f64; 2]>,
    #[serde(default)]
    cover: bool,
}

fn start_look(
    mut request: Request,
    root: &Path,
    control: &Arc<Mutex<Control>>,
    job_tx: &flume::Sender<CaptureJob>,
) -> Result<()> {
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body)?;
    let look: PeekRequest =
        serde_json::from_str(&body).map_err(|e| anyhow!("invalid look request: {e}"))?;
    let project_dir = project_dir(root, &look.project)?;
    let spec = if look.cover {
        PeekSpec::Cover
    } else if let Some(t) = look.t {
        PeekSpec::Single(t)
    } else if let Some([a, b]) = look.range {
        PeekSpec::Range(a.min(b), a.max(b))
    } else {
        PeekSpec::All
    };

    let (tx, rx) = mpsc::channel();
    let id = {
        let mut control = control.lock().expect("control lock");
        control.next_id += 1;
        let id = control.next_id;
        control.jobs.insert(
            id,
            Job {
                project_dir,
                kind: JobKind::Peek(spec.clone()),
                done_tx: tx,
                width: 0,
                height: 0,
                times: Vec::new(),
                passes: 1,
                expected: 0,
                frames: HashMap::new(),
                problems: Vec::new(),
                ffmpeg: None,
                written: 0,
                render_out: PathBuf::new(),
                fps: 0.0,
                aspect: None,
                crop: false,
            },
        );
        id
    };
    let job = CaptureJob {
        id,
        project: look.project.clone(),
    };
    if job_tx.send(job).is_err() {
        control.lock().expect("control lock").jobs.remove(&id);
        return respond_json(
            request,
            500,
            &serde_json::json!({"error": "lumireel is not running"}),
        );
    }

    match rx.recv_timeout(Duration::from_secs(180)) {
        Ok(Ok(JobDone::Peek(done))) => respond_json(
            request,
            200,
            &serde_json::json!({
                "sheet": done.sheet,
                "frames": done.frames,
                "problems": done.problems,
            }),
        ),
        Ok(Ok(_)) => respond_json(
            request,
            500,
            &serde_json::json!({"error": "wrong job result"}),
        ),
        Ok(Err(error)) => respond_json(
            request,
            500,
            &serde_json::json!({"error": format!("{error:#}")}),
        ),
        Err(error) => {
            let mut control = control.lock().expect("control lock");
            if let Some(mut job) = control.jobs.remove(&id) {
                if let Some((mut child, _)) = job.ffmpeg.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            respond_json(
                request,
                504,
                &serde_json::json!({"error": format!("capture timed out: {error}")}),
            )
        }
    }
}

#[derive(Deserialize)]
struct RenderRequest {
    project: String,
    #[serde(default = "default_fps")]
    fps: f64,
    #[serde(default)]
    out: Option<String>,
    /// Target aspect, e.g. `[9, 16]`; absent keeps the film's own frame.
    #[serde(default)]
    aspect: Option<[u32; 2]>,
    /// Fill the target frame by cropping, instead of fitting with black bars.
    #[serde(default)]
    crop: bool,
}

/// How the app asks for an export.
#[derive(Clone, Debug, Default)]
pub struct RenderOptions {
    pub fps: f64,
    /// Absolute path of the MP4; `None` is `<project>/<project>.mp4`.
    pub out: Option<PathBuf>,
    pub aspect: Option<[u32; 2]>,
    pub crop: bool,
}

/// The output size for `aspect`, keeping the film's short side (1920×1080 at 9:16 is 1080×1920),
/// even-sized for H.264. `None` when that is the film's own size.
pub fn export_size(width: u32, height: u32, aspect: Option<[u32; 2]>) -> Option<(u32, u32)> {
    let [aw, ah] = aspect?;
    if aw == 0 || ah == 0 || width == 0 || height == 0 {
        return None;
    }
    let short = width.min(height) as f64;
    let even = |v: f64| ((v / 2.0).round() as u32 * 2).max(2);
    let size = if aw >= ah {
        (even(short * aw as f64 / ah as f64), even(short))
    } else {
        (even(short), even(short * ah as f64 / aw as f64))
    };
    (size != (width, height)).then_some(size)
}

/// The ffmpeg video filter that puts a `width`×`height` film into the export frame: scaled to fit
/// with black bars, or scaled to fill and cropped. The film itself is never re-laid-out.
fn video_filter(width: u32, height: u32, aspect: Option<[u32; 2]>, crop: bool) -> Option<String> {
    let (w, h) = export_size(width, height, aspect)?;
    Some(if crop {
        format!(
            "scale={w}:{h}:force_original_aspect_ratio=increase:flags=lanczos,crop={w}:{h},setsar=1"
        )
    } else {
        format!(
            "scale={w}:{h}:force_original_aspect_ratio=decrease:flags=lanczos,\
             pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1"
        )
    })
}

fn default_fps() -> f64 {
    30.0
}

fn start_render(
    mut request: Request,
    root: &Path,
    control: &Arc<Mutex<Control>>,
    job_tx: &flume::Sender<CaptureJob>,
) -> Result<()> {
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body)?;
    let render: RenderRequest =
        serde_json::from_str(&body).map_err(|e| anyhow!("invalid render request: {e}"))?;
    let project_dir = project_dir(root, &render.project)?;
    let out = match &render.out {
        Some(out) => {
            let path = PathBuf::from(out);
            if path.is_absolute() {
                path
            } else {
                project_dir.join(path)
            }
        }
        None => project_dir.join(format!("{}.mp4", render.project)),
    };
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)?;
    }

    let (tx, rx) = mpsc::channel();
    let id = {
        let mut control = control.lock().expect("control lock");
        control.next_id += 1;
        let id = control.next_id;
        control.jobs.insert(
            id,
            Job {
                project_dir,
                kind: JobKind::Render,
                done_tx: tx,
                width: 0,
                height: 0,
                times: Vec::new(),
                passes: 1,
                expected: 0,
                frames: HashMap::new(),
                problems: Vec::new(),
                ffmpeg: None,
                written: 0,
                render_out: out.clone(),
                fps: render.fps.max(1.0),
                aspect: render.aspect,
                crop: render.crop,
            },
        );
        id
    };
    let job = CaptureJob {
        id,
        project: render.project.clone(),
    };
    if job_tx.send(job).is_err() {
        control.lock().expect("control lock").jobs.remove(&id);
        return respond_json(
            request,
            500,
            &serde_json::json!({"error": "lumireel is not running"}),
        );
    }

    match rx.recv_timeout(Duration::from_secs(3600)) {
        Ok(Ok(JobDone::Render(done))) => respond_json(
            request,
            200,
            &serde_json::json!({"file": done.file, "problems": done.problems}),
        ),
        Ok(Ok(_)) => respond_json(
            request,
            500,
            &serde_json::json!({"error": "wrong job result"}),
        ),
        Ok(Err(error)) => respond_json(
            request,
            500,
            &serde_json::json!({"error": format!("{error:#}")}),
        ),
        Err(error) => {
            let mut control = control.lock().expect("control lock");
            if let Some(mut job) = control.jobs.remove(&id) {
                if let Some((mut child, _)) = job.ffmpeg.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            respond_json(
                request,
                504,
                &serde_json::json!({"error": format!("render timed out: {error}")}),
            )
        }
    }
}

fn job_info(
    request: Request,
    control: &Arc<Mutex<Control>>,
    query: &HashMap<String, String>,
) -> Result<()> {
    let Some(id) = query.get("id").and_then(|id| id.parse::<u64>().ok()) else {
        return respond_json(request, 400, &serde_json::json!({"error": "missing job"}));
    };
    let control = control.lock().expect("control lock");
    let Some(job) = control.jobs.get(&id) else {
        return respond_json(request, 404, &serde_json::json!({"error": "unknown job"}));
    };
    log::info!("capture job {id} requested");
    let payload = match &job.kind {
        JobKind::Peek(spec) => serde_json::json!({
            "id": id,
            "kind": "look",
            // A cover is a thumbnail, not a check: no second, shuffled pass.
            "passes": if matches!(spec, PeekSpec::Cover) { 1 } else { 2 },
            "spec": spec_json(spec),
        }),
        JobKind::Render => serde_json::json!({
            "id": id,
            "kind": "render",
            "passes": 1,
            "fps": job.fps,
        }),
    };
    respond_json(request, 200, &payload)
}

#[derive(Deserialize)]
struct PlanRequest {
    width: usize,
    height: usize,
    #[serde(default)]
    times: Vec<f64>,
    #[serde(default)]
    passes: usize,
    #[serde(default)]
    frames: usize,
    #[serde(default)]
    fps: f64,
    #[serde(default)]
    sounds: Vec<crate::mix::Sound>,
}

fn plan_job(
    mut request: Request,
    control: &Arc<Mutex<Control>>,
    query: &HashMap<String, String>,
) -> Result<()> {
    let Some(id) = query.get("job").and_then(|id| id.parse::<u64>().ok()) else {
        return respond_json(request, 400, &serde_json::json!({"error": "missing job"}));
    };
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body)?;
    let plan: PlanRequest =
        serde_json::from_str(&body).map_err(|e| anyhow!("invalid plan: {e}"))?;
    if plan.width == 0 || plan.height == 0 {
        return respond_json(
            request,
            400,
            &serde_json::json!({"error": "bad frame size"}),
        );
    }

    let mut control = control.lock().expect("control lock");
    let Some(job) = control.jobs.get_mut(&id) else {
        return respond_json(request, 404, &serde_json::json!({"error": "unknown job"}));
    };
    job.width = plan.width;
    job.height = plan.height;
    match &job.kind {
        JobKind::Peek(_) => {
            let passes = plan.passes.max(1);
            job.times = plan.times;
            job.passes = passes;
            job.expected = job.times.len() * passes;
            respond_json(request, 200, &serde_json::json!({"ok": true}))
        }
        JobKind::Render => {
            job.fps = if plan.fps > 0.0 { plan.fps } else { job.fps };
            job.expected = plan.frames;
            let mut sounds = plan.sounds;
            let mut kept = Vec::new();
            for sound in sounds.drain(..) {
                if job.project_dir.join(&sound.src).is_file() {
                    kept.push(sound);
                } else {
                    job.problems
                        .push(format!("sound file is missing: {}", sound.src).into());
                }
            }
            let duration = if job.fps > 0.0 {
                plan.frames as f64 / job.fps
            } else {
                0.0
            };
            let mix = crate::mix::plan(&kept, duration);
            match start_ffmpeg(job, &mix) {
                Ok(()) => respond_json(request, 200, &serde_json::json!({"ok": true})),
                Err(error) => {
                    let message = format!("could not start ffmpeg: {error:#}");
                    job.problems.push(message.clone().into());
                    respond_json(request, 500, &serde_json::json!({"error": message}))
                }
            }
        }
    }
}

fn start_ffmpeg(job: &mut Job, mix: &crate::mix::MixPlan) -> Result<()> {
    if !has_ffmpeg() {
        bail!("ffmpeg was not found on PATH");
    }
    let fps = format!("{}", job.fps.max(1.0));
    let size = format!("{}x{}", job.width, job.height);
    let mut command = Command::new("ffmpeg");
    command
        .arg("-y")
        .args(["-loglevel", "error", "-nostats"])
        .args([
            "-f", "rawvideo", "-pix_fmt", "rgba", "-s", &size, "-r", &fps, "-i", "-",
        ]);
    for input in &mix.inputs {
        command.arg("-i").arg(input);
    }
    let video = video_filter(job.width as u32, job.height as u32, job.aspect, job.crop);
    let graph = [
        video.as_ref().map(|filter| format!("[0:v]{filter}[video]")),
        mix.filter.clone(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if !graph.is_empty() {
        command.arg("-filter_complex").arg(graph.join(";"));
    }
    command.args(["-map", if video.is_some() { "[video]" } else { "0:v" }]);
    if mix.filter.is_some() {
        command.args(["-map", "[audio]"]);
    }
    command.args([
        "-c:v",
        "libx264",
        "-pix_fmt",
        "yuv420p",
        "-preset",
        "medium",
        "-crf",
        "18",
        "-movflags",
        "+faststart",
    ]);
    if mix.filter.is_some() {
        command.args(["-c:a", "aac", "-b:a", "192k"]);
    }
    command
        .arg(&job.render_out)
        .current_dir(&job.project_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("spawning ffmpeg")?;
    let stdin = child.stdin.take().expect("ffmpeg stdin");
    job.ffmpeg = Some((child, stdin));
    Ok(())
}

fn spec_json(spec: &PeekSpec) -> serde_json::Value {
    match spec {
        PeekSpec::All => serde_json::json!({"type": "all"}),
        PeekSpec::Single(t) => serde_json::json!({"type": "single", "t": t}),
        PeekSpec::Range(a, b) => serde_json::json!({"type": "range", "start": a, "end": b}),
        PeekSpec::Cover => serde_json::json!({"type": "cover"}),
    }
}

fn has_ffmpeg() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn receive_frame(
    mut request: Request,
    control: &Arc<Mutex<Control>>,
    query: &HashMap<String, String>,
) -> Result<()> {
    let Some(id) = query.get("job").and_then(|id| id.parse::<u64>().ok()) else {
        return respond_json(request, 400, &serde_json::json!({"error": "missing job"}));
    };
    let index = query
        .get("i")
        .and_then(|i| i.parse::<usize>().ok())
        .ok_or_else(|| anyhow!("missing frame index"))?;
    let pass = query
        .get("pass")
        .and_then(|p| p.parse::<usize>().ok())
        .unwrap_or(0);

    let mut body = Vec::new();
    request.as_reader().read_to_end(&mut body)?;

    let mut control = control.lock().expect("control lock");
    let Some(job) = control.jobs.get_mut(&id) else {
        return respond_json(request, 404, &serde_json::json!({"error": "unknown job"}));
    };
    let expected_len = job.width * job.height * 4;
    if body.len() != expected_len {
        job.problems.push(
            format!(
                "frame {index} has {} bytes, expected {expected_len}",
                body.len()
            )
            .into(),
        );
        return respond_json(
            request,
            400,
            &serde_json::json!({"error": "bad frame size"}),
        );
    }

    match &job.kind {
        JobKind::Peek(_) => {
            job.frames.insert((pass, index), body);
            respond_json(request, 200, &serde_json::json!({"ok": true}))
        }
        JobKind::Render => {
            if index != job.written {
                job.problems.push(
                    format!(
                        "frame {index} arrived out of order (expected {})",
                        job.written
                    )
                    .into(),
                );
                return respond_json(request, 409, &serde_json::json!({"error": "out of order"}));
            }
            let Some((_, stdin)) = job.ffmpeg.as_mut() else {
                return respond_json(
                    request,
                    500,
                    &serde_json::json!({"error": "ffmpeg not started"}),
                );
            };
            if let Err(error) = stdin.write_all(&body) {
                job.problems
                    .push(format!("ffmpeg write failed: {error}").into());
                return respond_json(request, 500, &serde_json::json!({"error": "write failed"}));
            }
            job.written += 1;
            let _ = stdin.flush();
            respond_json(request, 200, &serde_json::json!({"ok": true}))
        }
    }
}

#[derive(Deserialize)]
struct DoneRequest {
    #[serde(default)]
    problems: Vec<ProblemIn>,
    /// The film's length, reported by the page; kept for the project list.
    #[serde(default)]
    duration: Option<f64>,
}

#[derive(Deserialize)]
struct ProblemIn {
    message: String,
    #[serde(default)]
    source: Option<String>,
}

fn finish_job(
    mut request: Request,
    control: &Arc<Mutex<Control>>,
    query: &HashMap<String, String>,
) -> Result<()> {
    let Some(id) = query.get("job").and_then(|id| id.parse::<u64>().ok()) else {
        return respond_json(request, 400, &serde_json::json!({"error": "missing job"}));
    };
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body)?;
    let done: DoneRequest = serde_json::from_str(&body).unwrap_or(DoneRequest {
        problems: Vec::new(),
        duration: None,
    });
    let DoneRequest { problems, duration } = done;

    let mut control = control.lock().expect("control lock");
    let Some(mut job) = control.jobs.remove(&id) else {
        return respond_json(request, 404, &serde_json::json!({"error": "unknown job"}));
    };
    for problem in problems {
        job.problems.push(Problem {
            message: problem.message,
            source: problem.source,
        });
    }
    if let Some(duration) = duration
        && let Err(error) = crate::library::save_duration(&job.project_dir, duration)
    {
        log::warn!(
            "saving duration for {}: {error:#}",
            job.project_dir.display()
        );
    }

    let result = match &job.kind {
        JobKind::Peek(_) => finish_look(&mut job),
        JobKind::Render => finish_render(&mut job),
    };
    let _ = job.done_tx.send(result);
    respond_json(request, 200, &serde_json::json!({"ok": true}))
}

/// Width of the cover thumbnail; the list shows it at a fraction of this.
const COVER_WIDTH: u32 = 320;

fn finish_look(job: &mut Job) -> Result<JobDone> {
    if matches!(job.kind, JobKind::Peek(PeekSpec::Cover)) {
        return finish_cover(job);
    }
    let peek_dir = job.project_dir.join(".reel").join("peek");
    let frames_dir = peek_dir.join("frames");
    fs::create_dir_all(&frames_dir)?;

    if job.expected == 0 {
        job.problems
            .push("the film did not report any frames".into());
    }

    let mut paths = Vec::new();
    let mut images = Vec::new();
    for (index, t) in job.times.iter().enumerate() {
        let Some(data) = job.frames.get(&(0, index)) else {
            job.problems
                .push(format!("frame at t={t:.3} was not captured").into());
            continue;
        };
        let image = RgbaImage::from_raw(job.width as u32, job.height as u32, data.clone())
            .ok_or_else(|| anyhow!("captured frame has the wrong size"))?;
        let path = frames_dir.join(format!("frame-{index:02}-{t:.3}.png"));
        image
            .save_with_format(&path, image::ImageFormat::Png)
            .with_context(|| format!("writing {}", path.display()))?;
        paths.push(path);
        images.push((*t, image));

        if let Some(other) = job.frames.get(&(1, index)) {
            let difference = mean_difference(data, other);
            if difference > 1.0 / 255.0 {
                job.problems.push(
                    format!(
                        "t={t:.3}: the picture depends on the order it was drawn in \
                         (mean difference {:.4})",
                        difference
                    )
                    .into(),
                );
            }
        }
    }

    if images.is_empty() {
        dedupe_problems(&mut job.problems);
        let details = job
            .problems
            .iter()
            .map(|problem| problem.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        if details.is_empty() {
            bail!("no frames were captured");
        }
        bail!("no frames were captured: {details}");
    }
    dedupe_problems(&mut job.problems);
    let sheet = contact_sheet(&images, job.width, job.height)?;
    let sheet_path = peek_dir.join("sheet.png");
    sheet
        .save_with_format(&sheet_path, image::ImageFormat::Png)
        .with_context(|| format!("writing {}", sheet_path.display()))?;

    Ok(JobDone::Peek(PeekDone {
        sheet: absolute(&sheet_path),
        frames: paths.into_iter().map(|p| absolute(&p)).collect(),
        problems: job.problems.clone(),
    }))
}

/// Save the single captured frame small, apart from `reel peek`'s sheet so the agent's is untouched.
fn finish_cover(job: &mut Job) -> Result<JobDone> {
    let data = job
        .frames
        .get(&(0, 0))
        .ok_or_else(|| anyhow!("the cover frame was not captured"))?;
    let image = RgbaImage::from_raw(job.width as u32, job.height as u32, data.clone())
        .ok_or_else(|| anyhow!("captured frame has the wrong size"))?;
    let height = (COVER_WIDTH as u64 * job.height as u64 / job.width.max(1) as u64).max(1) as u32;
    let small = image::imageops::resize(
        &image,
        COVER_WIDTH,
        height,
        image::imageops::FilterType::Triangle,
    );
    let dir = job.project_dir.join(".reel");
    fs::create_dir_all(&dir)?;
    // Written whole then renamed, so the list never reads a half-written PNG.
    let path = dir.join("cover.png");
    let partial = dir.join("cover.png.partial");
    small
        .save_with_format(&partial, image::ImageFormat::Png)
        .with_context(|| format!("writing {}", partial.display()))?;
    fs::rename(&partial, &path)?;
    Ok(JobDone::Peek(PeekDone {
        sheet: absolute(&path),
        frames: Vec::new(),
        problems: job.problems.clone(),
    }))
}

fn finish_render(job: &mut Job) -> Result<JobDone> {
    let Some((child, stdin)) = job.ffmpeg.take() else {
        dedupe_problems(&mut job.problems);
        let details = job
            .problems
            .iter()
            .map(|problem| problem.message.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        if details.is_empty() {
            bail!("ffmpeg was never started");
        }
        bail!("ffmpeg was never started: {details}");
    };
    drop(stdin);
    let incomplete = job.written != job.expected;
    if incomplete {
        job.problems.push(
            format!(
                "only {} of {} frames were captured",
                job.written, job.expected
            )
            .into(),
        );
    }
    dedupe_problems(&mut job.problems);
    let output = child.wait_with_output().context("waiting for ffmpeg")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        job.problems
            .push(format!("ffmpeg failed: {}", stderr.trim()).into());
    }
    if !output.status.success() || incomplete {
        let _ = fs::remove_file(&job.render_out);
    }
    Ok(JobDone::Render(RenderDone {
        file: absolute(&job.render_out),
        problems: job.problems.clone(),
    }))
}

fn dedupe_problems(problems: &mut Vec<Problem>) {
    let mut seen = std::collections::HashSet::new();
    problems.retain(|problem| seen.insert(problem.message.clone()));
}

fn mean_difference(a: &[u8], b: &[u8]) -> f64 {
    if a.is_empty() || a.len() != b.len() {
        return 1.0;
    }
    let total: u64 = a.iter().zip(b).map(|(x, y)| x.abs_diff(*y) as u64).sum();
    total as f64 / a.len() as f64 / 255.0
}

fn contact_sheet(frames: &[(f64, RgbaImage)], width: usize, height: usize) -> Result<RgbaImage> {
    if frames.len() == 1 {
        return Ok(frames[0].1.clone());
    }
    let columns = 4usize;
    let rows = frames.len().div_ceil(columns);
    let thumb_w = 480usize;
    let thumb_h = ((height as f64 / width as f64) * thumb_w as f64).round() as usize;
    let mut sheet = RgbaImage::from_pixel(
        (columns * thumb_w) as u32,
        (rows * thumb_h) as u32,
        image::Rgba([8, 8, 10, 255]),
    );
    for (index, (t, frame)) in frames.iter().enumerate() {
        let col = index % columns;
        let row = index / columns;
        let thumb = image::imageops::resize(
            frame,
            thumb_w as u32,
            thumb_h as u32,
            image::imageops::FilterType::Triangle,
        );
        image::imageops::replace(
            &mut sheet,
            &thumb,
            (col * thumb_w) as i64,
            (row * thumb_h) as i64,
        );
        draw_label(
            &mut sheet,
            (col * thumb_w + 8) as i64,
            (row * thumb_h + 8) as i64,
            &format!("{t:.3}s"),
        );
    }
    Ok(sheet)
}

fn draw_label(image: &mut RgbaImage, x: i64, y: i64, text: &str) {
    let scale = 2i64;
    let char_w = 6 * scale;
    let padding = 4i64;
    let box_w = text.chars().count() as i64 * char_w + padding * 2;
    let box_h = 7 * scale + padding * 2;
    for py in 0..box_h {
        for px in 0..box_w {
            let ix = x + px;
            let iy = y + py;
            if ix >= 0 && iy >= 0 && ix < image.width() as i64 && iy < image.height() as i64 {
                let pixel = image.get_pixel_mut(ix as u32, iy as u32);
                let alpha = 0.6;
                for channel in 0..3 {
                    pixel[channel] = (pixel[channel] as f64 * (1.0 - alpha) + 8.0 * alpha) as u8;
                }
            }
        }
    }
    for (index, ch) in text.chars().enumerate() {
        let glyph = glyph(ch);
        if glyph.is_none() {
            continue;
        }
        let glyph = glyph.unwrap();
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..5 {
                if bits & (1 << (4 - col)) == 0 {
                    continue;
                }
                for sy in 0..scale {
                    for sx in 0..scale {
                        let ix = x + padding + index as i64 * char_w + col * scale + sx;
                        let iy = y + padding + row as i64 * scale + sy;
                        if ix >= 0
                            && iy >= 0
                            && ix < image.width() as i64
                            && iy < image.height() as i64
                        {
                            image.put_pixel(
                                ix as u32,
                                iy as u32,
                                image::Rgba([255, 255, 255, 255]),
                            );
                        }
                    }
                }
            }
        }
    }
}

fn glyph(ch: char) -> Option<[u8; 7]> {
    let rows = match ch {
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ],
        '.' => [0, 0, 0, 0, 0, 0b00110, 0b00110],
        ':' => [0, 0b00110, 0b00110, 0, 0b00110, 0b00110, 0],
        's' => [0, 0, 0b01111, 0b10000, 0b01110, 0b00001, 0b11110],
        '-' => [0, 0, 0, 0b11111, 0, 0, 0],
        _ => return None,
    };
    Some(rows)
}

fn serve_player(request: Request, root: &Path, query: &HashMap<String, String>) -> Result<()> {
    let project = query.get("project").map(String::as_str).unwrap_or_default();
    let src = query.get("src").map(String::as_str).unwrap_or_default();
    if project.is_empty() || src.is_empty() || !safe_relative(Path::new(src)) {
        return respond_json(
            request,
            400,
            &serde_json::json!({"error": "invalid player request"}),
        );
    }
    let Ok(dir) = project_dir(root, project) else {
        return respond_json(request, 404, &serde_json::json!({"error": "not found"}));
    };
    let Ok(file) = dir.join(src).canonicalize() else {
        return respond_json(request, 404, &serde_json::json!({"error": "not found"}));
    };
    if !file.starts_with(&dir) || !file.is_file() {
        return respond_json(request, 403, &serde_json::json!({"error": "forbidden"}));
    }
    let ext = file
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_video = matches!(ext.as_str(), "mp4" | "m4v" | "mov" | "webm" | "mkv" | "avi");
    let is_audio = matches!(
        ext.as_str(),
        "mp3" | "wav" | "m4a" | "aac" | "ogg" | "oga" | "flac" | "opus" | "aiff" | "aif"
    );
    if !is_video && !is_audio {
        return respond_json(request, 400, &serde_json::json!({"error": "not playable"}));
    }
    let encoded = src
        .split('/')
        .filter(|part| !part.is_empty())
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/");
    let media_url = format!("/p/{}/{}", encode_segment(project), encoded);
    let name = file
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let html = if is_video {
        player_video(&name, &media_url)
    } else {
        player_audio(&name, &media_url)
    };
    serve_bytes(request, html.as_bytes(), "text/html; charset=utf-8", None)
}

fn player_video(name: &str, media_url: &str) -> String {
    format!(
        r#"<!doctype html>
<meta charset="utf-8">
<title>{title}</title>
<style>
html,body{{margin:0;height:100%;background:#000;overflow:hidden}}
video{{display:block;width:100%;height:100%;object-fit:contain;background:#000}}
</style>
<video src="{url}" controls autoplay playsinline></video>
<script>
const media = document.querySelector('video,audio');
if (media) media.play().catch(() => {{}});
</script>"#,
        title = escape_html(name),
        url = media_url
    )
}

fn player_audio(name: &str, media_url: &str) -> String {
    format!(
        r#"<!doctype html>
<meta charset="utf-8">
<title>{title}</title>
<style>
html,body{{margin:0;height:100%;background:#0b0b0e;color:#e8e8ec;overflow:hidden;font:13px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}}
body{{display:flex;align-items:center;justify-content:center}}
.card{{width:min(520px,80%);padding:28px;border:1px solid #2a2a31;border-radius:12px;background:#15151b;text-align:center}}
.badge{{width:48px;height:48px;margin:0 auto 14px;border-radius:50%;background:#26262f;display:flex;align-items:center;justify-content:center;font-size:22px}}
.name{{font-weight:600;font-size:15px;word-break:break-all}}
.hint{{margin:2px 0 18px;color:#8a8a96;font-size:11px}}
audio{{width:100%}}
</style>
<div class="card">
  <div class="badge">&#9835;</div>
  <div class="name">{title}</div>
  <div class="hint">{hint}</div>
  <audio src="{url}" controls autoplay></audio>
</div>
<script>
const media = document.querySelector('audio');
if (media) media.play().catch(() => {{}});
</script>"#,
        title = escape_html(name),
        hint = t!("preview.audio_asset"),
        url = media_url
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn serve_project_file(request: Request, root: &Path, path: &str, token: &str) -> Result<()> {
    let rest = path.trim_start_matches("/p/");
    let mut parts = rest.splitn(2, '/');
    let project = percent_decode(parts.next().unwrap_or_default());
    let file = parts.next().unwrap_or_default();
    if project.is_empty() || project == "." || project == ".." {
        return respond_json(request, 404, &serde_json::json!({"error": "not found"}));
    }
    let project_dir = root.join(project);
    let decoded = percent_decode(file);
    if !safe_relative(Path::new(&decoded)) {
        return respond_json(request, 403, &serde_json::json!({"error": "forbidden"}));
    }
    let target = project_dir.join(&decoded);
    let Ok(canonical) = target.canonicalize() else {
        return respond_json(request, 404, &serde_json::json!({"error": "not found"}));
    };
    let Ok(root_canonical) = project_dir.canonicalize() else {
        return respond_json(request, 404, &serde_json::json!({"error": "not found"}));
    };
    if !canonical.starts_with(&root_canonical) || !canonical.is_file() {
        return respond_json(request, 403, &serde_json::json!({"error": "forbidden"}));
    }

    let ext = canonical
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mime = mime_for(&ext);
    if ext == "html" || ext == "htm" {
        let mut html = fs::read_to_string(&canonical)?;
        html = inject_host(&html, token);
        return serve_bytes(request, html.as_bytes(), mime, Some(&canonical));
    }

    let data = fs::read(&canonical)?;
    serve_bytes(request, &data, mime, Some(&canonical))
}

fn inject_host(html: &str, token: &str) -> String {
    let injection = format!(
        "<script>window.__LUMIREEL_TOKEN__={:?};</script><script src=\"/__reel/host.js\"></script>",
        token
    );
    let lower = html.to_ascii_lowercase();
    if let Some(index) = lower.find("<head") {
        if let Some(end) = lower[index..].find('>') {
            let at = index + end + 1;
            let mut out = String::with_capacity(html.len() + injection.len());
            out.push_str(&html[..at]);
            out.push_str(&injection);
            out.push_str(&html[at..]);
            return out;
        }
    }
    format!("{injection}{html}")
}

fn safe_relative(path: &Path) -> bool {
    path.components().all(|component| {
        matches!(
            component,
            Component::Normal(_) | Component::CurDir | Component::RootDir
        )
    }) && !path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
}

fn project_dir(root: &Path, project: &str) -> Result<PathBuf> {
    if project.is_empty()
        || project.contains('/')
        || project.contains('\\')
        || project == "."
        || project == ".."
    {
        bail!("invalid project name");
    }
    let dir = root.join(project);
    let canonical = dir
        .canonicalize()
        .with_context(|| format!("project {} does not exist", project))?;
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    if !canonical.starts_with(&root) {
        bail!("invalid project name");
    }
    Ok(canonical)
}

fn serve_bytes(request: Request, data: &[u8], mime: &str, file: Option<&Path>) -> Result<()> {
    if let Some(range) = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Range"))
        .map(|h| h.value.as_str().to_string())
        .and_then(|value| parse_range(&value))
    {
        let len = data.len();
        let (start, end) = range;
        let end = end.min(len.saturating_sub(1));
        if start > end || start >= len {
            let mut response = Response::empty(StatusCode(416));
            response
                .add_header(Header::from_bytes("Content-Range", format!("bytes */{len}")).unwrap());
            request.respond(response)?;
            return Ok(());
        }
        let mut response =
            Response::from_data(data[start..=end].to_vec()).with_status_code(StatusCode(206));
        response.add_header(
            Header::from_bytes("Content-Range", format!("bytes {start}-{end}/{len}")).unwrap(),
        );
        add_headers(&mut response, mime, file, true);
        request.respond(response)?;
        return Ok(());
    }
    let mut response = Response::from_data(data.to_vec());
    add_headers(&mut response, mime, file, true);
    request.respond(response)?;
    Ok(())
}

fn add_headers(
    response: &mut Response<std::io::Cursor<Vec<u8>>>,
    mime: &str,
    file: Option<&Path>,
    ranges: bool,
) {
    response.add_header(Header::from_bytes("Content-Type", mime).unwrap());
    response.add_header(Header::from_bytes("Cache-Control", "no-store").unwrap());
    if ranges {
        response.add_header(Header::from_bytes("Accept-Ranges", "bytes").unwrap());
    }
    if let Some(file) = file {
        let _ = file;
    }
}

fn parse_range(value: &str) -> Option<(usize, usize)> {
    let value = value.strip_prefix("bytes=")?;
    let (a, b) = value.split_once('-')?;
    let start = a.parse().ok()?;
    let end = if b.is_empty() {
        usize::MAX
    } else {
        b.parse().ok()?
    };
    Some((start, end))
}

fn mime_for(ext: &str) -> &'static str {
    match ext {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "txt" | "md" => "text/plain; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ => "application/octet-stream",
    }
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            Some((percent_decode(key), percent_decode(value)))
        })
        .collect()
}

fn encode_segment(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(a), Some(b)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(a * 16 + b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn absolute(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn respond_json(request: Request, status: u16, value: &serde_json::Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    let mut response = Response::from_data(body).with_status_code(StatusCode(status));
    response.add_header(Header::from_bytes("Content-Type", "application/json").unwrap());
    response.add_header(Header::from_bytes("Cache-Control", "no-store").unwrap());
    request.respond(response)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_paths() {
        assert!(!safe_relative(Path::new("../etc/passwd")));
        assert!(!safe_relative(Path::new("a/../../etc/passwd")));
        assert!(safe_relative(Path::new("assets/a.png")));
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(percent_decode("a%20b%2Fc"), "a b/c");
    }

    #[test]
    fn export_keeps_the_short_side_and_reframes_without_relaying_out() {
        // The film's own frame needs no filter.
        assert_eq!(export_size(1920, 1080, None), None);
        assert_eq!(export_size(1920, 1080, Some([16, 9])), None);
        assert_eq!(export_size(1920, 1080, Some([9, 16])), Some((1080, 1920)));
        assert_eq!(export_size(1920, 1080, Some([1, 1])), Some((1080, 1080)));
        assert_eq!(export_size(1920, 1080, Some([4, 5])), Some((1080, 1350)));
        assert_eq!(export_size(1920, 1080, Some([4, 3])), Some((1440, 1080)));
        // Odd results are rounded to even, which yuv420p needs.
        assert_eq!(export_size(1001, 1001, Some([16, 9])), Some((1780, 1002)));

        let pad = video_filter(1920, 1080, Some([9, 16]), false).unwrap();
        assert!(
            pad.contains("force_original_aspect_ratio=decrease") && pad.contains("pad=1080:1920")
        );
        let crop = video_filter(1920, 1080, Some([9, 16]), true).unwrap();
        assert!(
            crop.contains("force_original_aspect_ratio=increase")
                && crop.contains("crop=1080:1920")
        );
        assert_eq!(video_filter(1920, 1080, None, true), None);
    }

    #[test]
    fn host_is_injected_before_other_scripts() {
        let html = "<!doctype html><html><head><script src=\"app.js\"></script></head></html>";
        let out = inject_host(html, "tok");
        assert!(out.contains("window.__LUMIREEL_TOKEN__"));
        assert!(out.find("host.js").unwrap() < out.find("app.js").unwrap());
    }
}
