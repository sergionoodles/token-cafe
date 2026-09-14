--!nonstrict
-- Venice.ai provider: /api_keys/rate_limits per configured key.

local schema = require("./schema.luau")

local M = {}
M.id = "venice"

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
	return cfg("venice_enabled", false) == true
end

function M.apiKeys()
	local seen = {}
	local keys = {}
	local function add(raw)
		local k = trim(raw)
		if k ~= "" and not seen[k] then
			seen[k] = true
			table.insert(keys, k)
		end
	end
	local configured = cfg("venice_keys", "")
	if type(configured) == "table" then
		for _, v in ipairs(configured) do
			add(v)
		end
	else
		for part in tostring(configured or ""):gmatch("([^,\n]+)") do
			add(part)
		end
	end
	add(noctalia.getenv("VENICE_API_KEY") or "")
	return keys
end

local function fnum(v)
	if v == nil or v == "" then
		return nil
	end
	local n = tonumber(v)
	return n
end

function M.refresh(done)
	if not M.isEnabled() then
		done(nil)
		return
	end
	local keys = M.apiKeys()
	local p = schema.blank("venice")
	p.primaryLabel = "Remaining balance"
	if #keys == 0 then
		p.error = "Add a Venice API key"
		done(p)
		return
	end
	local pending = #keys
	local accounts = {}
	for i, k in ipairs(keys) do
		accounts[i] = {
			suffix = k:sub(-6),
			ready = false,
			error = "Loading",
			remaining = "",
			usd = nil,
			diem = nil,
			tier = "",
			reset = "",
		}
	end
	local finished = false
	local function finish()
		if finished then
			return
		end
		finished = true
		local readyRows = {}
		for _, a in ipairs(accounts) do
			if a.ready then
				table.insert(readyRows, a)
			end
		end
		p.ready = #readyRows > 0
		if p.ready then
			p.error = #readyRows == #accounts and "" or "Some API keys could not be read"
			if #readyRows == 1 then
				p.balanceLabel = "Balance"
				p.balanceValue = readyRows[1].remaining
			else
				p.balanceLabel = "API keys"
				p.balanceValue = #readyRows .. " tracked"
			end
			local tiers = {}
			for _, a in ipairs(readyRows) do
				if a.tier ~= "" then
					tiers[a.tier] = true
				end
				if p.primaryReset == "" and a.reset ~= "" then
					p.primaryReset = a.reset
				end
			end
			local names = {}
			for name in pairs(tiers) do
				table.insert(names, name)
			end
			p.tier = table.concat(names, ", ")
			local det = {}
			for _, a in ipairs(accounts) do
				table.insert(det, {
					k = "…" .. a.suffix,
					v = a.ready and a.remaining or ("Error: " .. a.error),
				})
			end
			p.details = det
		else
			p.error = "Venice API key requests failed"
		end
		done(p)
	end

	for i, key in ipairs(keys) do
		local idx = i
		local keyCopy = key
		local ok = noctalia.http({
			url = "https://api.venice.ai/api/v1/api_keys/rate_limits",
			method = "GET",
			headers = {
				"Authorization: Bearer " .. keyCopy,
				"Accept: application/json",
			},
		}, function(resp)
			local a = accounts[idx]
			if resp and resp.ok and (resp.status or 0) >= 200 and (resp.status or 0) < 300 then
				local dec, payload = pcall(noctalia.json.decode, resp.body or "")
				if dec and type(payload) == "table" then
					local data = payload.data
					if type(data) ~= "table" then
						data = payload
					end
					local balances = type(data.balances) == "table" and data.balances or {}
					local usd = fnum(balances.USD)
					local diem = fnum(balances.DIEM)
					local parts = {}
					if usd ~= nil then
						table.insert(parts, string.format("$%.2f", usd))
					end
					if diem ~= nil then
						table.insert(parts, string.format("%.2f DIEM", diem))
					end
					a.usd = usd
					a.diem = diem
					a.remaining = #parts > 0 and table.concat(parts, " + ") or "No balance reported"
					a.tier = type(data.apiTier) == "table" and tostring(data.apiTier.id or "") or ""
					a.reset = tostring(data.nextEpochBegins or "")
					if data.accessPermitted == false then
						a.error = "Access not permitted"
					else
						a.ready = true
						a.error = ""
					end
				else
					a.error = "Invalid API response"
				end
			else
				a.error = "HTTP " .. tostring(resp and resp.status or "?")
			end
			pending -= 1
			if pending <= 0 then
				finish()
			end
		end)
		if not ok then
			accounts[idx].error = "Request failed"
			pending -= 1
			if pending <= 0 then
				finish()
				return
			end
		end
	end
end

return M
