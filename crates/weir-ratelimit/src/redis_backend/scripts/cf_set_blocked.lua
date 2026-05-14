-- KEYS[1] = cf blocked_until key
-- ARGV[1] = retry_after_ms, ARGV[2] = ttl_grace_ms
-- returns: {new_blocked_until_ms, server_now_ms}

local t = redis.call('TIME')
local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

local existing = tonumber(redis.call('GET', KEYS[1])) or 0
local new_bu = now + tonumber(ARGV[1])
if new_bu < existing then new_bu = existing end

redis.call('SET', KEYS[1], new_bu, 'PX', (new_bu - now) + tonumber(ARGV[2]))
return {new_bu, now}
