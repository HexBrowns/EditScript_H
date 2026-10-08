//! スクリプトの置き場と見出し（先頭の `--@キー: 値`）
//!
//! 見出しはファイルの先頭から続くコメント行だけを読む。空行と `--@` で始まらないコメントは読み飛ばし、
//! コメントでない行が来たら終わる。

use std::path::{Path, PathBuf};

/// 右クリックメニューに出す場所
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MenuPlace {
    None,
    Object,
    Layer,
    Edit,
}

/// 書き換えるか、読むだけか
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Edit,
    Read,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    Int,
    Float,
    Str,
    Bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParamValue {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParamDef {
    pub name: String,
    pub kind: ParamKind,
    pub default: ParamValue,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

impl ParamDef {
    /// 範囲に収めた値
    pub fn clamp(&self, v: &ParamValue) -> ParamValue {
        let lo = self.min.unwrap_or(f64::NEG_INFINITY);
        let hi = self.max.unwrap_or(f64::INFINITY);
        match v {
            ParamValue::Int(i) => ParamValue::Int((*i as f64).clamp(lo, hi) as i64),
            ParamValue::Float(f) => ParamValue::Float(f.clamp(lo, hi)),
            other => other.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub name: Option<String>,
    pub menu: MenuPlace,
    pub mode: Mode,
    /// シーンの設定を変えてよい（Undo できない操作）
    pub scene: bool,
    pub params: Vec<ParamDef>,
    /// 読めなかった行（パネルに出す）
    pub errors: Vec<String>,
}

impl Default for Header {
    fn default() -> Self {
        Self { name: None, menu: MenuPlace::None, mode: Mode::Edit, scene: false, params: Vec::new(), errors: Vec::new() }
    }
}

/// 値の後ろに ` --` で付けた説明を外す
fn strip_comment(s: &str) -> &str {
    match s.find(" --") {
        Some(i) => s[..i].trim(),
        None => s.trim(),
    }
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// `名前, 型, 初期値[, 最小, 最大]`。string の初期値は残り全部（`,` を含んでよい）
fn parse_param(value: &str) -> Result<ParamDef, String> {
    let mut parts = value.splitn(3, ',');
    let name = parts.next().unwrap_or("").trim().to_string();
    let kind_s = parts.next().unwrap_or("").trim().to_ascii_lowercase();
    let rest = parts.next().unwrap_or("");
    if !is_identifier(&name) {
        return Err(format!("名前「{name}」は英数字と _ にする（Lua の変数名として使う）"));
    }
    let kind = match kind_s.as_str() {
        "int" | "integer" => ParamKind::Int,
        "float" | "number" => ParamKind::Float,
        "string" | "str" => ParamKind::Str,
        "bool" | "boolean" => ParamKind::Bool,
        _ => return Err(format!("型「{kind_s}」は int / float / string / bool のどれか")),
    };
    if kind == ParamKind::Str {
        return Ok(ParamDef { name, kind, default: ParamValue::Str(rest.trim().to_string()), min: None, max: None });
    }
    let fields: Vec<&str> = strip_comment(rest).split(',').map(str::trim).collect();
    let default_s = fields.first().copied().unwrap_or("");
    let num = |s: &str, what: &str| -> Result<Option<f64>, String> {
        if s.is_empty() {
            return Ok(None);
        }
        s.parse::<f64>().map(Some).map_err(|_| format!("{what}「{s}」が数値でない"))
    };
    let min = num(fields.get(1).copied().unwrap_or(""), "最小")?;
    let max = num(fields.get(2).copied().unwrap_or(""), "最大")?;
    let default = match kind {
        ParamKind::Int => {
            let v = if default_s.is_empty() { 0 } else { default_s.parse::<i64>().map_err(|_| format!("初期値「{default_s}」が整数でない"))? };
            ParamValue::Int(v)
        }
        ParamKind::Float => ParamValue::Float(num(default_s, "初期値")?.unwrap_or(0.0)),
        ParamKind::Bool => {
            let v = if default_s.is_empty() { false } else { parse_bool(default_s).ok_or_else(|| format!("初期値「{default_s}」が true / false でない"))? };
            ParamValue::Bool(v)
        }
        ParamKind::Str => unreachable!(),
    };
    let def = ParamDef { name, kind, default, min, max };
    let clamped = def.clamp(&def.default);
    Ok(ParamDef { default: clamped, ..def })
}

pub fn parse_header(src: &str) -> Header {
    let mut h = Header::default();
    let src = src.strip_prefix('\u{feff}').unwrap_or(src);
    for (i, raw) in src.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if !line.starts_with("--") {
            break;
        }
        let Some(body) = line.strip_prefix("--@") else { continue };
        let Some((key, value)) = body.split_once(':') else {
            h.errors.push(format!("{} 行目: `--@キー: 値` の形にする", i + 1));
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        match key.as_str() {
            "name" => {
                let v = strip_comment(value);
                if !v.is_empty() {
                    h.name = Some(v.to_string());
                }
            }
            "menu" => match strip_comment(value).to_ascii_lowercase().as_str() {
                "object" => h.menu = MenuPlace::Object,
                "layer" => h.menu = MenuPlace::Layer,
                "edit" => h.menu = MenuPlace::Edit,
                "none" | "" => h.menu = MenuPlace::None,
                v => h.errors.push(format!("{} 行目: menu「{v}」は object / layer / edit / none のどれか", i + 1)),
            },
            "mode" => match strip_comment(value).to_ascii_lowercase().as_str() {
                "edit" => h.mode = Mode::Edit,
                "read" => h.mode = Mode::Read,
                v => h.errors.push(format!("{} 行目: mode「{v}」は edit / read のどちらか", i + 1)),
            },
            "scene" => match parse_bool(strip_comment(value)) {
                Some(b) => h.scene = b,
                None => h.errors.push(format!("{} 行目: scene は true / false", i + 1)),
            },
            "param" => match parse_param(value) {
                Ok(p) => {
                    if h.params.iter().any(|q| q.name == p.name) {
                        h.errors.push(format!("{} 行目: param「{}」が重なっている", i + 1, p.name));
                    } else {
                        h.params.push(p);
                    }
                }
                Err(e) => h.errors.push(format!("{} 行目: {e}", i + 1)),
            },
            other => h.errors.push(format!("{} 行目: 知らないキー「{other}」", i + 1)),
        }
    }
    h
}

#[derive(Debug, Clone)]
pub struct ScriptFile {
    pub path: PathBuf,
    pub stem: String,
    pub header: Header,
}

impl ScriptFile {
    pub fn display_name(&self) -> &str {
        self.header.name.as_deref().unwrap_or(&self.stem)
    }

    pub fn load(path: &Path) -> std::io::Result<(Self, String)> {
        let src = read_source(path)?;
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        Ok((Self { path: path.to_path_buf(), stem, header: parse_header(&src) }, src))
    }
}

/// UTF-8 として読む。BOM は外す
pub fn read_source(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    let s = String::from_utf8(bytes).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "UTF-8 で保存されていない"))?;
    Ok(s.strip_prefix('\u{feff}').map(str::to_string).unwrap_or(s))
}

/// `dir` 直下の `*.lua` を名前順に
pub fn scan(dir: &Path) -> Vec<ScriptFile> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<ScriptFile> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("lua")))
        .filter_map(|p| match ScriptFile::load(&p) {
            Ok((f, _)) => Some(f),
            Err(e) => {
                tracing::warn!("スクリプトを読めませんでした: {}: {e}", p.display());
                None
            }
        })
        .collect();
    out.sort_by(|a, b| a.display_name().cmp(b.display_name()));
    out
}

/// 新しいスクリプトのひな形
pub const TEMPLATE: &str = "--@name: 新しいスクリプト
--@menu: none
--@mode: edit

-- edit の使い方は Plugin/EditScript_H/API.md（パネルの「AI 向けの説明をコピー」と同じ内容）
local objs = edit.selected()
print(#objs .. \" 個を選択中\")
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_basic() {
        let h = parse_header(
            "--@name: 選択を3フレームずつずらす   -- 説明\n--@menu: object   -- 右クリック\n--@mode: edit\n--@param: step, int, 3, 1, 60\n\n-- 普通のコメント\nlocal x = 1\n--@name: 本文の中は読まない\n",
        );
        assert_eq!(h.name.as_deref(), Some("選択を3フレームずつずらす"));
        assert_eq!(h.menu, MenuPlace::Object);
        assert_eq!(h.mode, Mode::Edit);
        assert_eq!(h.params.len(), 1);
        let p = &h.params[0];
        assert_eq!(p.name, "step");
        assert_eq!(p.kind, ParamKind::Int);
        assert_eq!(p.default, ParamValue::Int(3));
        assert_eq!((p.min, p.max), (Some(1.0), Some(60.0)));
        assert!(h.errors.is_empty(), "{:?}", h.errors);
    }

    #[test]
    fn header_defaults_and_bom() {
        let h = parse_header("\u{feff}local x = 1\n");
        assert_eq!(h, Header::default());
    }

    #[test]
    fn param_kinds() {
        let h = parse_header(
            "--@param: label, string, こんにちは, 世界\n--@param: amount, float, 0.5, 0, 1\n--@param: on, bool, true\n--@param: big, int, 100, 0, 10\n",
        );
        assert!(h.errors.is_empty(), "{:?}", h.errors);
        assert_eq!(h.params[0].default, ParamValue::Str("こんにちは, 世界".into()));
        assert_eq!(h.params[1].default, ParamValue::Float(0.5));
        assert_eq!(h.params[2].default, ParamValue::Bool(true));
        // 初期値は範囲に収める
        assert_eq!(h.params[3].default, ParamValue::Int(10));
    }

    #[test]
    fn header_errors() {
        let h = parse_header(
            "--@menu: timeline\n--@mode: write\n--@param: 1abc, int, 3\n--@param: a, vec, 1\n--@param: b, int, x\n--@foo: 1\n--@name 区切り無し\n--@param: c, int, 1\n--@param: c, int, 2\n",
        );
        assert_eq!(h.errors.len(), 8, "{:?}", h.errors);
        assert_eq!(h.params.len(), 1);
    }

    #[test]
    fn scene_flag() {
        assert!(parse_header("--@scene: true\n").scene);
        assert!(!parse_header("--@scene: false\n").scene);
    }

    #[test]
    fn template_parses() {
        let h = parse_header(TEMPLATE);
        assert!(h.errors.is_empty());
        assert_eq!(h.name.as_deref(), Some("新しいスクリプト"));
    }
}
