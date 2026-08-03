-- KEYS[1] = global key, KEYS[2] = bucket key
-- ARGV[1] = global_limit, ARGV[2] = global_window_ms
-- ARGV[3] = bucket_refill_fallback_ms, ARGV[4] = ttl_grace_ms
-- ARGV[5] = global_mode: 1 = check and consume a slot, 2 = ban check only
-- returns: {allowed:0|1, retry_after_ms, reason:0 = none, 1 = global, 2 = bucket}

local t   = redis.call('TIME')
local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)

local g_mode = tonumber(ARGV[5])

if g_mode == 1 then
  local g     = redis.call('HMGET', KEYS[1], 'window_start', 'count', 'blocked_until')
  local ws    = tonumber(g[1]) or 0
  local ct    = tonumber(g[2]) or 0
  local bu    = tonumber(g[3]) or 0
  local g_lim = tonumber(ARGV[1])
  local g_win = tonumber(ARGV[2])

  if g_lim < 1 then return {0, g_win, 1} end
  if now < bu then return {0, bu - now, 1} end

  local g_rolls = (now - ws >= g_win)
  if not g_rolls and ct >= g_lim then
    return {0, ws + g_win - now, 1}
  end

  -- Global passes. Commit the slot now so bucket-denied attempts still count
  -- against the per-token window (matches memory backend ordering).
  if g_rolls then
    redis.call('HSET', KEYS[1], 'window_start', now, 'count', 1)
    redis.call('PEXPIRE', KEYS[1], g_win + tonumber(ARGV[4]))
  else
    redis.call('HINCRBY', KEYS[1], 'count', 1)
  end
elseif g_mode == 2 then
  -- Retry after a bucket wait: the slot is already committed, so only a hard
  -- ban can still deny.
  local bu = tonumber(redis.call('HGET', KEYS[1], 'blocked_until')) or 0
  if now < bu then return {0, bu - now, 1} end
end

local b     = redis.call('HMGET', KEYS[2], 'remaining', 'reset_at', 'limit')
local rem   = tonumber(b[1])
if rem == nil then return {1, 0, 0} end

local b_rst = tonumber(b[2]) or 0
local b_lim = tonumber(b[3]) or 0
if b_lim < 1 then b_lim = 1 end

local b_rolls = (now >= b_rst)
if not b_rolls and rem <= 0 then
  return {0, b_rst - now, 2}
end

if b_rolls then
  b_rst = now + tonumber(ARGV[3])
  redis.call('HSET', KEYS[2],
    'remaining', b_lim - 1,
    'reset_at',  b_rst,
    'limit',     b_lim)
else
  redis.call('HINCRBY', KEYS[2], 'remaining', -1)
end
redis.call('PEXPIRE', KEYS[2], (b_rst - now) + tonumber(ARGV[4]))

return {1, 0, 0}
