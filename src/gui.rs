//! 編集スクリプトのウィンドウ（egui）
//!
//! 一覧・本文・param・実行ボタン・ログ。実行は「実行」ボタンを押したときだけ（`runner.rs`）。
//! 本文は保存しなくても実行できる（エディタの中身を走らせる）。メニューから走るのは保存したファイル。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use aviutl2_eframe::{eframe, egui, AviUtl2EframeHandle};

use crate::script::{self, MenuPlace, Mode, ParamKind, ParamValue, ScriptFile};
use crate::{runner, SharedState};

const RED: egui::Color32 = egui::Color32::from_rgb(235, 110, 110);
const GREEN: egui::Color32 = egui::Color32::from_rgb(110, 200, 110);
const YELLOW: egui::Color32 = egui::Color32::from_rgb(225, 195, 85);

fn menu_label(m: MenuPlace) -> &'static str {
    match m {
        MenuPlace::None => "",
        MenuPlace::Object => "オブジェクト",
        MenuPlace::Layer => "レイヤー",
        MenuPlace::Edit => "編集",
    }
}

pub struct EditScriptApp {
    _handle: AviUtl2EframeHandle,
    shared: SharedState,
    scripts: Vec<ScriptFile>,
    scanned: bool,
    selected: Option<PathBuf>,
    /// エディタの中身と、読み込んだときの中身（変更の有無を見る）
    text: String,
    loaded_text: String,
    status: String,
}

impl EditScriptApp {
    pub fn new(cc: &eframe::CreationContext<'_>, handle: AviUtl2EframeHandle, shared: SharedState) -> Self {
        cc.egui_ctx.all_styles_mut(|style| {
            style.visuals = aviutl2_eframe::aviutl2_visuals();
        });
        // Monospace（本文の欄）に日本語グリフが無いと □ になるため、Control フォントをフォールバックに足す
        let mut fonts = aviutl2_eframe::aviutl2_fonts();
        if fonts.font_data.contains_key("aviutl2::Control") {
            let mono = fonts.families.entry(egui::FontFamily::Monospace).or_default();
            if !mono.iter().any(|n| n == "aviutl2::Control") {
                mono.push("aviutl2::Control".to_string());
            }
        }
        cc.egui_ctx.set_fonts(fonts);
        shared.write().egui_ctx = Some(cc.egui_ctx.clone());
        Self {
            _handle: handle,
            shared,
            scripts: Vec::new(),
            scanned: false,
            selected: None,
            text: String::new(),
            loaded_text: String::new(),
            status: String::new(),
        }
    }

    fn dir(&self) -> PathBuf {
        self.shared.read().scripts_dir.clone()
    }

    fn dirty(&self) -> bool {
        self.selected.is_some() && self.text != self.loaded_text
    }

    fn rescan(&mut self) {
        self.scripts = script::scan(&self.dir());
        self.scanned = true;
        if let Some(sel) = &self.selected {
            if !self.scripts.iter().any(|s| &s.path == sel) {
                self.selected = None;
                self.text.clear();
                self.loaded_text.clear();
            }
        }
    }

    fn select(&mut self, path: &Path) {
        match script::read_source(path) {
            Ok(src) => {
                self.selected = Some(path.to_path_buf());
                self.text = src.clone();
                self.loaded_text = src;
                self.status.clear();
            }
            Err(e) => self.status = format!("読めませんでした: {e}"),
        }
    }

    fn save(&mut self) {
        let Some(path) = self.selected.clone() else { return };
        match std::fs::write(&path, self.text.as_bytes()) {
            Ok(()) => {
                self.loaded_text = self.text.clone();
                self.status = format!("保存しました: {}", path.file_name().unwrap_or_default().to_string_lossy());
                self.rescan();
            }
            Err(e) => self.status = format!("保存できませんでした: {e}"),
        }
    }

    fn create_new(&mut self) {
        let dir = self.dir();
        let mut path = dir.join("新しいスクリプト.lua");
        let mut n = 2;
        while path.exists() {
            path = dir.join(format!("新しいスクリプト{n}.lua"));
            n += 1;
        }
        if let Err(e) = std::fs::write(&path, script::TEMPLATE) {
            self.status = format!("作れませんでした: {e}");
            return;
        }
        self.rescan();
        self.select(&path);
    }

    fn run_current(&mut self, dry: bool) {
        let Some(path) = self.selected.clone() else { return };
        let header = script::parse_header(&self.text);
        let name = header.name.clone().unwrap_or_else(|| {
            path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
        });
        let report = crate::run_source(&self.shared, Some(&path), &name, self.text.clone(), dry);
        self.status = match (dry, report.ok) {
            (true, _) => format!("予行しました（書き換えの予定 {} 件。タイムラインは変わっていません）", report.plan.len()),
            (false, true) => "実行しました".into(),
            (false, false) => "失敗しました（ログを見てください）".into(),
        };
        self.shared.write().push_report(report);
    }

    /// AI に渡す説明書と、今の選択の要約をクリップボードへ
    fn copy_for_ai(&mut self, ctx: &egui::Context) {
        let summary = selection_summary();
        let mut text = String::from(crate::API_DOC);
        text.push_str("\n\n## 今の状態（コピーした時点）\n\n");
        text.push_str(&summary);
        ctx.copy_text(text);
        self.status = "AI 向けの説明と今の選択をコピーしました。チャットに貼って、作りたい処理を書き添えてください".into();
    }

    fn render_top(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("新規").clicked() {
                self.create_new();
            }
            if ui.button("フォルダを開く").clicked() {
                let _ = std::process::Command::new("explorer").arg(self.dir()).spawn();
            }
            if ui.button("一覧を読み直す").on_hover_text("フォルダに足したスクリプトを一覧に出す（メニューに出すには再起動が要る）").clicked() {
                self.rescan();
            }
            if ui
                .button("AI 向けの説明をコピー")
                .on_hover_text("edit の使い方の説明書と、今の選択の要約をクリップボードへ写す。Claude などのチャットに貼って頼むと、このパネルで動くスクリプトを書いてくれる")
                .clicked()
            {
                self.copy_for_ai(ui.ctx());
            }
        });
    }

    fn render_list(&mut self, ui: &mut egui::Ui) {
        let registered = self.shared.read().registered_menus.clone();
        let mut clicked = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            if self.scripts.is_empty() {
                ui.weak("スクリプトがありません。「新規」で作るか、フォルダに .lua を置いてください");
            }
            for s in &self.scripts {
                let sel = self.selected.as_ref() == Some(&s.path);
                let mut label = s.display_name().to_string();
                if s.header.mode == Mode::Read {
                    label.push_str("  [読む]");
                }
                let resp = ui.selectable_label(sel, label);
                let menu_now = s.header.menu;
                let menu_reg = registered.get(&s.path).copied().unwrap_or(MenuPlace::None);
                let mut hover = s.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                if menu_now != MenuPlace::None {
                    hover.push_str(&format!("\n右クリックメニュー: {}", menu_label(menu_now)));
                }
                if menu_now != menu_reg {
                    hover.push_str("\nメニューへの反映には AviUtl2 の再起動が要る");
                }
                if !s.header.errors.is_empty() {
                    hover.push_str("\n見出しに誤りがある");
                }
                if resp.on_hover_text(hover).clicked() && !sel {
                    clicked = Some(s.path.clone());
                }
            }
        });
        if let Some(p) = clicked {
            if self.dirty() {
                self.status = "保存していない変更があります。保存するか「元に戻す」を押してから切り替えてください".into();
            } else {
                self.select(&p);
            }
        }
    }

    fn render_params(&mut self, ui: &mut egui::Ui, header: &script::Header) {
        if header.params.is_empty() {
            return;
        }
        let Some(path) = self.selected.clone() else { return };
        let mut s = self.shared.write();
        let values: &mut HashMap<String, ParamValue> = s.param_values.entry(path.clone()).or_default();
        // 値は打ち込みの途中では変えず、Enter かほかをクリックしたときに確定する。入力中の Esc は取り消し
        // （ルール au2-rs-plugin「入力の確定と取り消し」）
        egui::Grid::new("params").num_columns(2).show(ui, |ui| {
            for def in &header.params {
                ui.label(&def.name);
                let v = values.entry(def.name.clone()).or_insert_with(|| def.default.clone());
                if std::mem::discriminant(&*v) != std::mem::discriminant(&def.default) {
                    *v = def.default.clone();
                }
                match (def.kind, v) {
                    (ParamKind::Int, ParamValue::Int(i)) => {
                        drag_value(ui, i, |d| match (def.min, def.max) {
                            (Some(lo), Some(hi)) => d.speed(0.2).range(lo as i64..=hi as i64),
                            _ => d.speed(0.2),
                        });
                    }
                    (ParamKind::Float, ParamValue::Float(f)) => {
                        drag_value(ui, f, |d| match (def.min, def.max) {
                            (Some(lo), Some(hi)) => d.speed(0.01).range(lo..=hi),
                            _ => d.speed(0.01),
                        });
                    }
                    (ParamKind::Str, ParamValue::Str(t)) => {
                        committed_text(ui, ("param", &path, &def.name), t);
                    }
                    (ParamKind::Bool, ParamValue::Bool(b)) => {
                        ui.checkbox(b, "");
                    }
                    _ => {}
                }
                ui.end_row();
            }
        });
    }

    fn render_editor(&mut self, ui: &mut egui::Ui) {
        if self.selected.is_none() {
            ui.weak("左の一覧からスクリプトを選んでください");
            return;
        }
        let header = script::parse_header(&self.text);
        for e in &header.errors {
            ui.colored_label(RED, format!("見出し: {e}"));
        }
        self.render_params(ui, &header);

        ui.horizontal(|ui| {
            let (label, hover) = match (header.mode, header.scene) {
                (Mode::Read, _) => ("読み取り", "読むだけ。タイムラインは変わらない"),
                (Mode::Edit, true) => ("実行（シーンの変更は Undo できません）", "シーンの設定（名前・サイズ・フレームレート）は Ctrl+Z で戻らない"),
                (Mode::Edit, false) => ("実行（Ctrl+Z 1 回で戻る）", "このスクリプトの書き換えはまとめて 1 回の Undo になる"),
            };
            let run = ui.add_enabled(header.errors.is_empty(), egui::Button::new(label)).on_hover_text(hover);
            if run.clicked() {
                self.run_current(false);
            }
            if header.mode == Mode::Edit {
                let dry = ui
                    .add_enabled(header.errors.is_empty(), egui::Button::new("予行"))
                    .on_hover_text("書き換えずに、何をどう書き換えるかの一覧だけをログに出す。作る予定のものを読もうとした所で止まる");
                if dry.clicked() {
                    self.run_current(true);
                }
            }
            if ui.add_enabled(self.dirty(), egui::Button::new("保存")).clicked() {
                self.save();
            }
            if ui.add_enabled(self.dirty(), egui::Button::new("元に戻す")).on_hover_text("保存した内容に戻す").clicked() {
                self.text = self.loaded_text.clone();
            }
            if self.dirty() {
                ui.colored_label(YELLOW, "未保存（実行はこの本文で走る。メニューから走るのは保存した内容）");
            }
        });

        // 本文の欄は Esc で戻さない（書いた内容が消える）。Esc はフォーカスを外すだけ（egui の既定。`lock_focus` は Tab だけを取る）
        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut self.text)
                    .font(egui::TextStyle::Monospace)
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .desired_rows(20)
                    .lock_focus(true),
            );
        });
    }

    fn render_log(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("ログ");
            if ui.small_button("消す").clicked() {
                self.shared.write().reports.clear();
            }
            if !self.status.is_empty() {
                ui.separator();
                ui.small(&self.status);
            }
        });
        let reports = self.shared.read().reports.clone();
        egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            for r in &reports {
                if r.dry {
                    render_dry_report(ui, r);
                    ui.separator();
                    continue;
                }
                let head = if r.ok {
                    match r.mode {
                        Mode::Edit => format!("○ {}: 書き換え {} 回（{} ms）", r.name, r.writes, r.elapsed.as_millis()),
                        Mode::Read => format!("○ {}: 読み取り（{} ms）", r.name, r.elapsed.as_millis()),
                    }
                } else {
                    format!("× {}", r.name)
                };
                ui.colored_label(if r.ok { GREEN } else { RED }, head);
                for l in &r.logs {
                    ui.monospace(l);
                }
                if let Some(e) = &r.error {
                    ui.colored_label(RED, e);
                    if r.mode == Mode::Edit && r.writes > 0 {
                        ui.colored_label(YELLOW, format!("途中で止まりました。止まるまでの書き換え {} 回は Ctrl+Z 1 回で戻せます", r.writes));
                    }
                }
                ui.separator();
            }
        });
    }
}

/// 確定してから使う 1 行の文字（ルール au2-rs-plugin「入力の確定と取り消し」）。
/// 打っている間は下書き（egui の一時領域）だけを書き換え、Enter かほかをクリックしてフォーカスが外れたときに `value` へ写す。
/// 入力中の Esc は下書きを捨てる（`value` は入力前のまま）。`DragValue::update_while_editing(false)` と同じ振る舞い
fn committed_text(ui: &mut egui::Ui, id_salt: impl std::hash::Hash + std::fmt::Debug, value: &mut String) -> egui::Response {
    let id = ui.make_persistent_id(id_salt);
    let key = id.with("draft");
    // 下書きは編集中だけ一時領域にある（フォーカスが外れたフレームで消す）。ほかをクリックしたときは、
    // egui がこの欄を描いた後でフォーカスを移すことがあるので、フォーカスの有無ではなく下書きの有無で見る
    let mut draft = ui.data_mut(|d| d.get_temp::<String>(key)).unwrap_or_else(|| value.clone());
    let resp = ui.add(egui::TextEdit::singleline(&mut draft).id(id));
    if resp.lost_focus() {
        ui.data_mut(|d| d.remove_temp::<String>(key));
        if !ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            *value = draft;
        }
    } else if resp.has_focus() {
        ui.data_mut(|d| d.insert_temp(key, draft));
    } else {
        ui.data_mut(|d| d.remove_temp::<String>(key));
    }
    resp
}

/// 数値欄（ルール au2-rs-plugin「入力の確定と取り消し」）。`configure` で範囲・速さを付けた `DragValue` に
/// `update_while_editing(false)` を足して置く。打っている途中の値は使わず、Enter かほかをクリックで確定する。
///
/// egui 0.36.2 の `DragValue` は `update_while_editing(false)` でも、Esc の次のフレームで打った文字を値にしてしまう
/// （フォーカスを失ったとみなす期間が 2 フレームあり、2 フレーム目には Esc が押されていないため）。
/// Esc のフレームの値を覚えておき、次のフレームで戻す（参照実装 LayerSilenceCut_H の `setting_value`）。
/// 戻したフレームは、欄を描く前と値が同じなら `changed()` を偽にする
fn drag_value<T>(
    ui: &mut egui::Ui,
    value: &mut T,
    configure: impl for<'v> FnOnce(egui::DragValue<'v>) -> egui::DragValue<'v>,
) -> egui::Response
where
    T: egui::emath::Numeric + Default + Send + Sync,
{
    let before = *value;
    let mut resp = ui.add(configure(egui::DragValue::new(&mut *value)).update_while_editing(false));
    let key = resp.id.with("escaped");
    let pass = ui.ctx().cumulative_pass_nr();
    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        ui.data_mut(|d| d.insert_temp(key, (pass, *value)));
    } else if let Some((at, kept)) = ui.data_mut(|d| d.remove_temp::<(u64, T)>(key)) {
        if pass == at + 1 {
            *value = kept;
            if *value == before {
                // `flags` は egui の doc(hidden) の公開フィールド。`changed()` を外す手段がこれしかない（0.36.2）
                resp.flags.remove(egui::response::Flags::CHANGED);
            }
        }
    }
    resp
}

/// 予行の結果: 書き換えの予定を番号付きで並べる
fn render_dry_report(ui: &mut egui::Ui, r: &runner::RunReport) {
    let stopped = r.error.as_deref().is_some_and(|e| e.starts_with(runner::DRY_STOP));
    let (color, head) = match (r.ok, stopped) {
        (true, _) => (GREEN, format!("△ 予行 {}: 書き換えの予定 {} 件（タイムラインは変わっていない）", r.name, r.plan.len())),
        (false, true) => (YELLOW, format!("△ 予行 {}: 途中まで {} 件", r.name, r.plan.len())),
        (false, false) => (RED, format!("× 予行 {}: エラーで止まった（予定 {} 件まで）", r.name, r.plan.len())),
    };
    ui.colored_label(color, head);
    for (i, line) in r.plan.iter().enumerate() {
        ui.monospace(format!("{:>4}. {line}", i + 1));
    }
    for l in &r.logs {
        ui.monospace(format!("print: {l}"));
    }
    if let Some(e) = &r.error {
        ui.colored_label(if stopped { YELLOW } else { RED }, e);
    }
    if r.plan.len() > 0 || r.ok {
        ui.weak("読み取りは書き換える前の値を返す（予行では書き換えないため）。本番では結果が変わることがある");
    }
}

/// 今の選択の要約（AI に渡す）。ボタンを押したときだけ呼ぶ（読み取りなので Undo には触れない）
fn selection_summary() -> String {
    if !runner::EDIT_HANDLE.is_ready() {
        return "（編集 API の準備ができていないので取れなかった）\n".into();
    }
    let info = runner::EDIT_HANDLE.get_edit_info();
    let r = runner::EDIT_HANDLE.call_read_section(move |sec| {
        let mut out = format!(
            "- シーン: {}x{}、{}/{} fps、カーソル レイヤー {} フレーム {}（どちらも 0 始まり）\n",
            info.width,
            info.height,
            info.fps.numer(),
            info.fps.denom(),
            info.layer,
            info.frame
        );
        let selected = sec.get_selected_objects().unwrap_or_default();
        out.push_str(&format!("- 選択中のオブジェクト: {} 個\n", selected.len()));
        for (i, h) in selected.iter().enumerate().take(30) {
            let lf = sec.get_object_layer_frame(*h);
            let effects: Vec<String> = sec
                .get_effects(*h)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|e| sec.get_effect_name(e).ok())
                .collect();
            match lf {
                Ok(lf) => out.push_str(&format!(
                    "  {}. レイヤー {} フレーム {}-{}: {}\n",
                    i + 1,
                    lf.layer,
                    lf.start,
                    lf.end,
                    effects.join(" / ")
                )),
                Err(_) => out.push_str(&format!("  {}. （読めなかった）\n", i + 1)),
            }
        }
        if selected.len() > 30 {
            out.push_str(&format!("  ほか {} 個\n", selected.len() - 30));
        }
        out
    });
    r.unwrap_or_else(|e| format!("（取れなかった: {e:?}）\n"))
}

impl eframe::App for EditScriptApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.scanned {
            self.rescan();
        }
        egui::Panel::top("top").show(ui, |ui| self.render_top(ui));
        egui::Panel::bottom("log").resizable(true).default_size(180.0).show(ui, |ui| self.render_log(ui));
        egui::Panel::left("list").resizable(true).default_size(200.0).show(ui, |ui| self.render_list(ui));
        // 中央パネルは最後に追加する
        egui::CentralPanel::default().show(ui, |ui| self.render_editor(ui));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 画面なしで 1 フレーム回す。出力の textures_delta を空にしてから捨てる
    /// （そのまま捨てると、デバッグビルドで epaint の debug_assert「Dropped TexturesDelta with N unapplied deltas」に落ちる）
    fn run_frame(ctx: &egui::Context, input: egui::RawInput, f: impl FnMut(&mut egui::Ui)) {
        let mut out = ctx.run_ui(input, f);
        out.textures_delta.clear();
    }

    fn key(k: egui::Key) -> egui::Event {
        egui::Event::Key { key: k, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }
    }

    enum Focus {
        Keep,
        Give,
        /// ほかをクリックしたときの代わり
        Take,
        /// この欄を描いた後で、ほかの欄がフォーカスを取る
        TakeAfter,
    }

    /// 画面なしで 1 フレーム描き、欄の ID を返す
    fn frame(ctx: &egui::Context, value: &mut String, events: Vec<egui::Event>, focus: Focus) -> egui::Id {
        let mut id = egui::Id::NULL;
        let input = egui::RawInput { events, ..Default::default() };
        run_frame(ctx, input, |ui| {
            let target = ui.make_persistent_id("p");
            match focus {
                Focus::Keep | Focus::TakeAfter => {}
                Focus::Give => ui.memory_mut(|m| m.request_focus(target)),
                Focus::Take => ui.memory_mut(|m| m.surrender_focus(target)),
            }
            id = committed_text(ui, "p", value).id;
            let other = ui.add(egui::TextEdit::singleline(&mut String::new()).id_salt("other"));
            if matches!(focus, Focus::TakeAfter) {
                other.request_focus();
            }
        });
        id
    }

    fn start_typing(ctx: &egui::Context, value: &mut String, typed: &str) -> egui::Id {
        frame(ctx, value, vec![], Focus::Keep);
        let id = frame(ctx, value, vec![], Focus::Give);
        assert!(ctx.memory(|m| m.has_focus(id)));
        frame(ctx, value, vec![egui::Event::Text(typed.into())], Focus::Keep);
        id
    }

    /// 打っている間は値を変えない。Enter で確定する
    #[test]
    fn committed_text_commits_on_enter() {
        let ctx = egui::Context::default();
        let mut value = String::from("abc");
        start_typing(&ctx, &mut value, "xy");
        assert_eq!(value, "abc");
        frame(&ctx, &mut value, vec![key(egui::Key::Enter)], Focus::Keep);
        assert_eq!(value, "abcxy");
    }

    /// ほかをクリックしてフォーカスが外れたときも確定する
    #[test]
    fn committed_text_commits_on_blur() {
        let ctx = egui::Context::default();
        let mut value = String::from("abc");
        start_typing(&ctx, &mut value, "xy");
        frame(&ctx, &mut value, vec![], Focus::Take);
        assert_eq!(value, "abcxy");

        // フォーカスがフレームの途中で移った（この欄はまだフォーカスを持って描かれた）ときは、次のフレームで確定する
        let mut value = String::from("abc");
        let ctx = egui::Context::default();
        start_typing(&ctx, &mut value, "z");
        frame(&ctx, &mut value, vec![], Focus::TakeAfter);
        assert_eq!(value, "abc");
        frame(&ctx, &mut value, vec![], Focus::Keep);
        assert_eq!(value, "abcz");
    }

    /// 入力中の Esc は取り消し。値は入力前のまま、次に入るときも元の値から始まる
    #[test]
    fn committed_text_discards_on_escape() {
        let ctx = egui::Context::default();
        let mut value = String::from("abc");
        let id = start_typing(&ctx, &mut value, "xy");
        frame(&ctx, &mut value, vec![key(egui::Key::Escape)], Focus::Keep);
        assert_eq!(value, "abc");
        assert!(!ctx.memory(|m| m.has_focus(id)));
        start_typing(&ctx, &mut value, "z");
        frame(&ctx, &mut value, vec![key(egui::Key::Enter)], Focus::Keep);
        assert_eq!(value, "abcz");
    }

    /// 欄の外で値が変わったら（スクリプトを選び直した等）、編集していない欄はその値を出す
    #[test]
    fn committed_text_follows_value_when_idle() {
        let ctx = egui::Context::default();
        let mut value = String::from("abc");
        frame(&ctx, &mut value, vec![], Focus::Keep);
        value = "def".into();
        start_typing(&ctx, &mut value, "!");
        frame(&ctx, &mut value, vec![key(egui::Key::Enter)], Focus::Keep);
        assert_eq!(value, "def!");
    }

    // ---- 数値欄（param の整数・小数） ----

    /// 画面なしで数値欄を 1 フレーム描き、`changed()` を返す。`focus` なら描く前にフォーカスを移す（クリックと同じく全選択で編集に入る）
    fn frame_num<T>(ctx: &egui::Context, value: &mut T, events: Vec<egui::Event>, focus: bool) -> bool
    where
        T: egui::emath::Numeric + Default + Send + Sync,
    {
        let mut changed = false;
        let input = egui::RawInput { events, ..Default::default() };
        run_frame(ctx, input, |ui| {
            if focus {
                let id = ui.data(|d| d.get_temp::<egui::Id>(egui::Id::new("num_id")));
                if let Some(id) = id {
                    ui.memory_mut(|m| m.request_focus(id));
                }
            }
            let resp = drag_value(ui, value, |d| d.speed(0.2).range(T::from_f64(-1000.0)..=T::from_f64(1000.0)));
            ui.data_mut(|d| d.insert_temp(egui::Id::new("num_id"), resp.id));
            changed = resp.changed();
        });
        changed
    }

    /// 打つ（まだ確定しない）
    fn type_num<T>(ctx: &egui::Context, value: &mut T, typed: &str)
    where
        T: egui::emath::Numeric + Default + Send + Sync,
    {
        frame_num(ctx, value, vec![], false);
        frame_num(ctx, value, vec![], true);
        frame_num(ctx, value, vec![egui::Event::Text(typed.into())], false);
    }

    /// 打つ → Esc → その後 3 フレーム、値は入力前のまま。Enter で確定した値は残る
    fn check_escape_and_enter<T>(start: T, typed: &str, expected: T)
    where
        T: egui::emath::Numeric + Default + Send + Sync + std::fmt::Debug,
    {
        let ctx = egui::Context::default();
        let mut v = start;
        type_num(&ctx, &mut v, typed);
        assert_eq!(v, start, "打っている途中の値を使っている");
        assert!(!frame_num(&ctx, &mut v, vec![key(egui::Key::Escape)], false));
        for _ in 0..3 {
            let changed = frame_num(&ctx, &mut v, vec![], false);
            assert_eq!(v, start, "Esc の後のフレームでも入力前の値のまま");
            assert!(!changed);
        }

        type_num(&ctx, &mut v, typed);
        assert!(frame_num(&ctx, &mut v, vec![key(egui::Key::Enter)], false));
        for _ in 0..3 {
            frame_num(&ctx, &mut v, vec![], false);
        }
        assert_eq!(v, expected, "Enter で確定した値は残る");
    }

    #[test]
    fn int_param_escape_discards_and_enter_commits() {
        check_escape_and_enter::<i64>(3, "42", 42);
    }

    #[test]
    fn float_param_escape_discards_and_enter_commits() {
        check_escape_and_enter::<f64>(0.5, "2.25", 2.25);
    }
}
