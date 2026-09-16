--!nonstrict
-- Token Cafe backend: fan-out across provider modules, publish one
-- shared snapshot for the widget + panel.
--
-- State channel:
--   tc_snapshot <- { providers = {...}, updatedAt, revision, refreshing }
--   tc_command  -> { action = "refresh", requestId }

local schema = require("./lib/schema.luau")
local codex = require("./lib/codex.luau")
local claude = require("./lib/claude.luau")
local antigravity = require("./lib/antigravity.luau")
local opencode = require("./lib/opencode.luau")
local openrouter = require("./lib/openrouter.luau")
local grok = require("./lib/grok.luau")
local venice = require("./lib/venice.luau")

local STATE_KEY = "tc_snapshot"
local COMMAND_KEY = "tc_command"
local RESULT_KEY = "tc_action_result"

local MODULES = { codex, claude, antigravity, opencode, openrouter, grok, venice }

local snapshot = {
	providers = {},
	updatedAt = 0,
	revision = 0,
	refreshing = false,
}

local generation = 0
local pending = false

-- Active refresh state, visible to update() so one hung provider cannot
-- block the whole snapshot forever (noctalia.http has no timeout).
local activeMods = {}
local refreshResults = {}
local refreshRemaining = 0
local refreshGen = 0
local refreshDeadlineMs = 0

local function nowMs()
	local ok, v = pcall(function()
		return noctalia.nowMs()
	end)
	if ok and type(v) == "number" then
		return v
	end
	return os.time() * 1000
end

local function probeTimeoutMs()
	local s = tonumber(noctalia.getConfig("probe_timeout_sec")) or 25
	s = math.max(5, math.min(60, math.floor(s)))
	return s * 1000 + 5000
end

-- Custom provider order, persisted across restarts in the plugin data dir.
-- The panel sends the full Overview order; the service sorts every
-- snapshot by it so panel tabs + bar widget follow the same order.
local customOrder = nil
local publish -- forward: defined with snapshot below, used by applyReorder.

local function orderFilePath()
	local ok, dir = pcall(function()
		return noctalia.pluginDataDir()
	end)
	if ok and type(dir) == "string" and dir ~= "" then
		return dir .. "/provider_order.json"
	end
	return nil
end

local function loadCustomOrder()
	local ids = {}
	local path = orderFilePath()
	if path ~= nil then
		local ok, raw = pcall(function()
			return noctalia.readFile(path)
		end)
		if ok and type(raw) == "string" and raw ~= "" then
			local dok, decoded = pcall(function()
				return noctalia.json.decode(raw)
			end)
			if dok and type(decoded) == "table" then
				for _, v in ipairs(decoded) do
					if type(v) == "string" and v ~= "" then
						table.insert(ids, v)
					end
				end
			end
		end
	end
	local seen = {}
	local merged = {}
	for _, id in ipairs(ids) do
		if not seen[id] then
			seen[id] = true
			table.insert(merged, id)
		end
	end
	for _, id in ipairs(schema.ORDER) do
		if not seen[id] then
			seen[id] = true
			table.insert(merged, id)
		end
	end
	customOrder = merged
end

local function saveCustomOrder()
	if type(customOrder) ~= "table" then
		return
	end
	local path = orderFilePath()
	if path == nil then
		return
	end
	local ok, encoded = pcall(function()
		return noctalia.json.encode(customOrder)
	end)
	if ok and type(encoded) == "string" then
		pcall(function()
			noctalia.writeFile(path, encoded)
		end)
	end
end

local function orderIndex()
	if type(customOrder) ~= "table" then
		loadCustomOrder()
	end
	local map = {}
	for i, id in ipairs(customOrder) do
		map[id] = i
	end
	return map
end

local function sortProviders(providers)
	local index = orderIndex()
	table.sort(providers, function(a, b)
		return (index[a.id] or 99) < (index[b.id] or 99)
	end)
end

local function applyReorder(newOrder)
	if type(newOrder) ~= "table" then
		return false
	end
	local cleaned = {}
	local seen = {}
	for _, v in ipairs(newOrder) do
		if type(v) == "string" and v ~= "" and not seen[v] then
			seen[v] = true
			table.insert(cleaned, v)
		end
	end
	if #cleaned == 0 then
		return false
	end
	if type(customOrder) == "table" then
		for _, id in ipairs(customOrder) do
			if not seen[id] then
				seen[id] = true
				table.insert(cleaned, id)
			end
		end
	end
	for _, id in ipairs(schema.ORDER) do
		if not seen[id] then
			seen[id] = true
			table.insert(cleaned, id)
		end
	end
	customOrder = cleaned
	saveCustomOrder()
	if type(snapshot.providers) == "table" then
		sortProviders(snapshot.providers)
		publish()
	end
	return true
end

local function refreshIntervalMs()
	local sec = tonumber(noctalia.getConfig("refresh_interval")) or 300
	sec = math.max(60, math.min(3600, math.floor(sec)))
	return sec * 1000
end

publish = function()
	noctalia.state.set(STATE_KEY, snapshot)
end

local function seedBlanks()
	local providers = {}
	for _, mod in ipairs(MODULES) do
		if mod.isEnabled() then
			table.insert(providers, schema.blank(mod.id))
		end
	end
	-- Custom Overview order.
	sortProviders(providers)
	snapshot.providers = providers
end

-- Publish whatever has arrived; providers that never called back get a
-- timeout card so one hung request cannot hide every other provider.
local function finishRefresh()
	if not pending or refreshGen ~= generation then
		return
	end
	local seen = {}
	for _, p in ipairs(refreshResults) do
		if type(p) == "table" and p.id ~= nil then
			seen[p.id] = true
		end
	end
	for _, mod in ipairs(activeMods) do
		if not seen[mod.id] then
			local p = schema.blank(mod.id)
			p.error = "Request timed out"
			table.insert(refreshResults, p)
		end
	end
	local providers = {}
	for _, p in ipairs(refreshResults) do
		if type(p) == "table" then
			table.insert(providers, p)
		end
	end
	sortProviders(providers)
	snapshot.providers = providers
	snapshot.refreshing = false
	snapshot.updatedAt = os.time()
	snapshot.revision += 1
	publish()
	pending = false
	refreshRemaining = 0
	noctalia.setUpdateInterval(refreshIntervalMs())
end

local function refreshAll()
	if pending then
		return
	end
	pending = true
	generation += 1
	local gen = generation
	-- First load seeds blanks so the bar can show "…" while loading.
	-- Background refreshes keep last-known providers visible so the
	-- bar updates silently without flickering to "…".
	local isFirstLoad = #snapshot.providers == 0
	if isFirstLoad then
		seedBlanks()
		snapshot.refreshing = true
		snapshot.updatedAt = os.time()
		publish()
	else
		local byId = {}
		for _, p in ipairs(snapshot.providers) do
			if type(p) == "table" and p.id ~= nil then
				byId[p.id] = p
			end
		end
		local providers = {}
		for _, mod in ipairs(MODULES) do
			if mod.isEnabled() then
				if byId[mod.id] then
					table.insert(providers, byId[mod.id])
				else
					table.insert(providers, schema.blank(mod.id))
				end
			end
		end
		sortProviders(providers)
		snapshot.providers = providers
		snapshot.refreshing = true
		publish()
	end

	local active = {}
	for _, mod in ipairs(MODULES) do
		if mod.isEnabled() then
			table.insert(active, mod)
		end
	end
	if #active == 0 then
		snapshot.refreshing = false
		snapshot.revision += 1
		publish()
		pending = false
		noctalia.setUpdateInterval(refreshIntervalMs())
		return
	end

	refreshGen = gen
	activeMods = active
	refreshResults = {}
	refreshRemaining = #active
	refreshDeadlineMs = nowMs() + probeTimeoutMs()
	-- Fast tick while waiting so update() can enforce the deadline;
	-- finishRefresh() restores the slow periodic interval.
	noctalia.setUpdateInterval(1000)

	local function finish()
		if gen ~= generation or not pending then
			return
		end
		refreshRemaining -= 1
		if refreshRemaining > 0 then
			return
		end
		finishRefresh()
	end

	for _, mod in ipairs(active) do
		local thisMod = mod
		local called = false
		local ok, err = pcall(function()
			thisMod.refresh(function(p)
				if called or gen ~= generation or not pending then
					return
				end
				called = true
				if p ~= nil then
					table.insert(refreshResults, p)
				else
					local blank = schema.blank(thisMod.id)
					blank.error = "No data"
					table.insert(refreshResults, blank)
				end
				finish()
			end)
		end)
		if not ok then
			noctalia.log("token-cafe: " .. tostring(mod.id) .. " refresh failed: " .. tostring(err))
			local p = schema.blank(mod.id)
			p.error = "Refresh failed"
			table.insert(refreshResults, p)
			finish()
		end
	end
end

noctalia.state.watch(COMMAND_KEY, function(cmd)
	if type(cmd) ~= "table" then
		return
	end
	if cmd.action == "reorder" then
		if applyReorder(cmd.order) then
			noctalia.state.set(RESULT_KEY, {
				requestId = tostring(cmd.requestId or ""),
				action = "reorder",
				ok = true,
				message = "",
			})
		end
		return
	end
	if cmd.action == "refresh" then
		pending = false
		refreshAll()
		noctalia.state.set(RESULT_KEY, {
			requestId = tostring(cmd.requestId or ""),
			action = "refresh",
			ok = true,
			message = "Refreshing…",
		})
	end
end)

function onConfigChanged()
	pending = false
	refreshAll()
end

function onIpc(event, payload)
	if event == "refresh" then
		pending = false
		refreshAll()
	elseif type(payload) == "table" and type(payload.action) == "string" and payload.action == "refresh" then
		pending = false
		refreshAll()
	end
end

noctalia.setUpdateInterval(refreshIntervalMs())
loadCustomOrder()
refreshAll()

function update()
	if pending then
		-- A provider that never calls back (thrown async callback,
		-- stalled HTTP with no timeout) must not hold the snapshot
		-- in "…" forever: publish what arrived plus timeout cards.
		if refreshRemaining > 0 and nowMs() >= refreshDeadlineMs then
			noctalia.log("token-cafe: refresh timed out, publishing partial results")
			finishRefresh()
		end
		return
	end
	refreshAll()
end
