//! シーンの設定（解像度・fps・サンプルレート）との照合（仕様の機能 2）

use crate::probe::{rate_f, FrameRateKind, Report};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// 音ずれ・コマ落ちの原因になりうる
    Warn,
    /// 知っておくとよいこと
    Info,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub level: Level,
    pub text: String,
}

/// 表示中のシーンの設定
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SceneSettings {
    pub width: usize,
    pub height: usize,
    /// (分子, 分母)
    pub fps: (u64, u64),
    pub sample_rate: usize,
}

fn fps_text(f: f64) -> String {
    let r = (f * 1000.0).round() / 1000.0;
    if (r - r.round()).abs() < 1e-9 {
        format!("{}", r as i64)
    } else {
        let s = format!("{r:.3}");
        s.trim_end_matches('0').to_string()
    }
}

/// a が b の整数倍か約数なら true（60fps の素材を 30fps のシーンに置くのは問題にしない）
fn compatible(a: f64, b: f64) -> bool {
    if a <= 0.0 || b <= 0.0 {
        return true;
    }
    // 29.97 と 30 の差（0.1%）は 33 秒で 1 フレームずれるので、許すのは 0.01% まで
    let near_int = |x: f64| x >= 0.9999 && (x - x.round()).abs() < 0.0001 * x.max(1.0);
    near_int(a / b) || near_int(b / a)
}

pub fn check(r: &Report, scene: &SceneSettings) -> Vec<Finding> {
    let mut out = Vec::new();
    let warn = |t: String| Finding { level: Level::Warn, text: t };
    let info = |t: String| Finding { level: Level::Info, text: t };
    if let FrameRateKind::Variable(why) = &r.frame_rate {
        out.push(warn(format!(
            "可変フレームレートです（{why}）。音ずれするなら、オブジェクトの「動画ファイル」の「fps調整」でフレームレートを固定してください"
        )));
    }
    let scene_fps = rate_f(scene.fps);
    if let Some(v) = &r.video {
        if !v.still {
            // 固定なら平均、可変なら公称で比べる（可変の平均は下がるだけで、上の注意と同じことを 2 回言うことになる）
            let variable = matches!(r.frame_rate, FrameRateKind::Variable(_));
            let rate = if variable { v.r_rate.or(v.avg_rate) } else { v.avg_rate.or(v.r_rate) };
            if let Some(f) = rate.map(rate_f) {
                if !compatible(f, scene_fps) {
                    out.push(warn(format!("{}fps です。シーンは {}fps", fps_text(f), fps_text(scene_fps))));
                }
            }
            if v.interlaced {
                out.push(warn("インターレースです。縞が出るなら、インターレース解除してから使ってください".into()));
            }
        }
        if (v.width as usize, v.height as usize) != (scene.width, scene.height) && v.width > 0 {
            out.push(info(format!("{}x{} です。シーンは {}x{}", v.width, v.height, scene.width, scene.height)));
        }
        match v.color_transfer.as_deref() {
            Some("smpte2084") => out.push(info("HDR（PQ）です。そのままでは白っぽく・暗く見えることがあります".into())),
            Some("arib-std-b67") => out.push(info("HDR（HLG）です。そのままでは白っぽく・暗く見えることがあります".into())),
            _ => {}
        }
        if v.bit_depth >= 10 {
            out.push(info(format!("{}bit です", v.bit_depth)));
        }
    }
    for (i, a) in r.audio.iter().enumerate() {
        if let Some(sr) = a.sample_rate {
            if sr as usize != scene.sample_rate && scene.sample_rate > 0 {
                let which = if r.audio.len() > 1 { format!("音声 {} は", i + 1) } else { String::new() };
                out.push(info(format!("{which}{} です。シーンは {}", khz(sr as usize), khz(scene.sample_rate))));
            }
        }
    }
    out
}

pub fn khz(hz: usize) -> String {
    if hz % 1000 == 0 {
        format!("{}kHz", hz / 1000)
    } else {
        format!("{:.1}kHz", hz as f64 / 1000.0)
    }
}

pub fn fps_label(r: (u64, u64)) -> String {
    fps_text(rate_f(r))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{parse, tests::MP4};

    const FHD30: SceneSettings = SceneSettings { width: 1920, height: 1080, fps: (30, 1), sample_rate: 48000 };

    fn texts(r: &Report) -> Vec<String> {
        check(r, &FHD30).into_iter().map(|f| f.text).collect()
    }

    #[test]
    fn ntsc_rate_and_44k_are_flagged() {
        let r = parse(MP4.as_bytes()).unwrap();
        let t = texts(&r);
        assert_eq!(t, ["29.97fps です。シーンは 30fps", "44.1kHz です。シーンは 48kHz"]);
        assert_eq!(check(&r, &FHD30)[0].level, Level::Warn);
    }

    #[test]
    fn multiples_are_fine() {
        let mut r = parse(MP4.as_bytes()).unwrap();
        for (num, den) in [(60, 1), (30, 1), (15, 1), (120, 1)] {
            let v = r.video.as_mut().unwrap();
            v.r_rate = Some((num, den));
            v.avg_rate = Some((num, den));
            assert!(!texts(&r).iter().any(|t| t.contains("fps")), "{num}");
        }
        let v = r.video.as_mut().unwrap();
        v.avg_rate = Some((24, 1));
        assert!(texts(&r).iter().any(|t| t == "24fps です。シーンは 30fps"));
    }

    #[test]
    fn vfr_hdr_depth_and_size() {
        let mut r = parse(MP4.as_bytes()).unwrap();
        r.frame_rate = FrameRateKind::Variable("ばらつき".into());
        let v = r.video.as_mut().unwrap();
        v.color_transfer = Some("smpte2084".into());
        v.bit_depth = 10;
        v.width = 1280;
        v.height = 720;
        // 可変で平均が下がっていても（作った見本は公称 30 / 平均 20.886）、fps の注意を重ねない
        v.r_rate = Some((30, 1));
        v.avg_rate = Some((1650, 79));
        let t = texts(&r);
        assert!(t[0].starts_with("可変フレームレートです"));
        assert!(!t.iter().any(|x| x.contains("fps です")), "{t:?}");
        assert!(t.contains(&"HDR（PQ）です。そのままでは白っぽく・暗く見えることがあります".to_string()));
        assert!(t.contains(&"10bit です".to_string()));
        assert!(t.contains(&"1280x720 です。シーンは 1920x1080".to_string()));
    }

    #[test]
    fn labels() {
        assert_eq!(fps_label((30000, 1001)), "29.97");
        assert_eq!(fps_label((24000, 1001)), "23.976");
        assert_eq!(fps_label((30, 1)), "30");
        assert_eq!(khz(44100), "44.1kHz");
        assert_eq!(khz(48000), "48kHz");
    }
}
