-- KEYS[1] = health hash key
-- ARGV[1] = cooldown_ms
-- returns: {disabled_at_ms, server_now_ms}
--   if disabled_at > 0 AND now < disabled_at + cooldown, token is disabled.
--   cooldown expiry is auto-cleared so the next read sees disabled_at = 0.

local t = redis.call('TIME')
local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

local disabled_at = tonumber(redis.call('HGET', KEYS[1], 'disabled_at')) or 0
if disabled_at > 0 and now - disabled_at >= tonumber(ARGV[1]) then
  redis.call('HSET', KEYS[1], 'disabled_at', 0, 'error_streak', 0)
  disabled_at = 0
end

return {disabled_at, now}
