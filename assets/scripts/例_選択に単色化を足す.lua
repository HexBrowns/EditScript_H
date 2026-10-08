--@name: 例: 選択に単色化を足す
--@menu: object
--@mode: edit
--@param: amount, float, 100, 0, 100
--@param: color, string, ffffff

-- 選択したオブジェクトの末尾に「単色化」を足して、強さと色を入れる
local n = 0
for _, o in ipairs(edit.selected()) do
  local e = o:add_effect("単色化")
  e:set("強さ", param.amount)
  e:set("色", param.color)
  n = n + 1
end
print(n .. " 個に単色化を足した")
