--!nonstrict
-- Codex provider: weekly + 5h quota via `tc-probe codex`
-- (`codex app-server` under the hood), prompt/session counts from history.

local schema = require("./schema.luau")
local runner = require("./runner.luau")

local M = {}
M.id = "codex"

function M.isEnabled()
	return runner.isEnabled("codex")
end

local function countHistory()
	local path = noctalia.expandPath("~/.codex/history.jsonl")
	local raw = noctalia.readFile(path)
	if not raw then
		return 0, 0
	end
	local now = os.time()
	local lt = os.date("*t", now)
	local dayStart = os.time({ year = lt.year, month = lt.month, day = lt.day, hour = 0 })
	local prompts = 0
	local sessions = {}
	for line in (tostring(raw) .. "\n"):gmatch("(.-)\n") do
		if line:match("%S") then
			local ok, entry = pcall(noctalia.json.decode, line)
			if ok and type(entry) == "table" then
				local ts = tonumber(entry.ts) or 0
				if ts >= dayStart then
					prompts += 1
					if entry.session_id and entry.session_id ~= "" then
						sessions[tostring(entry.session_id)] = true
					end
				end
			end
		end
	end
	local n = 0
	for _ in pairs(sessions) do
		n += 1
	end
	return prompts, n
end

function M.refresh(done)
	if not M.isEnabled() then
		done(nil)
		return
	end
	runner.runCliProbe("codex", function(p)
		local prompts, sessions = countHistory()
		p.todayPrompts = prompts
		p.todaySessions = sessions
		local det = {}
		if p.balanceValue ~= "" then
			table.insert(det, { k = p.balanceLabel ~= "" and p.balanceLabel or "Balance", v = p.balanceValue })
		end
		if p.tier ~= "" then
			table.insert(det, { k = "Plan", v = p.tier })
		end
		p.details = det
		done(p)
	end)
end

return M
