--!nonstrict
-- OpenRouter provider: /key limits + 7d /activity usage.

local schema = require("./schema.luau")

local M = {}
M.id = "openrouter"

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
	return cfg("openrouter_enabled", false) == true
end

function M.apiKey()
	local env = noctalia.getenv("OPENROUTER_API_KEY") or ""
	if trim(env) ~= "" then
		return trim(env)
	end
	return trim(cfg("openrouter_key", ""))
end

local function num(v, fallback)
	local n = tonumber(v)
	if n == nil then
		return fallback
	end
	return n
end

function M.refresh(done)
	if not M.isEnabled() then
		done(nil)
		return
	end
	local key = M.apiKey()
	local p = schema.blank("openrouter")
	if key == "" then
		p.error = "Add an OpenRouter API key"
		done(p)
		return
	end
	local finished = false
	local function finish()
		if not finished then
			finished = true
			done(p)
		end
	end

	local function applyActivity(days, results)
		local models = {}
		local total = 0
		for _, day in ipairs(days) do
			local r = results[day] or { requests = 0, tokens = 0, entries = {} }
			total += r.tokens
			table.insert(p.recentDays, { date = day, tokens = r.tokens, messages = r.requests })
			for _, e in ipairs(r.entries) do
				models[e.model] = (models[e.model] or 0) + e.tokens
			end
		end
		local today = results[days[#days]] or { requests = 0, tokens = 0, entries = {} }
		p.todayPrompts = today.requests
		p.todayTokens = today.tokens
		local todayRows = {}
		for _, e in ipairs(today.entries) do
			todayRows[e.model] = (todayRows[e.model] or 0) + e.tokens
		end
		local top = {}
		for model, toks in pairs(todayRows) do
			table.insert(top, { model = model, tokens = toks })
		end
		table.sort(top, function(a, b)
			return a.tokens > b.tokens
		end)
		while #top > 5 do
			table.remove(top)
		end
		p.topModels = top
		if not p.ready and total > 0 then
			p.ready = true
			p.error = ""
		end
		finish()
	end

	local function fetchActivity()
		local days = {}
		local now = os.time()
		for i = 6, 0, -1 do
			table.insert(days, os.date("%Y-%m-%d", now - i * 86400))
		end
		local pending = #days
		local results = {}
		for _, day in ipairs(days) do
			local dayCopy = day
			local started = noctalia.http({
				url = "https://openrouter.ai/api/v1/activity?date=" .. dayCopy,
				method = "GET",
				headers = { "Authorization: Bearer " .. key },
			}, function(resp)
				local reqs = 0
				local toks = 0
				local entries = {}
				if resp and resp.ok and (resp.status or 0) == 200 then
					local dec, data = pcall(noctalia.json.decode, resp.body or "")
					if dec and type(data) == "table" and type(data.data) == "table" then
						for _, e in ipairs(data.data) do
							if type(e) == "table" then
								reqs += math.floor(num(e.requests, 0))
								local t = math.floor(num(e.prompt_tokens, 0)) + math.floor(num(e.completion_tokens, 0))
								toks += t
								table.insert(entries, { model = tostring(e.model or "unknown"), tokens = t })
							end
						end
					end
				end
				results[dayCopy] = { requests = reqs, tokens = toks, entries = entries }
				pending -= 1
				if pending <= 0 then
					applyActivity(days, results)
				end
			end)
			if not started then
				pending -= 1
				if pending <= 0 then
					applyActivity(days, results)
				end
			end
		end
	end

	local ok = noctalia.http({
		url = "https://openrouter.ai/api/v1/key",
		method = "GET",
		headers = { "Authorization: Bearer " .. key },
	}, function(resp)
		if not (resp and resp.ok and (resp.status or 0) == 200) then
			p.error = "Key request failed"
			fetchActivity()
			return
		end
		local dec, data = pcall(noctalia.json.decode, resp.body or "")
		if not dec or type(data) ~= "table" then
			p.error = "Invalid key response"
			fetchActivity()
			return
		end
		local info = data.data
		if type(info) ~= "table" then
			info = data
		end
		local weekly = num(info.usage_weekly, 0)
		local limit = num(info.limit, -1)
		local remaining = num(info.limit_remaining, -1)
		if limit > 0 then
			p.primaryPct = math.min(1, math.max(0, weekly / limit))
			p.primaryLabel = string.format("Spending ($%.2f / $%.2f)", weekly, limit)
		elseif remaining >= 0 and (weekly + remaining) > 0 then
			local budget = weekly + remaining
			p.primaryPct = math.min(1, math.max(0, weekly / budget))
			p.primaryLabel = string.format("Budget ($%.2f / $%.2f)", weekly, budget)
		else
			p.primaryPct = 0
			p.primaryLabel = "No spending limit"
		end
		p.primaryReset = tostring(info.limit_reset or "")
		p.tier = info.is_free_tier == true and "Free" or "Paid"
		p.details = {
			{ k = "Daily", v = string.format("$%.2f", num(info.usage_daily, 0)) },
			{ k = "Weekly", v = string.format("$%.2f", weekly) },
			{ k = "Monthly", v = string.format("$%.2f", num(info.usage_monthly, 0)) },
		}
		p.ready = true
		p.error = ""
		fetchActivity()
	end)
	if not ok then
		finish()
		return
	end
end

return M
