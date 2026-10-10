//! 本体に登録されている移動方法の名前を集める（トラックバーの値の検査に使う）。
//!
//! **`aviutl2.ini` の `[Movement.*]` は使わない。** 消したスクリプトの名前も残り続けるので、それを通すと本体が
//! 例外 `not found movement` を出し、編集のコールバックが途中で打ち切られる（ルール `au2-rs-plugin`「設定項目の値の形式」）。
//! 作り方は MidpointTable_H の `movement.rs` と同じ。本体が読む場所の `.tra2` から名前を作る:
//! - `Script/` と、その一つ下のフォルダの `.tra2`
//!   - `@名前.tra2`（マルチセクション）: 行頭の `@セクション` ごとに `セクション@名前`
//!   - それ以外: ファイル名から `.tra2` を除いたもの
//! - 本体のフォルダの `script.tra2`（同梱）: セクション名そのもの
//! - 本体に組み込みのもの（`BUILTIN`）
//!
//! 本体は `.tra2` を起動時に読むので、プラグインが読み込まれた時刻より新しいファイルは外す。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// 本体に組み込みの移動方法。「移動無し」は書くと移動なし（値 1 つ）になる
pub const BUILTIN: &[&str] = &[
    "直線移動",
    "直線移動(時間制御)",
    "直線移動(回転)",
    "補間移動",
    "補間移動(時間制御)",
    "補間移動(回転)",
    "瞬間移動",
    "移動量指定",
    "ランダム移動",
    "再生範囲",
    "移動無し",
];

/// 値が始点と終点の 2 つで保存される移動方法（設定の中間点無視とは別に）
pub const TWO_VALUE: &[&str] = &["再生範囲"];

/// 1 ファイルから名前を作る。`bundled` は本体同梱の `script.tra2`（名前に `@ファイル名` を付けない）
pub fn names_in_file(path: &Path, text: &str, bundled: bool) -> Vec<String> {
    let file = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
    let Some(stem) = file.strip_suffix(".tra2") else { return Vec::new() };
    let sections = || text.lines().filter_map(|l| l.trim_end_matches('\r').strip_prefix('@').map(str::to_string));
    if bundled {
        sections().collect()
    } else if let Some(base) = stem.strip_prefix('@') {
        sections().map(|s| format!("{s}@{base}")).collect()
    } else {
        vec![stem.to_string()]
    }
}

fn tra2_files(script_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(script_dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Ok(sub) = std::fs::read_dir(&p) {
                out.extend(sub.flatten().map(|e| e.path()).filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "tra2")));
            }
        } else if p.extension().is_some_and(|x| x == "tra2") {
            out.push(p);
        }
    }
    out
}

/// 名前の一覧を作る。`loaded_at` より新しいファイルは外す
pub fn scan(script_dir: &Path, bundled: Option<&Path>, loaded_at: Option<SystemTime>) -> HashSet<String> {
    let mut set: HashSet<String> = BUILTIN.iter().map(|s| s.to_string()).collect();
    if let Some(b) = bundled {
        if let Ok(bytes) = std::fs::read(b) {
            set.extend(names_in_file(b, &String::from_utf8_lossy(&bytes), true));
        }
    }
    for f in tra2_files(script_dir) {
        let newer = loaded_at.is_some_and(|t| std::fs::metadata(&f).and_then(|m| m.modified()).is_ok_and(|m| m > t));
        if newer {
            continue;
        }
        let Ok(bytes) = std::fs::read(&f) else { continue };
        set.extend(names_in_file(&f, &String::from_utf8_lossy(&bytes), false));
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_from_files() {
        let multi = names_in_file(Path::new("Script/@Basic_S.tra2"), "@4次式\n--param:強さ,2\n@円形\n", false);
        assert_eq!(multi, vec!["4次式@Basic_S", "円形@Basic_S"]);
        assert_eq!(names_in_file(Path::new("Script/バウンス.tra2"), "--param:高さ,100\n", false), vec!["バウンス"]);
        assert_eq!(names_in_file(Path::new("script.tra2"), "@反復移動\n@回転\n", true), vec!["反復移動", "回転"]);
        assert!(names_in_file(Path::new("Script/x.anm2"), "@A\n", false).is_empty());
    }

    #[test]
    fn scan_dir() {
        let dir = std::env::temp_dir().join(format!("editscript_movement_scan_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Sub")).unwrap();
        std::fs::write(dir.join("@Multi.tra2"), "@A\n@B\n").unwrap();
        std::fs::write(dir.join("Sub").join("単体.tra2"), "--param:1\n").unwrap();
        let s = scan(&dir, None, None);
        assert!(s.contains("A@Multi") && s.contains("B@Multi") && s.contains("単体") && s.contains("直線移動"));
        // 読み込んだ時刻より新しいファイルは外す
        let past = SystemTime::now() - std::time::Duration::from_secs(3600);
        let s = scan(&dir, None, Some(past));
        assert_eq!(s.len(), BUILTIN.len());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
