--!nonstrict
-- Provider registry + normalized snapshot shape for Token Cafe.
-- Every provider module normalizes into blank() so widget/panel
-- can render without per-provider branches.

local M = {}

M.ORDER = { "codex", "claude", "antigravity", "opencode", "openrouter", "grok", "venice" }

M.META = {
	codex = { name = "Codex", glyph = "terminal-2" },
	claude = { name = "Claude Code", glyph = "sparkles" },
	antigravity = { name = "Google Antigravity", glyph = "planet" },
	opencode = { name = "OpenCode", glyph = "code" },
	openrouter = { name = "OpenRouter", glyph = "world" },
	grok = { name = "Grok", glyph = "bolt" },
	venice = { name = "Venice", glyph = "crown" },
}

function M.displayName(id)
	local meta = M.META[id]
	if meta then
		return meta.name
	end
	return tostring(id or "Provider")
end

function M.glyph(id)
	local meta = M.META[id]
	if meta then
		return meta.glyph
	end
	return "cpu"
end

-- Canonical empty provider card. All fields are plain data
-- (safe for noctalia.state across isolated entries).
function M.blank(id)
	return {
		id = id,
		name = M.displayName(id),
		glyph = M.glyph(id),
		enabled = true,
		ready = false,
		error = "Loading…",
		primaryLabel = "Usage",
		primaryPct = -1,
		primaryReset = "",
		secondaryLabel = "",
		secondaryPct = -1,
		secondaryReset = "",
		windows = {},
		tier = "",
		balanceLabel = "",
		balanceValue = "",
		resetCreditsAvailable = -1,
		resetCredits = {},
		todayTokens = 0,
		todayPrompts = 0,
		todaySessions = 0,
		lastUsedAt = 0,
		details = {},
		topModels = {},
		recentDays = {},
	}
end

local function num(v, fallback)
	local n = tonumber(v)
	if n == nil then
		return fallback
	end
	return n
end

local function str(v)
	if v == nil then
		return ""
	end
	return tostring(v)
end

-- Merge a decoded probe JSON object into a blank card.
-- Used by the CLI-backed providers (codex/opencode/grok/antigravity).
function M.fromProbe(id, data)
	local p = M.blank(id)
	if type(data) ~= "table" then
		p.error = "Probe failed"
		return p
	end
	p.ready = data.ready == true
	p.error = str(data.usageStatusText or data.error)
	if p.ready and p.error == "" then
		p.error = ""
	end
	p.primaryLabel = str(data.rateLimitLabel) ~= "" and str(data.rateLimitLabel) or "Usage"
	p.primaryPct = num(data.rateLimitPercent, -1)
	p.primaryReset = str(data.rateLimitResetAt)
	p.secondaryLabel = str(data.secondaryRateLimitLabel or "")
	p.secondaryPct = num(data.secondaryRateLimitPercent, -1)
	p.secondaryReset = str(data.secondaryRateLimitResetAt or "")
	if type(data.quotaWindows) == "table" then
		for _, w in ipairs(data.quotaWindows) do
			if type(w) == "table" then
				table.insert(p.windows, {
					label = str(w.label) ~= "" and str(w.label) or "Quota",
					pct = num(w.usedPercent, -1),
					reset = str(w.resetAt or ""),
				})
			end
		end
	end
	p.tier = str(data.tierLabel or "")
	p.balanceLabel = str(data.balanceLabel or "")
	p.balanceValue = str(data.balanceValue or "")
	p.resetCreditsAvailable = num(data.resetCreditsAvailable, -1)
	if type(data.resetCredits) == "table" then
		for _, credit in ipairs(data.resetCredits) do
			if type(credit) == "table" then
				table.insert(p.resetCredits, {
					id = str(credit.id),
					expiresAt = str(credit.expiresAt),
				})
			end
		end
	end
	p.todayTokens = math.floor(num(data.todayTotalTokens, 0))
	p.todayPrompts = math.floor(num(data.todayPrompts, 0))
	p.todaySessions = math.floor(num(data.todaySessions, 0))
	p.lastUsedAt = math.floor(num(data.lastUsedAt or data.lastActivityAt, 0))
	if type(data.todayTokensByModel) == "table" then
		local rows = {}
		for model, toks in pairs(data.todayTokensByModel) do
			table.insert(rows, { model = str(model), tokens = math.floor(num(toks, 0)) })
		end
		table.sort(rows, function(a, b)
			return a.tokens > b.tokens
		end)
		while #rows > 5 do
			table.remove(rows)
		end
		p.topModels = rows
	end
	if type(data.recentDays) == "table" then
		for _, d in ipairs(data.recentDays) do
			if type(d) == "table" then
				table.insert(p.recentDays, {
					date = str(d.date),
					tokens = math.floor(num(d.tokens, 0)),
					messages = math.floor(num(d.messageCount or d.requests, 0)),
				})
			end
		end
	end
	if type(data.dailyUsageBuckets) == "table" then
		for _, d in ipairs(data.dailyUsageBuckets) do
			if type(d) == "table" then
				table.insert(p.recentDays, {
					date = str(d.date),
					tokens = math.floor(num(d.tokens, 0)),
					messages = 0,
				})
			end
		end
	end
	return p
end

function M.readyCount(providers)
	local n = 0
	for _, p in ipairs(providers or {}) do
		if type(p) == "table" and p.ready == true then
			n += 1
		end
	end
	return n
end

-- Highest primary pct across ready providers (-1 when none).
function M.worstPct(providers)
	local worst = -1
	for _, p in ipairs(providers or {}) do
		if type(p) == "table" and p.ready == true then
			local v = tonumber(p.primaryPct) or -1
			if v > worst then
				worst = v
			end
		end
	end
	return worst
end

return M
