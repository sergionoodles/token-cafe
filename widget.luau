--!nonstrict
-- Token Cafe bar widget: compact multi-provider glance.
-- Shows up to `bar_provider_limit` ready meters + overflow count.

local PANEL_ID = "codingnoodles/token-cafe:panel"
local STATE_KEY = "tc_snapshot"
local COMMAND_KEY = "tc_command"

local format = require("./lib/format.luau")
local schema = require("./lib/schema.luau")

local snapshot = noctalia.state.get(STATE_KEY) or { providers = {}, refreshing = false }
local requestId = 0

local function limit()
	local n = tonumber(noctalia.getConfig("bar_provider_limit")) or 2
	return math.max(1, math.min(4, math.floor(n)))
end

local function statusColor(p)
	if p.ready ~= true then
		return "on_surface_variant"
	end
	return format.usageColor(tonumber(p.primaryPct) or -1)
end

local function render()
	local providers = (type(snapshot) == "table" and snapshot.providers) or {}
	local ready = {}
	for _, p in ipairs(providers) do
		if type(p) == "table" and p.ready == true and (tonumber(p.primaryPct) or -1) >= 0 then
			table.insert(ready, p)
		end
	end
	table.sort(ready, function(a, b)
		return (tonumber(a.primaryPct) or -1) > (tonumber(b.primaryPct) or -1)
	end)

	local children = {
		ui.glyph({ name = "coffee", size = 15 }),
	}
	local shown = limit()
	for i = 1, math.min(shown, #ready) do
		local p = ready[i]
		table.insert(children, ui.label({
			text = format.pctText(p.primaryPct),
			fontWeight = "bold",
			color = statusColor(p),
		}))
		table.insert(children, ui.box({
			width = 7,
			height = 7,
			radius = 4,
			fill = statusColor(p),
		}))
	end
	if #ready > shown then
		table.insert(children, ui.label({
			text = "+" .. tostring(#ready - shown),
			color = "on_surface_variant",
		}))
	end
	if #providers == 0 then
		table.insert(children, ui.label({ text = "—", color = "on_surface_variant" }))
	elseif #ready == 0 then
		local n = schema.readyCount(providers)
		table.insert(children, ui.label({
			text = snapshot.refreshing == true and "…" or (n .. "/" .. #providers),
			color = "on_surface_variant",
		}))
	end

	local container = barWidget.isVertical() and ui.column or ui.row
	barWidget.render(container({ gap = 5, align = "center" }, children))

	-- Tooltip: one line per provider.
	if #providers == 0 then
		barWidget.setTooltip(noctalia.tr("widget.tooltip_empty"))
		return
	end
	local lines = {}
	for _, p in ipairs(providers) do
		if p.ready == true then
			local pct = (tonumber(p.primaryPct) or -1) >= 0 and (" " .. format.pctText(p.primaryPct)) or ""
			local bal = (p.balanceValue or "") ~= "" and (" · " .. p.balanceValue) or ""
			table.insert(lines, p.name .. pct .. bal)
		else
			table.insert(lines, p.name .. " · " .. ((p.error or "") ~= "" and p.error or "unavailable"))
		end
	end
	barWidget.setTooltip(table.concat(lines, "\n"))
end

noctalia.state.watch(STATE_KEY, function(value)
	if type(value) == "table" then
		snapshot = value
		render()
	end
end)

noctalia.setUpdateInterval(15000)
render()

function update()
	render()
end

function onConfigChanged()
	render()
end

function onClick()
	noctalia.togglePanel(PANEL_ID)
end

function onRightClick()
	requestId += 1
	noctalia.state.set(COMMAND_KEY, { action = "refresh", requestId = "widget-" .. tostring(requestId) })
	noctalia.notify(noctalia.tr("title"), noctalia.tr("widget.refresh_requested"))
end
