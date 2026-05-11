-- KEYS[1] = global key
-- ARGV[1] = retry_after_ms, ARGV[2] = ttl_grace_ms
local t = redis.call('TIME')
local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

local existing = tonumber(redis.call('HGET', KEYS[1], 'blocked_until')) or 0
local new_bu = now + tonumber(ARGV[1])
if new_bu < existing then new_bu = existing end

redis.call('HSET', KEYS[1], 'blocked_until', new_bu)
redis.call('PEXPIRE', KEYS[1], (new_bu - now) + tonumber(ARGV[2]))
return 1
