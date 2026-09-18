--!nonstrict
-- Claude Code provider: local stats-cache/history files plus the
-- authoritative OAuth usage endpoint (https://api.anthropic.com/api/oauth/usage).
-- An expired access token is renewed through the probe helper's OAuth
-- refresh (same grant the CLI performs on startup) before reporting
-- "Token expired".

local schema = require("./schema.luau")
local runner = require("./runner.luau")

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
		while #p.recentDays > 30 do
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
	local cleaned = tostring(v or ""):gsub("%%", "")
	local n = tonumber(cleaned)
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
		local okParse, parseErr = pcall(function()
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
						if sPct >= 0 then
							p.primaryLabel = "5-hour"
							p.primaryPct = sPct
							p.primaryReset = sReset
							if wPct >= 0 then
								p.secondaryLabel = "Weekly (7-day)"
								p.secondaryPct = wPct
								p.secondaryReset = wReset
							end
						else
							p.primaryLabel = "Weekly (7-day)"
							p.primaryPct = wPct
							p.primaryReset = wReset
						end
						p.ready = true
						p.error = ""
					end
				end
			elseif resp and (resp.status == 401 or resp.status == 403) then
				p.error = "Token expired"
			end
		end)
		if not okParse then
			noctalia.log("token-cafe: claude usage parse failed: " .. tostring(parseErr))
			if p.ready ~= true then
				p.error = "Usage parse failed"
			end
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
	local historyLastUsed = tonumber(p.lastUsedAt) or 0

	local credPath = noctalia.expandPath(trim(cfg("claude_creds_path", "~/.claude/.credentials.json")))

	-- Read the OAuth credentials file. Called again after a CLI refresh
	-- so a renewed access token is picked up without a restart.
	local function readCreds()
		local token = ""
		local refreshToken = ""
		local refreshExpired = false
		local expired = false
		local oauth = nil
		local raw = credPath ~= "" and noctalia.readFile(credPath) or nil
		if raw then
			local ok, data = pcall(noctalia.json.decode, raw)
			if ok and type(data) == "table" and type(data.claudeAiOauth) == "table" then
				oauth = data.claudeAiOauth
				local t = tostring(oauth.accessToken or "")
				local expires = tonumber(oauth.expiresAt) or 0
				local nowMs = os.time() * 1000
				if t ~= "" and not (expires > 0 and expires <= nowMs) then
					token = t
				elseif expires > 0 and expires <= nowMs then
					expired = true
				end
				refreshToken = tostring(oauth.refreshToken or "")
				local refreshExp = tonumber(oauth.refreshTokenExpiresAt) or 0
				if refreshExp > 0 and refreshExp <= nowMs then
					refreshExpired = true
				end
			end
		end
		return { token = token, refreshToken = refreshToken, refreshExpired = refreshExpired, expired = expired, oauth = oauth }
	end

	local function applyTier(oauth)
		if type(oauth) == "table" then
			local sub = tostring(oauth.subscriptionType or "")
			if sub ~= "" then
				p.tier = sub:sub(1, 1):upper() .. sub:sub(2)
			end
		end
	end

	local creds = readCreds()
	applyTier(creds.oauth)
	local token = creds.token
	if token == "" then
		if creds.oauth == nil then
			p.error = "Waiting for auth"
		elseif creds.refreshExpired then
			p.error = "Re-login required"
		elseif creds.expired then
			p.error = "Token expired"
		else
			p.error = "Waiting for auth"
		end
	end
	-- Local stats already give token counts; OAuth gives rate limits.
	if p.todayTokens > 0 then
		p.ready = true
		p.error = ""
	end

	-- Merge `tc-probe claude` transcript history (Codex-style token
	-- chart): today totals + per-model rows + 15-day recentDays.
	-- Falls back to stats-cache message counts when transcripts are
	-- missing so the chart still renders.
	local function mergeHistory(data)
		if type(data) ~= "table" then
			return
		end
		local lastUsed = math.floor(tonumber(data.lastUsedAt) or 0)
		if lastUsed > historyLastUsed then
			p.lastUsedAt = lastUsed
		end
		if type(data.recentDays) == "table" and #data.recentDays > 0 then
			local rows = {}
			local tokenSum = 0
			for _, d in ipairs(data.recentDays) do
				if type(d) == "table" then
					local toks = math.floor(tonumber(d.tokens) or 0)
					local msgs = math.floor(tonumber(d.messageCount or d.messages) or 0)
					tokenSum += toks
					table.insert(rows, {
						date = tostring(d.date or ""),
						tokens = toks,
						messages = msgs,
					})
				end
			end
			if tokenSum > 0 then
				p.recentDays = rows
			end
		end
		local today = math.floor(tonumber(data.todayTotalTokens) or 0)
		if today > 0 then
			p.todayTokens = today
			if type(data.todayTokensByModel) == "table" then
				local rows = {}
				for model, toks in pairs(data.todayTokensByModel) do
					table.insert(rows, { model = tostring(model), tokens = math.floor(tonumber(toks) or 0) })
				end
				table.sort(rows, function(a, b)
					return a.tokens > b.tokens
				end)
				while #rows > 5 do
					table.remove(rows)
				end
				p.topModels = rows
			end
			p.ready = true
			if p.error == "Waiting for data" or p.error == "Waiting for auth" then
				p.error = ""
			end
		end
	end

	local historyDone = false
	local oauthDone = false
	local finished = false
	local refreshAttempted = false
	local function maybeFinish()
		if historyDone and oauthDone and not finished then
			finished = true
			done(p)
		end
	end
	runner.runClaudeHistory(function(data)
		local okM, errM = pcall(mergeHistory, data)
		if not okM then
			noctalia.log("token-cafe: claude history merge failed: " .. tostring(errM))
		end
		historyDone = true
		maybeFinish()
	end)

	-- Renew the OAuth access token through the probe helper (standard
	-- refresh-token grant, same as the CLI performs on startup), then
	-- re-read the credentials file. Calls cb with the fresh access token
	-- (possibly "").
	local function attemptRefresh(cb)
		if refreshAttempted then
			cb(readCreds().token)
			return
		end
		refreshAttempted = true
		runner.runClaudeAuthRefresh(function(data)
			local fresh = readCreds()
			applyTier(fresh.oauth)
			local newToken = fresh.token
			token = newToken
			if newToken == "" then
				local loggedIn = nil
				if type(data) == "table" then
					loggedIn = data.loggedIn
				end
				if loggedIn == false or fresh.refreshExpired then
					p.error = "Re-login required"
				elseif p.error == "Waiting for data" then
					p.error = "Token expired"
				end
			end
			cb(newToken)
		end)
	end

	local function canRefresh()
		if refreshAttempted then
			return false
		end
		local fresh = readCreds()
		return fresh.refreshToken ~= "" and not fresh.refreshExpired
	end

	local function probeWith(tok, onDone)
		probeOAuth(p, tok, onDone)
	end

	-- Probe usage, retrying once via a token refresh when the endpoint
	-- rejects the token (stale/revoked access token with a live refresh
	-- token). finishOAuth runs exactly once per M.refresh call.
	local function finishOAuth()
		if not oauthDone then
			oauthDone = true
			maybeFinish()
		end
	end
	local function beginUsage(tok)
		probeWith(tok, function()
			if p.error == "Token expired" and canRefresh() then
				attemptRefresh(function(newToken)
					if newToken ~= "" then
						probeWith(newToken, function()
							if p.error == "Token expired" then
								p.error = "Re-login required"
							end
							finishOAuth()
						end)
					else
						finishOAuth()
					end
				end)
				return
			end
			finishOAuth()
		end)
	end

	if token ~= "" then
		beginUsage(token)
	elseif canRefresh() then
		-- Expired access token but a refresh token exists: renew it first
		-- instead of showing "Token expired".
		attemptRefresh(function(newToken)
			if newToken ~= "" then
				beginUsage(newToken)
			else
				finishOAuth()
			end
		end)
	else
		-- No OAuth token: still show local history chart.
		finishOAuth()
	end
end

return M
