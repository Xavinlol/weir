-- KEYS[1] = global key, KEYS[2] = bucket key
-- ARGV[1] = global_limit, ARGV[2] = global_window_ms
-- ARGV[3] = bucket_refill_fallback_ms, ARGV[4] = ttl_grace_ms
-- returns: {allowed:0|1, retry_after_ms}

local t   = redis.call('TIME')
local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

local g     = redis.call('HMGET', KEYS[1], 'window_start', 'count', 'blocked_until')
local ws    = tonumber(g[1]) or 0
local ct    = tonumber(g[2]) or 0
local bu    = tonumber(g[3]) or 0
local g_lim = tonumber(ARGV[1])
local g_win = tonumber(ARGV[2])

if g_lim < 1 then return {0, g_win} end
if now < bu then return {0, bu - now} end

local g_rolls = (now - ws >= g_win)
if not g_rolls and ct >= g_lim then
  return {0, ws + g_win - now}
end

-- Global passes. Commit the global slot now so bucket-denied attempts still
-- count against the per-token window (matches memory backend ordering).
if g_rolls then
  redis.call('HSET', KEYS[1], 'window_start', now, 'count', 1)
  redis.call('PEXPIRE', KEYS[1], g_win + tonumber(ARGV[4]))
else
  redis.call('HINCRBY', KEYS[1], 'count', 1)
end

local b     = redis.call('HMGET', KEYS[2], 'remaining', 'reset_at', 'limit')
local rem   = tonumber(b[1])
if rem == nil then return {1, 0} end

local b_rst = tonumber(b[2]) or 0
local b_lim = tonumber(b[3]) or 0
if b_lim <= 0 then
  redis.log(redis.LOG_WARNING, 'weir: bucket key ' .. KEYS[2] .. ' has invalid limit, coercing to 1')
  b_lim = 1
end

local b_rolls = (now >= b_rst)
if not b_rolls and rem <= 0 then
  return {0, b_rst - now}
end

if b_rolls then
  b_rst = now + tonumber(ARGV[3])
  redis.call('HSET', KEYS[2],
    'remaining', b_lim - 1,
    'reset_at',  b_rst,
    'limit',     b_lim)
  redis.call('PEXPIRE', KEYS[2], (b_rst - now) + tonumber(ARGV[4]))
else
  redis.call('HINCRBY', KEYS[2], 'remaining', -1)
  redis.call('PEXPIRE', KEYS[2], (b_rst - now) + tonumber(ARGV[4]))
end

return {1, 0}
