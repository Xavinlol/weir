-- KEYS[1] = cf blocked_until key
-- returns: {blocked_until_ms, server_now_ms}

local t = redis.call('TIME')
local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
local bu = tonumber(redis.call('GET', KEYS[1])) or 0
return {bu, now}
