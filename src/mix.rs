use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Sound {
    pub src: String,
    pub at: f64,
    #[serde(rename = "in", default)]
    pub in_point: f64,
    #[serde(rename = "out", default)]
    pub out_point: f64,
    #[serde(default = "default_volume")]
    pub volume: f64,
}

fn default_volume() -> f64 {
    1.0
}

#[derive(Clone, Debug, PartialEq)]
pub struct MixPlan {
    /// Sound files, in ffmpeg input order after the raw video on stdin.
    pub inputs: Vec<String>,
    /// The `-filter_complex` graph, mapping `[audio]`, when there is any sound.
    pub filter: Option<String>,
}

/// Build the ffmpeg audio graph. Input 0 is always the raw video from stdin.
pub fn plan(sounds: &[Sound], duration: f64) -> MixPlan {
    let inputs = sounds.iter().map(|s| s.src.clone()).collect::<Vec<_>>();
    if sounds.is_empty() {
        return MixPlan {
            inputs,
            filter: None,
        };
    }

    let mut chains = Vec::new();
    let mut labels = Vec::new();
    for (index, sound) in sounds.iter().enumerate() {
        let input = index + 1;
        let in_point = sound.in_point.max(0.0);
        let out_point = if sound.out_point > in_point {
            sound.out_point
        } else {
            duration.max(in_point)
        };
        let delay_ms = (sound.at.max(0.0) * 1000.0).round() as i64;
        let volume = if sound.volume.is_finite() && sound.volume >= 0.0 {
            sound.volume
        } else {
            1.0
        };
        chains.push(format!(
            "[{input}:a]atrim={in_point:.6}:{out_point:.6},\
             asetpts=PTS-STARTPTS,volume={volume:.6},\
             adelay={delay_ms}:all=1[a{index}]"
        ));
        labels.push(format!("[a{index}]"));
    }

    let mix = format!(
        "{}amix=inputs={}:normalize=0,apad=whole_dur={:.6},atrim=0:{:.6}[audio]",
        labels.join(""),
        sounds.len(),
        duration.max(0.0),
        duration.max(0.0)
    );
    chains.push(mix);
    MixPlan {
        inputs,
        filter: Some(chains.join(";")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sound(src: &str, at: f64, in_point: f64, out_point: f64, volume: f64) -> Sound {
        Sound {
            src: src.into(),
            at,
            in_point,
            out_point,
            volume,
        }
    }

    #[test]
    fn no_sounds_means_no_audio_track() {
        let plan = plan(&[], 10.0);
        assert!(plan.inputs.is_empty());
        assert_eq!(plan.filter, None);
    }

    #[test]
    fn single_sound() {
        let plan = plan(&[sound("assets/a.mp3", 0.0, 0.0, 12.0, 0.6)], 12.0);
        assert_eq!(plan.inputs, vec!["assets/a.mp3"]);
        assert_eq!(
            plan.filter.as_deref(),
            Some(
                "[1:a]atrim=0.000000:12.000000,asetpts=PTS-STARTPTS,volume=0.600000,\
                 adelay=0:all=1[a0];[a0]amix=inputs=1:normalize=0,apad=whole_dur=12.000000,atrim=0:12.000000[audio]"
            )
        );
    }

    #[test]
    fn many_sounds_and_late_start() {
        let plan = plan(
            &[
                sound("assets/a.mp3", 0.0, 1.0, 5.0, 1.0),
                sound("assets/b.mp3", 2.5, 0.0, 4.0, 0.35),
            ],
            12.0,
        );
        assert_eq!(plan.inputs, vec!["assets/a.mp3", "assets/b.mp3"]);
        let filter = plan.filter.unwrap();
        assert!(filter.contains("[1:a]atrim=1.000000:5.000000"));
        assert!(filter.contains("adelay=2500:all=1[a1]"));
        assert!(filter.contains(
            "[a0][a1]amix=inputs=2:normalize=0,apad=whole_dur=12.000000,atrim=0:12.000000[audio]"
        ));
    }

    #[test]
    fn sound_without_out_uses_reel_duration() {
        let plan = plan(&[sound("assets/a.mp3", 1.0, 0.0, 0.0, 1.0)], 8.0);
        let filter = plan.filter.unwrap();
        assert!(filter.contains("atrim=0.000000:8.000000"));
    }
}
