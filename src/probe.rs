//! ffprobe の呼び出しと、出力（JSON）の読み取り
//!
//! - 探す順: 設定のパス → `PATH` → データフォルダの `Plugin/` の一つ下のフォルダの `ffprobe.exe`
//!   （本体同梱の ffmpegOut の `ffmpeg_full` には ffprobe.exe が無い。2026-10-05 に確かめた）
//! - 子プロセスはウィンドウを出さない（`CREATE_NO_WINDOW`）。止める合図か新しい依頼が来たら殺す
//! - 可変フレームレートの判定（2026-10-09 に作った見本と手元の録画で決めた。仕様の「未確認」の 3）:
//!   先頭 5 秒のパケットの間隔のうち、中央値から 10% 以上ずれたものが 5% を超える、
//!   または公称（r_frame_rate）と平均（avg_frame_rate）が 1% 以上違う。
//!   OBS の録画にあったフレーム落ち 1 か所（ずれ 1.8%）は固定と判定する

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const TIMEOUT: Duration = Duration::from_secs(30);

/// ffprobe.exe を探す
pub fn find_ffprobe(configured: Option<&Path>, plugin_dir: &Path) -> Option<PathBuf> {
    if let Some(p) = configured.filter(|p| p.is_file()) {
        return Some(p.to_path_buf());
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let p = dir.join("ffprobe.exe");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(plugin_dir).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    dirs.into_iter().map(|d| d.join("ffprobe.exe")).find(|p| p.is_file())
}

/// 子プロセスを走らせて標準出力を返す。`cancel` が true を返したら殺して `Err`
pub fn run(exe: &Path, args: &[&str], cancel: &dyn Fn() -> bool) -> Result<Vec<u8>, String> {
    use std::os::windows::process::CommandExt;
    let mut child = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("ffprobe を起動できませんでした: {e}"))?;
    // 出力が大きいとパイプが詰まるので、読む側は別に回す（このスレッドが終わるまで待つ）
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        let mut err = Vec::new();
        let _ = stderr.read_to_end(&mut err);
        (out, err)
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Ok(s),
            Ok(None) if cancel() => break Err("やめました".to_string()),
            Ok(None) if started.elapsed() > TIMEOUT => break Err(format!("ffprobe が {} 秒で終わりませんでした", TIMEOUT.as_secs())),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => break Err(format!("ffprobe を待てませんでした: {e}")),
        }
    };
    if status.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let (out, err) = reader.join().unwrap_or_default();
    let status = status?;
    if !status.success() {
        let msg = String::from_utf8_lossy(&err);
        return Err(format!("ffprobe が失敗しました: {}", msg.lines().next().unwrap_or("").trim()));
    }
    Ok(out)
}

// ---- ffprobe の JSON ----

#[derive(Debug, Deserialize, Default)]
struct Json {
    #[serde(default)]
    streams: Vec<JsonStream>,
    #[serde(default)]
    format: JsonFormat,
}

#[derive(Debug, Deserialize, Default)]
struct JsonFormat {
    format_name: Option<String>,
    format_long_name: Option<String>,
    duration: Option<String>,
    size: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct JsonStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    r_frame_rate: Option<String>,
    avg_frame_rate: Option<String>,
    pix_fmt: Option<String>,
    bits_per_raw_sample: Option<String>,
    color_space: Option<String>,
    color_transfer: Option<String>,
    color_primaries: Option<String>,
    color_range: Option<String>,
    field_order: Option<String>,
    sample_rate: Option<String>,
    channels: Option<u32>,
    channel_layout: Option<String>,
    sample_fmt: Option<String>,
    duration: Option<String>,
    nb_frames: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Video {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    /// 公称（r_frame_rate）と平均（avg_frame_rate）。(分子, 分母)
    pub r_rate: Option<(u64, u64)>,
    pub avg_rate: Option<(u64, u64)>,
    pub pix_fmt: String,
    pub bit_depth: u32,
    pub alpha: bool,
    pub color_space: Option<String>,
    pub color_transfer: Option<String>,
    pub color_primaries: Option<String>,
    pub color_range: Option<String>,
    /// インターレース（tt / bb / tb / bt）
    pub interlaced: bool,
    /// 静止画（png・jpg など。1 枚だけのもの）
    pub still: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Audio {
    pub codec: String,
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
    pub layout: Option<String>,
    /// サンプルの形式（s16 / fltp など）
    pub sample_fmt: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FrameRateKind {
    /// 調べていない（静止画・映像なし）
    Unknown,
    Constant,
    /// 可変。理由
    Variable(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub container: String,
    pub duration: Option<f64>,
    pub size: Option<u64>,
    pub video: Option<Video>,
    pub audio: Vec<Audio>,
    pub frame_rate: FrameRateKind,
    /// 可変の判定をファイル全体で行ったか（「全体で判定」。第 2 段階）
    pub frame_rate_whole: bool,
}

pub fn rate(s: &str) -> Option<(u64, u64)> {
    let (a, b) = s.split_once('/').unwrap_or((s, "1"));
    let (a, b) = (a.trim().parse::<u64>().ok()?, b.trim().parse::<u64>().ok()?);
    (a > 0 && b > 0).then_some((a, b))
}

pub fn rate_f(r: (u64, u64)) -> f64 {
    r.0 as f64 / r.1 as f64
}

/// 画素形式からビット深度（`yuv420p10le` → 10、`rgb48le` → 16、`p010le` → 10、無ければ 8）
pub fn depth_of(pix_fmt: &str) -> u32 {
    let f = pix_fmt.trim_end_matches("le").trim_end_matches("be");
    if f.starts_with("p0") || f.starts_with("p2") || f.starts_with("p4") {
        // p010 / p016 / p210 / p410 …
        return f[2..].parse().unwrap_or(8);
    }
    if let Some(rest) = f.strip_prefix("rgb").or_else(|| f.strip_prefix("bgr")) {
        if let Ok(n) = rest.trim_start_matches('a').parse::<u32>() {
            // rgb48 / rgba64 / bgr24
            return match n {
                24 | 32 => 8,
                48 | 64 => 16,
                _ => 8,
            };
        }
    }
    let digits: String = f.chars().rev().take_while(|c| c.is_ascii_digit()).collect::<Vec<_>>().into_iter().rev().collect();
    match digits.parse::<u32>() {
        Ok(n) if (f.contains('p') || f.starts_with("gray")) && (9..=16).contains(&n) => n,
        _ => 8,
    }
}

fn has_alpha(pix_fmt: &str) -> bool {
    let f = pix_fmt.to_ascii_lowercase();
    f.starts_with("yuva") || f.starts_with("rgba") || f.starts_with("bgra") || f.starts_with("argb") || f.starts_with("abgr")
        || f.starts_with("gbrap") || f.starts_with("ya") || f == "pal8"
}

/// `-show_streams -show_format -of json` の出力を読む
pub fn parse(json: &[u8]) -> Result<Report, String> {
    let j: Json = serde_json::from_slice(json).map_err(|e| format!("ffprobe の出力を読めませんでした: {e}"))?;
    let format_name = j.format.format_name.clone().unwrap_or_default();
    let image_format = format_name.contains("image2") || format_name.ends_with("_pipe") || format_name.contains("webp");
    let video = j.streams.iter().find(|s| s.codec_type.as_deref() == Some("video")).map(|s| {
        let pix_fmt = s.pix_fmt.clone().unwrap_or_default();
        let raw_bits = s.bits_per_raw_sample.as_deref().and_then(|b| b.parse::<u32>().ok()).filter(|b| *b > 0);
        let still = image_format || s.nb_frames.as_deref() == Some("1");
        Video {
            codec: s.codec_name.clone().unwrap_or_default(),
            width: s.width.unwrap_or(0),
            height: s.height.unwrap_or(0),
            r_rate: s.r_frame_rate.as_deref().and_then(rate),
            avg_rate: s.avg_frame_rate.as_deref().and_then(rate),
            bit_depth: raw_bits.unwrap_or_else(|| depth_of(&pix_fmt)),
            alpha: has_alpha(&pix_fmt),
            pix_fmt,
            color_space: s.color_space.clone(),
            color_transfer: s.color_transfer.clone(),
            color_primaries: s.color_primaries.clone(),
            color_range: s.color_range.clone(),
            interlaced: matches!(s.field_order.as_deref(), Some("tt" | "bb" | "tb" | "bt")),
            still,
        }
    });
    let audio = j
        .streams
        .iter()
        .filter(|s| s.codec_type.as_deref() == Some("audio"))
        .map(|s| Audio {
            codec: s.codec_name.clone().unwrap_or_default(),
            sample_rate: s.sample_rate.as_deref().and_then(|r| r.parse().ok()),
            channels: s.channels,
            layout: s.channel_layout.clone(),
            sample_fmt: s.sample_fmt.clone(),
        })
        .collect();
    let duration = j
        .format
        .duration
        .as_deref()
        .or_else(|| j.streams.iter().find_map(|s| s.duration.as_deref()))
        .and_then(|d| d.parse::<f64>().ok());
    Ok(Report {
        container: j.format.format_long_name.or(j.format.format_name).unwrap_or_default(),
        duration,
        size: j.format.size.as_deref().and_then(|s| s.parse().ok()),
        video,
        audio,
        frame_rate: FrameRateKind::Unknown,
        frame_rate_whole: false,
    })
}

/// パケットの時刻（`-show_entries packet=pts_time -of csv=p=0`）と、公称・平均のフレームレートから判定する
pub fn judge_frame_rate(pts_csv: &str, video: &Video) -> FrameRateKind {
    if let (Some(r), Some(a)) = (video.r_rate, video.avg_rate) {
        let (r, a) = (rate_f(r), rate_f(a));
        if (r - a).abs() / r > 0.01 {
            return FrameRateKind::Variable(format!("公称 {r:.3}fps と平均 {a:.3}fps が違う"));
        }
    }
    let mut ts: Vec<f64> = pts_csv.split_whitespace().filter_map(|x| x.trim_end_matches(',').parse().ok()).collect();
    ts.sort_by(|a, b| a.total_cmp(b));
    let mut d: Vec<f64> = ts.windows(2).map(|w| w[1] - w[0]).filter(|x| *x > 0.0).collect();
    if d.len() < 10 {
        return FrameRateKind::Constant;
    }
    d.sort_by(|a, b| a.total_cmp(b));
    let med = d[d.len() / 2];
    let off = d.iter().filter(|x| (**x - med).abs() > med * 0.1).count();
    let ratio = off as f64 / d.len() as f64;
    if ratio > 0.05 {
        FrameRateKind::Variable(format!("先頭のフレームの間隔のうち {:.0}% がばらついている", ratio * 100.0))
    } else {
        FrameRateKind::Constant
    }
}

/// 映像のパケットの時刻から判定する。`whole` なら先頭 5 秒ではなくファイル全体を読む（重い。第 2 段階）
pub fn judge_file(exe: &Path, file: &Path, video: &Video, whole: bool, cancel: &dyn Fn() -> bool) -> Result<FrameRateKind, String> {
    let f = file.to_string_lossy();
    let mut args = vec!["-v", "error", "-select_streams", "v:0"];
    if !whole {
        args.extend(["-read_intervals", "%+5"]);
    }
    args.extend(["-show_entries", "packet=pts_time", "-of", "csv=p=0", &f]);
    let pts = run(exe, &args, cancel)?;
    let kind = judge_frame_rate(&String::from_utf8_lossy(&pts), video);
    Ok(match kind {
        FrameRateKind::Variable(why) if whole => FrameRateKind::Variable(why.replace("先頭の", "全体の")),
        k => k,
    })
}

/// ファイルを調べる（情報と、映像があれば先頭 5 秒での可変フレームレートの判定）
pub fn probe(exe: &Path, file: &Path, cancel: &dyn Fn() -> bool) -> Result<Report, String> {
    let f = file.to_string_lossy();
    let json = run(exe, &["-v", "error", "-show_streams", "-show_format", "-of", "json", &f], cancel)?;
    let mut report = parse(&json)?;
    if let Some(v) = report.video.clone().filter(|v| !v.still) {
        report.frame_rate = judge_file(exe, file, &v, false, cancel)?;
    }
    Ok(report)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const MP4: &str = r#"{"streams":[
        {"codec_type":"video","codec_name":"h264","width":1920,"height":1080,"r_frame_rate":"30000/1001","avg_frame_rate":"30000/1001",
         "pix_fmt":"yuv420p","color_space":"bt709","color_transfer":"bt709","color_primaries":"bt709","color_range":"tv","field_order":"progressive","nb_frames":"449"},
        {"codec_type":"audio","codec_name":"aac","sample_rate":"44100","channels":2,"channel_layout":"stereo","sample_fmt":"fltp"}],
        "format":{"format_name":"mov,mp4,m4a,3gp,3g2,mj2","format_long_name":"QuickTime / MOV","duration":"15.069751","size":"5100000"}}"#;

    #[test]
    fn parses_video_and_audio() {
        let r = parse(MP4.as_bytes()).unwrap();
        let v = r.video.as_ref().unwrap();
        assert_eq!((v.codec.as_str(), v.width, v.height, v.r_rate, v.bit_depth, v.alpha, v.interlaced, v.still), ("h264", 1920, 1080, Some((30000, 1001)), 8, false, false, false));
        assert_eq!(v.color_range.as_deref(), Some("tv"));
        assert_eq!(r.audio[0].sample_rate, Some(44100));
        assert_eq!(r.container, "QuickTime / MOV");
        assert_eq!(r.duration, Some(15.069751));
        assert_eq!(r.size, Some(5_100_000));
    }

    #[test]
    fn still_image_and_alpha() {
        let png = r#"{"streams":[{"codec_type":"video","codec_name":"png","width":512,"height":512,"r_frame_rate":"25/1","avg_frame_rate":"0/0","pix_fmt":"rgba"}],
            "format":{"format_name":"png_pipe","format_long_name":"piped png sequence"}}"#;
        let v = parse(png.as_bytes()).unwrap().video.unwrap();
        assert!(v.still && v.alpha);
        assert_eq!(v.avg_rate, None);
    }

    #[test]
    fn audio_only() {
        let wav = r#"{"streams":[{"codec_type":"audio","codec_name":"pcm_s16le","sample_rate":"48000","channels":1}],"format":{"format_name":"wav","duration":"3.0"}}"#;
        let r = parse(wav.as_bytes()).unwrap();
        assert!(r.video.is_none());
        assert_eq!(r.audio[0].channels, Some(1));
    }

    #[test]
    fn bad_json_is_error() {
        assert!(parse(b"{").is_err());
        assert!(parse(b"").is_err());
    }

    #[test]
    fn bit_depths() {
        for (f, d) in [("yuv420p", 8), ("yuv420p10le", 10), ("yuv422p12be", 12), ("p010le", 10), ("rgb48le", 16), ("rgba64be", 16), ("rgb24", 8), ("gbrp12le", 12), ("gray", 8), ("gray10le", 10), ("nv12", 8)] {
            assert_eq!(depth_of(f), d, "{f}");
        }
        assert!(has_alpha("yuva420p") && has_alpha("bgra") && !has_alpha("yuv420p"));
    }

    fn video(r: &str, a: &str) -> Video {
        parse(MP4.replace("30000/1001\",\"avg_frame_rate\":\"30000/1001", &format!("{r}\",\"avg_frame_rate\":\"{a}")).as_bytes()).unwrap().video.unwrap()
    }

    fn pts(intervals: &[f64]) -> String {
        let mut t = 0.0;
        let mut s = String::from("0.000000\n");
        for d in intervals {
            t += d;
            s += &format!("{t:.6}\n");
        }
        s
    }

    #[test]
    fn frame_rate_judgement() {
        let cfr = video("30/1", "30/1");
        // 固定
        assert_eq!(judge_frame_rate(&pts(&[1.0 / 30.0; 150]), &cfr), FrameRateKind::Constant);
        // フレーム落ち 1 か所（OBS の録画の実例は 56 個中 1 個）
        let mut drop = vec![1.0 / 30.0; 55];
        drop[20] = 2.0 / 30.0;
        assert_eq!(judge_frame_rate(&pts(&drop), &cfr), FrameRateKind::Constant);
        // 途中で 30 → 15fps（作った見本では 22%）
        let mut step = vec![1.0 / 30.0; 90];
        step.extend(vec![2.0 / 30.0; 26]);
        assert!(matches!(judge_frame_rate(&pts(&step), &cfr), FrameRateKind::Variable(_)));
        // 公称と平均が違う（作った見本では 30 / 20.886）
        assert!(matches!(judge_frame_rate(&pts(&[1.0 / 30.0; 150]), &video("30/1", "1650/79")), FrameRateKind::Variable(_)));
        // 後半だけ揺れて平均は 30fps のまま（1/60 と 1/20 の交互）: 先頭 5 秒では固定、全体では可変（「全体で判定」の意味）
        let mut late = vec![1.0 / 30.0; 150];
        let head = pts(&late);
        for i in 0..150 {
            late.push(if i % 2 == 0 { 1.0 / 60.0 } else { 1.0 / 20.0 });
        }
        assert_eq!(judge_frame_rate(&head, &cfr), FrameRateKind::Constant);
        assert!(matches!(judge_frame_rate(&pts(&late), &cfr), FrameRateKind::Variable(_)));
        // パケットが少なすぎるときは決めない（固定とする）
        assert_eq!(judge_frame_rate("0\n0.1\n", &cfr), FrameRateKind::Constant);
    }

    /// 手元の ffprobe と、作った見本で試す（`cargo test -- --ignored --nocapture real_ffprobe`）
    #[test]
    #[ignore]
    fn real_ffprobe() {
        let Some(exe) = find_ffprobe(None, Path::new("C:/ProgramData/aviutl2/Plugin")) else { return };
        let dir = std::env::var("MP_SAMPLES").unwrap_or_default();
        for f in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = f.path();
            if p.extension().is_some_and(|x| x == "mp4") {
                let r = probe(&exe, &p, &|| false).unwrap();
                let whole = judge_file(&exe, &p, r.video.as_ref().unwrap(), true, &|| false).unwrap();
                println!("{}: 先頭 {:?} / 全体 {:?}", p.display(), r.frame_rate, whole);
                // 途中でやめると Err になる
                assert!(judge_file(&exe, &p, r.video.as_ref().unwrap(), true, &|| true).is_err());
            }
        }
    }
}
