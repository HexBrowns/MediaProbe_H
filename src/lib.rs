//! MediaProbe_H（素材情報）
//!
//! フォーカス中の動画・音声・画像オブジェクトの素材ファイルについて、コーデック・フレームレート（可変かどうか）・色空間・
//! ビット深度・サンプルレートを出し、シーンの設定と合わないものに印を付ける。情報は ffprobe で取る。
//! 仕様は `AI/specifications/20261005_MediaInfo_H_spec.md`（企画時の名前は MediaInfo_H。MediaInfo は他者の製品名なので改めた）。
//!
//! - 本体には何も書き込まない（`call_edit_section` は一度も呼ばない）。読み取りは `call_read_section` だけ
//! - 調べる処理は 1 本のスレッド（`worker.rs`）。プラグインの `Drop` で止めて終わりを待つ
//! - 第 2 段階: 表示中のシーンの素材をまとめて調べる、可変の判定をファイル全体で行う（どちらもボタンを押したときだけ）

mod check;
mod host_fonts;
mod gui;
mod probe;
mod worker;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Arc;

use aviutl2::raw_window_handle::HasWindowHandle;
use aviutl2::AnyResult;
use parking_lot::{Mutex, RwLock};

use worker::{Shared, SharedState, Status, Worker, EDIT_HANDLE};

pub const WINDOW_NAME: &str = "素材情報";

/// ウィンドウの HWND（見えているかを調べるため）
static WINDOW_HWND: AtomicIsize = AtomicIsize::new(0);

/// ウィンドウが見えていない間に来たイベントの印。見えたときに 1 回だけ調べる（v0.2.1。
/// 出していない間も、選ぶたびに ffprobe を起こしていた。`issues/20261010_probes_while_hidden.md`）
static MISSED: AtomicBool = AtomicBool::new(false);

#[link(name = "user32")]
unsafe extern "system" {
    fn IsWindowVisible(hwnd: isize) -> i32;
}

/// ウィンドウが見えているか（本体のパネルに出ていなければ偽）。ハンドルが取れていなければ見えているとみなす
pub(crate) fn window_visible() -> bool {
    let hwnd = WINDOW_HWND.load(Ordering::Acquire);
    hwnd == 0 || unsafe { IsWindowVisible(hwnd) } != 0
}

/// 見えていない間にイベントが来ていたか（読んだら下ろす）
pub(crate) fn take_missed() -> bool {
    MISSED.swap(false, Ordering::AcqRel)
}

fn init_logging() {
    use aviutl2::tracing::Level;
    let level = if cfg!(debug_assertions) { Level::DEBUG } else { Level::INFO };
    let _ = aviutl2::tracing_subscriber::fmt()
        .with_max_level(level)
        .event_format(aviutl2::logger::AviUtl2Formatter)
        .with_writer(aviutl2::logger::AviUtl2LogWriter)
        .try_init();
}

/// 設定ファイル（`Plugin/MediaProbe_H/設定.json`）。今は ffprobe の場所だけ
pub fn settings_path() -> PathBuf {
    aviutl2::config::app_data_path().join("Plugin").join("MediaProbe_H").join("設定.json")
}

pub fn load_ffprobe_setting() -> Option<PathBuf> {
    let text = std::fs::read_to_string(settings_path()).ok()?;
    let v: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    v.get("ffprobe").and_then(|s| s.as_str()).filter(|s| !s.is_empty()).map(PathBuf::from)
}

pub fn save_ffprobe_setting(path: Option<&std::path::Path>) -> Result<(), String> {
    save_ffprobe_setting_at(&settings_path(), path)
}

/// 設定ファイルの `ffprobe` だけを差し替えて書く（v0.2.1 までは `ffprobe` だけの JSON で丸ごと上書きしていたので、
/// 新しい版が足したキーを古い版で保存すると消えた。`issues/20261010_publish_checklist_audit.md`）。
///
/// - 書く直前にファイルを読み、ほかのキーはそのまま残す
/// - 読めない（JSON のオブジェクトでない・UTF-8 でない・ロック中など）なら `設定.json.broken` へ退避してから書く。
///   退避できなければ書かない（利用者の設定を消さない）
/// - 一時ファイルに書いてから置き換える（書いている途中で落ちても壊れたファイルを残さない）
fn save_ffprobe_setting_at(p: &std::path::Path, ffprobe: Option<&std::path::Path>) -> Result<(), String> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("設定を保存できませんでした: {e}"))?;
    }
    let mut map = match read_settings_object(p) {
        Ok(m) => m,
        Err(why) => {
            let broken = sibling_path(p, ".broken");
            std::fs::rename(p, &broken)
                .map_err(|e| format!("設定ファイルを読めず、退避もできなかったので保存しませんでした（{why} / {e}）"))?;
            tracing::warn!("MediaProbe_H: 設定ファイルを読めなかったので {} へ退避した（{why}）", broken.display());
            serde_json::Map::new()
        }
    };
    let value = ffprobe.map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    map.insert("ffprobe".into(), serde_json::Value::String(value));
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(map)).map_err(|e| format!("設定を保存できませんでした: {e}"))?;
    let tmp = sibling_path(p, ".tmp");
    let written = std::fs::write(&tmp, text.as_bytes()).and_then(|_| std::fs::rename(&tmp, p));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("設定を保存できませんでした: {e}"));
    }
    Ok(())
}

/// 設定ファイルを JSON のオブジェクトとして読む。ファイルが無ければ空。読めなければ理由
fn read_settings_object(p: &std::path::Path) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let bytes = match std::fs::read(p) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(serde_json::Map::new()),
        Err(e) => return Err(format!("読み込みに失敗: {e}")),
    };
    let text = String::from_utf8(bytes).map_err(|_| "UTF-8 ではない".to_string())?;
    match serde_json::from_str(text.trim_start_matches('\u{feff}')) {
        Ok(serde_json::Value::Object(m)) => Ok(m),
        Ok(_) => Err("JSON のオブジェクトではない".into()),
        Err(e) => Err(format!("JSON として読めない: {e}")),
    }
}

/// `設定.json` → `設定.json.broken` のように、名前の後ろに付け足したパス
fn sibling_path(p: &std::path::Path, suffix: &str) -> PathBuf {
    let mut name = p.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(suffix);
    p.with_file_name(name)
}

/// 初期化にかかった時間をログに 1 行出す（強化デバッグ。仕様 `AI/specifications/20261012_CurveEditor_H_キーフレーム速度_spec.md` の 7.0）。
/// `register` の始めに作り、`register` を抜けるとき（途中の `return` も含む）に new と register の時間を出す
struct InitTimeLog {
    new_time: std::time::Duration,
    register_start: std::time::Instant,
}

impl InitTimeLog {
    fn start(new_time: std::time::Duration) -> Self {
        Self { new_time, register_start: std::time::Instant::now() }
    }
}

impl Drop for InitTimeLog {
    fn drop(&mut self) {
        let register = self.register_start.elapsed();
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
        aviutl2::tracing::info!(
            "MediaProbe_H: 初期化 {:.1} ms（new {:.1} ms・register {:.1} ms）",
            ms(self.new_time + register),
            ms(self.new_time),
            ms(register)
        );
    }
}

#[aviutl2::plugin(GenericPlugin)]
pub struct MediaProbePlugin {
    window: Mutex<Option<aviutl2_eframe::EframeWindow>>,
    shared: SharedState,
    worker: Option<Worker>,
    /// `new` にかかった時間（初期化の時間をログに出すため。`InitTimeLog`）
    init_new_time: std::time::Duration,
}

impl MediaProbePlugin {
    /// 調べ直しを頼む。ウィンドウが見えていなければ印だけ立てる（見えたときに `gui.rs` が頼む）
    fn notify_changed(&self) {
        if !window_visible() {
            MISSED.store(true, Ordering::Release);
            return;
        }
        if let Some(w) = &self.worker {
            w.sender().changed();
        }
    }
}

impl aviutl2::generic::GenericPlugin for MediaProbePlugin {
    fn new(_info: aviutl2::AviUtl2Info) -> AnyResult<Self> {
        let init_t0 = std::time::Instant::now();
        init_logging();
        tracing::info!("MediaProbe_H v{} 初期化", env!("CARGO_PKG_VERSION"));
        let shared = Arc::new(RwLock::new(Shared {
            status: Status::Idle(String::new()),
            scan: Default::default(),
            whole_running: false,
            egui_ctx: None,
            ffprobe_setting: load_ffprobe_setting(),
        }));
        Ok(Self { window: Mutex::new(None), shared, worker: None, init_new_time: init_t0.elapsed() })
    }

    fn plugin_info(&self) -> aviutl2::generic::GenericPluginTable {
        aviutl2::generic::GenericPluginTable {
            name: "MediaProbe_H".to_string(),
            information: format!("MediaProbe_H v{} - 素材情報 / by HexBrowns", env!("CARGO_PKG_VERSION")),
        }
    }

    fn register(&mut self, registry: &mut aviutl2::generic::HostAppHandle) {
        // 抜けるときに初期化の時間をログに出す（`InitTimeLog`）
        let _init_time = InitTimeLog::start(self.init_new_time);
        EDIT_HANDLE.init(registry.create_edit_handle());
        let plugin_dir = aviutl2::config::app_data_path().join("Plugin");
        let worker = Worker::start(Arc::clone(&self.shared), plugin_dir.clone());
        let notifier = worker.sender();
        self.worker = Some(worker);

        let shared = Arc::clone(&self.shared);
        let window = match aviutl2_eframe::EframeWindow::new(WINDOW_NAME, move |cc, handle| {
            Ok(Box::new(gui::MediaProbeApp::new(cc, handle, shared, notifier, plugin_dir)))
        }) {
            Ok(w) => w,
            Err(e) => {
                tracing::error!("素材情報のウィンドウを作れませんでした: {e:#}");
                return;
            }
        };
        match window.handle() {
            Ok(handle) => {
                if let Err(e) = registry.register_window_client(WINDOW_NAME, &handle) {
                    tracing::error!("register_window_client 失敗: {e}");
                }
                if let Ok(raw) = handle.window_handle() {
                    if let aviutl2::raw_window_handle::RawWindowHandle::Win32(h) = raw.as_raw() {
                        WINDOW_HWND.store(h.hwnd.get(), Ordering::Release);
                    }
                }
            }
            Err(e) => tracing::error!("素材情報のウィンドウのハンドルを取れませんでした: {e:#}"),
        }
        *self.window.lock() = Some(window);
    }

    /// フォーカスが変わったら通知だけを送る（読み取りはスレッドで行う。ここでは本体の API を呼ばない）
    fn event_change_focus_object(&mut self) {
        self.notify_changed();
    }

    /// 設定が変わったとき（ファイルを差し替えたなど）も読み直す
    fn event_update_object_info(&mut self) {
        self.notify_changed();
    }
}

impl Drop for MediaProbePlugin {
    fn drop(&mut self) {
        // 本体が DLL を外す前に、調べる処理のスレッドを止めて終わりを待つ（Worker の Drop）
        self.worker.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn temp_settings(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("settings_test").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("設定.json")
    }

    fn read_json(p: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    /// 新しい版が足したキー（ここでは "window" と "future"）を、保存しても消さない
    #[test]
    fn save_keeps_unknown_keys() {
        let p = temp_settings("keep");
        std::fs::write(&p, "\u{feff}{\"ffprobe\": \"C:/old/ffprobe.exe\", \"future\": 3, \"window\": {\"w\": 640}}").unwrap();
        save_ffprobe_setting_at(&p, Some(Path::new("D:/ffmpeg/ffprobe.exe"))).unwrap();
        let v = read_json(&p);
        assert_eq!(v["ffprobe"], "D:/ffmpeg/ffprobe.exe");
        assert_eq!(v["future"], 3);
        assert_eq!(v["window"]["w"], 640);
        assert!(!sibling_path(&p, ".tmp").exists(), "一時ファイルが残った");
        assert!(!sibling_path(&p, ".broken").exists());
    }

    /// 空で保存すると "ffprobe" は空文字（読むときは「探す」になる）。ほかのキーは残る
    #[test]
    fn save_empty_clears_only_ffprobe() {
        let p = temp_settings("empty");
        std::fs::write(&p, r#"{"ffprobe": "C:/x/ffprobe.exe", "future": true}"#).unwrap();
        save_ffprobe_setting_at(&p, None).unwrap();
        let v = read_json(&p);
        assert_eq!(v["ffprobe"], "");
        assert_eq!(v["future"], true);
    }

    #[test]
    fn save_creates_new_file() {
        let p = temp_settings("new");
        save_ffprobe_setting_at(&p, Some(Path::new("C:/a/ffprobe.exe"))).unwrap();
        assert_eq!(read_json(&p), serde_json::json!({ "ffprobe": "C:/a/ffprobe.exe" }));
    }

    /// 読めないファイルは上書きせずに退避してから書く
    #[test]
    fn save_backs_up_broken_file() {
        let p = temp_settings("broken");
        std::fs::write(&p, "{ not json").unwrap();
        save_ffprobe_setting_at(&p, Some(Path::new("C:/a/ffprobe.exe"))).unwrap();
        assert_eq!(std::fs::read_to_string(sibling_path(&p, ".broken")).unwrap(), "{ not json");
        assert_eq!(read_json(&p)["ffprobe"], "C:/a/ffprobe.exe");

        // JSON でもオブジェクトでなければ同じ
        let p = temp_settings("array");
        std::fs::write(&p, "[1, 2]").unwrap();
        save_ffprobe_setting_at(&p, None).unwrap();
        assert_eq!(std::fs::read_to_string(sibling_path(&p, ".broken")).unwrap(), "[1, 2]");
    }
}

aviutl2::register_generic_plugin!(MediaProbePlugin);
