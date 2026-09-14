--!nonstrict
-- Token Cafe panel: dashboard cards + per-provider detail.

local STATE_KEY = "tc_snapshot"
local COMMAND_KEY = "tc_command"
local RESULT_KEY = "tc_action_result"

local format = require("./lib/format.luau")
local schema = require("./lib/schema.luau")

local snapshot = noctalia.state.get(STATE_KEY) or { providers = {}, refreshing = false, updatedAt = 0 }
local tab = "dashboard" -- "dashboard" | provider id
local feedback = ""
local feedbackError = false
local dirty = true
local requestCounter = 0
local refreshPhase = 0

local REFRESH_GLYPHS = { "loader-quarter", "loader-3", "loader-2", "loader" }
local REFRESH_OPACITY = { 0.55, 0.78, 1.0, 0.78 }
local REFRESH_UPDATE_MS = 100
local IDLE_UPDATE_MS = 1000

local render

local function send(action)
	requestCounter += 1
	noctalia.state.set(COMMAND_KEY, { action = action, requestId = "panel-" .. tostring(requestCounter) })
end

local function statusColor(p)
	if type(p) ~= "table" or p.ready ~= true then
		return "on_surface_variant"
	end
	return format.usageColor(tonumber(p.primaryPct) or -1)
end

local function statusText(p)
	if type(p) ~= "table" then
		return "Unavailable"
	end
	if p.ready ~= true then
		return (p.error and p.error ~= "") and p.error or "Waiting for data"
	end
	if (tonumber(p.primaryPct) or -1) >= 0 then
		return (p.primaryLabel or "Usage") .. " " .. format.pctText(p.primaryPct)
	end
	if (p.balanceValue or "") ~= "" then
		return p.balanceValue
	end
	return "Ready"
end

-- Brand logo when known, Tabler glyph fallback otherwise.
-- Logos are off-white SVGs (see assets/); status is still
-- conveyed by the adjacent status text color.
local function providerIcon(p, size)
	if type(p) == "table" and (p.icon or "") ~= "" then
		return ui.image({ path = p.icon, width = size, height = size, fit = "contain" })
	end
	local name = (type(p) == "table" and p.glyph) or "cpu"
	return ui.glyph({ name = name, size = size, color = statusColor(p) })
end

local function quotaRows(p)
	local rows = {}
	if type(p.windows) == "table" and #p.windows > 0 then
		for _, w in ipairs(p.windows) do
			table.insert(rows, {
				label = w.label or "Quota",
				pct = tonumber(w.pct) or -1,
				reset = w.reset or "",
			})
		end
		return rows
	end
	if (tonumber(p.primaryPct) or -1) >= 0 then
		table.insert(rows, { label = p.primaryLabel or "Usage", pct = p.primaryPct, reset = p.primaryReset or "" })
	end
	if (tonumber(p.secondaryPct) or -1) >= 0 then
		table.insert(rows, { label = p.secondaryLabel or "Usage", pct = p.secondaryPct, reset = p.secondaryReset or "" })
	end
	return rows
end

local HISTORY_DAYS = 15
local HISTORY_BAR_MAX_H = 56
local HISTORY_BAR_W = 16

-- Last N calendar days ending today, zero-filled from p.recentDays.
-- Uses tokens, falling back to messages (Claude reports messages only).
local function historySeries(p)
	local byDate = {}
	local tokenSum = 0
	if type(p.recentDays) == "table" then
		for _, d in ipairs(p.recentDays) do
			if type(d) == "table" then
				local key = tostring(d.date or "")
				if key ~= "" then
					local tokens = tonumber(d.tokens) or 0
					local messages = tonumber(d.messages) or 0
					byDate[key] = { tokens = tokens, messages = messages }
					tokenSum += tokens
				end
			end
		end
	end
	local useMessages = tokenSum <= 0
	local now = os.time()
	local series = {}
	for i = HISTORY_DAYS - 1, 0, -1 do
		local t = now - i * 86400
		local key = os.date("%Y-%m-%d", t)
		local entry = byDate[key]
		local v = 0
		if entry then
			v = useMessages and (entry.messages or 0) or (entry.tokens or 0)
		end
		table.insert(series, {
			date = key,
			day = os.date("%d", t),
			month = os.date("%b", t),
			monthKey = os.date("%Y-%m", t),
			value = math.max(0, math.floor(tonumber(v) or 0)),
			isToday = i == 0,
		})
	end
	return series, useMessages
end

-- True when p.recentDays carries any non-zero usage (tokens or messages).
-- Providers without history (e.g. Grok billing snapshot) skip the chart
-- instead of rendering an empty one.
local function hasHistoryData(p)
	if type(p) ~= "table" or type(p.recentDays) ~= "table" then
		return false
	end
	for _, d in ipairs(p.recentDays) do
		if type(d) == "table" then
			if (tonumber(d.tokens) or 0) > 0 or (tonumber(d.messages) or 0) > 0 then
				return true
			end
		end
	end
	return false
end

local function historyChart(p)
	local series, useMessages = historySeries(p)
	local max = 0
	for _, s in ipairs(series) do
		if s.value > max then
			max = s.value
		end
	end
	if max <= 0 then
		max = 1
	end
	local bars = {}
	local prevMonthKey = nil
	for _, s in ipairs(series) do
		local frac = s.value / max
		local h = math.floor(frac * HISTORY_BAR_MAX_H + 0.5)
		if h < 3 then
			h = 3
		end
		local fill = "surface_variant"
		if s.value > 0 then
			fill = s.isToday and "tertiary" or "primary"
		end
		-- Month separator: show the 3-letter month under the first bar
		-- of the window and wherever the month rolls over. A blank
		-- placeholder keeps the rows aligned on the other days.
		local showMonth = prevMonthKey == nil or s.monthKey ~= prevMonthKey
		prevMonthKey = s.monthKey
		table.insert(bars, ui.column({ key = "hist-" .. s.date, gap = 1, align = "center", flexGrow = 1 }, {
			ui.box({ fill = fill, width = HISTORY_BAR_W, height = h, radius = 2 }),
			ui.label({
				text = s.day,
				fontSize = 9,
				color = s.isToday and "tertiary" or "on_surface_variant",
				fontWeight = s.isToday and "bold" or nil,
			}),
			ui.label({
				text = showMonth and s.month or " ",
				fontSize = 8,
				color = "on_surface_variant/0.7",
			}),
		}))
	end
	local unit = useMessages and (tostring(max) .. " msgs max") or (format.formatTokens(max) .. " max")
	return ui.column({
		gap = 4,
		align = "stretch",
		padding = 10,
		fill = "surface_variant/0.25",
		radius = 8,
		border = "surface_variant/0.6",
		borderWidth = 1,
	}, {
		ui.row({ align = "center" }, {
			ui.label({ text = noctalia.tr("panel.history"), fontWeight = "bold", fontSize = 12, flexGrow = 1 }),
			ui.label({ text = "Last " .. tostring(HISTORY_DAYS) .. " days · " .. unit, fontSize = 11, color = "on_surface_variant" }),
		}),
		ui.row({ gap = 4, align = "end" }, bars),
	})
end

local function quotaRow(label, pct, resetIso)
	local reset = format.resetLabel(resetIso, os.time())
	local sub = format.pctText(pct) .. (reset ~= "" and (" · resets " .. reset) or "")
	local value = math.min(1, math.max(0, tonumber(pct) or 0))
	return ui.column({ gap = 4, align = "stretch" }, {
		ui.row({ align = "center" }, {
			ui.label({ text = label, fontSize = 12, color = "on_surface_variant", flexGrow = 1, maxLines = 1 }),
			ui.label({ text = format.pctText(pct), fontSize = 12, fontWeight = "bold", color = format.usageColor(pct) }),
		}),
		ui.progress({ progress = value, fill = format.usageColor(pct), track = "surface_variant", radius = 3, height = 6 }),
		ui.label({ text = sub, fontSize = 11, color = "on_surface_variant", visible = reset ~= "" }),
	})
end

local function providerCard(p)
	local rows = quotaRows(p)
	local body = {
		ui.row({ gap = 8, align = "center" }, {
			providerIcon(p, 18),
			ui.column({ gap = 0, flexGrow = 1 }, {
				ui.label({ text = p.name or "Provider", fontWeight = "bold", maxLines = 1 }),
				ui.label({ text = statusText(p), fontSize = 11, color = statusColor(p), maxLines = 1 }),
			}),
			ui.button({
				text = noctalia.tr("panel.open"),
				variant = "ghost",
				controlSize = "sm",
				onClick = function()
					tab = p.id
					render()
				end,
			}),
		}),
	}
	for _, r in ipairs(rows) do
		table.insert(body, quotaRow(r.label, r.pct, r.reset))
	end
	if (p.balanceValue or "") ~= "" then
		table.insert(body, ui.label({
			text = ((p.balanceLabel or "") ~= "" and (p.balanceLabel .. ": ") or "") .. p.balanceValue,
			fontSize = 11,
			color = "on_surface_variant",
			maxLines = 1,
		}))
	end
	return ui.column({
		key = "card-" .. tostring(p.id),
		gap = 6,
		padding = 12,
		fill = "surface_variant/0.45",
		radius = 10,
		align = "stretch",
	}, body)
end

local function detailView(p)
	if not p then
		return ui.label({ text = noctalia.tr("panel.empty"), color = "on_surface_variant" })
	end
	local nodes = {
		ui.row({ gap = 8, align = "center" }, {
			providerIcon(p, 22),
			ui.column({ gap = 0, flexGrow = 1 }, {
				ui.label({ text = p.name or "Provider", fontSize = 16, fontWeight = "bold" }),
				ui.label({ text = statusText(p), fontSize = 12, color = statusColor(p) }),
			}),
		}),
	}
	for _, r in ipairs(quotaRows(p)) do
		table.insert(nodes, quotaRow(r.label, r.pct, r.reset))
	end
	if (p.balanceValue or "") ~= "" or (p.tier or "") ~= "" then
		table.insert(nodes, ui.row({ gap = 16 }, {
			ui.label({
				text = ((p.balanceLabel or "") ~= "" and (p.balanceLabel .. ": ") or "") .. (p.balanceValue or "—"),
				fontSize = 12,
				visible = (p.balanceValue or "") ~= "",
			}),
			ui.label({ text = "Tier: " .. p.tier, fontSize = 12, visible = (p.tier or "") ~= "" }),
		}))
	end
	local resetCount = tonumber(p.resetCreditsAvailable) or -1
	if resetCount >= 0 then
		table.insert(nodes, ui.row({ align = "center" }, {
			ui.label({ text = "Reset credits", fontSize = 12, color = "on_surface_variant", flexGrow = 1 }),
			ui.label({ text = tostring(math.floor(resetCount)), fontSize = 12 }),
		}))
	end
	if (p.todayTokens or 0) > 0 or (p.todayPrompts or 0) > 0 then
		table.insert(nodes, ui.label({
			text = noctalia.tr("panel.today", {
				tokens = format.formatTokens(p.todayTokens or 0),
				prompts = tostring(p.todayPrompts or 0),
				sessions = tostring(p.todaySessions or 0),
			}),
			fontSize = 12,
			color = "on_surface_variant",
		}))
	end
	if type(p.details) == "table" and #p.details > 0 then
		local det = {}
		for _, d in ipairs(p.details) do
			table.insert(det, ui.row({ align = "center" }, {
				ui.label({ text = tostring(d.k or ""), fontSize = 12, color = "on_surface_variant", flexGrow = 1, maxLines = 1 }),
				ui.label({ text = tostring(d.v or ""), fontSize = 12, maxLines = 1 }),
			}))
		end
		table.insert(nodes, ui.column({ gap = 3, align = "stretch" }, det))
	end
	if type(p.topModels) == "table" and #p.topModels > 0 then
		local mrows = {
			ui.label({ text = noctalia.tr("panel.models"), fontWeight = "bold", fontSize = 12 }),
		}
		for _, m in ipairs(p.topModels) do
			table.insert(mrows, ui.row({ align = "center" }, {
				ui.label({ text = tostring(m.model or ""), fontSize = 12, flexGrow = 1, maxLines = 1 }),
				ui.label({ text = format.formatTokens(m.tokens or 0), fontSize = 12, color = "on_surface_variant" }),
			}))
		end
		table.insert(nodes, ui.column({ gap = 3, align = "stretch" }, mrows))
	end
	if hasHistoryData(p) then
		table.insert(nodes, historyChart(p))
	end
	return ui.column({ gap = 10, align = "stretch" }, nodes)
end

local function findProvider(id)
	for _, p in ipairs(snapshot.providers or {}) do
		if type(p) == "table" and p.id == id then
			return p
		end
	end
	return nil
end

render = function()
	dirty = false
	local providers = snapshot.providers or {}
	local tabs = {
		ui.button({
			key = "tab-dashboard",
			text = noctalia.tr("tabs.dashboard"),
			variant = tab == "dashboard" and "primary" or "ghost",
			controlSize = "sm",
			onClick = function()
				tab = "dashboard"
				render()
			end,
		}),
	}
	for _, p in ipairs(providers) do
		local id = p.id
		table.insert(tabs, ui.button({
			key = "tab-" .. tostring(id),
			text = tostring(p.name or id),
			variant = tab == id and "primary" or "ghost",
			controlSize = "sm",
			onClick = function()
				tab = id
				render()
			end,
		}))
	end

	local content
	if tab == "dashboard" then
		if #providers == 0 then
			content = ui.column({ gap = 8, padding = 24, align = "center" }, {
				ui.glyph({ name = "coffee", size = 36, color = "on_surface_variant" }),
				ui.label({ text = noctalia.tr("panel.no_providers"), color = "on_surface_variant", textAlign = "center" }),
				ui.button({ text = noctalia.tr("panel.configure"), variant = "outline", onClick = "onConfigure" }),
			})
		else
			local cards = {}
			for _, p in ipairs(providers) do
				table.insert(cards, providerCard(p))
			end
			content = ui.column({ gap = 8, align = "stretch" }, cards)
		end
	else
		content = detailView(findProvider(tab))
	end

	local notes = {}
	if feedback ~= "" then
		table.insert(notes, ui.label({ text = feedback, color = feedbackError and "error" or "tertiary", fontSize = 11 }))
	end
	local refreshing = snapshot.refreshing == true
	local refreshIndex = (refreshPhase % #REFRESH_GLYPHS) + 1
	noctalia.setUpdateInterval(refreshing and REFRESH_UPDATE_MS or IDLE_UPDATE_MS)

	panel.render(ui.column({ gap = 10, flexGrow = 1 }, {
		ui.row({ gap = 8, align = "center" }, {
			ui.glyph({ name = "coffee", size = 22, color = "primary" }),
			ui.column({ gap = 0, flexGrow = 1 }, {
				ui.label({ text = noctalia.tr("title"), fontSize = 18, fontWeight = "bold" }),
				ui.label({
					text = #providers == 0 and noctalia.tr("panel.subtitle_empty")
						or noctalia.tr("panel.subtitle", {
							ready = schema.readyCount(providers),
							total = #providers,
						}),
					fontSize = 11,
					color = "on_surface_variant",
				}),
			}),
			ui.button({
				key = "refresh",
				glyph = refreshing and REFRESH_GLYPHS[refreshIndex] or "refresh",
				variant = "ghost",
				enabled = not refreshing,
				opacity = refreshing and REFRESH_OPACITY[refreshIndex] or 1,
				tooltip = refreshing and noctalia.tr("panel.refreshing") or noctalia.tr("panel.refresh"),
				onClick = "onRefresh",
			}),
			ui.button({
				key = "settings",
				glyph = "settings",
				variant = "ghost",
				tooltip = noctalia.tr("panel.configure"),
				onClick = "onConfigure",
			}),
			ui.button({ glyph = "close", variant = "ghost", onClick = "onClose" }),
		}),
		ui.row({ gap = 4, align = "center" }, tabs),
		ui.column({ gap = 2, align = "stretch", visible = feedback ~= "" }, notes),
		ui.scroll({ key = "scroll-" .. tab, flexGrow = 1, gap = 8, align = "stretch" }, { content }),
		ui.row({ align = "center" }, {
			ui.spacer({ flexGrow = 1 }),
			ui.label({
				text = (tonumber(snapshot.updatedAt) or 0) > 0
						and noctalia.tr("panel.updated", { time = noctalia.formatTime("%H:%M:%S", snapshot.updatedAt) })
					or "",
				color = "on_surface_variant",
				fontSize = 11,
				textAlign = "end",
			}),
		}),
	}))
end

noctalia.state.watch(STATE_KEY, function(value)
	if type(value) ~= "table" then
		return
	end
	snapshot = value
	if tab ~= "dashboard" and not findProvider(tab) then
		tab = "dashboard"
	end
	dirty = true
end)

noctalia.state.watch(RESULT_KEY, function(result)
	if type(result) ~= "table" then
		return
	end
	if type(result.requestId) ~= "string" or not result.requestId:match("^panel%-") then
		return
	end
	if result.ok == true then
		-- Refresh progress is represented by the header control, not a status label.
		feedback = ""
		feedbackError = false
	else
		feedback = tostring(result.message or "")
		feedbackError = true
	end
	dirty = true
end)

function onOpen(_context)
	feedback = ""
	send("refresh")
	render()
end

function update()
	if snapshot.refreshing == true then
		refreshPhase = (refreshPhase + 1) % #REFRESH_GLYPHS
		dirty = true
	end
	if dirty then
		render()
	end
end

function onClose()
	panel.close()
end

function onRefresh()
	send("refresh")
end

function onConfigure()
	noctalia.openSettings()
end
