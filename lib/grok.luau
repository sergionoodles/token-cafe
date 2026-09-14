--!nonstrict
-- Grok provider: Grok Build CLI billing snapshot via `tc-probe grok`
-- (`grok agent stdio` + `x.ai/billing`, with legacy `_x.ai/billing` fallback).

local runner = require("./runner.luau")

local M = {}
M.id = "grok"

function M.isEnabled()
	return runner.isEnabled("grok")
end

function M.refresh(done)
	if not M.isEnabled() then
		done(nil)
		return
	end
	runner.runCliProbe("grok", function(p)
		local det = {}
		if p.tier ~= "" then
			table.insert(det, { k = "Tier", v = p.tier })
		end
		if p.balanceValue ~= "" then
			table.insert(det, { k = p.balanceLabel ~= "" and p.balanceLabel or "Balance", v = p.balanceValue })
		end
		p.details = det
		done(p)
	end)
end

return M
