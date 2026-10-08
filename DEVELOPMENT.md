# EditScript_H

タイムラインを編集する Lua スクリプト（オブジェクトの作成・移動・設定値の変更・エフェクトの追加など）を、
パネルのボタンと右クリックメニューから走らせる汎用プラグイン（Rust 製 `.aux2`）。1 回の実行は Ctrl+Z 1 回で戻る。

- **バージョン:** 0.2.0（aviutl2-rs / aviutl2-eframe **0.48**。本体 **2.1.11** 以上）。LuaJIT を静的リンク（`mlua` 0.11）
- **仕様:** [`AI/specifications/20261008_EditScript_H_spec.md`](../../specifications/20261008_EditScript_H_spec.md)（第 1 段階と、第 2 段階の予行）
- **スクリプトの書き方:** [`assets/API.md`](assets/API.md)（配布物の `Plugin/EditScript_H/API.md`。パネルの「AI 向けの説明をコピー」も同じ内容）

## ビルド

```powershell
python AI/tools/au2_build.py EditScript_H               # テスト → au2 release → 本番（C:\ProgramData\aviutl2）へ配置
python AI/tools/au2_build.py EditScript_H --no-deploy   # 配置しない（本番との違いだけ出す）
```

- **LuaJIT のビルド（`msvcbuild.bat`）は今いるフォルダの `minilua` を呼ぶ。** 環境変数 `NoDefaultCurrentDirectoryInExePath` が
  立っていると見つからずに落ちる（Claude Code のシェルは立てている）。`aviutl2.toml` の `build` と `prebuild` はこれを消してから走らせる。
  手で `cargo` を叩くときは `env -u NoDefaultCurrentDirectoryInExePath cargo test`（bash）
- `--skip-test` は効かない（`prebuild` のテストが `set ...&& cargo test` で始まるため）
- `THIRD_PARTY_CRATES.md` は `python AI/tools/make_crate_notices.py AI/plugins/EditScript_H` で作る。LuaJIT は build 依存（luajit-src）から
  入るので一覧に出ない。`THIRD_PARTY_NOTICES.md` に手で書いている

## 構成

| ファイル | 中身 |
|---|---|
| `src/lib.rs` | 登録・右クリックメニュー（起動時に `scripts/` を読んで登録）・実行の記録 |
| `src/script.rs` | 見出し（`--@name:` など）の読み取りと一覧（`cargo test` の中心） |
| `src/runner.rs` | Lua の制限（標準ライブラリ・JIT 切り・5 秒で停止）と `edit` テーブル（EditSection のバインディング）。予行（書き換えを記録だけする）もここ |
| `src/gui.rs` | パネル |
| `assets/API.md` | スクリプトの書き方（AI に渡す説明書）。`include_str!` でバイナリにも入る |
| `assets/scripts/例_*.lua` | 同梱の例。版上げで上書きされる |

## 設計の要点

- 書き換えるスクリプトは、1 回の実行全体を 1 回の `call_edit_section` の中で走らせる（本体は 1 回分の編集を 1 つの Undo にまとめる）
- 走らせるのはボタンとメニューの操作のときだけ（本体は `call_edit_section` の開始時にマウス操作中の Undo を捨てる。
  `AI/host/issues/20260913_host_undo_last_operation_lost.md`）
- 区画（`ReadSection` / `EditSection`）はコールバックの間だけ有効なので、Lua には生ポインタで渡し、区画を抜ける前に Lua の状態ごと捨てる
- トラックの値は書く前に形と移動方法の登録を確かめる（ClaudeBridge の 2026-07 の記録で本体が落ちた条件。現行で同じかは未確認）
