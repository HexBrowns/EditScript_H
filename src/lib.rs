//! EditScript_H（編集スクリプト）
//!
//! タイムラインを編集する Lua スクリプト（`Plugin/EditScript_H/scripts/*.lua`）を、パネルのボタンと
//! 右クリックメニューから走らせる。仕様は `AI/specifications/20261008_EditScript_H_spec.md`。
//!
//! - 書き換えは 1 回の実行を 1 回の `call_edit_section` にまとめる（Ctrl+Z 1 回で戻る。`runner.rs`）
//! - 走らせるのはボタンとメニューの操作のときだけ。監視・タイマーからは走らせない
//! - 自分で起こすスレッドは無い

mod gui;
mod movement;
mod runner;
mod script;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use aviutl2::AnyResult;
use aviutl2_eframe::egui;
use parking_lot::{Mutex, RwLock};

use crate::runner::{RunReport, RunRequest};
use crate::script::{Header, MenuPlace, ParamValue};

pub const WINDOW_NAME: &str = "編集スクリプト";
const MENU_ROOT: &str = "編集スクリプト";
/// 止める時間
pub const TIMEOUT: Duration = Duration::from_secs(5);
/// パネルのログに残す実行の数
const MAX_REPORTS: usize = 200;

/// AI に渡す説明書（`assets/API.md`。配布物の `Plugin/EditScript_H/API.md` と同じ）
pub const API_DOC: &str = include_str!("../assets/API.md");

/// プラグイン本体と UI で共有する状態
pub struct Shared {
    pub scripts_dir: PathBuf,
    /// 実行の記録（新しいものが後ろ）
    pub reports: Vec<RunReport>,
    /// スクリプトごとの param の値（パネルで変えたもの。メニューから走らせるときもこれを使う）
    pub param_values: HashMap<PathBuf, HashMap<String, ParamValue>>,
    /// 起動時にメニューへ登録したスクリプトと場所
    pub registered_menus: HashMap<PathBuf, MenuPlace>,
    pub egui_ctx: Option<egui::Context>,
}

pub type SharedState = Arc<RwLock<Shared>>;

impl Shared {
    pub fn push_report(&mut self, r: RunReport) {
        log_report(&r);
        self.reports.push(r);
        if self.reports.len() > MAX_REPORTS {
            let extra = self.reports.len() - MAX_REPORTS;
            self.reports.drain(..extra);
        }
        if let Some(ctx) = &self.egui_ctx {
            ctx.request_repaint();
        }
    }
}

/// 本体のログにも残す（失敗だけ WARN）
fn log_report(r: &RunReport) {
    if r.dry {
        return;
    }
    if r.ok {
        tracing::info!("EditScript_H: {} を実行（書き換え {} 回、{} ms）", r.name, r.writes, r.elapsed.as_millis());
    } else {
        tracing::warn!("EditScript_H: {} が失敗: {}", r.name, r.error.as_deref().unwrap_or(""));
    }
}

/// 見出しの param と、保存してある値から、実行に渡す値を作る
pub fn resolve_params(header: &Header, saved: Option<&HashMap<String, ParamValue>>) -> Vec<(String, ParamValue)> {
    header
        .params
        .iter()
        .map(|def| {
            let v = saved
                .and_then(|m| m.get(&def.name))
                .filter(|v| std::mem::discriminant(*v) == std::mem::discriminant(&def.default))
                .map(|v| def.clamp(v))
                .unwrap_or_else(|| def.default.clone());
            (def.name.clone(), v)
        })
        .collect()
}

/// スクリプトを走らせる。見出しが読めないときは走らせない。`dry` なら予行（書き換えずに記録だけ）
pub fn run_source(shared: &SharedState, path: Option<&Path>, name: &str, source: String, dry: bool) -> RunReport {
    let header = script::parse_header(&source);
    if !header.errors.is_empty() {
        return RunReport::failed(
            name,
            header.mode,
            dry,
            format!("見出しを読めないので走らせませんでした:\n{}", header.errors.join("\n")),
        );
    }
    let params = {
        let s = shared.read();
        resolve_params(&header, path.and_then(|p| s.param_values.get(p)))
    };
    runner::run(RunRequest { name: name.to_string(), source, header, params, timeout: TIMEOUT, dry })
}

/// メニューから: ファイルを読み直して走らせる（パネルで直した後も再起動なしで効く）
fn run_from_menu(shared: &SharedState, path: &Path) {
    let report = match script::ScriptFile::load(path) {
        Ok((f, src)) => run_source(shared, Some(path), &f.display_name().to_string(), src, false),
        Err(e) => RunReport::failed(
            &path.display().to_string(),
            script::Mode::Edit,
            false,
            format!("スクリプトを読めませんでした: {e}"),
        ),
    };
    shared.write().push_report(report);
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

#[aviutl2::plugin(GenericPlugin)]
pub struct EditScriptPlugin {
    window: Mutex<Option<aviutl2_eframe::EframeWindow>>,
    shared: SharedState,
    /// 読み込まれた時刻。これより新しい .tra2 は本体に登録されていない（`movement.rs`）
    loaded_at: std::time::SystemTime,
}

impl aviutl2::generic::GenericPlugin for EditScriptPlugin {
    fn new(_info: aviutl2::AviUtl2Info) -> AnyResult<Self> {
        init_logging();
        tracing::info!("EditScript_H v{} 初期化", env!("CARGO_PKG_VERSION"));
        let scripts_dir = aviutl2::config::app_data_path().join("Plugin").join("EditScript_H").join("scripts");
        if let Err(e) = std::fs::create_dir_all(&scripts_dir) {
            tracing::warn!("スクリプトのフォルダを作れませんでした: {}: {e}", scripts_dir.display());
        }
        Ok(Self {
            window: Mutex::new(None),
            shared: Arc::new(RwLock::new(Shared {
                scripts_dir,
                reports: Vec::new(),
                param_values: HashMap::new(),
                registered_menus: HashMap::new(),
                egui_ctx: None,
            })),
            loaded_at: std::time::SystemTime::now(),
        })
    }

    fn plugin_info(&self) -> aviutl2::generic::GenericPluginTable {
        aviutl2::generic::GenericPluginTable {
            name: "EditScript_H".to_string(),
            information: format!("EditScript_H v{} - 編集スクリプト / by HexBrowns", env!("CARGO_PKG_VERSION")),
        }
    }

    fn register(&mut self, registry: &mut aviutl2::generic::HostAppHandle) {
        runner::EDIT_HANDLE.init(registry.create_edit_handle());

        // トラックバーの値の検査に使う移動方法の名前（本体が起動時に読んだ .tra2 から。aviutl2.ini は使わない）
        let script_dir = aviutl2::config::app_data_path().join("Script");
        let bundled = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("script.tra2")));
        let movements = movement::scan(&script_dir, bundled.as_deref(), Some(self.loaded_at));
        tracing::info!("EditScript_H: 移動方法 {} 件", movements.len());
        runner::init_movements(movements);

        // 右クリックメニュー。登録できるのは起動時だけ
        let dir = self.shared.read().scripts_dir.clone();
        let mut used = std::collections::HashSet::new();
        let mut registered = HashMap::new();
        for f in script::scan(&dir) {
            if f.header.menu == MenuPlace::None || !f.header.errors.is_empty() {
                continue;
            }
            // `\` はサブメニューの区切りなので名前からは外す。同じ名前はファイル名で区別する
            let mut label = f.display_name().replace(char::from(92), "／");
            if !used.insert((f.header.menu, label.clone())) {
                label = format!("{label}（{}）", f.stem);
                used.insert((f.header.menu, label.clone()));
            }
            let menu_name = format!("{MENU_ROOT}{}{label}", char::from(92));
            let shared = Arc::clone(&self.shared);
            let path = f.path.clone();
            let callback = move || run_from_menu(&shared, &path);
            match f.header.menu {
                MenuPlace::Object => registry.register_object_menu(&menu_name, callback),
                MenuPlace::Layer => registry.register_layer_menu(&menu_name, callback),
                MenuPlace::Edit => registry.register_edit_menu(&menu_name, callback),
                MenuPlace::None => unreachable!(),
            }
            registered.insert(f.path.clone(), f.header.menu);
        }
        tracing::info!("EditScript_H: メニューに {} 本を登録", registered.len());
        self.shared.write().registered_menus = registered;

        let shared = Arc::clone(&self.shared);
        let window = match aviutl2_eframe::EframeWindow::new(WINDOW_NAME, move |cc, handle| {
            Ok(Box::new(gui::EditScriptApp::new(cc, handle, shared)))
        }) {
            Ok(w) => w,
            Err(e) => {
                tracing::error!("編集スクリプトのウィンドウを作れませんでした: {e:#}");
                return;
            }
        };
        match window.handle() {
            Ok(handle) => {
                if let Err(e) = registry.register_window_client(WINDOW_NAME, &handle) {
                    tracing::error!("register_window_client 失敗: {e}");
                }
            }
            Err(e) => tracing::error!("編集スクリプトのウィンドウのハンドルを取れませんでした: {e:#}"),
        }
        *self.window.lock() = Some(window);
    }
}

aviutl2::register_generic_plugin!(EditScriptPlugin);
