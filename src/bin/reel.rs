use std::{env, process::ExitCode};

use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::{Value, json};

const HELP: &str = "\
reel — peek at, render and make media for the video in this folder

  reel peek [t | a-b]        contact sheet of the whole video (or one frame / a range)
  reel render [--fps 30] [-o out.mp4]
  reel speech \"text\" [--voice alloy] [-o assets/x.mp3]
  reel image \"prompt\" [--size 1024x1024] [-o assets/x.png]
  reel video \"prompt\" [--seconds 4] [--size 1280x720] [-o assets/x.mp4]

Run it inside a lumireel terminal.
";

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("reel: {error:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<u8> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let Some(command) = args.first().map(String::as_str) else {
        print!("{HELP}");
        return Ok(0);
    };
    match command {
        "help" | "-h" | "--help" => {
            print!("{HELP}");
            Ok(0)
        }
        "peek" => {
            let (url, token, project) = connection()?;
            let mut body = json!({ "project": project });
            if let Some(arg) = args.get(1) {
                if let Some((start, end)) = parse_range(arg) {
                    body["range"] = json!([start, end]);
                } else if let Ok(t) = arg.parse::<f64>() {
                    body["t"] = json!(t);
                } else {
                    bail!("peek expects a time like 4.5 or a range like 3-6");
                }
            }
            let response = post(&url, &token, "/__ctl/peek", &body)?;
            let sheet = response
                .get("sheet")
                .and_then(Value::as_str)
                .unwrap_or_default();
            println!("sheet: {sheet}");
            if let Some(frames) = response.get("frames").and_then(Value::as_array) {
                for frame in frames {
                    if let Some(frame) = frame.as_str() {
                        println!("frame: {frame}");
                    }
                }
            }
            Ok(report_problems(&response))
        }
        "render" => {
            let (url, token, project) = connection()?;
            let mut fps = 30.0;
            let mut out: Option<String> = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--fps" => {
                        let value = args
                            .get(i + 1)
                            .ok_or_else(|| anyhow!("--fps needs a number"))?;
                        fps = value.parse().context("--fps must be a number")?;
                        i += 2;
                    }
                    "-o" | "--out" => {
                        out = Some(
                            args.get(i + 1)
                                .ok_or_else(|| anyhow!("{0} needs a path", args[i]))?
                                .clone(),
                        );
                        i += 2;
                    }
                    other => bail!("unknown render option: {other}"),
                }
            }
            if fps <= 0.0 {
                bail!("fps must be greater than 0");
            }
            let mut body = json!({ "project": project, "fps": fps });
            if let Some(out) = out {
                body["out"] = json!(out);
            }
            let response = post(&url, &token, "/__ctl/render", &body)?;
            if let Some(file) = response.get("file").and_then(Value::as_str) {
                println!("file: {file}");
            }
            Ok(report_problems(&response))
        }
        "speech" | "image" | "video" => {
            let (url, token, project) = connection()?;
            let mut body = json!({ "project": project, "kind": command });
            let mut prompt: Option<&str> = None;
            let mut i = 1;
            while i < args.len() {
                let key = match args[i].as_str() {
                    "-o" | "--out" => "out",
                    "--voice" => "voice",
                    "--size" => "size",
                    "--seconds" => "seconds",
                    flag if flag.starts_with("--") => bail!("unknown {command} option: {flag}"),
                    text => {
                        if prompt.replace(text).is_some() {
                            bail!("{command} takes one prompt: quote it");
                        }
                        i += 1;
                        continue;
                    }
                };
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow!("{} needs a value", args[i]))?;
                body[key] = json!(value);
                i += 2;
            }
            body["prompt"] = json!(prompt.ok_or_else(|| anyhow!("{command} needs a prompt"))?);
            let response = post(&url, &token, "/__ctl/generate", &body)?;
            if let Some(file) = response.get("file").and_then(Value::as_str) {
                println!("file: {file}");
            }
            Ok(0)
        }
        other => {
            eprintln!("reel: unknown command {other}\n");
            print!("{HELP}");
            Ok(1)
        }
    }
}

fn report_problems(response: &Value) -> u8 {
    let Some(problems) = response.get("problems").and_then(Value::as_array) else {
        return 0;
    };
    if problems.is_empty() {
        return 0;
    }
    for problem in problems {
        let message = problem
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("problem");
        let source = problem.get("source").and_then(Value::as_str);
        match source {
            Some(source) if !source.is_empty() => println!("problem: {message} ({source})"),
            _ => println!("problem: {message}"),
        }
    }
    1
}

fn connection() -> Result<(String, String, String)> {
    let url = env::var("LUMIREEL_URL").context("请在 lumireel 的终端里运行")?;
    let token = env::var("LUMIREEL_TOKEN").context("请在 lumireel 的终端里运行")?;
    let project = env::var("LUMIREEL_PROJECT").context("请在 lumireel 的终端里运行")?;
    let project = std::path::Path::new(&project)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .unwrap_or(project);
    Ok((url.trim_end_matches('/').to_string(), token, project))
}

fn post(base: &str, token: &str, path: &str, body: &Value) -> Result<Value> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let mut response = agent
        .post(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send_json(body)
        .with_context(|| format!("calling lumireel at {base}"))?;
    let status = response.status();
    let text = response.body_mut().read_to_string()?;
    let value: Value =
        serde_json::from_str(&text).unwrap_or_else(|_| json!({ "error": text.trim().to_string() }));
    if !status.is_success() {
        let message = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("request failed");
        bail!("{message} (HTTP {status})");
    }
    Ok(value)
}

fn parse_range(value: &str) -> Option<(f64, f64)> {
    let (start, end) = value.split_once('-')?;
    let start = start.trim().parse::<f64>().ok()?;
    let end = end.trim().parse::<f64>().ok()?;
    Some((start.min(end), start.max(end)))
}
