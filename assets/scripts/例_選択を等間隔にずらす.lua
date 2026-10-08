--@name: 例: 選択を等間隔にずらす
--@menu: object
--@mode: edit
--@param: step, int, 3, 0, 600

-- 選択したオブジェクトを開始位置の順に並べ、先頭から step フレームずつずらす（レイヤーはそのまま）
local objs = edit.selected()
if #objs == 0 then
  print("オブジェクトを選んでから実行してください")
  return
end
table.sort(objs, function(a, b)
  local _, sa = a:range()
  local _, sb = b:range()
  return sa < sb
end)
local _, first = objs[1]:range()
-- 後ろから動かす（前へ詰めるとき、まだ動かしていないものとぶつかりにくい）
for i = #objs, 1, -1 do
  local o = objs[i]
  local layer = o:range()
  local ok, err = pcall(function() o:move(layer, first + (i - 1) * param.step) end)
  if not ok then
    print(tostring(o) .. " は動かせなかった: " .. tostring(err))
  end
end
print(#objs .. " 個を並べた")
