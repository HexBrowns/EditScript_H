# 編集スクリプト（EditScript_H）の書き方

AviUtl2 のプラグイン EditScript_H で動く Lua スクリプトの説明書。AI にスクリプトを書いてもらうときは、この文書を丸ごと渡す。

## 前提

- タイムラインを編集するスクリプトを書く。**描画用の Lua（`.anm2` など）とは別物で、`obj.*` は使えない**
- 1 回の実行で行った書き換えは、まとめて **Ctrl+Z 1 回** で元に戻る（シーンの設定の変更だけは戻らない）
- 処理系は LuaJIT（Lua 5.1 相当）。使えるのは `string` `table` `math` `bit` と `print` だけ。
  `os` `io` `require` `load` `ffi` `utf8` は**無い**。ファイルの読み書きはできない
- 5 秒を超えると止まる
- パネルの **予行** は、書き換えずに「何をどう書き換えるか」の一覧だけを出す（下の「予行」）
- `print(...)` の出力はパネルのログに出る
- 失敗は Lua のエラーになる（`error` で止まる）。止まるまでに行った書き換えは残るが、Ctrl+Z 1 回で戻る。
  失敗しそうな呼び出しを続けたいときは `pcall` で包む

## 番号の数え方（重要）

- **レイヤー・フレームは 0 始まり。** 画面の「レイヤー 1」は `0`。フレームの 0 はシーンの先頭
- 中間点の番号、同名エフェクトの番号（後述の `index`）も 0 始まり
- `edit.selected()` などが返す**配列は Lua の慣習どおり 1 始まり**

## ファイルの形

`Plugin/EditScript_H/scripts/` に UTF-8 の `.lua` として置く。先頭のコメントが見出し:

```lua
--@name: 選択を等間隔にずらす        -- 一覧とメニューに出る名前
--@menu: object                     -- 右クリックメニューに出す場所: object / layer / edit / none（既定 none）
--@mode: edit                       -- edit（書き換える。既定）/ read（読むだけ。書き換えの関数はエラー）
--@param: step, int, 3, 0, 60       -- 実行前にパネルで変えられる値: 名前, 型, 初期値[, 最小, 最大]
--@scene: false                     -- true のときだけシーンの設定を変えられる（Undo できない）

print(param.step .. " フレームずつ")
```

- `--@param:` の型は `int` / `float` / `string` / `bool`。値は Lua から `param.名前` で読む（`param.step`）。名前は英数字と `_`
- `string` の初期値は残り全部（`,` を含んでよい）
- **メニュー（`--@menu:`）に出すのは起動時だけ。** 新しく足したら AviUtl2 の再起動が要る。本文の変更は再起動なしで効く
- `例_` で始まるファイルは同梱の例で、プラグインの版上げで上書きされる。直すなら別名で保存する

## 設定値の名前

`o:get(効果名, 項目名)` の効果名・項目名は、**オブジェクトの設定画面に出ている表示名そのまま**（例 `"標準描画"` の `"X"`、`"テキスト"` の `"テキスト"`、`"単色化"` の `"強さ"`）。
スクリプトの効果は多くが `"効果名@スクリプト名"`（例 `"色調補正@Basic_S"`）だが、そうでないものもある（`"AutoClipping_S"`）。
確かめるには `o:alias()` を `print` する。オブジェクトの全設定が出て、`effect.name=` の値が効果名、その下の `項目名=値` が項目名と今の値。

同じ名前のエフェクトが 2 つあるときは、引数 `index` で何番目か（0 始まり）を指定する。オブジェクト内の通し番号ではない。

値は文字列で渡す（数値を渡すと文字列に直す。`true` / `false` は `"1"` / `"0"`）。

### トラックバーの値

- 動かないとき: `"100"` のように数値 1 つ
- 動くとき: `"開始値,終了値,移動方法,設定"`（例 `"-960,960,直線移動,0"`）。中間点があると数値が増える（`"0,50,100,直線移動,0"`）
  - **数値の数は、オブジェクトの点（開始・中間点・終了）の数に合わせる**（`#o:sections()` + 1）。中間点無視（設定のビット 4）と再生範囲だけは 2 つでよい
  - 設定は省略できない（無ければ `0`）。設定の中にカンマが入ることがある（`"0,100,プローブ移動_H,0|9,0,1"`）
- 移動方法は、本体が起動時に読んだもの（`Script` フォルダの `.tra2` と組み込み）だけ使える。起動後に置いた `.tra2` は再起動まで使えない
- 登録されていない名前・設定の無い値・数値の数が点と合わない値は、プラグインが手前で止める（本体が編集を途中で打ち切るか、動きが黙って変わるため）
- **動いているトラックに数値 1 つを書くと、移動なしになる。** 移動方法と中間点の値は消える（中間点そのものは残る）

## edit テーブル

### 探す・読む

| 関数 | 戻り値 |
|---|---|
| `edit.info()` | 表: `width` `height` `fps` `rate` `scale` `sample_rate` `frame`（カーソル）`layer`（選択レイヤー）`frame_max` `layer_max` `display_frame_start` `display_layer_start` `select_start` `select_end`（範囲選択が無ければ nil）`scene_id` |
| `edit.selected()` | 選択中のオブジェクトの配列 |
| `edit.focused()` | 設定画面に出ているオブジェクト（無ければ nil） |
| `edit.objects(layer)` | そのレイヤーのオブジェクトを時間順に並べた配列 |
| `edit.find(layer, frame)` | そのフレーム以降で最初のオブジェクト（無ければ nil） |
| `edit.layer_name(layer)` / `edit.layer_enable(layer)` / `edit.layer_lock(layer)` | レイヤーの名前（無ければ nil）・表示・ロック |
| `edit.marks()` | マーカーの配列（`{frame=, memo=}`） |
| `edit.bpm()` | BPM グリッドの配列（`{tempo=, beat=, start=秒, offset=秒}`） |
| `edit.scene_name()` | シーン名 |
| `edit.media_info(path)` | 動画・画像・音声ファイルの情報（`width` `height` `duration` 秒 `video_tracks` `audio_tracks`） |
| `edit.palette_name()` / `edit.palette([名前])` | パレット（色の配列 `{r=,g=,b=,a=}`、0〜255） |
| `edit.effect_names()` | 使えるエフェクト名の一覧 |
| `edit.writes()` | ここまでの書き換えの回数 |
| `edit.is_dry_run()` | 予行なら true（`edit.info().dry_run` も同じ） |

### 作る（戻り値は新しいオブジェクト）

| 関数 | 内容 |
|---|---|
| `edit.create(効果名, layer, frame [, length])` | `"テキスト"` `"図形"` などのオブジェクトを作る。length を省くと既定の長さ |
| `edit.create_alias(エイリアス文字列, layer, frame, length)` | `o:alias()` の文字列（`.object` ファイルの中身）から作る。複製に使える |
| `edit.create_media(path, layer, frame [, length])` | 動画・画像・音声ファイルから作る |

置く場所に別のオブジェクトがあると失敗する。空いているかは `edit.find` で確かめる。

### レイヤー・画面・マーカー

| 関数 | 内容 |
|---|---|
| `edit.set_layer_name(layer, 名前 or nil)` / `edit.set_layer_enable(layer, bool)` / `edit.set_layer_lock(layer, bool)` | レイヤーの設定 |
| `edit.set_cursor(layer, frame)` | カーソルを動かす |
| `edit.set_display(layer, frame)` | タイムラインの表示位置を動かす |
| `edit.set_select_range(start, end)` / `edit.clear_select_range()` | フレームの範囲選択 |
| `edit.set_mark(frame [, memo])` / `edit.clear_mark(frame)` | マーカー |
| `edit.set_bpm({ {tempo=120, beat=4, start=0, offset=0}, ... })` | BPM グリッドを置き換える |

### シーン（Undo できない。見出しに `--@scene: true` が要る）

`edit.set_scene_name(名前)` / `edit.set_scene_size(w, h)` / `edit.set_scene_fps(rate [, scale])` / `edit.set_scene_sample_rate(r)`

## オブジェクト（`edit.selected()` などが返すもの）

`o:メソッド(...)` の形で呼ぶ。`o1 == o2` で同じオブジェクトか比べられる。`tostring(o)` で位置が分かる。

| メソッド | 内容 |
|---|---|
| `o:range()` | `layer, start, end` の 3 つを返す（end はそのオブジェクトの最後のフレーム） |
| `o:exists()` / `o:id()` | まだあるか / 識別番号 |
| `o:name()` / `o:set_name(名前 or nil)` | タイムラインに出る名前 |
| `o:alias()` | 全設定のエイリアス文字列 |
| `o:get(効果, 項目 [, index])` | 設定値（文字列） |
| `o:set(効果, 項目, 値 [, index])` | 設定値を書く |
| `o:value(効果, 項目, frame [, index])` | トラックバーのそのフレームでの数値（frame はオブジェクトの先頭からの数え） |
| `o:check(効果, 項目, frame [, index])` | チェックボックスの値 |
| `o:track_info(効果, 項目 [, index])` | トラックバーの情報（`mode` 移動方法 `params` `accelerate` `decelerate` …） |
| `o:effects()` | 付いているエフェクトの配列（上から） |
| `o:effect(名前 [, index])` | 名前でエフェクトを探す（無ければ nil） |
| `o:count(名前)` | その名前のエフェクトの数 |
| `o:add_effect(名前)` | エフェクトを足して返す（末尾に付く） |
| `o:move(layer, frame)` | 動かす（frame は新しい開始フレーム） |
| `o:delete()` | 消す |
| `o:focus()` | 設定画面に出す |
| `o:sections()` | 中間点のフレームの配列 |
| `o:add_section(frame)` / `o:delete_section(i)` / `o:move_section(i, frame)` | 中間点 |
| `o:flag(名前)` / `o:set_flag(名前, bool)` | `"group"`（グループ制御の対象）`"camera"`（カメラ制御の対象）`"clipping"` `"clipping_upper"`（上のオブジェクトでクリッピング） |

## エフェクト（`o:effects()` などが返すもの）

| メソッド | 内容 |
|---|---|
| `e:name()` / `e:id()` / `e:object()` | 名前 / 識別番号 / 付いているオブジェクト |
| `e:enable()` / `e:set_enable(bool)` | 有効か |
| `e:lock()` / `e:set_lock(bool)` | ロック |
| `e:get(項目)` / `e:set(項目, 値)` | 設定値 |
| `e:value(項目, frame)` / `e:check(項目, frame)` / `e:track_info(項目)` | オブジェクトと同じ |
| `e:delete()` | 外す |
| `e:move(index)` | 並び順を変える（0 始まり） |

## 例

選択したオブジェクトを、開始位置の順に 3 フレームずつずらして並べる:

```lua
--@name: 選択を等間隔にずらす
--@menu: object
--@param: step, int, 3, 0, 600

local objs = edit.selected()
table.sort(objs, function(a, b)
  local _, sa = a:range()
  local _, sb = b:range()
  return sa < sb
end)
local _, first = objs[1]:range()
for i, o in ipairs(objs) do
  local layer = o:range()
  o:move(layer, first + (i - 1) * param.step)
end
print(#objs .. " 個を並べた")
```

選択したテキストの X を 100 ずつずらす:

```lua
for i, o in ipairs(edit.selected()) do
  local x = tonumber(o:get("標準描画", "X")) or 0
  o:set("標準描画", "X", x + 100)
end
```

## 予行

パネルの **予行** を押すと、書き換えの関数は実行されず、記録だけが残る（Undo にも触れない）。

- `o:set` は「今の値 → 新しい値」を記録する
- `edit.create` や `o:add_effect` は「作る予定のもの」を返す。それへの `set` などの書き換えも記録される
- **作る予定のものを読む（`o:range()` `o:get()` など）と、予行はそこで止まる**（本体にまだ無いので読めない）。それまでの記録は出る
- 読み取りは書き換える前の値を返す。書いた値を読み返して分岐するスクリプトは、予行と本番で動きが変わる
- 予行のときだけ動きを変えたいときは `edit.is_dry_run()` で分ける

## 書くときの注意

- 消す・動かす前に、対象を配列に集めてから処理する（処理中にタイムラインが変わる）
- 動かすときは、移動先が空いていないと失敗する。並べ直しは「遠い方から」動かすと衝突しにくい
- 値を書いたら `o:get` で読み返すと、本当に入ったか確かめられる
- 大量に書き換えるときは、先に **予行** で一覧を見て確かめるとよい
