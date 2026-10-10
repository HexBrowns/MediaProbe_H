//! 調べる処理のスレッド（1 本）
//!
//! - フォーカスが変わったら（`event_change_focus_object`）通知だけを受け取り、0.3 秒まとめてから
//!   `call_read_section` でフォーカス中のオブジェクトのファイルを読む。**このスレッドから書き込み（`call_edit_section`）は呼ばない**
//! - ffprobe は新しい依頼が来たら途中でやめる（古い依頼は捨てて最新だけを調べる）
//! - 結果は「パス + 更新日時 + 大きさ」で覚える（同じファイルは 2 回調べない）
//! - 第 2 段階: 表示中のシーンの素材をまとめて調べる（`Msg::ScanScene`）、可変の判定をファイル全体で行う（`Msg::Whole`）。
//!   まとめて調べている間は、フォーカス中の素材は終わってから調べる
//! - プラグインの `Drop` で止める合図を出し、動いている ffprobe を殺してから終わりを待つ
//!   （`.claude/rules/au2-rs-plugin.md`「裏のスレッドは、プラグインが外れる前に止めて終わりを待つ」）

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use aviutl2::generic::{GlobalEditHandle, ObjectHandle};
use aviutl2_eframe::egui;
use parking_lot::RwLock;

use crate::check::{self, Finding, Level, SceneSettings};
use crate::probe::{self, Report};

pub static EDIT_HANDLE: GlobalEditHandle = GlobalEditHandle::new();

/// 素材ファイルを持つ効果（効果名。項目名はどれも「ファイル」）
const MEDIA_EFFECTS: &[&str] = &["動画ファイル", "音声ファイル", "画像ファイル"];
const DEBOUNCE: Duration = Duration::from_millis(300);
const JOIN_LIMIT: Duration = Duration::from_secs(3);

/// 本体が読んだときの情報（`get_media_info`）
#[derive(Debug, Clone, PartialEq)]
pub struct HostMedia {
    pub video_tracks: usize,
    pub audio_tracks: usize,
    pub seconds: f64,
    pub width: usize,
    pub height: usize,
}

#[derive(Debug, Clone)]
pub struct Probed {
    pub path: PathBuf,
    /// 動画ファイル / 音声ファイル / 画像ファイル
    pub effect: String,
    pub size: u64,
    pub report: Report,
    pub host: Option<HostMedia>,
    pub scene: SceneSettings,
    pub findings: Vec<Finding>,
    pub ffprobe: PathBuf,
}

#[derive(Debug, Clone)]
pub enum Status {
    /// 何も調べていない（理由）
    Idle(String),
    Probing(PathBuf),
    Done(Box<Probed>),
    Error(String),
}

/// シーンの素材を使うオブジェクト 1 つ（行をクリックしたときにフォーカスを移す先）。
///
/// ハンドルはスキャンの後も持ち越すので、使う前に `SceneScan.scene_id` と、この効果の「ファイル」が
/// `SceneItem.path` のままかを確かめる（`gui.rs` の `focus`。`.claude/rules/au2-plugin-checklist.md`「オブジェクトのハンドル」）
#[derive(Debug, Clone)]
pub struct SceneObject {
    pub handle: ObjectHandle,
    pub layer: usize,
    pub start: usize,
    /// この素材を読んでいる効果（動画ファイル / 音声ファイル / 画像ファイル）
    pub effect: String,
}

/// シーンの素材 1 つ（同じファイルを使うオブジェクトはまとめる）
#[derive(Debug, Clone)]
pub struct SceneItem {
    pub path: PathBuf,
    pub effect: String,
    pub objects: Vec<SceneObject>,
    pub findings: Vec<Finding>,
    pub error: Option<String>,
}

impl SceneItem {
    pub fn warns(&self) -> usize {
        self.findings.iter().filter(|f| f.level == Level::Warn).count() + usize::from(self.error.is_some())
    }
}

#[derive(Debug, Clone, Default)]
pub struct SceneScan {
    pub running: bool,
    pub done: usize,
    pub total: usize,
    pub scene_name: String,
    /// 調べたシーンの ID（行のクリックで、今のシーンと同じかを確かめる）
    pub scene_id: Option<i32>,
    pub items: Vec<SceneItem>,
    pub finished: bool,
    pub error: Option<String>,
}

pub struct Shared {
    pub status: Status,
    pub scan: SceneScan,
    /// 「全体で判定」を走らせている
    pub whole_running: bool,
    pub egui_ctx: Option<egui::Context>,
    /// 設定した ffprobe.exe（空なら探す）
    pub ffprobe_setting: Option<PathBuf>,
}

pub type SharedState = Arc<RwLock<Shared>>;

pub enum Msg {
    /// フォーカスが変わった
    Changed,
    /// 「調べ直す」（覚えている結果を使わない）
    Force,
    /// 表示中のシーンの素材をまとめて調べる
    ScanScene,
    /// 今出しているファイルの可変の判定を、ファイル全体で行う
    Whole,
    Stop,
}

pub struct Worker {
    tx: Sender<Msg>,
    stop: Arc<AtomicBool>,
    gen: Arc<AtomicU64>,
    job: Arc<AtomicU64>,
    handle: Option<JoinHandle<()>>,
}

impl Worker {
    pub fn start(shared: SharedState, plugin_dir: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let gen = Arc::new(AtomicU64::new(0));
        let job = Arc::new(AtomicU64::new(0));
        let ctx = Ctx { shared, plugin_dir, stop: Arc::clone(&stop), gen: Arc::clone(&gen), job: Arc::clone(&job), cache: HashMap::new() };
        let handle = std::thread::spawn(move || run(rx, ctx));
        Self { tx, stop, gen, job, handle: Some(handle) }
    }

    pub fn sender(&self) -> Notifier {
        Notifier { tx: self.tx.clone(), gen: Arc::clone(&self.gen), job: Arc::clone(&self.job) }
    }
}

/// UI とイベントから送る口
#[derive(Clone)]
pub struct Notifier {
    tx: Sender<Msg>,
    /// フォーカスの依頼の世代（新しい依頼が来たら、フォーカス中の素材の調べを途中でやめる）
    gen: Arc<AtomicU64>,
    /// 「まとめて調べる」「全体で判定」の世代（「やめる」でだけ進める。フォーカスの変化では止めない）
    job: Arc<AtomicU64>,
}

impl Notifier {
    pub fn changed(&self) {
        self.gen.fetch_add(1, Ordering::AcqRel);
        let _ = self.tx.send(Msg::Changed);
    }

    pub fn force(&self) {
        self.gen.fetch_add(1, Ordering::AcqRel);
        let _ = self.tx.send(Msg::Force);
    }

    pub fn scan_scene(&self) {
        let _ = self.tx.send(Msg::ScanScene);
    }

    pub fn whole(&self) {
        let _ = self.tx.send(Msg::Whole);
    }

    /// 走っている「まとめて調べる」「全体で判定」をやめる
    pub fn cancel(&self) {
        self.job.fetch_add(1, Ordering::AcqRel);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.tx.send(Msg::Stop);
        if let Some(h) = self.handle.take() {
            let deadline = Instant::now() + JOIN_LIMIT;
            while !h.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if h.is_finished() {
                let _ = h.join();
            } else {
                tracing::warn!("MediaProbe_H: 調べる処理のスレッドが {} 秒で止まりませんでした", JOIN_LIMIT.as_secs());
            }
        }
    }
}

type CacheKey = (PathBuf, SystemTime, u64);

struct Ctx {
    shared: SharedState,
    plugin_dir: PathBuf,
    stop: Arc<AtomicBool>,
    gen: Arc<AtomicU64>,
    job: Arc<AtomicU64>,
    cache: HashMap<CacheKey, Report>,
}

impl Ctx {
    fn repaint(&self) {
        if let Some(ctx) = &self.shared.read().egui_ctx {
            ctx.request_repaint();
        }
    }

    fn set(&self, status: Status) {
        self.shared.write().status = status;
        self.repaint();
    }

    fn ffprobe(&self) -> Option<PathBuf> {
        let setting = self.shared.read().ffprobe_setting.clone();
        probe::find_ffprobe(setting.as_deref(), &self.plugin_dir)
    }

    /// 覚えている結果か、調べた結果。`cancel` が立ったら Err(None)
    fn report(&mut self, exe: &Path, key: &CacheKey, force: bool, cancel: &dyn Fn() -> bool) -> Result<Report, Option<String>> {
        if !force {
            if let Some(r) = self.cache.get(key) {
                return Ok(r.clone());
            }
        }
        match probe::probe(exe, &key.0, cancel) {
            Ok(r) => {
                self.cache.insert(key.clone(), r.clone());
                Ok(r)
            }
            Err(_) if cancel() => Err(None),
            Err(e) => Err(Some(e)),
        }
    }
}

fn hint() -> String {
    concat!(
        "フォーカス中のオブジェクト（オブジェクト設定に出ている 1 つ）が動画・音声・画像なら、その素材ファイルの情報を出します。",
        "複数を選んでいても、出すのはフォーカス中のオブジェクトだけです"
    )
    .into()
}

/// フォーカス中のオブジェクトの素材ファイルと、本体が読んだときの情報
fn read_focused() -> Result<Option<(String, PathBuf, Option<HostMedia>)>, String> {
    if !EDIT_HANDLE.is_ready() {
        return Err("本体の準備ができていません".into());
    }
    EDIT_HANDLE
        .call_read_section(|read| {
            let Ok(Some(handle)) = read.get_focused_object() else { return None };
            for effect in MEDIA_EFFECTS {
                if let Ok(file) = read.get_object_effect_item(handle, effect, 0, "ファイル") {
                    if file.is_empty() {
                        return None;
                    }
                    let host = read.get_media_info(&file).ok().map(|m| HostMedia {
                        video_tracks: m.video_track_num.map(|n| n.get()).unwrap_or(0),
                        audio_tracks: m.audio_track_num.map(|n| n.get()).unwrap_or(0),
                        seconds: m.total_time,
                        width: m.width,
                        height: m.height,
                    });
                    return Some((effect.to_string(), PathBuf::from(file), host));
                }
            }
            None
        })
        .map_err(|e| format!("読み取りに失敗しました: {e:?}"))
}

/// 表示中のシーンの素材ファイルを全部読む（同じファイルはまとめる）。シーンの名前と ID も返す
fn read_scene_media() -> Result<(String, i32, Vec<SceneItem>), String> {
    if !EDIT_HANDLE.is_ready() {
        return Err("本体の準備ができていません".into());
    }
    EDIT_HANDLE
        .call_read_section(move |read| {
            // 読み取りの中で取る（同じスレッドが持っている参照ロックのまま読める。plugin2.h の get_edit_info）。
            // 外で取ると、その間にシーンが切り替わったときに ID と中身が食い違う
            let info = EDIT_HANDLE.get_edit_info();
            let (layer_max, scene_id) = (info.layer_max, info.scene_id);
            let name = read.get_scene_name().unwrap_or_default();
            let mut items: Vec<SceneItem> = Vec::new();
            for layer in 0..=layer_max {
                for (lf, handle) in read.objects_in_layer(layer) {
                    for effect in MEDIA_EFFECTS {
                        let Ok(file) = read.get_object_effect_item(handle, effect, 0, "ファイル") else { continue };
                        if file.is_empty() {
                            break;
                        }
                        let path = PathBuf::from(&file);
                        let object = SceneObject { handle, layer: lf.layer, start: lf.start, effect: effect.to_string() };
                        match items.iter_mut().find(|i| i.path == path) {
                            Some(i) => i.objects.push(object),
                            None => items.push(SceneItem {
                                path,
                                effect: effect.to_string(),
                                objects: vec![object],
                                findings: Vec::new(),
                                error: None,
                            }),
                        }
                        break;
                    }
                }
            }
            (name, scene_id, items)
        })
        .map_err(|e| format!("読み取りに失敗しました: {e:?}"))
}

fn scene_settings() -> SceneSettings {
    let info = EDIT_HANDLE.get_edit_info();
    let (n, d) = (*info.fps.numer(), *info.fps.denom());
    SceneSettings {
        width: info.width,
        height: info.height,
        fps: (n.max(1) as u64, d.max(1) as u64),
        sample_rate: info.sample_rate,
    }
}

fn key_of(path: &Path) -> Option<CacheKey> {
    let m = std::fs::metadata(path).ok()?;
    Some((path.to_path_buf(), m.modified().ok()?, m.len()))
}

fn run(rx: Receiver<Msg>, mut c: Ctx) {
    c.set(Status::Idle(hint()));
    let mut queue: VecDeque<Msg> = VecDeque::new();
    loop {
        let msg = match queue.pop_front() {
            Some(m) => m,
            None => match rx.recv() {
                Ok(m) => m,
                Err(_) => return,
            },
        };
        match msg {
            Msg::Stop => return,
            Msg::Changed | Msg::Force => {
                let mut force = matches!(msg, Msg::Force);
                // 続けて来た通知をまとめる（フォーカスを次々に変えたら最後の 1 つだけ）。ほかの依頼は後に回す
                let until = Instant::now() + DEBOUNCE;
                loop {
                    let left = until.saturating_duration_since(Instant::now());
                    match rx.recv_timeout(left) {
                        Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                        Ok(Msg::Changed) => {}
                        Ok(Msg::Force) => force = true,
                        Ok(other) => queue.push_back(other),
                        Err(RecvTimeoutError::Timeout) => break,
                    }
                }
                focused(&mut c, force);
            }
            Msg::ScanScene => scan(&mut c),
            Msg::Whole => whole(&mut c),
        }
        if c.stop.load(Ordering::Acquire) {
            return;
        }
    }
}

fn focused(c: &mut Ctx, force: bool) {
    let my_gen = c.gen.load(Ordering::Acquire);
    let (effect, path, host) = match read_focused() {
        Ok(Some(x)) => x,
        Ok(None) => return c.set(Status::Idle(hint())),
        Err(e) => return c.set(Status::Error(e)),
    };
    let scene = scene_settings();
    let Some(key) = key_of(&path) else {
        return c.set(Status::Error(format!("ファイルが見つかりません: {}", path.display())));
    };
    let Some(exe) = c.ffprobe() else {
        return c.set(Status::Error("ffprobe.exe が見つかりません。下の「設定」で場所を指定してください".into()));
    };
    if force || !c.cache.contains_key(&key) {
        c.set(Status::Probing(path.clone()));
    }
    let (stop, gen) = (Arc::clone(&c.stop), Arc::clone(&c.gen));
    let cancel = move || stop.load(Ordering::Acquire) || gen.load(Ordering::Acquire) != my_gen;
    let report = match c.report(&exe, &key, force, &cancel) {
        Ok(r) => r,
        Err(None) => return, // 新しい依頼か止める合図。次の通知で調べ直す
        Err(Some(e)) => return c.set(Status::Error(e)),
    };
    let findings = check::check(&report, &scene);
    tracing::info!(
        "MediaProbe_H: {}（注意 {} / 情報 {}）",
        effect,
        findings.iter().filter(|f| f.level == Level::Warn).count(),
        findings.iter().filter(|f| f.level == Level::Info).count()
    );
    c.set(Status::Done(Box::new(Probed { path, effect, size: key.2, report, host, scene, findings, ffprobe: exe })));
}

/// 表示中のシーンの素材をまとめて調べる
fn scan(c: &mut Ctx) {
    let my_job = c.job.load(Ordering::Acquire);
    let started = Instant::now();
    let fail = |c: &Ctx, e: String| {
        c.shared.write().scan = SceneScan { finished: true, error: Some(e), ..Default::default() };
        c.repaint();
    };
    let Some(exe) = c.ffprobe() else {
        return fail(c, "ffprobe.exe が見つかりません。下の「設定」で場所を指定してください".into());
    };
    let (scene_name, scene_id, mut items) = match read_scene_media() {
        Ok(x) => x,
        Err(e) => return fail(c, e),
    };
    let scene = scene_settings();
    c.shared.write().scan =
        SceneScan { running: true, total: items.len(), scene_name: scene_name.clone(), scene_id: Some(scene_id), ..Default::default() };
    c.repaint();
    let (stop, job) = (Arc::clone(&c.stop), Arc::clone(&c.job));
    let cancel = move || stop.load(Ordering::Acquire) || job.load(Ordering::Acquire) != my_job;
    for (n, item) in items.iter_mut().enumerate() {
        if cancel() {
            let mut s = c.shared.write();
            s.scan.running = false;
            s.scan.finished = true;
            s.scan.error = Some(format!("やめました（{n} / {} 個）", s.scan.total));
            drop(s);
            return c.repaint();
        }
        match key_of(&item.path) {
            None => item.error = Some("ファイルが見つかりません".into()),
            Some(key) => match c.report(&exe, &key, false, &cancel) {
                Ok(r) => item.findings = check::check(&r, &scene),
                Err(None) => continue, // 次の周の頭でやめる
                Err(Some(e)) => item.error = Some(e),
            },
        }
        c.shared.write().scan.done = n + 1;
        c.repaint();
    }
    let warn = items.iter().filter(|i| i.warns() > 0).count();
    tracing::info!("MediaProbe_H: シーンの素材 {} 個を調べた（注意あり {warn} 個、{:.1} 秒）", items.len(), started.elapsed().as_secs_f64());
    c.shared.write().scan =
        SceneScan { running: false, done: items.len(), total: items.len(), scene_name, scene_id: Some(scene_id), items, finished: true, error: None };
    c.repaint();
}

/// 今出しているファイルの可変の判定を、ファイル全体で行う
fn whole(c: &mut Ctx) {
    let my_job = c.job.load(Ordering::Acquire);
    let Status::Done(p) = c.shared.read().status.clone() else { return };
    let Some(video) = p.report.video.clone().filter(|v| !v.still) else { return };
    let Some(key) = key_of(&p.path) else { return };
    c.shared.write().whole_running = true;
    c.repaint();
    let (stop, job) = (Arc::clone(&c.stop), Arc::clone(&c.job));
    let cancel = move || stop.load(Ordering::Acquire) || job.load(Ordering::Acquire) != my_job;
    let started = Instant::now();
    let result = probe::judge_file(&p.ffprobe, &p.path, &video, true, &cancel);
    c.shared.write().whole_running = false;
    match result {
        Ok(kind) => {
            let mut report = p.report.clone();
            report.frame_rate = kind;
            report.frame_rate_whole = true;
            c.cache.insert(key, report.clone());
            tracing::info!("MediaProbe_H: 全体で判定（{:.1} 秒）: {:?}", started.elapsed().as_secs_f64(), report.frame_rate);
            let findings = check::check(&report, &p.scene);
            // その間にほかの素材を選んでいたら、出している結果は差し替えない
            let mut s = c.shared.write();
            if matches!(&s.status, Status::Done(cur) if cur.path == p.path) {
                s.status = Status::Done(Box::new(Probed { report, findings, ..(*p).clone() }));
            }
        }
        Err(_) if cancel() => {}
        Err(e) => c.set(Status::Error(e)),
    }
    c.repaint();
}
