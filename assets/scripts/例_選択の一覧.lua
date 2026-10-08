--@name: 例: 選択の一覧
--@menu: object
--@mode: read

-- 選択したオブジェクトの位置とエフェクトをログに出す（書き換えない）
local info = edit.info()
print(string.format("シーン %dx%d %.3ffps", info.width, info.height, info.fps))
for i, o in ipairs(edit.selected()) do
  local layer, s, e = o:range()
  local names = {}
  for _, ef in ipairs(o:effects()) do
    names[#names + 1] = ef:name()
  end
  print(string.format("%d. レイヤー %d フレーム %d-%d: %s", i, layer, s, e, table.concat(names, " / ")))
end
