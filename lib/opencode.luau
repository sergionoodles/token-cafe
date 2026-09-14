--!nonstrict
-- OpenCode provider: local SQLite activity via `tc-probe opencode`.

local runner = require("./runner.luau")

local M = {}
M.id = "opencode"

function M.isEnabled()
	return runner.isEnabled("opencode")
end

function M.refresh(done)
	if not M.isEnabled() then
		done(nil)
		return
	end
	runner.runCliProbe("opencode", function(p)
		p.primaryLabel = "Local usage"
		p.primaryPct = -1
		p.details = {
			{ k = "Today", v = (p.todayPrompts or 0) .. " prompts · " .. (p.todaySessions or 0) .. " sessions" },
			{ k = "Today cost", v = p.balanceValue ~= "" and p.balanceValue or "$0.00" },
		}
		done(p)
	end)
end

return M
