-- KEYS[1] = invalid count key
-- ARGV[1] = window_ms
-- returns: new count after increment

local count = redis.call('INCR', KEYS[1])
if count == 1 then
  redis.call('PEXPIRE', KEYS[1], tonumber(ARGV[1]))
end
return count
