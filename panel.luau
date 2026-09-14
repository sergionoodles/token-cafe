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

local function quotaRow(label, pct, resetIso)
	local reset = format.resetLabel(resetIso, os.time())
	local sub = format.pctText(pct) .. (reset ~= "" and (" · resets " .. reset) or "")
	return ui.column({ gap = 2, align = "stretch" }, {
		ui.row({ align = "center" }, {
			ui.label({ text = label, fontSize = 12, color = "on_surface_variant", flexGrow = 1, maxLines = 1 }),
			ui.label({ text = format.pctText(pct), fontSize = 12, fontWeight = "bold", color = format.usageColor(pct) }),
		}),
		ui.label({ text = sub, fontSize = 11, color = "on_surface_variant", visible = reset ~= "" }),
		ui.progress({ progress = math.min(1, math.max(0, tonumber(pct) or 0)), fill = format.usageColor(pct) }),
	})
end

local function providerCard(p)
	local rows = quotaRows(p)
	local body = {
		ui.row({ gap = 8, align = "center" }, {
			ui.glyph({ name = p.glyph or "cpu", size = 18, color = statusColor(p) }),
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
			ui.glyph({ name = p.glyph or "cpu", size = 22, color = statusColor(p) }),
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
	if type(p.recentDays) == "table" and #p.recentDays > 0 then
		local days = {}
		for _, d in ipairs(p.recentDays) do
			table.insert(days, tonumber(d.tokens) or 0)
		end
		local max = 1
		for _, v in ipairs(days) do
			if v > max then
				max = v
			end
		end
		local norm = {}
		for _, v in ipairs(days) do
			table.insert(norm, v / max)
		end
		table.insert(nodes, ui.column({ gap = 4, align = "stretch" }, {
			ui.label({ text = noctalia.tr("panel.history"), fontWeight = "bold", fontSize = 12 }),
			ui.graph({ values = norm, width = 480, height = 64 }),
		}))
	end
	if p.ready ~= true then
		table.insert(nodes, ui.label({
			text = (p.error and p.error ~= "") and p.error or noctalia.tr("panel.empty"),
			color = "error",
			fontSize = 12,
		}))
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
	if snapshot.refreshing == true then
		table.insert(notes, ui.label({ text = noctalia.tr("panel.refreshing"), color = "primary", fontSize = 11 }))
	end
	if feedback ~= "" then
		table.insert(notes, ui.label({ text = feedback, color = feedbackError and "error" or "tertiary", fontSize = 11 }))
	end

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
			ui.button({ glyph = "refresh", variant = "ghost", onClick = "onRefresh" }),
			ui.button({ glyph = "close", variant = "ghost", onClick = "onClose" }),
		}),
		ui.row({ gap = 4, align = "center" }, tabs),
		ui.column({ gap = 2, align = "stretch" }, notes),
		ui.scroll({ key = "scroll-" .. tab, flexGrow = 1, gap = 8, align = "stretch" }, { content }),
		ui.label({
			text = (tonumber(snapshot.updatedAt) or 0) > 0
					and noctalia.tr("panel.updated", { time = noctalia.formatTime("%H:%M:%S", snapshot.updatedAt) })
				or "",
			color = "on_surface_variant",
			fontSize = 11,
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
	feedback = tostring(result.message or "")
	feedbackError = result.ok ~= true
	dirty = true
end)

function onOpen(_context)
	feedback = ""
	send("refresh")
	render()
end

function update()
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
