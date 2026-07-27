-- KEYS[1] = bucket key, KEYS[2] = route-map key
-- ARGV[1] = remaining, ARGV[2] = reset_after_ms, ARGV[3] = limit
-- ARGV[4] = ttl_grace_ms
-- ARGV[5] = bucket_hash_string, ARGV[6] = route_ttl_ms
-- Last-write-wins on bucket state, matching the memory backend.

redis.call('SET', KEYS[2], ARGV[5], 'PX', tonumber(ARGV[6]))

local t = redis.call('TIME')
local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
local reset_after = tonumber(ARGV[2])
local rst = now + reset_after

local lim = tonumber(ARGV[3])
if lim <= 0 then
  lim = tonumber(redis.call('HGET', KEYS[1], 'limit')) or 1
end

local pexpire_ms = reset_after + tonumber(ARGV[4])
if pexpire_ms < 1000 then pexpire_ms = 1000 end

redis.call('HSET', KEYS[1],
  'remaining', tonumber(ARGV[1]),
  'reset_at',  rst,
  'limit',     lim)
redis.call('PEXPIRE', KEYS[1], pexpire_ms)
return 1
