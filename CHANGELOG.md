## [0.1.10](https://github.com/Xavinlol/weir/compare/v0.1.9...v0.1.10) (2026-04-18)


### Bug Fixes

* **ratelimit:** prevent bucket refill from clobbering concurrent update ([#13](https://github.com/Xavinlol/weir/issues/13)) ([39eccda](https://github.com/Xavinlol/weir/commit/39eccda0dcfc34e9512e2a1cfb8a5aaf99a98e8a))



## [0.1.9](https://github.com/Xavinlol/weir/compare/v0.1.8...v0.1.9) (2026-04-18)


### Bug Fixes

* **ratelimit:** always try acquire after queue wait ([#12](https://github.com/Xavinlol/weir/issues/12)) ([f68680c](https://github.com/Xavinlol/weir/commit/f68680cad26e2c6f0343721ec6dee25ca9d3cbb4))



## [0.1.8](https://github.com/Xavinlol/weir/compare/v0.1.7...v0.1.8) (2026-04-18)


### Bug Fixes

* **ratelimit:** webhook health cleanup and README fixes ([072e2a1](https://github.com/Xavinlol/weir/commit/072e2a132543618c0754ee7d3521d36c174761e2))



## [0.1.7](https://github.com/Xavinlol/weir/compare/v0.1.6...v0.1.7) (2026-04-18)


### Performance Improvements

* **proxy:** eliminate redundant string allocations in metrics recording ([36aa38d](https://github.com/Xavinlol/weir/commit/36aa38d017116eb7b4491c231ab83f8196422626))
* **proxy:** use Cow for method and status labels to avoid allocations ([8a3c87b](https://github.com/Xavinlol/weir/commit/8a3c87b913a2b8af4841a28f911f1d06a7fc5376))



## [0.1.6](https://github.com/Xavinlol/weir/compare/v0.1.5...v0.1.6) (2026-04-18)


### Performance Improvements

* **proxy:** stream request and response bodies instead of buffering ([#8](https://github.com/Xavinlol/weir/issues/8)) ([6ea04d8](https://github.com/Xavinlol/weir/commit/6ea04d82716499739cfd123f318312b5d0bc413f))



