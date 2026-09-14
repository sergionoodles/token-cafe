--!nonstrict
-- One-shot runner for the bundled Rust probe helper (`helper/` crate,
-- binary `tc-probe`). Replaces the old Python sidecar entirely.

local schema = require("./schema.luau")

local M = {}

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

function M.isEnabled(provider)
	return cfg(provider .. "_enabled", false) == true
end

function M.cliBin(provider, fallback)
	local v = trim(cfg(provider .. "_bin", fallback))
	if v == "" then
		return fallback
	end
	return v
end

function M.probeTimeoutSec()
	return tonumber(cfg("probe_timeout_sec", 25)) or 25
end

-- Resolve the helper binary: explicit `probe_bin` override, else the
-- release build, else a debug build (dev iteration).
function M.probeBin()
	local override = trim(cfg("probe_bin", ""))
	if override ~= "" then
		return override
	end
	local dir = noctalia.pluginDir() or ""
	local release = dir .. "/helper/target/release/tc-probe"
	if noctalia.fileExists(release) then
		return release
	end
	return dir .. "/helper/target/debug/tc-probe"
end

local function missingHelper(provider)
	local p = schema.blank(provider)
	p.error = "Probe helper missing — build it: cargo build --release (in helper/)"
	return p
end

-- Run `tc-probe <provider>` and normalize the JSON it prints.
function M.runCliProbe(provider, done)
	local bin = M.probeBin()
	if not noctalia.fileExists(bin) then
		done(missingHelper(provider))
		return
	end
	local timeout = tostring(M.probeTimeoutSec())
	local accepted = noctalia.runAsync(
		{ bin, provider, "--cli-bin", M.cliBin(provider, provider), "--timeout", timeout },
		function(result)
			local ok, data = pcall(function()
				return noctalia.json.decode((result and result.stdout) or "")
			end)
			if not ok or type(data) ~= "table" then
				local p = schema.blank(provider)
				p.error = "Probe failed"
				done(p)
				return
			end
			done(schema.fromProbe(provider, data))
		end,
		M.probeTimeoutSec() * 1000 + 5000
	)
	if not accepted then
		local p = schema.blank(provider)
		p.error = "Probe could not start"
		done(p)
	end
end

function M.runAntigravityProbe(done)
	local bin = M.probeBin()
	if not noctalia.fileExists(bin) then
		done(missingHelper("antigravity"))
		return
	end
	local db = trim(cfg("antigravity_db", "~/.config/Antigravity/User/globalStorage/state.vscdb"))
	if db == "" then
		db = "~/.config/Antigravity/User/globalStorage/state.vscdb"
	end
	local args = { bin, "antigravity", "--state-db", db, "--timeout", tostring(M.probeTimeoutSec()) }
	local pid = trim(cfg("antigravity_project", ""))
	if pid ~= "" then
		table.insert(args, "--project-id")
		table.insert(args, pid)
	end
	local accepted = noctalia.runAsync(args, function(result)
		local ok, data = pcall(function()
			return noctalia.json.decode((result and result.stdout) or "")
		end)
		if not ok or type(data) ~= "table" then
			local p = schema.blank("antigravity")
			p.error = "Probe failed"
			done(p)
			return
		end
		done(schema.fromProbe("antigravity", data))
	end, M.probeTimeoutSec() * 1000 + 5000)
	if not accepted then
		local p = schema.blank("antigravity")
		p.error = "Probe could not start"
		done(p)
	end
end

return M
