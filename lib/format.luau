--!nonstrict
-- Shared formatting helpers for Token Cafe.
-- Pure functions, no noctalia.* calls, safe to require from any entry.

local M = {}

function M.formatTokens(n)
	n = tonumber(n) or 0
	if n >= 1e9 then
		return string.format("%.1fB", n / 1e9)
	elseif n >= 1e6 then
		return string.format("%.1fM", n / 1e6)
	elseif n >= 1e3 then
		return string.format("%.1fK", n / 1e3)
	end
	return tostring(math.floor(n))
end

function M.pctText(p)
	p = tonumber(p) or -1
	if p < 0 then
		return "—"
	end
	return tostring(math.floor(p * 100 + 0.5)) .. "%"
end

-- Theme-aware usage color token / hex.
function M.usageColor(p)
	p = tonumber(p) or -1
	if p > 0.8 then
		return "error"
	elseif p >= 0.6 then
		return "#EAB308"
	end
	return "primary"
end

-- Days since civil 1970-01-01 (Howard Hinnant's algorithm).
-- Pure integer math in double precision: exact for real-world dates.
local function daysFromCivil(y, m, d)
	y = (m <= 2) and (y - 1) or y
	local era = math.floor(((y >= 0) and y or (y - 399)) / 400)
	local yoe = y - era * 400
	local mp = (m > 2) and (m - 3) or (m + 9)
	local doy = math.floor((153 * mp + 2) / 5) + d - 1
	local doe = yoe * 365 + math.floor(yoe / 4) - math.floor(yoe / 100) + doy
	return era * 146097 + doe - 719468
end

-- Parse an ISO-8601 timestamp to epoch seconds.
-- Trailing Z / numeric offsets are honored with DST-safe civil math;
-- naive stamps (no zone) are read as local time. Returns 0 when unparseable.
function M.isoToEpoch(s)
	if type(s) ~= "string" or s == "" then
		return 0
	end
	-- Split off any trailing timezone: Z or ±HH:MM / ±HHMM.
	-- (Lua patterns have no alternation, so test the tail explicitly.)
	local body, tz = s, ""
	local last = s:sub(-1)
	if last == "Z" or last == "z" then
		body, tz = s:sub(1, -2), "Z"
	else
		local off = s:match("([+-]%d%d:?%d%d)$")
		if off then
			body, tz = s:sub(1, -(#off + 1)), off
		end
	end
	local y, mo, d, h, mi, sec = body:match("^(%d+)-(%d+)-(%d+)[T ](%d+):(%d+):?(%d*)")
	if not y then
		return 0
	end
	y, mo, d = tonumber(y) or 0, tonumber(mo) or 0, tonumber(d) or 0
	h, mi, sec = tonumber(h) or 0, tonumber(mi) or 0, tonumber(sec) or 0
	if tz == "" then
		local ok, epoch = pcall(os.time, { year = y, month = mo, day = d, hour = h, min = mi, sec = sec })
		if not ok or type(epoch) ~= "number" then
			return 0
		end
		return epoch
	end
	local epoch = daysFromCivil(y, mo, d) * 86400 + h * 3600 + mi * 60 + sec
	if tz ~= "Z" and tz ~= "z" then
		local sign, oh, om = tz:match("^([+-])(%d%d):?(%d%d)$")
		if sign then
			local off = (tonumber(oh) or 0) * 3600 + (tonumber(om) or 0) * 60
			if sign == "+" then
				epoch = epoch - off
			else
				epoch = epoch + off
			end
		end
	end
	return epoch
end

-- "5h 12m" / "3d 2h" / "42m" / "now" / "" relative label.
function M.resetLabel(iso, nowSec)
	if type(iso) ~= "string" or iso == "" then
		return ""
	end
	nowSec = tonumber(nowSec) or os.time()
	local epoch = M.isoToEpoch(iso)
	if epoch <= 0 then
		return ""
	end
	local diff = epoch - nowSec
	if diff <= 0 then
		return "now"
	end
	local hours = math.floor(diff / 3600)
	local mins = math.floor((diff % 3600) / 60)
	if hours >= 24 then
		return math.floor(hours / 24) .. "d " .. (hours % 24) .. "h"
	elseif hours > 0 then
		return hours .. "h " .. mins .. "m"
	end
	return mins .. "m"
end

function M.trim(s)
	return noctalia.string.trim(tostring(s or ""))
end

return M
