//! 素材情報のウィンドウ（egui）

use std::path::PathBuf;

use aviutl2_eframe::{eframe, egui, AviUtl2EframeHandle};

use crate::check::{fps_label, khz, Level};
use crate::probe::{self, FrameRateKind};
use crate::worker::{Notifier, Probed, SceneItem, SharedState, Status, EDIT_HANDLE};

const YELLOW: egui::Color32 = egui::Color32::from_rgb(235, 200, 90);
const RED: egui::Color32 = egui::Color32::from_rgb(235, 110, 110);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    /// 選んでいるオブジェクトの素材
    Focused,
    /// 表示中のシーンの素材をまとめて（第 2 段階）
    Scene,
}

pub struct MediaProbeApp {
    _handle: AviUtl2EframeHandle,
    shared: SharedState,
    notifier: Notifier,
    plugin_dir: PathBuf,
    settings_open: bool,
    ffprobe_text: String,
    note: String,
    tab: Tab,
    /// シーンの一覧で、注意があるものだけを出す
    warn_only: bool,
}

/// 一覧の行をクリックしたときだけ、そのオブジェクトにフォーカスを移す（`call_edit_section`。マウスは離れている）
fn focus(handle: aviutl2::generic::ObjectHandle) -> Result<(), String> {
    if !EDIT_HANDLE.is_ready() {
        return Err("本体の準備ができていません".into());
    }
    EDIT_HANDLE
        .call_edit_section(move |edit| {
            if edit.object_exists(handle) {
                let _ = edit.set_focus_object(Some(handle));
            }
        })
        .map_err(|e| format!("フォーカスを移せませんでした: {e:?}"))
}

fn size_text(n: u64) -> String {
    let f = n as f64;
    if f >= 1e9 {
        format!("{:.2}GB", f / 1e9)
    } else if f >= 1e6 {
        format!("{:.1}MB", f / 1e6)
    } else {
        format!("{:.0}KB", f / 1e3)
    }
}

fn time_text(sec: f64) -> String {
    let s = sec.max(0.0);
    let m = (s / 60.0).floor();
    format!("{}:{:05.2}", m as u64, s - m * 60.0)
}

fn opt(v: &Option<String>) -> &str {
    v.as_deref().unwrap_or("—")
}

/// 表とコピー用の行（項目, 値）
fn rows(p: &Probed) -> Vec<(String, Vec<(String, String)>)> {
    let r = &p.report;
    let mut out = Vec::new();
    let mut common = vec![
        ("ファイル".to_string(), p.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
        ("大きさ".into(), size_text(p.size)),
        ("コンテナ".into(), r.container.clone()),
    ];
    if let Some(d) = r.duration {
        common.push(("長さ".into(), time_text(d)));
    }
    out.push(("ファイル".to_string(), common));
    if let Some(v) = &r.video {
        let mut rows = vec![
            ("コーデック".to_string(), v.codec.clone()),
            ("解像度".into(), format!("{}x{}", v.width, v.height)),
        ];
        if !v.still {
            let fr = match (v.r_rate, v.avg_rate) {
                (Some(r), Some(a)) if r == a => format!("{}fps", fps_label(r)),
                (Some(r), Some(a)) => format!("{}fps（平均 {}）", fps_label(r), fps_label(a)),
                (Some(r), None) => format!("{}fps", fps_label(r)),
                (None, Some(a)) => format!("平均 {}fps", fps_label(a)),
                (None, None) => "—".into(),
            };
            rows.push(("フレームレート".into(), fr));
            let kind = match &r.frame_rate {
                FrameRateKind::Constant => "固定".to_string(),
                FrameRateKind::Variable(why) => format!("可変（{why}）"),
                FrameRateKind::Unknown => "—".into(),
            };
            let kind = if r.frame_rate_whole { format!("{kind}（全体で判定）") } else { kind };
            rows.push(("固定 / 可変".into(), kind));
            if v.interlaced {
                rows.push(("走査".into(), "インターレース".into()));
            }
        }
        rows.push(("画素形式".into(), format!("{} {}bit", v.pix_fmt, v.bit_depth)));
        rows.push(("アルファ".into(), if v.alpha { "あり" } else { "なし" }.into()));
        rows.push(("色空間".into(), format!("行列 {} / 転送 {} / 原色 {}", opt(&v.color_space), opt(&v.color_transfer), opt(&v.color_primaries))));
        let range = match v.color_range.as_deref() {
            Some("tv") => "リミテッド（tv）",
            Some("pc") => "フル（pc）",
            _ => "—",
        };
        rows.push(("レンジ".into(), range.into()));
        out.push((if v.still { "画像" } else { "映像" }.to_string(), rows));
    }
    for (i, a) in r.audio.iter().enumerate() {
        let title = if r.audio.len() > 1 { format!("音声 {}", i + 1) } else { "音声".into() };
        out.push((
            title,
            vec![
                ("コーデック".to_string(), a.codec.clone()),
                ("サンプルレート".into(), a.sample_rate.map(|s| khz(s as usize)).unwrap_or_else(|| "—".into())),
                ("チャンネル".into(), format!("{}{}", a.channels.map(|c| c.to_string()).unwrap_or_else(|| "—".into()), a.layout.as_ref().map(|l| format!("（{l}）")).unwrap_or_default())),
                ("サンプル形式".into(), opt(&a.sample_fmt).to_string()),
            ],
        ));
    }
    if let Some(h) = &p.host {
        out.push((
            "本体の読み込み".to_string(),
            vec![
                ("トラック".to_string(), format!("映像 {} / 音声 {}", h.video_tracks, h.audio_tracks)),
                ("長さ".into(), time_text(h.seconds)),
                ("解像度".into(), format!("{}x{}", h.width, h.height)),
            ],
        ));
    }
    let s = &p.scene;
    out.push((
        "シーン".to_string(),
        vec![("設定".to_string(), format!("{}x{} / {}fps / {}", s.width, s.height, fps_label(s.fps), khz(s.sample_rate)))],
    ));
    out
}

fn copy_text(p: &Probed) -> String {
    let mut t = format!("{}（{}）\n", p.path.display(), p.effect);
    for (title, rs) in rows(p) {
        t += &format!("[{title}]\n");
        for (k, v) in rs {
            t += &format!("  {k}: {v}\n");
        }
    }
    for f in &p.findings {
        let mark = if f.level == Level::Warn { "注意" } else { "情報" };
        t += &format!("{mark}: {}\n", f.text);
    }
    t += &format!("（ffprobe: {}）\n", p.ffprobe.display());
    t
}

impl MediaProbeApp {
    pub fn new(cc: &eframe::CreationContext<'_>, handle: AviUtl2EframeHandle, shared: SharedState, notifier: Notifier, plugin_dir: PathBuf) -> Self {
        cc.egui_ctx.all_styles_mut(|style| {
            style.visuals = aviutl2_eframe::aviutl2_visuals();
        });
        cc.egui_ctx.set_fonts(aviutl2_eframe::aviutl2_fonts());
        let ffprobe_text = shared.read().ffprobe_setting.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
        shared.write().egui_ctx = Some(cc.egui_ctx.clone());
        Self { _handle: handle, shared, notifier, plugin_dir, settings_open: false, ffprobe_text, note: String::new(), tab: Tab::Focused, warn_only: true }
    }

    fn render_settings(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("ffprobe.exe:");
            ui.add(egui::TextEdit::singleline(&mut self.ffprobe_text).desired_width(260.0).hint_text("空なら PATH と Plugin フォルダから探す"));
            if ui.button("保存").clicked() {
                let t = self.ffprobe_text.trim().trim_matches('"');
                let p = (!t.is_empty()).then(|| PathBuf::from(t));
                if let Some(p) = p.as_ref().filter(|p| !p.is_file()) {
                    self.note = format!("{} が見つかりません", p.display());
                } else {
                    match crate::save_ffprobe_setting(p.as_deref()) {
                        Ok(()) => {
                            self.shared.write().ffprobe_setting = p;
                            self.note = "保存しました".into();
                            self.notifier.force();
                        }
                        Err(e) => self.note = e,
                    }
                }
            }
        });
        let setting = self.shared.read().ffprobe_setting.clone();
        let found = probe::find_ffprobe(setting.as_deref(), &self.plugin_dir);
        ui.weak(match found {
            Some(p) => format!("使っている ffprobe: {}", p.display()),
            None => "ffprobe.exe が見つかりません。FFmpeg を入れるか、場所を指定してください".into(),
        });
        if !self.note.is_empty() {
            ui.weak(&self.note);
        }
    }

    fn render_scene(&mut self, ui: &mut egui::Ui) {
        let scan = self.shared.read().scan.clone();
        ui.horizontal(|ui| {
            if scan.running {
                if ui.button("やめる").clicked() {
                    self.notifier.cancel();
                }
                ui.spinner();
                ui.weak(format!("調べています {} / {}", scan.done, scan.total));
            } else if ui.button("シーンの素材を調べる").on_hover_text("表示中のシーンの動画・音声・画像の素材ファイルを全部調べます").clicked() {
                self.notifier.scan_scene();
            }
            ui.checkbox(&mut self.warn_only, "注意があるものだけ");
        });
        if let Some(e) = &scan.error {
            ui.colored_label(RED, e);
        }
        if !scan.finished {
            if !scan.running {
                ui.weak("「シーンの素材を調べる」で、可変フレームレートや fps の違う素材を探します");
            }
            return;
        }
        let warn = scan.items.iter().filter(|i| i.warns() > 0).count();
        ui.label(format!("シーン「{}」の素材 {} 個のうち、注意があるもの {warn} 個", scan.scene_name, scan.items.len()));
        ui.separator();
        let mut clicked = None;
        let shown: Vec<&SceneItem> = scan.items.iter().filter(|i| !self.warn_only || i.warns() > 0).collect();
        if shown.is_empty() {
            ui.weak(if self.warn_only { "注意があるものはありません" } else { "素材がありません" });
        }
        for item in shown {
            ui.horizontal(|ui| {
                ui.strong(item.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
                    .on_hover_text(item.path.display().to_string());
                ui.weak(&item.effect);
            });
            ui.horizontal_wrapped(|ui| {
                ui.add_space(12.0);
                for (h, layer, start) in &item.objects {
                    let b = ui.add(egui::Label::new(egui::RichText::new(format!("L{} {start}", layer + 1)).monospace()).sense(egui::Sense::click()));
                    if b.on_hover_text("クリックでこのオブジェクトにフォーカスを移す").clicked() {
                        clicked = Some(*h);
                    }
                }
            });
            if let Some(e) = &item.error {
                ui.colored_label(RED, format!("    {e}"));
            }
            for f in &item.findings {
                let (color, mark) = if f.level == Level::Warn { (YELLOW, "⚠ ") } else { (ui.visuals().weak_text_color(), "ℹ ") };
                ui.colored_label(color, format!("    {mark}{}", f.text));
            }
            ui.add_space(4.0);
        }
        if let Some(h) = clicked {
            // フォーカスが移ると、「選んだ素材」の欄にも詳しく出る
            if let Err(e) = focus(h) {
                self.note = e;
            }
        }
        if !self.note.is_empty() {
            ui.weak(&self.note);
        }
    }

    fn render_probed(&self, ui: &mut egui::Ui, p: &Probed) {
        ui.horizontal(|ui| {
            ui.strong(p.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
                .on_hover_text(p.path.display().to_string());
            ui.weak(&p.effect);
        });
        for f in &p.findings {
            let (color, mark) = if f.level == Level::Warn { (YELLOW, "⚠ ") } else { (ui.visuals().text_color(), "ℹ ") };
            ui.colored_label(color, format!("{mark}{}", f.text));
        }
        if p.findings.is_empty() {
            ui.weak("シーンの設定と合わない点はありません");
        }
        if p.report.video.as_ref().is_some_and(|v| !v.still) {
            ui.horizontal(|ui| {
                if self.shared.read().whole_running {
                    if ui.button("やめる").clicked() {
                        self.notifier.cancel();
                    }
                    ui.spinner();
                    ui.weak("ファイル全体のフレームの間隔を調べています");
                } else if p.report.frame_rate_whole {
                    ui.weak("固定 / 可変はファイル全体で判定しました");
                } else if ui
                    .small_button("全体で判定")
                    .on_hover_text("固定 / 可変を、先頭 5 秒ではなくファイル全体で判定し直します（長い素材は時間がかかります）")
                    .clicked()
                {
                    self.notifier.whole();
                }
            });
        }
        ui.separator();
        for (i, (title, rs)) in rows(p).into_iter().enumerate() {
            egui::CollapsingHeader::new(&title).id_salt(i).default_open(title != "本体の読み込み").show(ui, |ui| {
                egui::Grid::new(("grid", i)).num_columns(2).striped(true).show(ui, |ui| {
                    for (k, v) in rs {
                        ui.weak(k);
                        ui.add(egui::Label::new(v).selectable(true).wrap());
                        ui.end_row();
                    }
                });
            });
        }
        ui.add_space(4.0);
        ui.weak("素材ファイルの情報です（本体がどう読んだかではありません。本体の分は「本体の読み込み」）");
    }
}

impl eframe::App for MediaProbeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 見えていない間に選び直していたら、見えた今、最新の分を 1 回だけ調べる（`lib.rs` の `notify_changed`）
        if crate::window_visible() && crate::take_missed() {
            self.notifier.changed();
        }
        let status = self.shared.read().status.clone();
        egui::Panel::bottom("bottom").show(ui, |ui| {
            ui.horizontal(|ui| {
                if let Status::Done(p) = &status {
                    if ui.button("コピー").on_hover_text("全項目をテキストでコピーします（質問や issue に貼る用）").clicked() {
                        ui.ctx().copy_text(copy_text(p));
                    }
                }
                if ui.button("調べ直す").clicked() {
                    self.notifier.force();
                }
                ui.toggle_value(&mut self.settings_open, "設定");
            });
            if self.settings_open {
                self.render_settings(ui);
            }
        });
        // 中央パネルは最後に追加する
        egui::Panel::top("tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Focused, "選んだ素材");
                ui.selectable_value(&mut self.tab, Tab::Scene, "シーンの素材");
            });
        });
        egui::CentralPanel::default().show(ui, |ui| {
            if self.tab == Tab::Scene {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.render_scene(ui));
                return;
            }
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match &status {
                Status::Idle(msg) => {
                    ui.weak(msg);
                }
                Status::Probing(p) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.weak(format!("調べています: {}", p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
                    });
                }
                Status::Error(e) => {
                    ui.colored_label(RED, e);
                }
                Status::Done(p) => self.render_probed(ui, p),
            });
        });
    }
}
