-- KEYS[1] = health hash key
-- ARGV[1] = cooldown_ms, ARGV[2] = ttl_grace_ms
-- No-op if the hash doesn't exist (parity with WebhookHealth::report_success).
-- Reset error_streak only; cooldown self-clears via health_read.

if redis.call('EXISTS', KEYS[1]) == 0 then return 0 end
redis.call('HSET', KEYS[1], 'error_streak', 0)
redis.call('PEXPIRE', KEYS[1], tonumber(ARGV[1]) + tonumber(ARGV[2]))
return 1
