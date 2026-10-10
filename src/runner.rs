//! スクリプトを走らせる（Lua から本体の編集 API を呼ぶ層）
//!
//! - **書き換えるスクリプトは、1 回の実行全体を 1 回の `call_edit_section` の中で走らせる。**
//!   本体はコールバック内の編集をまとめて 1 つの Undo に積む（`plugin2.h` の `EDIT_HANDLE::call_edit_section`）
//! - 走らせるのはボタンとメニューの操作のときだけ。本体は `call_edit_section` の開始時に、マウス操作中の Undo を捨てる
//!   （`AI/host/issues/20260913_host_undo_last_operation_lost.md`）
//! - **予行**は書き換えるスクリプトを `call_read_section` の中で走らせ、書き換えの関数は実行せずに記録だけする。
//!   作る予定のオブジェクト・エフェクトは「仮のもの」を返し、それへの書き換えも記録する。仮のものを読もうとしたら
//!   そこで止める（本体に無いので読めない）
//! - Lua の状態は実行ごとに作り直す。前の実行のオブジェクトを持ち越せない
//! - 標準ライブラリは string / table / math / bit と print だけ。**JIT は切る**（JIT でコンパイルされたループには
//!   命令数のフックが掛からず、止まらないスクリプトを止められない）

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;
use std::time::{Duration, Instant};

use aviutl2::generic::{
    BpmInfo, EditInfo, EditSection, EditSectionError, EditSectionResult, EffectHandle, GlobalEditHandle,
    ObjectFlagType, ObjectHandle, ReadSection, TrackInfo,
};
use aviutl2::Rational32;
use mlua::{
    AnyUserData, HookTriggers, Lua, LuaOptions, MetaMethod, StdLib, Table, UserData, UserDataMethods, Value,
    VmState,
};

use crate::script::{Header, Mode, ParamValue};

pub static EDIT_HANDLE: GlobalEditHandle = GlobalEditHandle::new();

/// ログに残す行数の上限（`print` の出し過ぎで止まらないように）
const MAX_LOG_LINES: usize = 2000;
/// 予行の記録の上限
const MAX_PLAN_LINES: usize = 5000;
/// 命令いくつごとに経過時間を見るか
const HOOK_EVERY: u32 = 1000;
/// 予行で仮のものを読んだときのエラーの目印（ログで「ここまで」と出す）
pub const DRY_STOP: &str = "予行はここまで";

pub struct RunRequest {
    pub name: String,
    pub source: String,
    pub header: Header,
    pub params: Vec<(String, ParamValue)>,
    pub timeout: Duration,
    /// 書き換えずに記録だけする
    pub dry: bool,
}

#[derive(Debug, Clone)]
pub struct RunReport {
    pub name: String,
    pub mode: Mode,
    pub dry: bool,
    pub ok: bool,
    pub logs: Vec<String>,
    pub error: Option<String>,
    /// 書き換えの呼び出しの数（作る・消す・動かす・値を変える）。予行では記録した数
    pub writes: usize,
    /// 予行で記録した書き換え
    pub plan: Vec<String>,
    pub elapsed: Duration,
}

impl RunReport {
    pub fn failed(name: &str, mode: Mode, dry: bool, error: String) -> Self {
        Self {
            name: name.to_string(),
            mode,
            dry,
            ok: false,
            logs: Vec::new(),
            error: Some(error),
            writes: 0,
            plan: Vec::new(),
            elapsed: Duration::ZERO,
        }
    }
}

/// 区画の外で用意しておくもの
struct Prepared {
    name: String,
    source: String,
    allow_scene: bool,
    params: Vec<(String, ParamValue)>,
    timeout: Duration,
    effect_names: Vec<String>,
    dry: bool,
}

struct Outcome {
    ok: bool,
    logs: Vec<String>,
    error: Option<String>,
    writes: usize,
    plan: Vec<String>,
}

pub fn run(req: RunRequest) -> RunReport {
    let started = Instant::now();
    let mode = req.header.mode;
    let dry = req.dry && mode == Mode::Edit;
    let mut report = RunReport::failed(&req.name, mode, dry, String::new());
    report.error = None;
    if !EDIT_HANDLE.is_ready() {
        report.error = Some("編集 API の準備ができていません".into());
        return report;
    }
    let effect_names = if req.source.contains("effect_names") {
        EDIT_HANDLE.get_effects().into_iter().map(|e| e.name).collect()
    } else {
        Vec::new()
    };
    let prepared = Prepared {
        name: req.name,
        source: req.source,
        allow_scene: req.header.scene,
        params: req.params,
        timeout: req.timeout,
        effect_names,
        dry,
    };

    let result = if mode == Mode::Edit && !dry {
        EDIT_HANDLE.call_edit_section(move |sec| {
            let info = sec.info.clone();
            guarded(|| execute(&**sec, Some(&*sec), info, &prepared))
        })
    } else {
        let info = EDIT_HANDLE.get_edit_info();
        EDIT_HANDLE.call_read_section(move |sec| guarded(|| execute(sec, None, info, &prepared)))
    };
    match result {
        Ok(out) => {
            report.ok = out.ok;
            report.logs = out.logs;
            report.error = out.error;
            report.writes = out.writes;
            report.plan = out.plan;
        }
        Err(e) => report.error = Some(format!("本体が編集を受け付けませんでした（出力中など）: {e:?}")),
    }
    report.elapsed = started.elapsed();
    report
}

/// 本体のコールバックの中で panic を FFI 越しに伝播させない
fn guarded(f: impl FnOnce() -> Outcome) -> Outcome {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(o) => o,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "不明な panic".into());
            tracing::error!("EditScript_H: 実行中に panic: {msg}");
            Outcome { ok: false, logs: Vec::new(), error: Some(format!("内部エラー(panic): {msg}")), writes: 0, plan: Vec::new() }
        }
    }
}

/// 本体に登録されている移動方法（`movement.rs`）。起動時の `register` で 1 回だけ作る。無ければ検査しない
static MOVEMENTS: std::sync::OnceLock<HashSet<String>> = std::sync::OnceLock::new();

pub fn init_movements(set: HashSet<String>) {
    let _ = MOVEMENTS.set(set);
}

// ---------------------------------------------------------------------------
// Lua に渡す文脈
//
// 本体の区画（ReadSection / EditSection）はコールバックの間だけ有効な借用なので、Lua の userdata には持たせられない。
// 区画の中で作った Lua の状態に生ポインタで渡し、区画を抜ける前に Lua の状態ごと捨てる。
// Lua の状態は execute() の中で作って execute() の中で捨てるので、ポインタが区画より長く生きることはない。

struct Ctx<'a> {
    read: &'a ReadSection,
    edit: Option<&'a EditSection>,
    info: EditInfo,
    writes: Cell<usize>,
    allow_scene: bool,
    movements: Option<&'a HashSet<String>>,
    effect_names: &'a [String],
    /// 予行なら記録先
    plan: Option<RefCell<Vec<String>>>,
    /// 予行で作る予定のものの通し番号
    next_virtual: Cell<u32>,
}

#[derive(Clone, Copy)]
struct CtxPtr(*const Ctx<'static>);

fn ctx(lua: &Lua) -> mlua::Result<&'static Ctx<'static>> {
    let p = lua.app_data_ref::<CtxPtr>().ok_or_else(|| mlua::Error::runtime("実行の外では使えません"))?;
    // SAFETY: CtxPtr は execute() が自分のスタック上の Ctx を指して置き、同じ関数の中で Lua ごと捨てる
    Ok(unsafe { &*p.0 })
}

/// 書き換えの先。`Dry` は予行（記録だけ）
enum Target {
    Real(&'static EditSection),
    Dry,
}

/// 書き換えの関数の入口。読むだけのスクリプトでは止める
fn writer(lua: &Lua) -> mlua::Result<Target> {
    let c = ctx(lua)?;
    let t = match (c.edit, &c.plan) {
        (Some(w), _) => Target::Real(w),
        (None, Some(_)) => Target::Dry,
        (None, None) => {
            return Err(mlua::Error::runtime("読むだけのスクリプト（--@mode: read）では書き換えられません"))
        }
    };
    c.writes.set(c.writes.get() + 1);
    Ok(t)
}

fn scene_writer(lua: &Lua) -> mlua::Result<Target> {
    if !ctx(lua)?.allow_scene {
        return Err(mlua::Error::runtime(
            "シーンの設定は Undo できないので、見出しに --@scene: true を書いたスクリプトでだけ変えられます",
        ));
    }
    writer(lua)
}

/// 予行の記録に 1 行足す
fn record(lua: &Lua, line: String) -> mlua::Result<()> {
    let c = ctx(lua)?;
    if let Some(plan) = &c.plan {
        let mut p = plan.borrow_mut();
        if p.len() < MAX_PLAN_LINES {
            p.push(line);
        } else if p.len() == MAX_PLAN_LINES {
            p.push(format!("（{MAX_PLAN_LINES} 件を超えたので、以降は省いた）"));
        }
    }
    Ok(())
}

fn next_virtual(lua: &Lua) -> mlua::Result<u32> {
    let c = ctx(lua)?;
    let n = c.next_virtual.get() + 1;
    c.next_virtual.set(n);
    Ok(n)
}

/// 書き換えを 1 つ: 本番なら `real` を呼び、予行なら `line` を記録して `dry` を返す
fn write<T>(
    lua: &Lua,
    target: Target,
    real: impl FnOnce(&EditSection) -> EditSectionResult<T>,
    line: impl FnOnce() -> String,
    dry: impl FnOnce() -> mlua::Result<T>,
) -> mlua::Result<T> {
    match target {
        Target::Real(w) => api(real(w)),
        Target::Dry => {
            record(lua, line())?;
            dry()
        }
    }
}

fn explain(e: EditSectionError) -> mlua::Error {
    let msg = match e {
        EditSectionError::ApiCallFailed => "本体が失敗を返しました（効果名・項目名・値・番号を確かめてください）".to_string(),
        EditSectionError::ObjectDoesNotExist => "オブジェクトがもうありません（消したか、別のシーンのものです）".to_string(),
        EditSectionError::EffectDoesNotExist => "エフェクトがもうありません".to_string(),
        EditSectionError::ValueOutOfRange(_) => "番号が範囲の外です（レイヤー・フレームは 0 以上）".to_string(),
        other => format!("{other}"),
    };
    mlua::Error::runtime(msg)
}

fn api<T>(r: EditSectionResult<T>) -> mlua::Result<T> {
    r.map_err(explain)
}

fn dry_stop(what: &str) -> mlua::Error {
    mlua::Error::runtime(format!(
        "{DRY_STOP}: 作る予定の{what}を読もうとした（本番でないと読めない）。ここまでの書き換えの予定を出します"
    ))
}

/// 設定値として渡す文字列。数値は余計な 0 を付けない
fn value_to_item_string(v: &Value) -> mlua::Result<String> {
    Ok(match v {
        Value::String(s) => s.to_str()?.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => format_number(*n),
        Value::Boolean(b) => if *b { "1" } else { "0" }.to_string(),
        other => {
            return Err(mlua::Error::runtime(format!("設定値には文字列か数値を渡してください（{} が来ました）", other.type_name())))
        }
    })
}

pub fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        let s = format!("{n:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// 記録に出す値（長い本文は縮め、改行は見える形にする）
pub fn short_value(s: &str) -> String {
    const MAX: usize = 40;
    let flat = s.replace("\r\n", "↵").replace('\n', "↵");
    if flat.chars().count() > MAX {
        format!("「{}…」", flat.chars().take(MAX).collect::<String>())
    } else {
        format!("「{flat}」")
    }
}

/// トラックの値として渡す文字列を確かめる。形は `値1,…,値N,移動方法,設定`（ルール `au2-rs-plugin`「設定項目の値の形式」）。
/// - 数値として読める並びの後の、最初の数値でない要素が移動方法。**残り全部が設定**（設定の中にカンマが入る。`0|9,0,1`）
/// - 設定は省略できない（無いと数値が移動方法の名前とみなされ、本体が例外 `not found movement` で編集を打ち切る）
/// - 移動方法は本体に登録されている名前だけ（`movements`。None なら検査しない）
/// - `points`（開始・中間点・終了の数）が分かれば、値の数と合うかも見る。中間点無視（設定のビット 4）と再生範囲は値 2 つでよい
pub fn check_track_value(value: &str, movements: Option<&HashSet<String>>, points: Option<usize>) -> Result<(), String> {
    let fields: Vec<&str> = value.split(',').map(str::trim).collect();
    let n_values = fields.iter().take_while(|f| f.parse::<f64>().is_ok()).count();
    if n_values == fields.len() {
        return match n_values {
            1 => Ok(()),
            _ => Err(format!(
                "トラックの値「{value}」に移動方法がありません。動かさないなら数値 1 つ、動かすなら「開始値,終了値,移動方法,設定」の形にしてください"
            )),
        };
    }
    if n_values == 0 {
        return Err(format!("トラックの値「{value}」の「{}」が数値ではありません", fields[0]));
    }
    let mode = fields[n_values];
    let setting = &fields[n_values + 1..];
    if setting.is_empty() || setting[0].is_empty() {
        return Err(format!(
            "トラックの値「{value}」に設定がありません。移動方法の後ろに設定（無ければ 0）を付けてください（無いと本体が編集を打ち切ります）"
        ));
    }
    if let Some(set) = movements {
        if !set.contains(mode) {
            return Err(format!(
                "移動方法「{mode}」は本体に登録されていません（Script フォルダの .tra2 と組み込みの名前に無い。起動後に置いた .tra2 は再起動まで使えません）"
            ));
        }
    }
    if n_values < 2 && mode != "移動無し" {
        return Err(format!("トラックの値「{value}」は、移動方法の前に開始値と終了値の 2 つ以上が要ります"));
    }
    if let Some(points) = points.filter(|_| mode != "移動無し") {
        let bits = setting[0].split('|').next().unwrap_or("").parse::<u32>().unwrap_or(0);
        let two_ok = bits & 4 != 0 || crate::movement::TWO_VALUE.contains(&mode);
        if n_values != points && !(two_ok && n_values == 2) {
            return Err(format!(
                "トラックの値「{value}」の数値は {n_values} 個ですが、このオブジェクトの点（開始・中間点・終了）は {points} 個です。数を合わせてください（合わないと本体はそのまま保存し、動きが変わります）"
            ));
        }
    }
    Ok(())
}

fn guard_track(lua: &Lua, track: EditSectionResult<TrackInfo>, object: Option<ObjectHandle>, value: &str) -> mlua::Result<()> {
    // トラックでない項目（テキストなど）は情報が取れない。そのときは検査しない。
    // 数値 1 つは移動なしにする書き方なので通す（動いているトラックでは移動と中間点の値が消える。API.md に書いた）
    if track.is_err() || !value.contains(',') && value.trim().parse::<f64>().is_ok() {
        return Ok(());
    }
    let c = ctx(lua)?;
    let points = object.and_then(|o| c.read.get_object_section_num(o).ok()).map(|n| n + 1);
    check_track_value(value, c.movements, points).map_err(mlua::Error::runtime)
}

fn track_info_table(lua: &Lua, t: TrackInfo) -> mlua::Result<Table> {
    let tbl = lua.create_table()?;
    tbl.set("mode", t.mode)?;
    tbl.set("params", t.params)?;
    tbl.set("accelerate", t.accelerate)?;
    tbl.set("decelerate", t.decelerate)?;
    tbl.set("twopoint", t.twopoint)?;
    tbl.set("timecontrol", t.timecontrol)?;
    tbl.set("group_num", t.group_num)?;
    tbl.set("group_index", t.group_index)?;
    tbl.set("group_name", t.group_name)?;
    Ok(tbl)
}

fn flag_type(name: &str) -> mlua::Result<ObjectFlagType> {
    Ok(match name {
        "group" => ObjectFlagType::EnableGroup,
        "camera" => ObjectFlagType::EnableCamera,
        "clipping" => ObjectFlagType::ClippingObject,
        "clipping_upper" => ObjectFlagType::ClippingUpperObject,
        _ => return Err(mlua::Error::runtime(format!("フラグ「{name}」は group / camera / clipping / clipping_upper のどれか"))),
    })
}

// ---------------------------------------------------------------------------
// オブジェクトとエフェクト

/// 本体のオブジェクトか、予行で作る予定の仮のもの
#[derive(Clone, Copy, PartialEq, Eq)]
enum Obj {
    Real(ObjectHandle),
    Virtual(u32),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Eff {
    Real(EffectHandle),
    Virtual(u32),
}

#[derive(Clone, Copy)]
struct LObject(Obj);

#[derive(Clone, Copy)]
struct LEffect {
    object: Obj,
    effect: Eff,
}

impl LObject {
    /// 本番の書き換えの中で使う（本番に仮のものは無い）
    fn rh(&self) -> EditSectionResult<ObjectHandle> {
        match self.0 {
            Obj::Real(h) => Ok(h),
            Obj::Virtual(_) => Err(EditSectionError::ObjectDoesNotExist),
        }
    }

    fn real(&self) -> mlua::Result<ObjectHandle> {
        match self.0 {
            Obj::Real(h) => Ok(h),
            Obj::Virtual(_) => Err(dry_stop("オブジェクト")),
        }
    }
}

impl LEffect {
    /// 本番の書き換えの中で使う（本番に仮のものは無い）
    fn rh(&self) -> EditSectionResult<EffectHandle> {
        match self.effect {
            Eff::Real(h) => Ok(h),
            Eff::Virtual(_) => Err(EditSectionError::EffectDoesNotExist),
        }
    }

    fn real(&self) -> mlua::Result<EffectHandle> {
        match self.effect {
            Eff::Real(h) => Ok(h),
            Eff::Virtual(_) => Err(dry_stop("エフェクト")),
        }
    }
}

/// 記録に出すオブジェクトの呼び名
fn describe_obj(lua: &Lua, o: Obj) -> String {
    match o {
        Obj::Virtual(n) => format!("[作る予定 #{n}]"),
        Obj::Real(h) => {
            let Ok(c) = ctx(lua) else { return "[?]".into() };
            let pos = match c.read.get_object_layer_frame(h) {
                Ok(lf) => format!("レイヤー {} フレーム {}-{}", lf.layer, lf.start, lf.end),
                Err(_) => "消えている".into(),
            };
            let kind = c
                .read
                .get_first_effect(h)
                .ok()
                .and_then(|e| c.read.get_effect_name(e).ok())
                .unwrap_or_default();
            if kind.is_empty() {
                format!("[{pos}]")
            } else {
                format!("[{pos} {kind}]")
            }
        }
    }
}

fn describe_eff(lua: &Lua, e: &LEffect) -> String {
    let obj = describe_obj(lua, e.object);
    let name = match e.effect {
        Eff::Virtual(n) => format!("足す予定のエフェクト #{n}"),
        Eff::Real(h) => ctx(lua).ok().and_then(|c| c.read.get_effect_name(h).ok()).unwrap_or_else(|| "?".into()),
    };
    format!("{obj} の {name}")
}

/// 今の値（予行の記録用。読めなければ空）
fn current_object_item(lua: &Lua, o: Obj, effect: &str, idx: usize, item: &str) -> Option<String> {
    let Obj::Real(h) = o else { return None };
    ctx(lua).ok()?.read.get_object_effect_item(h, effect, idx, item).ok()
}

fn effect_label(effect: &str, idx: usize) -> String {
    if idx == 0 {
        effect.to_string()
    } else {
        format!("{effect}:{idx}")
    }
}

impl UserData for LObject {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::Eq, |_, a, b: AnyUserData| {
            Ok(b.borrow::<LObject>().map(|b| b.0 == a.0).unwrap_or(false))
        });
        m.add_meta_method(MetaMethod::ToString, |lua, this, ()| {
            Ok(match this.0 {
                Obj::Virtual(n) => format!("Object(作る予定 #{n})"),
                Obj::Real(h) => match ctx(lua)?.read.get_object_layer_frame(h) {
                    Ok(lf) => format!("Object(layer={} {}-{})", lf.layer, lf.start, lf.end),
                    Err(_) => "Object(消えている)".to_string(),
                },
            })
        });
        m.add_method("exists", |lua, this, ()| {
            Ok(match this.0 {
                Obj::Real(h) => ctx(lua)?.read.object_exists(h),
                Obj::Virtual(_) => true,
            })
        });
        m.add_method("id", |lua, this, ()| api(ctx(lua)?.read.get_object_id(this.real()?)));
        m.add_method("range", |lua, this, ()| {
            let lf = api(ctx(lua)?.read.get_object_layer_frame(this.real()?))?;
            Ok((lf.layer, lf.start, lf.end))
        });
        m.add_method("alias", |lua, this, ()| api(ctx(lua)?.read.get_object_alias(this.real()?)));
        m.add_method("name", |lua, this, ()| api(ctx(lua)?.read.get_object_name(this.real()?)));
        m.add_method("set_name", |lua, this, name: Option<String>| {
            let t = writer(lua)?;
            let name = name.filter(|s| !s.is_empty());
            write(
                lua,
                t,
                |w| w.set_object_name(this.rh()?, name.as_deref()),
                || format!("{} の名前 → {}", describe_obj(lua, this.0), name.as_deref().map(short_value).unwrap_or("（無し）".into())),
                || Ok(()),
            )
        });
        m.add_method("get", |lua, this, (effect, item, index): (String, String, Option<usize>)| {
            api(ctx(lua)?.read.get_object_effect_item(this.real()?, &effect, index.unwrap_or(0), &item))
        });
        m.add_method("set", |lua, this, (effect, item, value, index): (String, String, Value, Option<usize>)| {
            let value = value_to_item_string(&value)?;
            let idx = index.unwrap_or(0);
            if let Obj::Real(h) = this.0 {
                let track = ctx(lua)?.read.get_object_track_info(h, &effect, idx, &item);
                guard_track(lua, track, Some(h), &value)?;
            }
            let t = writer(lua)?;
            write(
                lua,
                t,
                |w| w.set_object_effect_item(this.rh()?, &effect, idx, &item, &value),
                || {
                    let before = current_object_item(lua, this.0, &effect, idx, &item);
                    format!(
                        "{} の {}.{}: {} → {}",
                        describe_obj(lua, this.0),
                        effect_label(&effect, idx),
                        item,
                        before.as_deref().map(short_value).unwrap_or("?".into()),
                        short_value(&value)
                    )
                },
                || Ok(()),
            )
        });
        m.add_method("value", |lua, this, (effect, item, frame, index): (String, String, f64, Option<usize>)| {
            api(ctx(lua)?.read.get_object_track_value(this.real()?, &effect, index.unwrap_or(0), &item, frame))
        });
        m.add_method("check", |lua, this, (effect, item, frame, index): (String, String, usize, Option<usize>)| {
            api(ctx(lua)?.read.get_object_check_value(this.real()?, &effect, index.unwrap_or(0), &item, frame))
        });
        m.add_method("track_info", |lua, this, (effect, item, index): (String, String, Option<usize>)| {
            let t = api(ctx(lua)?.read.get_object_track_info(this.real()?, &effect, index.unwrap_or(0), &item))?;
            track_info_table(lua, t)
        });
        m.add_method("effects", |lua, this, ()| {
            let list = api(ctx(lua)?.read.get_effects(this.real()?))?;
            Ok(list.into_iter().map(|e| LEffect { object: this.0, effect: Eff::Real(e) }).collect::<Vec<_>>())
        });
        m.add_method("effect", |lua, this, (name, index): (String, Option<usize>)| {
            Ok(ctx(lua)?
                .read
                .find_effect(this.real()?, &name, index.unwrap_or(0))
                .ok()
                .map(|e| LEffect { object: this.0, effect: Eff::Real(e) }))
        });
        m.add_method("count", |lua, this, name: String| api(ctx(lua)?.read.count_object_effect(this.real()?, &name)));
        m.add_method("add_effect", |lua, this, name: String| {
            let t = writer(lua)?;
            let effect = write(
                lua,
                t,
                |w| w.create_effect(this.rh()?, &name).map(Eff::Real),
                || format!("{} に「{name}」を足す", describe_obj(lua, this.0)),
                || Ok(Eff::Virtual(next_virtual(lua)?)),
            )?;
            Ok(LEffect { object: this.0, effect })
        });
        m.add_method("move", |lua, this, (layer, frame): (usize, usize)| {
            let t = writer(lua)?;
            write(
                lua,
                t,
                |w| w.move_object(this.rh()?, layer, frame),
                || format!("{} → レイヤー {layer} フレーム {frame} へ動かす", describe_obj(lua, this.0)),
                || Ok(()),
            )
        });
        m.add_method("delete", |lua, this, ()| {
            let t = writer(lua)?;
            write(lua, t, |w| w.delete_object(this.rh()?), || format!("{} を消す", describe_obj(lua, this.0)), || Ok(()))
        });
        m.add_method("focus", |lua, this, ()| {
            let t = writer(lua)?;
            write(
                lua,
                t,
                |w| w.set_focus_object(Some(this.rh()?)),
                || format!("{} を設定画面に出す", describe_obj(lua, this.0)),
                || Ok(()),
            )
        });
        m.add_method("sections", |lua, this, ()| api(ctx(lua)?.read.get_object_section_frames(this.real()?)));
        m.add_method("add_section", |lua, this, frame: usize| {
            let t = writer(lua)?;
            write(
                lua,
                t,
                |w| w.create_object_section(this.rh()?, frame),
                || format!("{} のフレーム {frame} に中間点を足す", describe_obj(lua, this.0)),
                || Ok(()),
            )
        });
        m.add_method("delete_section", |lua, this, i: usize| {
            let t = writer(lua)?;
            write(
                lua,
                t,
                |w| w.delete_object_section(this.rh()?, i),
                || format!("{} の中間点 {i} を消す", describe_obj(lua, this.0)),
                || Ok(()),
            )
        });
        m.add_method("move_section", |lua, this, (i, frame): (usize, usize)| {
            let t = writer(lua)?;
            write(
                lua,
                t,
                |w| w.move_object_section(this.rh()?, i, frame),
                || format!("{} の中間点 {i} → フレーム {frame}", describe_obj(lua, this.0)),
                || Ok(()),
            )
        });
        m.add_method("flag", |lua, this, name: String| api(ctx(lua)?.read.get_object_flag(this.real()?, flag_type(&name)?)));
        m.add_method("set_flag", |lua, this, (name, value): (String, bool)| {
            let f = flag_type(&name)?;
            let t = writer(lua)?;
            write(
                lua,
                t,
                |w| w.set_object_flag(this.rh()?, f, value),
                || format!("{} のフラグ {name} → {value}", describe_obj(lua, this.0)),
                || Ok(()),
            )
        });
    }
}

impl UserData for LEffect {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::Eq, |_, a, b: AnyUserData| {
            Ok(b.borrow::<LEffect>().map(|b| b.effect == a.effect).unwrap_or(false))
        });
        m.add_meta_method(MetaMethod::ToString, |lua, this, ()| {
            Ok(match this.effect {
                Eff::Virtual(n) => format!("Effect(足す予定 #{n})"),
                Eff::Real(h) => match ctx(lua)?.read.get_effect_name(h) {
                    Ok(n) => format!("Effect({n})"),
                    Err(_) => "Effect(消えている)".to_string(),
                },
            })
        });
        m.add_method("object", |_, this, ()| Ok(LObject(this.object)));
        m.add_method("exists", |lua, this, ()| {
            Ok(match this.effect {
                Eff::Real(h) => ctx(lua)?.read.effect_exists(h),
                Eff::Virtual(_) => true,
            })
        });
        m.add_method("name", |lua, this, ()| api(ctx(lua)?.read.get_effect_name(this.real()?)));
        m.add_method("id", |lua, this, ()| api(ctx(lua)?.read.get_effect_id(this.real()?)));
        m.add_method("enable", |lua, this, ()| api(ctx(lua)?.read.get_effect_enable(this.real()?)));
        m.add_method("set_enable", |lua, this, b: bool| {
            let t = writer(lua)?;
            write(
                lua,
                t,
                |w| w.set_effect_enable(this.rh()?, b),
                || format!("{} を{}", describe_eff(lua, this), if b { "有効にする" } else { "無効にする" }),
                || Ok(()),
            )
        });
        m.add_method("lock", |lua, this, ()| api(ctx(lua)?.read.get_effect_lock(this.real()?)));
        m.add_method("set_lock", |lua, this, b: bool| {
            let t = writer(lua)?;
            write(
                lua,
                t,
                |w| w.set_effect_lock(this.rh()?, b),
                || format!("{} のロック → {b}", describe_eff(lua, this)),
                || Ok(()),
            )
        });
        m.add_method("get", |lua, this, item: String| api(ctx(lua)?.read.get_effect_item_value(this.real()?, &item)));
        m.add_method("set", |lua, this, (item, value): (String, Value)| {
            let value = value_to_item_string(&value)?;
            if let Eff::Real(h) = this.effect {
                let track = ctx(lua)?.read.get_effect_track_info(h, &item);
                let object = match this.object {
                    Obj::Real(o) => Some(o),
                    Obj::Virtual(_) => None,
                };
                guard_track(lua, track, object, &value)?;
            }
            let t = writer(lua)?;
            write(
                lua,
                t,
                |w| w.set_effect_item_value(this.rh()?, &item, &value),
                || {
                    let before = match this.effect {
                        Eff::Real(h) => ctx(lua).ok().and_then(|c| c.read.get_effect_item_value(h, &item).ok()),
                        Eff::Virtual(_) => None,
                    };
                    format!(
                        "{} の {item}: {} → {}",
                        describe_eff(lua, this),
                        before.as_deref().map(short_value).unwrap_or(if matches!(this.effect, Eff::Virtual(_)) { "（初期値）".into() } else { "?".into() }),
                        short_value(&value)
                    )
                },
                || Ok(()),
            )
        });
        m.add_method("value", |lua, this, (item, frame): (String, f64)| {
            api(ctx(lua)?.read.get_effect_track_value(this.real()?, &item, frame))
        });
        m.add_method("check", |lua, this, (item, frame): (String, usize)| {
            api(ctx(lua)?.read.get_effect_check_value(this.real()?, &item, frame))
        });
        m.add_method("track_info", |lua, this, item: String| {
            let t = api(ctx(lua)?.read.get_effect_track_info(this.real()?, &item))?;
            track_info_table(lua, t)
        });
        m.add_method("delete", |lua, this, ()| {
            let t = writer(lua)?;
            write(
                lua,
                t,
                |w| match this.object {
                    Obj::Real(o) => w.delete_effect(o, this.rh()?),
                    Obj::Virtual(_) => Err(EditSectionError::ObjectDoesNotExist),
                },
                || format!("{} を外す", describe_eff(lua, this)),
                || Ok(()),
            )
        });
        m.add_method("move", |lua, this, index: usize| {
            let t = writer(lua)?;
            write(
                lua,
                t,
                |w| match this.object {
                    Obj::Real(o) => w.move_effect(o, this.rh()?, index),
                    Obj::Virtual(_) => Err(EditSectionError::ObjectDoesNotExist),
                },
                || format!("{} を {index} 番目へ", describe_eff(lua, this)),
                || Ok(index),
            )
        });
    }
}

// ---------------------------------------------------------------------------
// edit テーブル

fn build_edit_table(lua: &Lua) -> mlua::Result<Table> {
    let t = lua.create_table()?;

    t.set(
        "info",
        lua.create_function(|lua, ()| {
            let i = &ctx(lua)?.info;
            let tbl = lua.create_table()?;
            tbl.set("width", i.width)?;
            tbl.set("height", i.height)?;
            tbl.set("rate", *i.fps.numer())?;
            tbl.set("scale", *i.fps.denom())?;
            tbl.set("fps", *i.fps.numer() as f64 / *i.fps.denom() as f64)?;
            tbl.set("sample_rate", i.sample_rate)?;
            tbl.set("frame", i.frame)?;
            tbl.set("layer", i.layer)?;
            tbl.set("frame_max", i.frame_max)?;
            tbl.set("layer_max", i.layer_max)?;
            tbl.set("display_frame_start", i.display_frame.start)?;
            tbl.set("display_layer_start", i.display_layer.start)?;
            if let Some(r) = &i.select_range {
                tbl.set("select_start", *r.start())?;
                tbl.set("select_end", *r.end())?;
            }
            tbl.set("scene_id", i.scene_id)?;
            tbl.set("dry_run", ctx(lua)?.plan.is_some())?;
            Ok(tbl)
        })?,
    )?;
    t.set("is_dry_run", lua.create_function(|lua, ()| Ok(ctx(lua)?.plan.is_some()))?)?;
    t.set("selected", lua.create_function(|lua, ()| {
        Ok(api(ctx(lua)?.read.get_selected_objects())?.into_iter().map(|h| LObject(Obj::Real(h))).collect::<Vec<_>>())
    })?)?;
    t.set("focused", lua.create_function(|lua, ()| {
        Ok(api(ctx(lua)?.read.get_focused_object())?.map(|h| LObject(Obj::Real(h))))
    })?)?;
    t.set("objects", lua.create_function(|lua, layer: usize| {
        Ok(ctx(lua)?.read.objects_in_layer(layer).map(|(_, h)| LObject(Obj::Real(h))).collect::<Vec<_>>())
    })?)?;
    t.set("find", lua.create_function(|lua, (layer, frame): (usize, usize)| {
        Ok(api(ctx(lua)?.read.find_object_after(layer, frame))?.map(|h| LObject(Obj::Real(h))))
    })?)?;
    t.set("create", lua.create_function(|lua, (effect, layer, frame, length): (String, usize, usize, Option<usize>)| {
        let t = writer(lua)?;
        let o = write(
            lua,
            t,
            |w| w.create_object(&effect, layer, frame, length).map(Obj::Real),
            || {
                let len = length.map(|l| format!("長さ {l}")).unwrap_or_else(|| "既定の長さ".into());
                format!("「{effect}」をレイヤー {layer} フレーム {frame} に作る（{len}）")
            },
            || Ok(Obj::Virtual(next_virtual(lua)?)),
        )?;
        Ok(LObject(o))
    })?)?;
    t.set("create_alias", lua.create_function(|lua, (alias, layer, frame, length): (String, usize, usize, usize)| {
        let t = writer(lua)?;
        let o = write(
            lua,
            t,
            |w| w.create_object_from_alias(&alias, layer, frame, length).map(Obj::Real),
            || format!("エイリアスからレイヤー {layer} フレーム {frame} に作る（長さ {length}）"),
            || Ok(Obj::Virtual(next_virtual(lua)?)),
        )?;
        Ok(LObject(o))
    })?)?;
    t.set("create_media", lua.create_function(|lua, (path, layer, frame, length): (String, usize, usize, Option<usize>)| {
        let t = writer(lua)?;
        let o = write(
            lua,
            t,
            |w| w.create_object_from_media_file(&path, layer, frame, length).map(Obj::Real),
            || format!("ファイル {} をレイヤー {layer} フレーム {frame} に置く", short_value(&path)),
            || Ok(Obj::Virtual(next_virtual(lua)?)),
        )?;
        Ok(LObject(o))
    })?)?;
    t.set("media_info", lua.create_function(|lua, path: String| {
        let m = api(ctx(lua)?.read.get_media_info(&path))?;
        let tbl = lua.create_table()?;
        tbl.set("width", m.width)?;
        tbl.set("height", m.height)?;
        tbl.set("duration", m.total_time)?;
        tbl.set("video_tracks", m.video_track_num.map_or(0, |n| n.get()))?;
        tbl.set("audio_tracks", m.audio_track_num.map_or(0, |n| n.get()))?;
        Ok(tbl)
    })?)?;

    // レイヤー
    t.set("layer_name", lua.create_function(|lua, l: usize| api(ctx(lua)?.read.get_layer_name(l)))?)?;
    t.set("set_layer_name", lua.create_function(|lua, (l, name): (usize, Option<String>)| {
        let name = name.filter(|s| !s.is_empty());
        let t = writer(lua)?;
        write(
            lua,
            t,
            |w| w.set_layer_name(l, name.as_deref()),
            || format!("レイヤー {l} の名前 → {}", name.as_deref().map(short_value).unwrap_or("（無し）".into())),
            || Ok(()),
        )
    })?)?;
    t.set("layer_enable", lua.create_function(|lua, l: usize| api(ctx(lua)?.read.get_layer_enable(l)))?)?;
    t.set("set_layer_enable", lua.create_function(|lua, (l, b): (usize, bool)| {
        let t = writer(lua)?;
        write(lua, t, |w| w.set_layer_enable(l, b), || format!("レイヤー {l} の表示 → {b}"), || Ok(()))
    })?)?;
    t.set("layer_lock", lua.create_function(|lua, l: usize| api(ctx(lua)?.read.get_layer_lock(l)))?)?;
    t.set("set_layer_lock", lua.create_function(|lua, (l, b): (usize, bool)| {
        let t = writer(lua)?;
        write(lua, t, |w| w.set_layer_lock(l, b), || format!("レイヤー {l} のロック → {b}"), || Ok(()))
    })?)?;

    // 画面
    t.set("set_cursor", lua.create_function(|lua, (l, f): (usize, usize)| {
        let t = writer(lua)?;
        write(lua, t, |w| w.set_cursor_layer_frame(l, f), || format!("カーソル → レイヤー {l} フレーム {f}"), || Ok(()))
    })?)?;
    t.set("set_display", lua.create_function(|lua, (l, f): (usize, usize)| {
        let t = writer(lua)?;
        write(lua, t, |w| w.set_display_layer_frame(l, f), || format!("表示位置 → レイヤー {l} フレーム {f}"), || Ok(()))
    })?)?;
    t.set("set_select_range", lua.create_function(|lua, (s, e): (usize, usize)| {
        let t = writer(lua)?;
        write(lua, t, |w| w.set_select_range(s, e), || format!("範囲選択 → フレーム {s}-{e}"), || Ok(()))
    })?)?;
    t.set("clear_select_range", lua.create_function(|lua, ()| {
        let t = writer(lua)?;
        write(lua, t, |w| w.clear_select_range(), || "範囲選択を外す".to_string(), || Ok(()))
    })?)?;

    // マーカーと BPM
    t.set("marks", lua.create_function(|lua, ()| {
        let c = ctx(lua)?;
        let out = lua.create_table()?;
        for (i, f) in api(c.read.get_mark_frame_list())?.into_iter().enumerate() {
            let m = lua.create_table()?;
            m.set("frame", f)?;
            m.set("memo", c.read.get_mark_frame_memo(f).unwrap_or_default())?;
            out.set(i + 1, m)?;
        }
        Ok(out)
    })?)?;
    t.set("set_mark", lua.create_function(|lua, (f, memo): (usize, Option<String>)| {
        let memo = memo.unwrap_or_default();
        let t = writer(lua)?;
        write(lua, t, |w| w.set_mark_frame(f, &memo), || format!("フレーム {f} にマーカー {}", short_value(&memo)), || Ok(()))
    })?)?;
    t.set("clear_mark", lua.create_function(|lua, f: usize| {
        let t = writer(lua)?;
        write(lua, t, |w| w.clear_mark_frame(f), || format!("フレーム {f} のマーカーを外す"), || Ok(()))
    })?)?;
    t.set("bpm", lua.create_function(|lua, ()| {
        let out = lua.create_table()?;
        for (i, b) in api(ctx(lua)?.read.get_grid_bpm_list())?.into_iter().enumerate() {
            let m = lua.create_table()?;
            m.set("tempo", b.tempo)?;
            m.set("beat", b.beat)?;
            m.set("start", b.start)?;
            m.set("offset", b.offset)?;
            out.set(i + 1, m)?;
        }
        Ok(out)
    })?)?;
    t.set("set_bpm", lua.create_function(|lua, list: Vec<Table>| {
        let mut v = Vec::with_capacity(list.len());
        for m in list {
            v.push(BpmInfo {
                tempo: m.get::<f32>("tempo")?,
                beat: m.get::<Option<i32>>("beat")?.unwrap_or(4),
                start: m.get::<Option<f64>>("start")?.unwrap_or(0.0),
                offset: m.get::<Option<f32>>("offset")?.unwrap_or(0.0),
            });
        }
        let t = writer(lua)?;
        write(
            lua,
            t,
            |w| w.set_grid_bpm_list(&v),
            || {
                let list: Vec<String> = v.iter().map(|b| format!("{}BPM {}拍子 {}秒から", format_number(b.tempo as f64), b.beat, format_number(b.start))).collect();
                format!("BPM グリッド → {}", list.join(" / "))
            },
            || Ok(()),
        )
    })?)?;

    // シーン（Undo できない。--@scene: true のときだけ書ける）
    t.set("scene_name", lua.create_function(|lua, ()| api(ctx(lua)?.read.get_scene_name()))?)?;
    t.set("set_scene_name", lua.create_function(|lua, name: String| {
        let t = scene_writer(lua)?;
        write(lua, t, |w| w.set_scene_name(&name), || format!("シーン名 → {}（Undo できない）", short_value(&name)), || Ok(()))
    })?)?;
    t.set("set_scene_size", lua.create_function(|lua, (w_, h): (usize, usize)| {
        let t = scene_writer(lua)?;
        write(lua, t, |w| w.set_scene_size(w_, h), || format!("シーンのサイズ → {w_}x{h}（Undo できない）"), || Ok(()))
    })?)?;
    t.set("set_scene_fps", lua.create_function(|lua, (rate, scale): (i32, Option<i32>)| {
        let scale = scale.unwrap_or(1);
        if rate <= 0 || scale <= 0 {
            return Err(mlua::Error::runtime("フレームレートは正の数で"));
        }
        let t = scene_writer(lua)?;
        write(
            lua,
            t,
            |w| w.set_scene_fps(Rational32::new(rate, scale)),
            || format!("シーンのフレームレート → {rate}/{scale}（Undo できない）"),
            || Ok(()),
        )
    })?)?;
    t.set("set_scene_sample_rate", lua.create_function(|lua, r: usize| {
        let t = scene_writer(lua)?;
        write(lua, t, |w| w.set_scene_sample_rate(r), || format!("シーンのサンプルレート → {r}（Undo できない）"), || Ok(()))
    })?)?;

    // 情報
    t.set("palette_name", lua.create_function(|lua, ()| api(ctx(lua)?.read.get_palette_name()))?)?;
    t.set("palette", lua.create_function(|lua, name: Option<String>| {
        let c = ctx(lua)?;
        let p = match name {
            Some(n) => api(c.read.get_palette_info(&n))?,
            None => api(c.read.get_current_palette_info())?,
        };
        let out = lua.create_table()?;
        for (i, col) in p.colors.iter().enumerate() {
            let m = lua.create_table()?;
            m.set("r", col.r)?;
            m.set("g", col.g)?;
            m.set("b", col.b)?;
            m.set("a", col.a)?;
            out.set(i + 1, m)?;
        }
        Ok(out)
    })?)?;
    t.set("effect_names", lua.create_function(|lua, ()| Ok(ctx(lua)?.effect_names.to_vec()))?)?;
    t.set("writes", lua.create_function(|lua, ()| Ok(ctx(lua)?.writes.get()))?)?;
    Ok(t)
}

// ---------------------------------------------------------------------------
// 実行

/// 制限した Lua を作る（区画が無くても作れる。テストもこれを使う）
pub fn new_sandbox(timeout: Duration, logs: Rc<RefCell<Vec<String>>>) -> mlua::Result<Lua> {
    let lua = Lua::new_with(StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::BIT | StdLib::JIT, LuaOptions::default())?;
    lua.load("jit.off() jit.flush()").set_name("=sandbox").exec()?;
    let g = lua.globals();
    for name in ["jit", "dofile", "loadfile", "load", "loadstring", "require", "module", "getfenv", "setfenv", "newproxy", "collectgarbage"] {
        g.raw_set(name, Value::Nil)?;
    }
    let string: Table = g.get("string")?;
    string.raw_set("dump", Value::Nil)?;

    let print = lua.create_function(move |_, args: mlua::Variadic<Value>| {
        let mut parts = Vec::with_capacity(args.len());
        for v in args.iter() {
            parts.push(match v {
                Value::Nil => "nil".to_string(),
                Value::Number(n) => format_number(*n),
                other => other.to_string().unwrap_or_else(|_| format!("<{}>", other.type_name())),
            });
        }
        let mut l = logs.borrow_mut();
        if l.len() < MAX_LOG_LINES {
            l.push(parts.join("\t"));
        } else if l.len() == MAX_LOG_LINES {
            l.push(format!("（{MAX_LOG_LINES} 行を超えたので、以降の print は省いた）"));
        }
        Ok(())
    })?;
    g.set("print", print)?;

    let start = Instant::now();
    lua.set_hook(HookTriggers::new().every_nth_instruction(HOOK_EVERY), move |_, _| {
        if start.elapsed() > timeout {
            Err(mlua::Error::runtime(format!("{} 秒を超えたので止めました（止まらないループかもしれません）", timeout.as_secs_f32())))
        } else {
            Ok(VmState::Continue)
        }
    })?;
    Ok(lua)
}

fn param_value(lua: &Lua, v: &ParamValue) -> mlua::Result<Value> {
    Ok(match v {
        ParamValue::Int(i) => Value::Integer(*i as _),
        ParamValue::Float(f) => Value::Number(*f),
        ParamValue::Str(s) => Value::String(lua.create_string(s)?),
        ParamValue::Bool(b) => Value::Boolean(*b),
    })
}

fn execute(read: &ReadSection, edit: Option<&EditSection>, info: EditInfo, p: &Prepared) -> Outcome {
    let logs = Rc::new(RefCell::new(Vec::new()));
    let c = Ctx {
        read,
        edit,
        info,
        writes: Cell::new(0),
        allow_scene: p.allow_scene,
        movements: MOVEMENTS.get(),
        effect_names: &p.effect_names,
        plan: p.dry.then(|| RefCell::new(Vec::new())),
        next_virtual: Cell::new(0),
    };
    let result = (|| -> mlua::Result<()> {
        let lua = new_sandbox(p.timeout, Rc::clone(&logs))?;
        // SAFETY: `c` はこの関数のスタックにあり、`lua` はこのクロージャの終わりで捨てる（ポインタは外へ出ない）
        let ptr = CtxPtr(&c as *const Ctx<'_> as *const Ctx<'static>);
        lua.set_app_data(ptr);
        let g = lua.globals();
        g.set("edit", build_edit_table(&lua)?)?;
        let params = lua.create_table()?;
        for (k, v) in &p.params {
            params.set(k.as_str(), param_value(&lua, v)?)?;
        }
        g.set("param", params)?;
        let r = lua.load(&p.source).set_name(format!("={}", p.name)).exec();
        lua.remove_app_data::<CtxPtr>();
        r
    })();
    let logs = Rc::try_unwrap(logs).map(RefCell::into_inner).unwrap_or_else(|rc| rc.borrow().clone());
    let plan = c.plan.map(RefCell::into_inner).unwrap_or_default();
    let writes = c.writes.get();
    match result {
        Ok(()) => Outcome { ok: true, logs, error: None, writes, plan },
        Err(e) => Outcome { ok: false, logs, error: Some(describe_error(&e)), writes, plan },
    }
}

/// mlua のエラーを読める形にする（呼び出し元の Rust の情報は外し、Lua のトレースバックは残す）
pub fn describe_error(e: &mlua::Error) -> String {
    match e {
        mlua::Error::CallbackError { traceback, cause } => {
            let inner = describe_error(cause);
            if traceback.is_empty() {
                inner
            } else {
                format!("{inner}\n{traceback}")
            }
        }
        mlua::Error::RuntimeError(s) | mlua::Error::SyntaxError { message: s, .. } => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> (Lua, Rc<RefCell<Vec<String>>>) {
        let logs = Rc::new(RefCell::new(Vec::new()));
        (new_sandbox(Duration::from_millis(500), Rc::clone(&logs)).unwrap(), logs)
    }

    #[test]
    fn dangerous_globals_are_gone() {
        let (lua, _) = sandbox();
        for expr in ["os", "io", "require", "load", "loadstring", "dofile", "loadfile", "jit", "ffi", "package", "debug", "string.dump"] {
            let v: Value = lua.load(format!("return {expr}")).eval().unwrap();
            assert!(v.is_nil(), "{expr} が残っている");
        }
        let n: i64 = lua.load("return bit.band(6, 3) + math.floor(2.5) + #string.rep('a', 2) + #table.concat({'x'})").eval().unwrap();
        assert_eq!(n, 2 + 2 + 2 + 1);
    }

    #[test]
    fn infinite_loop_stops() {
        let (lua, _) = sandbox();
        let t = Instant::now();
        let err = lua.load("while true do end").exec().unwrap_err();
        assert!(describe_error(&err).contains("止めました"), "{err}");
        assert!(t.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn print_collects() {
        let (lua, logs) = sandbox();
        lua.load("print('a', 1, 2.5, nil, true)").exec().unwrap();
        assert_eq!(logs.borrow().as_slice(), ["a\t1\t2.5\tnil\ttrue"]);
    }

    #[test]
    fn edit_outside_section_errors() {
        let (lua, _) = sandbox();
        lua.globals().set("edit", build_edit_table(&lua).unwrap()).unwrap();
        let err = lua.load("edit.selected()").exec().unwrap_err();
        assert!(describe_error(&err).contains("実行の外"), "{err}");
    }

    #[test]
    fn syntax_error_has_line() {
        let (lua, _) = sandbox();
        let err = lua.load("local x =\nprint(").set_name("=テスト").exec().unwrap_err();
        assert!(describe_error(&err).contains("テスト:2"), "{err}");
    }

    #[test]
    fn number_format() {
        assert_eq!(format_number(3.0), "3");
        assert_eq!(format_number(-12.5), "-12.5");
        assert_eq!(format_number(0.1 + 0.2), "0.3");
    }

    #[test]
    fn short_values() {
        assert_eq!(short_value("abc"), "「abc」");
        assert_eq!(short_value("一\n二"), "「一↵二」");
        let long = "あ".repeat(50);
        assert_eq!(short_value(&long), format!("「{}…」", "あ".repeat(40)));
    }

    /// 予行で仮のものを読んだときのエラーに目印が入る（パネルはこれで「ここまで」と出す）
    #[test]
    fn dry_stop_marker() {
        let lo = LObject(Obj::Virtual(1));
        let e = lo.real().unwrap_err();
        assert!(describe_error(&e).starts_with(DRY_STOP), "{e}");
        let le = LEffect { object: Obj::Virtual(1), effect: Eff::Virtual(2) };
        assert!(describe_error(&le.real().unwrap_err()).starts_with(DRY_STOP));
    }

    #[test]
    fn track_values() {
        let reg: HashSet<String> =
            ["直線移動", "プローブ移動_H", "再生範囲"].into_iter().map(str::to_string).collect();
        let ok = |v: &str, p: Option<usize>| check_track_value(v, Some(&reg), p).is_ok();
        assert!(ok("100", None));
        assert!(ok("-960.00,960.00,直線移動,4", None));
        assert!(ok("0,50,100,直線移動,0", None));
        assert!(!ok("0,100", None));
        assert!(!ok("0,100,直線移動", None));
        assert!(!ok("0,100,未登録の移動,0", None));
        assert!(check_track_value("0,100,未登録の移動,0", None, None).is_ok());
        assert!(check_track_value("abc", None, None).is_err());
        // 設定の中のカンマ（後ろから数えると移動方法を取り違える）
        assert!(ok("0,100,プローブ移動_H,0|9,0,1", Some(2)));
        // 点の数と値の数
        assert!(ok("0,50,100,直線移動,0", Some(3)));
        assert!(!ok("0,100,直線移動,0", Some(3)));
        assert!(!ok("0,50,100,直線移動,0", Some(2)));
        // 中間点無視（ビット 4）と再生範囲は値 2 つでよい
        assert!(ok("0,100,直線移動,4", Some(4)));
        assert!(ok("0,100,直線移動,5|", Some(4)));
        assert!(ok("0,100,再生範囲,0", Some(3)));
    }

    /// 同梱の例と、API.md のコードブロックが構文として通り、見出しも読める
    #[test]
    fn bundled_scripts_compile() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets").join("scripts");
        let mut n = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let src = crate::script::read_source(&path).unwrap();
            let h = crate::script::parse_header(&src);
            assert!(h.errors.is_empty(), "{}: {:?}", path.display(), h.errors);
            let (lua, _) = sandbox();
            lua.load(&src).into_function().unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            n += 1;
        }
        assert!(n >= 3, "例が {n} 本しか無い");

        let doc = crate::API_DOC;
        let mut blocks = 0;
        for part in doc.split("```lua\n").skip(1) {
            let code = part.split("```").next().unwrap();
            let (lua, _) = sandbox();
            lua.load(code).into_function().unwrap_or_else(|e| panic!("API.md のコード: {e}\n{code}"));
            blocks += 1;
        }
        assert!(blocks >= 3, "API.md のコードブロックが {blocks} 個しか無い");
    }
}
