-- KEYS[1] = health hash key
-- ARGV[1] = threshold, ARGV[2] = cooldown_ms, ARGV[3] = ttl_grace_ms
-- returns: 1 if this call flipped disabled false -> true, else 0

local t = redis.call('TIME')
local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

local streak = redis.call('HINCRBY', KEYS[1], 'error_streak', 1)
local disabled_at = tonumber(redis.call('HGET', KEYS[1], 'disabled_at')) or 0
local threshold = tonumber(ARGV[1])
if threshold < 1 then threshold = 1 end
local cooldown = tonumber(ARGV[2])

redis.call('PEXPIRE', KEYS[1], cooldown + tonumber(ARGV[3]))

if streak >= threshold and disabled_at == 0 then
  redis.call('HSET', KEYS[1], 'disabled_at', now)
  return 1
end
return 0
