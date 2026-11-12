//! Speech, images and video from the services in Settings (OpenAI-compatible, MiMo, Ark, MiniMax). The server runs
//! these for `reel speech|image|video`, so the agent gets files, never the API keys.

use std::{
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result, anyhow, bail};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::{Service, Services};

/// The body of `POST /__ctl/generate`.
#[derive(Debug, Deserialize)]
pub struct GenerateRequest {
    pub project: String,
    /// `speech`, `image` or `video`.
    pub kind: String,
    pub prompt: String,
    /// Where to write, relative to the project; default `assets/<kind>-<time>.<ext>`.
    #[serde(default)]
    pub out: Option<String>,
    #[serde(default)]
    pub voice: Option<String>,
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default)]
    pub seconds: Option<String>,
}

/// Video jobs run for minutes; past this the service is not going to answer.
const VIDEO_DEADLINE: Duration = Duration::from_secs(30 * 60);
const VIDEO_POLL: Duration = Duration::from_millis(if cfg!(test) { 10 } else { 10_000 });
/// Largest response read into memory: a long 1080p clip fits.
const MAX_BYTES: u64 = 1 << 30;

/// Generate what `request` asks for and write it into `project_dir`. Returns the file.
pub fn generate(
    services: &Services,
    request: &GenerateRequest,
    project_dir: &Path,
) -> Result<PathBuf> {
    if request.prompt.trim().is_empty() {
        bail!("the prompt is empty");
    }
    let service = match request.kind.as_str() {
        "speech" => &services.speech,
        "image" => &services.image,
        "video" => &services.video,
        other => bail!("unknown kind {other}: use speech, image or video"),
    };
    if service.url.trim().is_empty() {
        bail!(
            "the {} service is not set up: the person can add it in Settings → Generation services",
            request.kind
        );
    }
    let api = Api::new(service);
    let (bytes, ext) = match request.kind.as_str() {
        "speech" => {
            let bytes = api.speech(request)?;
            let ext = audio_ext(&bytes);
            (bytes, ext)
        }
        "image" => {
            let bytes = api.image(request)?;
            let ext = image_ext(&bytes);
            (bytes, ext)
        }
        _ => (api.video(request)?, "mp4"),
    };
    let file = out_path(project_dir, request, ext);
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&file, bytes).with_context(|| format!("writing {}", file.display()))?;
    Ok(file)
}

fn out_path(project_dir: &Path, request: &GenerateRequest, ext: &str) -> PathBuf {
    match &request.out {
        // Same rule as `reel render -o`: absolute paths are taken as given.
        Some(out) => project_dir.join(out),
        None => {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis());
            project_dir
                .join("assets")
                .join(format!("{}-{stamp}.{ext}", request.kind))
        }
    }
}

/// WAV or MP3 from the first bytes: Qwen only sends WAV.
fn audio_ext(bytes: &[u8]) -> &'static str {
    if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        "wav"
    } else {
        "mp3"
    }
}

/// PNG, JPEG or WebP from the first bytes; services differ in what they send.
fn image_ext(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0xFF, 0xD8]) {
        "jpg"
    } else if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "webp"
    } else {
        "png"
    }
}

struct Api<'a> {
    base: String,
    service: &'a Service,
    agent: ureq::Agent,
}

impl<'a> Api<'a> {
    fn new(service: &'a Service) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(600)))
            .build()
            .into();
        Self {
            base: service.url.trim().trim_end_matches('/').to_string(),
            service,
            agent,
        }
    }

    /// `body` with `model` added when one is set; without it the service picks its default.
    fn with_model(&self, mut body: Value) -> Value {
        let model = self.service.model.trim();
        if !model.is_empty() {
            body["model"] = json!(model);
        }
        body
    }

    fn post(&self, path: &str, body: &Value) -> Result<ureq::http::Response<ureq::Body>> {
        let key = self.service.api_key.trim();
        let request = self.agent.post(format!("{}{path}", self.base));
        let request = if self.service.provider() == "doubao" {
            // Doubao speech: the key and the model (its "resource id") go in their own headers.
            let id = format!("lumireel-{}", rand::random::<u64>());
            request
                .header("X-Api-Key", key)
                .header("X-Api-Resource-Id", self.service.model.trim())
                .header("X-Api-Request-Id", id)
        } else {
            request.header("Authorization", format!("Bearer {key}"))
        };
        let response = request
            .send_json(body)
            .with_context(|| format!("calling {}{path}", self.base))?;
        check(response, path)
    }

    fn get(&self, url: &str) -> Result<ureq::http::Response<ureq::Body>> {
        let mut request = self.agent.get(url);
        // Only our own service gets the key: a download link may point anywhere.
        if url.starts_with(&self.base) {
            request = request.header(
                "Authorization",
                format!("Bearer {}", self.service.api_key.trim()),
            );
        }
        let response = request.call().with_context(|| format!("calling {url}"))?;
        check(response, url)
    }

    fn speech(&self, request: &GenerateRequest) -> Result<Vec<u8>> {
        match self.service.provider() {
            "mimo" => {}
            "qwen" => return self.qwen_speech(request),
            "doubao" => return self.doubao_speech(request),
            _ => {
                let body = self.with_model(json!({
                    "input": request.prompt,
                    "voice": request.voice.as_deref().unwrap_or("alloy"),
                    "response_format": "mp3",
                }));
                return bytes(self.post("/audio/speech", &body)?);
            }
        }
        // MiMo speaks the assistant message of a chat completion and sends base64 back.
        let mut audio = json!({ "format": "mp3" });
        if let Some(voice) = &request.voice {
            audio["voice"] = json!(voice);
        }
        let body = self.with_model(json!({
            "messages": [{ "role": "assistant", "content": request.prompt }],
            "audio": audio,
        }));
        let answer = self.post_json("/chat/completions", &body)?;
        let Some(data) = answer["choices"][0]["message"]["audio"]["data"].as_str() else {
            bail!(
                "the speech service answered without audio: {}",
                snippet(&answer.to_string())
            )
        };
        // Some gateways prefix a data URI.
        let data = data.rsplit(',').next().unwrap_or(data);
        base64::engine::general_purpose::STANDARD
            .decode(data.trim())
            .context("the speech service sent bad base64")
    }

    /// Qwen TTS on Alibaba Model Studio (DashScope): answers a link to a WAV.
    fn qwen_speech(&self, request: &GenerateRequest) -> Result<Vec<u8>> {
        let body = self.with_model(json!({
            "input": {
                "text": request.prompt,
                "voice": request.voice.as_deref().unwrap_or("Cherry"),
            },
        }));
        let answer = self.post_json("/services/aigc/multimodal-generation/generation", &body)?;
        let audio = &answer["output"]["audio"];
        match (audio["url"].as_str(), audio["data"].as_str()) {
            (Some(url), _) if !url.is_empty() => bytes(self.get(url)?),
            (_, Some(data)) if !data.is_empty() => base64::engine::general_purpose::STANDARD
                .decode(data.trim())
                .context("the speech service sent bad base64"),
            _ => bail!(
                "the speech service answered without audio: {}",
                snippet(&answer.to_string())
            ),
        }
    }

    /// Doubao TTS: answers JSON objects one after another, each with a base64 piece of audio.
    fn doubao_speech(&self, request: &GenerateRequest) -> Result<Vec<u8>> {
        let body = json!({
            "user": { "uid": "lumireel" },
            "req_params": {
                "text": request.prompt,
                "speaker": request.voice.as_deref().unwrap_or("zh_female_vv_uranus_bigtts"),
                "audio_params": { "format": "mp3", "sample_rate": 24000 },
            },
        });
        let answer = bytes(self.post("/tts/unidirectional", &body)?)?;
        doubao_audio(&answer)
    }

    /// OpenAI `/images/generations` (also Volcengine Ark, SiliconFlow), or MiniMax.
    fn image(&self, request: &GenerateRequest) -> Result<Vec<u8>> {
        let minimax = self.service.provider() == "minimax";
        let mut body = self.with_model(json!({ "prompt": request.prompt }));
        if minimax {
            body["response_format"] = json!("base64");
        } else {
            body["n"] = json!(1);
        }
        if let Some(size) = &request.size {
            match dimensions(size) {
                Some((width, height)) if minimax => {
                    body["width"] = json!(width);
                    body["height"] = json!(height);
                }
                _ if minimax => {}
                _ => body["size"] = json!(size),
            }
        }
        let answer = if minimax {
            self.post_json("/image_generation", &body)?
        } else {
            self.post_json("/images/generations", &body)?
        };
        let data = answer["data"][0]["b64_json"]
            .as_str()
            .or(answer["data"]["image_base64"][0].as_str());
        if let Some(data) = data {
            return base64::engine::general_purpose::STANDARD
                .decode(data.trim())
                .context("the image service sent bad base64");
        }
        // SiliconFlow answers `images` instead of `data`.
        let url = answer["data"][0]["url"]
            .as_str()
            .or(answer["images"][0]["url"].as_str());
        if let Some(url) = url {
            return bytes(self.get(url)?);
        }
        bail!(
            "the image service answered without an image: {}",
            snippet(&answer.to_string())
        )
    }

    /// Start the job, wait for it, then download the clip.
    fn video(&self, request: &GenerateRequest) -> Result<Vec<u8>> {
        let provider = self.service.provider();
        let mut body = self.with_model(match provider {
            "ark" => json!({ "content": [{ "type": "text", "text": request.prompt }] }),
            _ => json!({ "prompt": request.prompt }),
        });
        if let Some(size) = &request.size {
            match (provider, dimensions(size)) {
                ("ark", Some((width, height))) => {
                    let gcd = gcd(width, height);
                    body["ratio"] = json!(format!("{}:{}", width / gcd, height / gcd));
                    body["resolution"] = json!(format!("{}p", width.min(height)));
                }
                ("minimax", Some((width, height))) => {
                    body["resolution"] = json!(format!("{}P", width.min(height)));
                }
                ("ark" | "minimax", None) => {}
                _ => body["size"] = json!(size),
            }
        }
        if let Some(seconds) = &request.seconds {
            body["seconds"] = json!(seconds);
            if provider != "openai" {
                let seconds: u32 = seconds
                    .trim()
                    .parse()
                    .with_context(|| format!("--seconds {seconds} is not a whole number"))?;
                body.as_object_mut().unwrap().remove("seconds");
                body["duration"] = json!(seconds);
            }
        }
        match provider {
            "ark" => {
                let path = "/contents/generations/tasks";
                let id = job_id(&self.post_json(path, &body)?, "id")?;
                let job = self.wait(&format!("{}{path}/{id}", self.base))?;
                bytes(self.get(&download_url(&job["content"]["video_url"])?)?)
            }
            "minimax" => {
                let id = job_id(&self.post_json("/video_generation", &body)?, "task_id")?;
                let job = self.wait(&format!(
                    "{}/query/video_generation?task_id={id}",
                    self.base
                ))?;
                let file = job_id(&job, "file_id")?;
                let found =
                    self.get_json(&format!("{}/files/retrieve?file_id={file}", self.base))?;
                bytes(self.get(&download_url(&found["file"]["download_url"])?)?)
            }
            _ => {
                let id = job_id(&self.post_json("/videos", &body)?, "id")?;
                self.wait(&format!("{}/videos/{id}", self.base))?;
                bytes(self.get(&format!("{}/videos/{id}/content", self.base))?)
            }
        }
    }

    /// Poll a job until it finishes; returns its last state.
    fn wait(&self, url: &str) -> Result<Value> {
        let started = Instant::now();
        loop {
            let job = self.get_json(url)?;
            match job["status"].as_str().unwrap_or_default() {
                "completed" | "succeeded" | "Success" => return Ok(job),
                "failed" | "cancelled" | "expired" | "Fail" => bail!(
                    "the video job failed: {}",
                    job["error"]["message"]
                        .as_str()
                        .or(job["base_resp"]["status_msg"].as_str())
                        .unwrap_or("no reason given")
                ),
                _ if started.elapsed() > VIDEO_DEADLINE => {
                    bail!("the video job did not finish in 30 minutes: {url}")
                }
                _ => thread::sleep(VIDEO_POLL),
            }
        }
    }

    fn get_json(&self, url: &str) -> Result<Value> {
        minimax_ok(self.get(url)?.body_mut().read_json()?)
    }

    fn post_json(&self, path: &str, body: &Value) -> Result<Value> {
        minimax_ok(self.post(path, body)?.body_mut().read_json()?)
    }
}

/// Join the audio pieces of a Doubao answer; code 0 is a piece, 20000000 the end.
fn doubao_audio(answer: &[u8]) -> Result<Vec<u8>> {
    let mut audio = Vec::new();
    for piece in serde_json::Deserializer::from_slice(answer).into_iter::<Value>() {
        let piece = piece.context("the speech service sent bad JSON")?;
        match piece["code"].as_i64().unwrap_or(0) {
            0 | 20000000 => {}
            code => bail!(
                "the speech service answered error {code}: {}",
                piece["message"].as_str().unwrap_or_default()
            ),
        }
        if let Some(data) = piece["data"].as_str().filter(|d| !d.is_empty()) {
            audio.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(data.trim())
                    .context("the speech service sent bad base64")?,
            );
        }
    }
    if audio.is_empty() {
        bail!("the speech service answered without audio");
    }
    Ok(audio)
}

/// A job or file id, which MiniMax sends as a string or a number.
fn job_id(answer: &Value, key: &str) -> Result<String> {
    match &answer[key] {
        Value::String(id) => Ok(id.clone()),
        Value::Number(id) => Ok(id.to_string()),
        _ => bail!(
            "the video service answered without {key}: {}",
            snippet(&answer.to_string())
        ),
    }
}

/// MiniMax reports errors in `base_resp` with HTTP 200.
fn minimax_ok(answer: Value) -> Result<Value> {
    match answer["base_resp"]["status_code"].as_i64() {
        Some(code) if code != 0 => bail!(
            "the service answered error {code}: {}",
            answer["base_resp"]["status_msg"]
                .as_str()
                .unwrap_or_default()
        ),
        _ => Ok(answer),
    }
}

fn download_url(url: &Value) -> Result<String> {
    url.as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("the video job finished without a download link"))
}

/// `1280x720` → (1280, 720); `None` for sizes like `auto` that only OpenAI takes.
fn dimensions(size: &str) -> Option<(u32, u32)> {
    size.split_once(['x', 'X', '*'])
        .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
        .filter(|&(w, h)| w > 0 && h > 0)
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

fn check(
    mut response: ureq::http::Response<ureq::Body>,
    what: &str,
) -> Result<ureq::http::Response<ureq::Body>> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let text = response.body_mut().read_to_string().unwrap_or_default();
    bail!("{what} answered HTTP {status}: {}", snippet(&text))
}

fn bytes(mut response: ureq::http::Response<ureq::Body>) -> Result<Vec<u8>> {
    Ok(response
        .body_mut()
        .with_config()
        .limit(MAX_BYTES)
        .read_to_vec()?)
}

/// Enough of an error body to explain it, never a whole page.
fn snippet(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(400) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiny_http::{Response, Server};

    /// A fake OpenAI-compatible service: answers each path the way the real API does.
    fn fake_service() -> String {
        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        thread::spawn(move || {
            for mut request in server.incoming_requests() {
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                let authed = request.headers().iter().any(|h| {
                    (h.field.equiv("Authorization") && h.value.as_str() == "Bearer k")
                        || (h.field.equiv("X-Api-Key") && h.value.as_str() == "k")
                });
                let resource = request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("X-Api-Resource-Id"))
                    .map(|h| h.value.to_string());
                let response = match (authed, request.url()) {
                    (_, file) if file.starts_with("/file/") => {
                        Response::from_data(file.as_bytes().to_vec())
                    }
                    (false, _) => Response::from_string("no key").with_status_code(401),
                    (_, "/mimo/chat/completions") => {
                        let body: Value = serde_json::from_str(&body).unwrap();
                        assert_eq!(body["messages"][0]["content"], "hello");
                        assert_eq!(body["audio"]["format"], "mp3");
                        let mp3 = base64::engine::general_purpose::STANDARD.encode(b"ID3mimo");
                        Response::from_string(
                            json!({"choices": [{"message": {"audio": {"data": mp3}}}]}).to_string(),
                        )
                    }
                    (_, "/v1/audio/speech") => {
                        let body: Value = serde_json::from_str(&body).unwrap();
                        assert_eq!(body["model"], "tts");
                        Response::from_data(b"ID3audio".to_vec())
                    }
                    (_, "/v1/images/generations") => {
                        let png = base64::engine::general_purpose::STANDARD.encode(b"\x89PNGdata");
                        Response::from_string(json!({"data": [{"b64_json": png}]}).to_string())
                    }
                    (_, "/v1/videos") => {
                        Response::from_string(json!({"id": "v1", "status": "queued"}).to_string())
                    }
                    (_, "/v1/videos/v1") => Response::from_string(
                        json!({"id": "v1", "status": "completed"}).to_string(),
                    ),
                    (_, "/v1/videos/v1/content") => Response::from_data(b"mp4data".to_vec()),
                    (_, "/qwen/services/aigc/multimodal-generation/generation") => {
                        let body: Value = serde_json::from_str(&body).unwrap();
                        assert_eq!(body["input"]["voice"], "Cherry");
                        let url = format!("http://127.0.0.1:{port}/file/qwen.wav");
                        Response::from_string(json!({"output": {"audio": {"url": url}}}).to_string())
                    }
                    (_, "/doubao/tts/unidirectional") => {
                        assert_eq!(resource.as_deref(), Some("seed-tts-2.0"));
                        let body: Value = serde_json::from_str(&body).unwrap();
                        assert_eq!(body["req_params"]["text"], "hello");
                        let b64 = |d: &[u8]| base64::engine::general_purpose::STANDARD.encode(d);
                        Response::from_string(format!(
                            "{}{}{}",
                            json!({"code": 0, "data": b64(b"ID3")}),
                            json!({"code": 0, "data": b64(b"dou")}),
                            json!({"code": 20000000, "message": "OK", "data": null}),
                        ))
                    }
                    (_, "/mm/image_generation") => {
                        let body: Value = serde_json::from_str(&body).unwrap();
                        assert_eq!(body["width"], 1280);
                        let jpg = base64::engine::general_purpose::STANDARD.encode(b"\xFF\xD8mm");
                        let answer = json!({"data": {"image_base64": [jpg]}, "base_resp": {"status_code": 0}});
                        Response::from_string(answer.to_string())
                    }
                    (_, "/ark/contents/generations/tasks") => {
                        let body: Value = serde_json::from_str(&body).unwrap();
                        assert_eq!(body["ratio"], "16:9");
                        assert_eq!(body["resolution"], "720p");
                        assert_eq!(body["duration"], 5);
                        Response::from_string(json!({"id": "cgt-1"}).to_string())
                    }
                    (_, "/ark/contents/generations/tasks/cgt-1") => Response::from_string(
                        json!({"status": "succeeded", "content": {"video_url": format!("http://127.0.0.1:{port}/file/ark.mp4")}}).to_string(),
                    ),
                    (_, "/mm/video_generation") if body.contains("bad") => Response::from_string(
                        json!({"base_resp": {"status_code": 1008, "status_msg": "insufficient balance"}}).to_string(),
                    ),
                    (_, "/mm/video_generation") => Response::from_string(
                        json!({"task_id": "t1", "base_resp": {"status_code": 0}}).to_string(),
                    ),
                    (_, "/mm/query/video_generation?task_id=t1") => Response::from_string(
                        json!({"status": "Success", "file_id": "f1"}).to_string(),
                    ),
                    (_, "/mm/files/retrieve?file_id=f1") => Response::from_string(
                        json!({"file": {"download_url": format!("http://127.0.0.1:{port}/file/mm.mp4")}}).to_string(),
                    ),
                    _ => Response::from_string("nope").with_status_code(404),
                };
                let _ = request.respond(response);
            }
        });
        format!("http://127.0.0.1:{port}/v1/")
    }

    fn request(kind: &str, out: Option<&str>) -> GenerateRequest {
        GenerateRequest {
            project: "p".into(),
            kind: kind.into(),
            prompt: "hello".into(),
            out: out.map(str::to_string),
            voice: None,
            size: None,
            seconds: None,
        }
    }

    #[test]
    fn generates_each_kind_into_the_project() {
        let url = fake_service();
        let service = |model: &str| Service {
            provider: "openai".into(),
            url: url.clone(),
            api_key: "k".into(),
            model: model.into(),
        };
        let services = Services {
            speech: service("tts"),
            image: service(""),
            video: service(""),
            accounts: Default::default(),
        };
        let dir = std::env::temp_dir().join(format!("lumireel-generate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        let speech = generate(&services, &request("speech", Some("assets/hi.mp3")), &dir).unwrap();
        assert_eq!(speech, dir.join("assets/hi.mp3"));
        assert_eq!(fs::read(&speech).unwrap(), b"ID3audio");

        let image = generate(&services, &request("image", None), &dir).unwrap();
        assert_eq!(image.extension().unwrap(), "png");
        assert_eq!(fs::read(&image).unwrap(), b"\x89PNGdata");

        let video = generate(&services, &request("video", Some("v.mp4")), &dir).unwrap();
        assert_eq!(fs::read(&video).unwrap(), b"mp4data");

        let mut unset = services.clone();
        unset.video.url.clear();
        let error = generate(&unset, &request("video", None), &dir).unwrap_err();
        assert!(error.to_string().contains("not set up"));

        let mut chat_only = services.clone();
        chat_only.speech.url = url.replace("/v1/", "/mimo");
        chat_only.speech.provider = "mimo".into();
        let speech = generate(&chat_only, &request("speech", Some("m.mp3")), &dir).unwrap();
        assert_eq!(fs::read(&speech).unwrap(), b"ID3mimo");

        let mut qwen = services.clone();
        qwen.speech.url = url.replace("/v1/", "/qwen");
        qwen.speech.provider = "qwen".into();
        let speech = generate(&qwen, &request("speech", Some("q.wav")), &dir).unwrap();
        assert_eq!(fs::read(&speech).unwrap(), b"/file/qwen.wav");

        let mut doubao = services.clone();
        doubao.speech.url = url.replace("/v1/", "/doubao");
        doubao.speech.provider = "doubao".into();
        doubao.speech.model = "seed-tts-2.0".into();
        let speech = generate(&doubao, &request("speech", None), &dir).unwrap();
        assert_eq!(speech.extension().unwrap(), "mp3");
        assert_eq!(fs::read(&speech).unwrap(), b"ID3dou");

        let mut other = services.clone();
        for service in [&mut other.image, &mut other.video] {
            service.url = url.replace("/v1/", "/mm");
            service.provider = "minimax".into();
        }
        let mut sized = request("image", None);
        sized.size = Some("1280x720".into());
        let image = generate(&other, &sized, &dir).unwrap();
        assert_eq!(fs::read(&image).unwrap(), b"\xFF\xD8mm");
        let video = generate(&other, &request("video", Some("mm.mp4")), &dir).unwrap();
        assert_eq!(fs::read(&video).unwrap(), b"/file/mm.mp4");
        let mut bad = request("video", None);
        bad.prompt = "bad".into();
        // MiniMax sends errors in base_resp with HTTP 200.
        let error = generate(&other, &bad, &dir).unwrap_err();
        assert!(
            error.to_string().contains("insufficient balance"),
            "{error}"
        );

        other.video.url = url.replace("/v1/", "/ark");
        other.video.provider = "ark".into();
        let mut ark = request("video", Some("ark.mp4"));
        ark.size = Some("1280x720".into());
        ark.seconds = Some("5".into());
        let video = generate(&other, &ark, &dir).unwrap();
        assert_eq!(fs::read(&video).unwrap(), b"/file/ark.mp4");

        let mut wrong_key = services.clone();
        wrong_key.speech.api_key = "x".into();
        let error = generate(&wrong_key, &request("speech", None), &dir).unwrap_err();
        assert!(error.to_string().contains("401"), "{error}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn guesses_the_provider_of_old_configs() {
        let service = |url: &str| Service {
            url: url.into(),
            ..Service::default()
        };
        assert_eq!(
            service("https://token-plan-cn.xiaomimimo.com/v1").provider(),
            "mimo"
        );
        assert_eq!(
            service("https://ark.cn-beijing.volces.com/api/v3").provider(),
            "ark"
        );
        assert_eq!(service("https://api.minimaxi.com/v1").provider(), "minimax");
        assert_eq!(service("https://api.openai.com/v1").provider(), "openai");
        assert_eq!(
            service("https://openspeech.bytedance.com/api/v3").provider(),
            "doubao"
        );
        assert_eq!(
            service("https://dashscope.aliyuncs.com/api/v1").provider(),
            "qwen"
        );
    }

    #[test]
    fn reports_doubao_errors() {
        let error = doubao_audio(br#"{"code":45000001,"message":"bad speaker"}"#).unwrap_err();
        assert!(error.to_string().contains("bad speaker"), "{error}");
    }

    #[test]
    fn knows_image_formats() {
        assert_eq!(image_ext(b"\xFF\xD8\xFFjpeg"), "jpg");
        assert_eq!(image_ext(b"RIFF\0\0\0\0WEBPVP8 "), "webp");
        assert_eq!(image_ext(b"\x89PNG\r\n"), "png");
        assert_eq!(audio_ext(b"RIFF\0\0\0\0WAVEfmt "), "wav");
        assert_eq!(audio_ext(b"ID3\x04"), "mp3");
    }
}
