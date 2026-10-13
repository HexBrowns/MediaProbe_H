// 生成物: 正本は AI/tools/plugin_common/host_fonts.rs（開発用のリポジトリ）。ここを直さず、正本を直して
// `python AI/tools/sync_plugin_common.py` で写す（`--check` で同じかを確かめる）。
//! 本体のフォントを egui に渡す（強化デバッグ 7.5。起動を速くする）。
//!
//! aviutl2-eframe の `aviutl2_fonts()` は、呼ぶたびに fontdb でシステムのフォントと `Font/` を全部読む
//! （作者の PC で約 1240 ファイル・4.9GB）。egui の窓を持つプラグインは、これを `App::new`（本体の `register` が
//! 窓の初期化を待つ）で呼んでいたので、本体の起動がプラグインの数だけ遅くなっていた。ここでは 2 つ変える。
//!
//! - **後で読む**（[`LazyFonts`]）: `App::new` では読まず、`ui` の先頭で、窓が見えているときに 1 回だけ読む。
//!   読んだフレームは描かずに次のフレームを頼む（`set_fonts` は次のフレームから効く）。スレッドは起こさない。
//!   見えていない窓は、見えるまで egui の既定のフォントのまま（描いても見えない）
//! - **要る書体だけを引く**（[`host_font_definitions`]）: Yu Gothic UI と、本体の style.conf の `Control` / `EditControl` の書体を
//!   DirectWrite でファイルまで引き、そのファイルだけをメモリに写す（同じファイルは 1 回。写しはプロセスの間持ち続け、
//!   egui には借りた形で渡すので、egui の中でも複製されない）。システムに無ければ `Font/` を探す。
//!   見つからない書体があるとき・DirectWrite が失敗したときは `aviutl2_fonts()` に戻す
//!
//! 作る結果は `aviutl2_fonts()`（aviutl2-eframe 0.45〜0.49 で同じ）と同じにする: 登録の名前（`aviutl2::Fallback` /
//! `aviutl2::Control` / `aviutl2::EditControl`）・並び・`y_offset_factor`・面の番号。
//! **面の番号は 0 に固定する。** `aviutl2_fonts()` は fontdb が見つけた面の番号を捨て、ファイルの先頭の面を使っている
//! （Yu Gothic UI の Regular は `YuGothM.ttc` の 1 番目で、0 番目は Yu Gothic Medium。今までの見た目はこちら）。
//! 見た目を変えないため、同じ形にしている。
//!
//! 正本は `AI/tools/plugin_common/host_fonts.rs`（開発用のリポジトリ）。各プラグインの `src/host_fonts.rs` はその写し。

#![allow(dead_code)]

use std::collections::HashMap;
use std::ffi::{c_void, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use aviutl2::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use aviutl2_eframe::egui;
use aviutl2_eframe::AviUtl2EframeHandle;
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, HWND};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory5, IDWriteFontFaceReference, IDWriteFontSet, IDWriteLocalFontFileLoader,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_LOCALITY_LOCAL,
};
use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_EXISTING};
use windows::Win32::System::Memory::{CreateFileMappingW, MapViewOfFile, FILE_MAP_READ, PAGE_READONLY};
use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;

/// `aviutl2_fonts()` が必ず足す書体（本体のハードコード）
pub const FALLBACK_FAMILY: &str = "Yu Gothic UI";
pub const KEY_FALLBACK: &str = "aviutl2::Fallback";
pub const KEY_CONTROL: &str = "aviutl2::Control";
pub const KEY_EDIT_CONTROL: &str = "aviutl2::EditControl";

/// フォントの定義をどこから作ったか（ログ用）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontSource {
    /// DirectWrite で 3 書体だけを引いた
    DirectWrite,
    /// `aviutl2_fonts()` に戻した（全部のフォントを読む。遅い）
    Upstream,
}

impl FontSource {
    pub fn label(self) -> &'static str {
        match self {
            FontSource::DirectWrite => "DirectWrite",
            FontSource::Upstream => "aviutl2_fonts",
        }
    }
}

/// 窓が見えてから本体のフォントを読む。App に 1 つ持ち、`ui` の先頭で [`LazyFonts::load_if_visible`] を呼ぶ。
pub struct LazyFonts {
    hwnd: Option<isize>,
    /// 本体の設定から読んだ書体名と `Font/`。**`new`（本体が `register` で待っている間）に読んでおく。**
    /// `ui` は egui のスレッドで、本体の終了処理（このプラグインの Drop が egui のスレッドの終わりを待つ）と重なることがある。
    /// そこで本体の API（`aviutl2::config`）を呼ぶと、本体と待ち合って終了が止まるおそれがある（2026-10-13、終了で止まった件）
    names: HostFontNames,
    loaded: bool,
    control_in_monospace: bool,
    log_name: Option<&'static str>,
}

impl LazyFonts {
    /// `App::new` で受け取った窓のハンドルから作る（ハンドルは見えているかを調べるのに使う）
    pub fn new(handle: &AviUtl2EframeHandle) -> Self {
        let hwnd = handle.window_handle().ok().and_then(|h| match h.as_raw() {
            RawWindowHandle::Win32(w) => Some(w.hwnd.get()),
            _ => None,
        });
        Self { hwnd, names: HostFontNames::from_host(), loaded: false, control_in_monospace: false, log_name: None }
    }

    /// Monospace（等幅の欄）に日本語の字形が無いと □ になるので、Control の書体を Monospace の最後に足す
    pub fn control_in_monospace(mut self) -> Self {
        self.control_in_monospace = true;
        self
    }

    /// 読んだ時間を INFO で 1 行出す（`<name>: フォント N ms（DirectWrite）`）
    pub fn log_time(mut self, name: &'static str) -> Self {
        self.log_name = Some(name);
        self
    }

    /// まだ読んでいない かつ 窓が見えているなら、ここで読んで `set_fonts` する。
    /// **true を返したフレームは描かずに `ui` から戻る**（新しいフォントは次のフレームから効く。次のフレームは頼んである）
    pub fn load_if_visible(&mut self, ctx: &egui::Context) -> bool {
        if self.loaded {
            return false;
        }
        if let Some(hwnd) = self.hwnd {
            if !window_visible(hwnd) {
                return false;
            }
        }
        self.loaded = true;
        let t0 = Instant::now();
        let control_in_monospace = self.control_in_monospace;
        let names = self.names.clone();
        let result = std::panic::catch_unwind(move || {
            let (mut defs, source) = definitions_for(&names);
            if control_in_monospace {
                add_control_to_monospace(&mut defs);
            }
            (defs, source)
        });
        let (defs, source) = match result {
            Ok(v) => v,
            Err(_) => {
                tracing::warn!("{}: 本体のフォントを読む途中で panic しました。egui の既定のフォントのままにします", env!("CARGO_PKG_NAME"));
                return false;
            }
        };
        ctx.set_fonts(defs);
        if let Some(name) = self.log_name {
            tracing::info!("{}: フォント {:.1} ms（{}）", name, t0.elapsed().as_secs_f64() * 1000.0, source.label());
        }
        ctx.request_repaint();
        true
    }
}

fn window_visible(hwnd: isize) -> bool {
    unsafe { IsWindowVisible(HWND(hwnd as *mut c_void)).as_bool() }
}

/// Monospace の最後に Control の書体を足す（CurveEditor_H・EditScript_H の等幅の欄）
pub fn add_control_to_monospace(defs: &mut egui::FontDefinitions) {
    if defs.font_data.contains_key(KEY_CONTROL) {
        let mono = defs.families.entry(egui::FontFamily::Monospace).or_default();
        if !mono.iter().any(|n| n == KEY_CONTROL) {
            mono.push(KEY_CONTROL.to_string());
        }
    }
}

/// 本体の設定から読む書体名と `Font/`（本体の API を呼ぶのはここだけ）
#[derive(Clone, Debug, Default)]
pub struct HostFontNames {
    pub control: String,
    pub edit: String,
    pub font_dir: PathBuf,
}

impl HostFontNames {
    /// 本体の設定（`aviutl2::config`）が読める状態で、本体が待っている間に呼ぶ（`App::new` など）
    pub fn from_host() -> Self {
        Self {
            control: aviutl2::config::get_font_info("Control").map(|f| f.name).unwrap_or_default(),
            edit: aviutl2::config::get_font_info("EditControl").map(|f| f.name).unwrap_or_default(),
            font_dir: aviutl2::config::app_data_path().join("Font"),
        }
    }
}

/// 本体のフォントの定義（`aviutl2_fonts()` と同じ結果）。本体の設定（`aviutl2::config`）が読める状態で呼ぶ
pub fn host_font_definitions() -> (egui::FontDefinitions, FontSource) {
    definitions_for(&HostFontNames::from_host())
}

/// 読んでおいた書体名から定義を作る。**DirectWrite で引ける間は本体の API を呼ばない**
/// （引けずに `aviutl2_fonts()` に戻すときだけ、それが本体の設定を読む）
pub fn definitions_for(names: &HostFontNames) -> (egui::FontDefinitions, FontSource) {
    match build_definitions(&names.control, &names.edit, &names.font_dir) {
        Ok(defs) => (defs, FontSource::DirectWrite),
        Err(e) => {
            tracing::warn!("{}: 本体のフォントを DirectWrite で引けなかったので、aviutl2_fonts() で読み直します（{}）", env!("CARGO_PKG_NAME"), e);
            (aviutl2_eframe::aviutl2_fonts(), FontSource::Upstream)
        }
    }
}

/// `control` / `edit` は style.conf の書体名（空なら `aviutl2_fonts()` と同じく足さない）。`font_dir` は本体の `Font/`
pub fn build_definitions(control: &str, edit: &str, font_dir: &Path) -> Result<egui::FontDefinitions, String> {
    let finder = Finder::new(font_dir)?;
    let fallback = map_file(&finder.find(FALLBACK_FAMILY)?)?;
    let control_data = if control.is_empty() { None } else { Some(map_file(&finder.find(control)?)?) };
    let edit_data = if edit.is_empty() { None } else { Some(map_file(&finder.find(edit)?)?) };

    let mut fonts = egui::FontDefinitions::default();
    let yu_gothic_tweak = || egui::FontTweak { y_offset_factor: 0.2, ..Default::default() };

    // 足す順も `aviutl2_fonts()` と同じ（どれも先頭へ差し込むので、後に足したものが前に来る）
    insert_front(&mut fonts, KEY_FALLBACK, font_data(fallback, yu_gothic_tweak()), &[egui::FontFamily::Proportional, egui::FontFamily::Monospace]);
    if let Some(data) = control_data {
        // `aviutl2_fonts()` と同じく、名前がちょうど "Yu Gothic UI" のときだけずらす
        let tweak = if control == FALLBACK_FAMILY { yu_gothic_tweak() } else { egui::FontTweak::default() };
        insert_front(&mut fonts, KEY_CONTROL, font_data(data, tweak), &[egui::FontFamily::Proportional]);
    }
    if let Some(data) = edit_data {
        insert_front(&mut fonts, KEY_EDIT_CONTROL, font_data(data, egui::FontTweak::default()), &[egui::FontFamily::Monospace]);
    }
    Ok(fonts)
}

fn font_data(bytes: &'static [u8], tweak: egui::FontTweak) -> egui::FontData {
    let mut data = egui::FontData::from_static(bytes);
    data.index = 0; // ファイルの先頭の面（`aviutl2_fonts()` と同じ。先頭の説明）
    data.tweak = tweak;
    data
}

fn insert_front(fonts: &mut egui::FontDefinitions, key: &str, data: egui::FontData, families: &[egui::FontFamily]) {
    fonts.font_data.insert(key.to_string(), std::sync::Arc::new(data));
    for family in families {
        fonts.families.entry(family.clone()).or_default().insert(0, key.to_string());
    }
}

/// 書体名からフォントファイルを引く（システム → `Font/`）
pub fn resolve(name: &str, font_dir: &Path) -> Result<PathBuf, String> {
    Finder::new(font_dir)?.find(name)
}

struct Finder {
    factory: IDWriteFactory5,
    system: IDWriteFontSet,
    font_dir: PathBuf,
    /// `Font/` のフォントセット（システムで見つからなかったときだけ作る）
    user: std::cell::OnceCell<Option<IDWriteFontSet>>,
}

impl Finder {
    fn new(font_dir: &Path) -> Result<Self, String> {
        let factory: IDWriteFactory5 =
            unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }.map_err(|e| format!("DWriteCreateFactory: {e}"))?;
        let system = unsafe { factory.GetSystemFontSet() }.map_err(|e| format!("GetSystemFontSet: {e}"))?;
        Ok(Self { factory, system, font_dir: font_dir.to_path_buf(), user: std::cell::OnceCell::new() })
    }

    fn find(&self, name: &str) -> Result<PathBuf, String> {
        if let Some(path) = match_in(&self.system, name)? {
            return Ok(path);
        }
        let user = self.user.get_or_init(|| user_font_set(&self.factory, &self.font_dir));
        if let Some(set) = user {
            if let Some(path) = match_in(set, name)? {
                return Ok(path);
            }
        }
        Err(format!("書体「{name}」がシステムにも Font フォルダにも無い"))
    }
}

/// 名前（ファミリー名・WWS のファミリー名・GDI のファミリー名・フル名のどれか）で、標準の太さ・幅・斜体に最も近い面のファイル
fn match_in(set: &IDWriteFontSet, name: &str) -> Result<Option<PathBuf>, String> {
    let name_w = wide(name);
    unsafe {
        let matched = set
            .GetMatchingFonts(PCWSTR(name_w.as_ptr()), DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL)
            .map_err(|e| format!("GetMatchingFonts({name}): {e}"))?;
        for i in 0..matched.GetFontCount() {
            let Ok(face) = matched.GetFontFaceReference(i) else {
                continue;
            };
            if face.GetLocality() != DWRITE_LOCALITY_LOCAL {
                continue;
            }
            if let Some(path) = local_path(&face) {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

fn local_path(face: &IDWriteFontFaceReference) -> Option<PathBuf> {
    unsafe {
        let file = face.GetFontFile().ok()?;
        let mut key: *mut c_void = std::ptr::null_mut();
        let mut size = 0u32;
        file.GetReferenceKey(&mut key, &mut size).ok()?;
        let loader: IDWriteLocalFontFileLoader = file.GetLoader().ok()?.cast().ok()?;
        let len = loader.GetFilePathLengthFromKey(key, size).ok()? as usize;
        let mut buf = vec![0u16; len + 1];
        loader.GetFilePathFromKey(key, size, &mut buf).ok()?;
        buf.truncate(len);
        Some(PathBuf::from(OsString::from_wide(&buf)))
    }
}

fn user_font_set(factory: &IDWriteFactory5, dir: &Path) -> Option<IDWriteFontSet> {
    let mut files = Vec::new();
    collect_font_files(dir, 0, &mut files);
    if files.is_empty() {
        return None;
    }
    unsafe {
        let builder = factory.CreateFontSetBuilder().ok()?;
        for path in &files {
            let path_w = wide_path(path);
            if let Ok(file) = factory.CreateFontFileReference(PCWSTR(path_w.as_ptr()), None) {
                let _ = builder.AddFontFile(&file);
            }
        }
        builder.CreateFontSet().ok()
    }
}

/// fontdb の `load_fonts_dir` と同じく、下のフォルダまでたどって .ttf / .ttc / .otf / .otc を集める
fn collect_font_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 16 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_font_files(&path, depth + 1, out);
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| ["ttf", "ttc", "otf", "otc"].iter().any(|x| e.eq_ignore_ascii_case(x)))
        {
            out.push(path);
        }
    }
}

/// フォントファイルを読み取り専用でメモリに写す。同じファイルは 1 回だけ（プロセスの間持ち続ける。
/// 写しはファイルと結び付いているので、プラグインどうし・本体とも物理メモリを分け合う）
fn map_file(path: &Path) -> Result<&'static [u8], String> {
    static MAPPED: OnceLock<Mutex<HashMap<PathBuf, &'static [u8]>>> = OnceLock::new();
    let mut cache = MAPPED.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&bytes) = cache.get(path) {
        return Ok(bytes);
    }
    let len = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?.len();
    if len == 0 || len > isize::MAX as u64 {
        return Err(format!("{}: 大きさが {len} バイト", path.display()));
    }
    let path_w = wide_path(path);
    let bytes = unsafe {
        let file = CreateFileW(PCWSTR(path_w.as_ptr()), GENERIC_READ.0, FILE_SHARE_READ, None, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, None)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let mapping = CreateFileMappingW(file, None, PAGE_READONLY, 0, 0, PCWSTR::null());
        let _ = CloseHandle(file);
        let mapping = mapping.map_err(|e| format!("{}: {e}", path.display()))?;
        let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
        let _ = CloseHandle(mapping); // 写しが生きている間は、写しの側が中身を保つ
        if view.Value.is_null() {
            return Err(format!("{}: MapViewOfFile に失敗", path.display()));
        }
        std::slice::from_raw_parts(view.Value as *const u8, len as usize)
    };
    cache.insert(path.to_path_buf(), bytes);
    Ok(bytes)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_path(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_font_dir() -> PathBuf {
        std::env::temp_dir().join("host_fonts_test_no_such_dir")
    }

    /// 13pt で描いたときの字の幅。字形が無ければ 0（`has_glyph` は、代わりの字形「◻」を持つ書体と同じ書体で
    /// 見つかった字を「無い」と答えるので使えない。egui 0.36 で確かめた）
    fn glyph_width(defs: egui::FontDefinitions, family: egui::FontFamily, c: char) -> f32 {
        let ctx = egui::Context::default();
        ctx.set_fonts(defs);
        let mut out = ctx.run_ui(Default::default(), |_| {});
        out.textures_delta.clear();
        ctx.fonts_mut(|f| f.glyph_width(&egui::FontId::new(13.0, family), c))
    }

    /// 日本語が □ にならない（本番の style.conf の既定: Control は Yu Gothic UI、EditControl は Consolas）
    #[test]
    fn japanese_glyphs_are_available() {
        let defs = build_definitions("Yu Gothic UI", "Consolas", &no_font_dir()).expect("3 書体を引けること");
        // 全角の字は 1em（13pt）の幅で描かれる
        for (family, c) in [
            (egui::FontFamily::Proportional, 'あ'),
            (egui::FontFamily::Proportional, '漢'),
            (egui::FontFamily::Monospace, 'あ'),
        ] {
            let w = glyph_width(defs.clone(), family.clone(), c);
            assert!(w >= 12.0, "{family:?} の「{c}」の幅が {w}");
        }
        // 比べ: egui の既定のフォントには日本語が無い（幅 0）
        assert_eq!(glyph_width(egui::FontDefinitions::default(), egui::FontFamily::Proportional, 'あ'), 0.0);
    }

    /// 登録の名前と並びが `aviutl2_fonts()` と同じ
    #[test]
    fn layout_matches_upstream() {
        let defs = build_definitions("Yu Gothic UI", "Consolas", &no_font_dir()).unwrap();
        let prop = &defs.families[&egui::FontFamily::Proportional];
        let mono = &defs.families[&egui::FontFamily::Monospace];
        assert_eq!(&prop[..2], &[KEY_CONTROL.to_string(), KEY_FALLBACK.to_string()]);
        assert_eq!(&mono[..2], &[KEY_EDIT_CONTROL.to_string(), KEY_FALLBACK.to_string()]);
        let fallback = &defs.font_data[KEY_FALLBACK];
        let control = &defs.font_data[KEY_CONTROL];
        let edit = &defs.font_data[KEY_EDIT_CONTROL];
        assert_eq!(fallback.index, 0);
        assert_eq!(fallback.tweak.y_offset_factor, 0.2);
        assert_eq!(control.tweak.y_offset_factor, 0.2, "Control が Yu Gothic UI のときはずらす");
        assert_eq!(edit.tweak.y_offset_factor, 0.0);
        // 同じファイルは 1 回だけ写す（Fallback と Control は同じ写しを指す）
        assert_eq!(fallback.font.as_ptr(), control.font.as_ptr());
        assert!(matches!(fallback.font, std::borrow::Cow::Borrowed(_)));

        let other = build_definitions("Meiryo", "", &no_font_dir()).unwrap();
        assert_eq!(other.font_data[KEY_CONTROL].tweak.y_offset_factor, 0.0, "Yu Gothic UI 以外はずらさない");
        assert!(!other.font_data.contains_key(KEY_EDIT_CONTROL), "名前が空なら足さない");
    }

    /// 名前の大小・日本語の名前でも同じファイルを引く（`aviutl2_fonts()` も大小を区別しない）
    #[test]
    fn names_match_case_insensitively_and_in_japanese() {
        let dir = no_font_dir();
        let a = resolve("Yu Gothic UI", &dir).unwrap();
        assert_eq!(resolve("yu gothic ui", &dir).unwrap(), a);
        assert_eq!(resolve("YU GOTHIC UI", &dir).unwrap(), a);
        assert_eq!(resolve("CONSOLAS", &dir).unwrap(), resolve("Consolas", &dir).unwrap());
        assert_eq!(resolve("メイリオ", &dir).unwrap(), resolve("Meiryo", &dir).unwrap());
        assert_eq!(resolve("游ゴシック", &dir).unwrap(), resolve("Yu Gothic", &dir).unwrap());
    }

    #[test]
    fn missing_family_is_an_error() {
        assert!(build_definitions("No Such Font 7c1e", "Consolas", &no_font_dir()).is_err());
    }
}
