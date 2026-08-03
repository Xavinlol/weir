## [0.2.1](https://github.com/Xavinlol/weir/compare/v0.2.0...v0.2.1) (2026-08-03)


### Bug Fixes

* **proxy:** add via header to proxy-generated 429s ([1433a1b](https://github.com/Xavinlol/weir/commit/1433a1b23985f4f3aecececd6d8ed9143d5f372a))
* **ratelimit:** scope webhook buckets per token ([936e5a2](https://github.com/Xavinlol/weir/commit/936e5a2eacca3485b01e9155eba1d76eceb6d4c6))
* **ratelimit:** stop throttling interaction endpoints ([08c05f9](https://github.com/Xavinlol/weir/commit/08c05f98cbd0297b9a3addcf4cc2f944496028c6))



# [0.2.0](https://github.com/Xavinlol/weir/compare/v0.1.12...v0.2.0) (2026-07-27)


### Features

* **ratelimit:** redis backed distributed mode ([aeef5f6](https://github.com/Xavinlol/weir/commit/aeef5f68e072af0302d18579f640a10d092c040f))



## [0.1.12](https://github.com/Xavinlol/weir/compare/v0.1.11...v0.1.12) (2026-04-22)


### Bug Fixes

* **ratelimit:** self-heal drained bucket when no update arrives ([#15](https://github.com/Xavinlol/weir/issues/15)) ([b0a84a6](https://github.com/Xavinlol/weir/commit/b0a84a676ae03d349f32c613fa19bf1298b26aea))



## [0.1.11](https://github.com/Xavinlol/weir/compare/v0.1.10...v0.1.11) (2026-04-19)


### Bug Fixes

* **ratelimit:** schedule wake for drained buckets with known reset ([#14](https://github.com/Xavinlol/weir/issues/14)) ([886b2b3](https://github.com/Xavinlol/weir/commit/886b2b3318deba28d3cc62c0bb54ba6ba0d674f9))



## [0.1.10](https://github.com/Xavinlol/weir/compare/v0.1.9...v0.1.10) (2026-04-18)


### Bug Fixes

* **ratelimit:** prevent bucket refill from clobbering concurrent update ([#13](https://github.com/Xavinlol/weir/issues/13)) ([39eccda](https://github.com/Xavinlol/weir/commit/39eccda0dcfc34e9512e2a1cfb8a5aaf99a98e8a))



