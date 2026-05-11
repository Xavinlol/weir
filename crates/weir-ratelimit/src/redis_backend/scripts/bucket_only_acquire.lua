-- KEYS[1] = bucket key
-- ARGV[1] = bucket_refill_fallback_ms, ARGV[2] = ttl_grace_ms
-- returns: {allowed:0|1, retry_after_ms}

local t   = redis.call('TIME')
local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

local b     = redis.call('HMGET', KEYS[1], 'remaining', 'reset_at', 'limit')
local rem   = tonumber(b[1])
if rem == nil then return {1, 0} end

local b_rst = tonumber(b[2]) or 0
local b_lim = tonumber(b[3]) or 0
if b_lim <= 0 then
  redis.log(redis.LOG_WARNING, 'weir: bucket key ' .. KEYS[1] .. ' has invalid limit, coercing to 1')
  b_lim = 1
end

local b_rolls = (now >= b_rst)
if not b_rolls and rem <= 0 then
  return {0, b_rst - now}
end

if b_rolls then
  b_rst = now + tonumber(ARGV[1])
  redis.call('HSET', KEYS[1],
    'remaining', b_lim - 1,
    'reset_at',  b_rst,
    'limit',     b_lim)
  redis.call('PEXPIRE', KEYS[1], (b_rst - now) + tonumber(ARGV[2]))
else
  redis.call('HINCRBY', KEYS[1], 'remaining', -1)
  redis.call('PEXPIRE', KEYS[1], (b_rst - now) + tonumber(ARGV[2]))
end

return {1, 0}
