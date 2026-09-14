--!nonstrict
-- Google Antigravity provider: local language-server / Cloud Code quota
-- via `tc-probe antigravity`.

local runner = require("./runner.luau")

local M = {}
M.id = "antigravity"

function M.isEnabled()
	return runner.isEnabled("antigravity")
end

function M.refresh(done)
	if not M.isEnabled() then
		done(nil)
		return
	end
	runner.runAntigravityProbe(function(p)
		local det = {}
		if p.tier ~= "" then
			table.insert(det, { k = "Plan", v = p.tier })
		end
		p.details = det
		done(p)
	end)
end

return M
