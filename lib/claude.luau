--!nonstrict
-- Claude Code provider: local stats-cache/history files plus the
-- authoritative OAuth usage endpoint (https://api.anthropic.com/api/oauth/usage).

local schema = require("./schema.luau")

local M = {}
M.id = "claude"

local function cfg(key, fallback)
	local v = noctalia.getConfig(key)
	if v == nil then
		return fallback
	end
	return v
end

local function trim(s)
	return noctalia.string.trim(tostring(s or ""))
end

function M.isEnabled()
	return cfg("claude_enabled", false) == true
end

local function localDay()
	return os.date("%Y-%m-%d")
end

local function parseStats(p)
	local path = noctalia.expandPath(trim(cfg("claude_stats_path", "~/.claude/stats-cache.json")))
	if path == "" then
		return
	end
	local raw = noctalia.readFile(path)
	if not raw then
		return
	end
	local ok, data = pcall(noctalia.json.decode, raw)
	if not ok or type(data) ~= "table" then
		return
	end
	local today = localDay()
	local byModel = {}
	if type(data.dailyModelTokens) == "table" then
		for _, d in ipairs(data.dailyModelTokens) do
			if type(d) == "table" and tostring(d.date) == today then
				if type(d.tokensByModel) == "table" then
					byModel = d.tokensByModel
				end
				break
			end
		end
	end
	local sum = 0
	local rows = {}
	for model, toks in pairs(byModel) do
		local n = math.floor(tonumber(toks) or 0)
		sum += n
		table.insert(rows, { model = tostring(model), tokens = n })
	end
	table.sort(rows, function(a, b)
		return a.tokens > b.tokens
	end)
	while #rows > 5 do
		table.remove(rows)
	end
	p.todayTokens = sum
	p.topModels = rows
	if type(data.modelUsage) == "table" then
		p.details = p.details or {}
	end
	p.totalPrompts = math.floor(tonumber(data.totalMessages) or 0)
	p.totalSessions = math.floor(tonumber(data.totalSessions) or 0)
	if type(data.dailyActivity) == "table" then
		for _, d in ipairs(data.dailyActivity) do
			if type(d) == "table" then
				table.insert(p.recentDays, {
					date = tostring(d.date or ""),
					tokens = 0,
					messages = math.floor(tonumber(d.messageCount or d.messages or 0)),
				})
			end
		end
		while #p.recentDays > 7 do
			table.remove(p.recentDays, 1)
		end
	end
end

local function parseHistory(p)
	local raw = noctalia.readFile(noctalia.expandPath("~/.claude/history.jsonl"))
	if not raw then
		return
	end
	local now = os.time()
	local lt = os.date("*t", now)
	local dayStart = os.time({ year = lt.year, month = lt.month, day = lt.day, hour = 0 })
	local prompts = 0
	local lastUsed = 0
	local sessions = {}
	for line in (tostring(raw) .. "\n"):gmatch("(.-)\n") do
		if line:match("%S") then
			local ok, entry = pcall(noctalia.json.decode, line)
			if ok and type(entry) == "table" then
				local ts = math.floor((tonumber(entry.timestamp) or 0) / 1000)
				if ts > 0 and ts > lastUsed then
					lastUsed = ts
				end
				if ts >= dayStart then
					prompts += 1
					if entry.sessionId and entry.sessionId ~= "" then
						sessions[tostring(entry.sessionId)] = true
					end
				end
			end
		end
	end
	local n = 0
	for _ in pairs(sessions) do
		n += 1
	end
	p.todayPrompts = prompts
	p.todaySessions = n
	p.lastUsedAt = lastUsed
end

local function normPct(v)
	local n = tonumber(tostring(v or ""):gsub("%%", ""))
	if n == nil or n < 0 then
		return -1
	end
	if n > 1 then
		n = n / 100
	end
	return math.min(1, math.max(0, n))
end

local function probeOAuth(p, token, done)
	local finished = false
	local function finish()
		if not finished then
			finished = true
			done(p)
		end
	end
	local ok = noctalia.http({
		url = "https://api.anthropic.com/api/oauth/usage",
		method = "GET",
		headers = {
			"Authorization: Bearer " .. token,
			"anthropic-beta: oauth-2025-04-20",
			"Accept: application/json",
		},
	}, function(resp)
		if resp and resp.ok and (resp.status or 0) >= 200 and (resp.status or 0) < 300 then
			local dec, data = pcall(noctalia.json.decode, resp.body or "")
			if dec and type(data) == "table" then
				local weekly = data.seven_day_oauth_apps or data.seven_day
				local five = data.five_hour
				local wPct = -1
				local sPct = -1
				local wReset = ""
				local sReset = ""
				if type(weekly) == "table" then
					wPct = normPct(weekly.utilization)
					wReset = tostring(weekly.resets_at or "")
				end
				if type(five) == "table" then
					sPct = normPct(five.utilization)
					sReset = tostring(five.resets_at or "")
				end
				if wPct >= 0 or sPct >= 0 then
					p.primaryLabel = "Weekly (7-day)"
					p.primaryPct = wPct >= 0 and wPct or sPct
					p.primaryReset = wReset ~= "" and wReset or sReset
					if sPct >= 0 then
						p.secondaryLabel = "Session (5-hour)"
						p.secondaryPct = sPct
						p.secondaryReset = sReset
					end
					p.ready = true
					p.error = ""
				end
			end
		elseif resp and (resp.status == 401 or resp.status == 403) then
			p.error = "Token expired"
		end
		finish()
	end)
	if not ok then
		finish()
	end
end

function M.refresh(done)
	if not M.isEnabled() then
		done(nil)
		return
	end
	local p = schema.blank("claude")
	p.ready = false
	p.error = "Waiting for data"
	parseStats(p)
	parseHistory(p)

	local credPath = noctalia.expandPath(trim(cfg("claude_creds_path", "~/.claude/.credentials.json")))
	local raw = credPath ~= "" and noctalia.readFile(credPath) or nil
	if not raw then
		p.error = "Waiting for auth"
		done(p)
		return
	end
	local ok, data = pcall(noctalia.json.decode, raw)
	if not ok or type(data) ~= "table" then
		p.error = "Waiting for auth"
		done(p)
		return
	end
	local oauth = data.claudeAiOauth
	if type(oauth) ~= "table" then
		p.error = "Waiting for auth"
		done(p)
		return
	end
	local token = tostring(oauth.accessToken or "")
	if token == "" then
		p.error = "Waiting for auth"
		done(p)
		return
	end
	local expires = tonumber(oauth.expiresAt) or 0
	if expires > 0 and expires <= os.time() * 1000 then
		p.error = "Token expired"
		done(p)
		return
	end
	local sub = tostring(oauth.subscriptionType or "")
	if sub ~= "" then
		p.tier = sub:sub(1, 1):upper() .. sub:sub(2)
	end
	-- Local stats already give token counts; OAuth gives rate limits.
	if p.todayTokens > 0 then
		p.ready = true
		p.error = ""
	end
	probeOAuth(p, token, done)
end

return M
