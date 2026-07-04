# Changelog

## [0.20.4](https://github.com/teh-hippo/rs-suno/compare/v0.20.3...v0.20.4) (2026-07-04)


### Performance Improvements

* fetch each clip's cover once, not twice ([#89](https://github.com/teh-hippo/rs-suno/issues/89)) ([f975e8d](https://github.com/teh-hippo/rs-suno/commit/f975e8d32fd9956a791a2a6839654baf5f4fe3e0))

## [0.20.3](https://github.com/teh-hippo/rs-suno/compare/v0.20.2...v0.20.3) (2026-07-04)


### Bug Fixes

* preserve the [id8] disambiguator when a file name is truncated ([#120](https://github.com/teh-hippo/rs-suno/issues/120)) ([26f2713](https://github.com/teh-hippo/rs-suno/commit/26f27138f5b0e4fc7e470f175039350008274756))

## [0.20.2](https://github.com/teh-hippo/rs-suno/compare/v0.20.1...v0.20.2) (2026-07-04)


### Performance Improvements

* add opt-level = "s" to the release profile ([d5a8528](https://github.com/teh-hippo/rs-suno/commit/d5a8528a6a3353e100be8560358cee18f487c254)), closes [#115](https://github.com/teh-hippo/rs-suno/issues/115)

## [0.20.1](https://github.com/teh-hippo/rs-suno/compare/v0.20.0...v0.20.1) (2026-07-03)


### Performance Improvements

* shrink the release binary with fat LTO, one codegen unit, and strip ([da5a341](https://github.com/teh-hippo/rs-suno/commit/da5a34116edd2242a6ac46f3aa05d5ff287ba3dc))

## [0.20.0](https://github.com/teh-hippo/rs-suno/compare/v0.19.0...v0.20.0) (2026-07-03)


### Features

* mirror existing Suno stems (download-only) ([#100](https://github.com/teh-hippo/rs-suno/issues/100)) ([f3c08e9](https://github.com/teh-hippo/rs-suno/commit/f3c08e9c6196101dde7b7d42cd62aea67eaf100f))

## [0.19.0](https://github.com/teh-hippo/rs-suno/compare/v0.18.0...v0.19.0) (2026-07-03)


### Features

* set the Year metadata tag to the lineage root's creation year ([2d9c604](https://github.com/teh-hippo/rs-suno/commit/2d9c6044fb4a2bc93e259274254f67154527f397))

## [0.18.0](https://github.com/teh-hippo/rs-suno/compare/v0.17.1...v0.18.0) (2026-07-03)


### Features

* synced (timed) lyrics — line-level .lrc sidecar and MP3 SYLT ([#48](https://github.com/teh-hippo/rs-suno/issues/48)) ([c3fd0eb](https://github.com/teh-hippo/rs-suno/commit/c3fd0ebd11af19c06348f9b21a89c800799b4e2d))

## [0.17.1](https://github.com/teh-hippo/rs-suno/compare/v0.17.0...v0.17.1) (2026-07-03)


### Bug Fixes

* count sidecar artifact writes in dry-run and sync summary ([6cebf60](https://github.com/teh-hippo/rs-suno/commit/6cebf6011be3c8d9a56acf8691f085007a72f858)), closes [#105](https://github.com/teh-hippo/rs-suno/issues/105)

## [0.17.0](https://github.com/teh-hippo/rs-suno/compare/v0.16.1...v0.17.0) (2026-07-03)


### Features

* manual album-name overrides in config ([#92](https://github.com/teh-hippo/rs-suno/issues/92)) ([bf71472](https://github.com/teh-hippo/rs-suno/commit/bf7147230a86d5a004a1b37e7ac02281ccd26f72))

## [0.16.1](https://github.com/teh-hippo/rs-suno/compare/v0.16.0...v0.16.1) (2026-07-02)


### Bug Fixes

* stop animated-cover transcode timing out ([#86](https://github.com/teh-hippo/rs-suno/issues/86)) ([fdfdee1](https://github.com/teh-hippo/rs-suno/commit/fdfdee1d5a5e2516c0cbc47247dfaa8fb1b2468d))

## [0.16.0](https://github.com/teh-hippo/rs-suno/compare/v0.15.1...v0.16.0) (2026-07-02)


### Features

* parallelise the executor with bounded download concurrency ([#72](https://github.com/teh-hippo/rs-suno/issues/72)) ([42bcd2c](https://github.com/teh-hippo/rs-suno/commit/42bcd2c5373cf2dfd96ce4ca9e8468c04c7fc7be))

## [0.15.1](https://github.com/teh-hippo/rs-suno/compare/v0.15.0...v0.15.1) (2026-07-02)


### Bug Fixes

* never delete a sidecar path another action writes this run ([#76](https://github.com/teh-hippo/rs-suno/issues/76)) ([#78](https://github.com/teh-hippo/rs-suno/issues/78)) ([063d266](https://github.com/teh-hippo/rs-suno/commit/063d2664ca639c4954b31efc0fa746b7cd3676e3))

## [0.15.0](https://github.com/teh-hippo/rs-suno/compare/v0.14.0...v0.15.0) (2026-07-02)


### Features

* download the standalone MP4 music video (off by default) ([#18](https://github.com/teh-hippo/rs-suno/issues/18)) ([#75](https://github.com/teh-hippo/rs-suno/issues/75)) ([b8145b9](https://github.com/teh-hippo/rs-suno/commit/b8145b94829d7d343062f358ed4fd244429f9716))

## [0.14.0](https://github.com/teh-hippo/rs-suno/compare/v0.13.0...v0.14.0) (2026-07-02)


### Features

* per-area sync/copy mode selection ([#21](https://github.com/teh-hippo/rs-suno/issues/21)) ([#73](https://github.com/teh-hippo/rs-suno/issues/73)) ([61d8814](https://github.com/teh-hippo/rs-suno/commit/61d88144fe8250b3ff7ed821bcc9b975d03c85a5))

## [0.13.0](https://github.com/teh-hippo/rs-suno/compare/v0.12.0...v0.13.0) (2026-07-02)


### Features

* configurable naming templates and character set ([238a074](https://github.com/teh-hippo/rs-suno/commit/238a07447e0fc62b0fcffec8035bf9455f079c3c))

## [0.12.0](https://github.com/teh-hippo/rs-suno/compare/v0.11.1...v0.12.0) (2026-07-02)


### Features

* adaptive AIMD rate limiter (reactive pacing) ([ecbc15f](https://github.com/teh-hippo/rs-suno/commit/ecbc15f88220fdb73fe64e773a00486b6614b3a9))

## [0.11.1](https://github.com/teh-hippo/rs-suno/compare/v0.11.0...v0.11.1) (2026-07-02)


### Bug Fixes

* compile config permission consts on all platforms ([68fbebd](https://github.com/teh-hippo/rs-suno/commit/68fbebdec816fe799da8ae37e4377333471b9d9f))

## [0.11.0](https://github.com/teh-hippo/rs-suno/compare/v0.10.0...v0.11.0) (2026-07-02)


### Features

* adopt /api/feed/v3 cursor pagination and retire feed v2 ([35b8940](https://github.com/teh-hippo/rs-suno/commit/35b89404939ae5bba5e480b7461d3e37facff16c))

## [0.10.0](https://github.com/teh-hippo/rs-suno/compare/v0.9.0...v0.10.0) (2026-07-02)


### Features

* migrate Clerk auth host to auth.suno.com ([0e822ce](https://github.com/teh-hippo/rs-suno/commit/0e822ce1b890971c53c1aa9814a5bb31b2de9916))

## [0.9.0](https://github.com/teh-hippo/rs-suno/compare/v0.8.0...v0.9.0) (2026-07-02)


### Features

* embed real lyrics and add an optional untimed .lrc sidecar ([#53](https://github.com/teh-hippo/rs-suno/issues/53)) ([3fb9df3](https://github.com/teh-hippo/rs-suno/commit/3fb9df3a10a1e9cf396988353c1bd3546c595bd8))

## [0.8.0](https://github.com/teh-hippo/rs-suno/compare/v0.7.0...v0.8.0) (2026-07-02)


### Features

* scope a sync to liked songs or specific playlists ([#50](https://github.com/teh-hippo/rs-suno/issues/50)) ([8eef731](https://github.com/teh-hippo/rs-suno/commit/8eef731a58ebf8d773bccb1953347d8c4eca5978))

## [0.7.0](https://github.com/teh-hippo/rs-suno/compare/v0.6.0...v0.7.0) (2026-07-02)


### Features

* warn when the __client cookie is nearing expiry ([#47](https://github.com/teh-hippo/rs-suno/issues/47)) ([0661777](https://github.com/teh-hippo/rs-suno/commit/066177727cab2e471255a778897ee799a49b5cd4))

## [0.6.0](https://github.com/teh-hippo/rs-suno/compare/v0.5.0...v0.6.0) (2026-07-02)


### Features

* optional per-song details and lyrics sidecar files ([#45](https://github.com/teh-hippo/rs-suno/issues/45)) ([8d403fc](https://github.com/teh-hippo/rs-suno/commit/8d403fc25500c54b7432a9d0d9bb1bf5b2788516)), closes [#15](https://github.com/teh-hippo/rs-suno/issues/15)

## [0.5.0](https://github.com/teh-hippo/rs-suno/compare/v0.4.0...v0.5.0) (2026-07-02)


### Features

* write a machine-readable library index for scripting ([#43](https://github.com/teh-hippo/rs-suno/issues/43)) ([0314cd4](https://github.com/teh-hippo/rs-suno/commit/0314cd427807ea93a861e0886e3dd9ad401046c3)), closes [#16](https://github.com/teh-hippo/rs-suno/issues/16)

## [0.4.0](https://github.com/teh-hippo/rs-suno/compare/v0.3.0...v0.4.0) (2026-07-01)


### Features

* refuse to run against a token for a different Suno account ([#41](https://github.com/teh-hippo/rs-suno/issues/41)) ([26af161](https://github.com/teh-hippo/rs-suno/commit/26af1615f118e57646826ab4d486982b9f4cecc2)), closes [#10](https://github.com/teh-hippo/rs-suno/issues/10)

## [0.3.0](https://github.com/teh-hippo/rs-suno/compare/v0.2.2...v0.3.0) (2026-07-01)


### Features

* detect a full disk and abort the run with a clear error ([#39](https://github.com/teh-hippo/rs-suno/issues/39)) ([5bfc8c8](https://github.com/teh-hippo/rs-suno/commit/5bfc8c8323d76bbe98eb12154717e72d93a019dc)), closes [#17](https://github.com/teh-hippo/rs-suno/issues/17)

## [0.2.2](https://github.com/teh-hippo/rs-suno/compare/v0.2.1...v0.2.2) (2026-07-01)


### Bug Fixes

* **executor:** don't abort the whole run on a CDN download rejection ([#35](https://github.com/teh-hippo/rs-suno/issues/35)) ([a7e36e1](https://github.com/teh-hippo/rs-suno/commit/a7e36e16efed60d9005511de47ab918c5580c652))

## [0.2.1](https://github.com/teh-hippo/rs-suno/compare/v0.2.0...v0.2.1) (2026-07-01)


### Bug Fixes

* **client:** ride through Suno rate limits when listing the library ([#30](https://github.com/teh-hippo/rs-suno/issues/30)) ([3009583](https://github.com/teh-hippo/rs-suno/commit/300958323fe58abf36e3fbc74bdf03185888e3e5))

## [0.2.0](https://github.com/teh-hippo/rs-suno/compare/v0.1.0...v0.2.0) (2026-07-01)


### Features

* add clip download with MP3 and FLAC tagging ([33f4d9a](https://github.com/teh-hippo/rs-suno/commit/33f4d9a93e83737889235fbb896bae9485108f6b))
* add pure media-extras generators to suno-core ([5784f8c](https://github.com/teh-hippo/rs-suno/commit/5784f8c2ee9351d952937a825ab5c0205d76b3db))
* add pure naming module ([269ea4a](https://github.com/teh-hippo/rs-suno/commit/269ea4a0749ec21c5da0f7acdb1b8eed69d10bbf))
* add pure reconcile engine and manifest model ([3e89c42](https://github.com/teh-hippo/rs-suno/commit/3e89c420fff1f2ae9be58abc872a817fdf6e17c2))
* add pure selection and filtering module to suno-core ([65ebc4c](https://github.com/teh-hippo/rs-suno/commit/65ebc4ce745784975baf57244ea341595e9e5062))
* add pure TOML config model and precedence loader to suno-core ([cf44d13](https://github.com/teh-hippo/rs-suno/commit/cf44d1378086e323dbf0fad32fa1deb933d4659a))
* authenticate and list the Suno library ([a6d2e32](https://github.com/teh-hippo/rs-suno/commit/a6d2e329b3292c5791ad177d32360d1fc89ef3cd))
* clean up moved sidecars and prune empty album directories ([0cdf955](https://github.com/teh-hippo/rs-suno/commit/0cdf9557dd770ad3f44e2e1c26a3cbe57deeb6d8))
* **cli:** add Filesystem, Ffmpeg, and Clock adapters and wire fetch through them ([4ecf702](https://github.com/teh-hippo/rs-suno/commit/4ecf7024fca71575eb61453f22a59d548721528d))
* **core:** add artifact reconcile actions with inherited deletion safety ([c4371f1](https://github.com/teh-hippo/rs-suno/commit/c4371f1f1a008bbf3619ad7c8d3d6e868a137350))
* **core:** add download executor with Filesystem, Ffmpeg, and Clock ports ([cbf58db](https://github.com/teh-hippo/rs-suno/commit/cbf58db4468cc93597050a5bcb3f9dfdc19d961a))
* **core:** add durable, monotonic lineage graph store ([1e45de0](https://github.com/teh-hippo/rs-suno/commit/1e45de0e2abbfb0b6ced2506d6a2fe9b1b2ec442))
* **core:** add pure lineage resolver (typed edges, roots, gap-fill) ([f772022](https://github.com/teh-hippo/rs-suno/commit/f772022981d4c4c8b6c7baa1712676f0061b5345))
* **core:** add stable meta_hash and art_hash change sentinels ([d6a0972](https://github.com/teh-hippo/rs-suno/commit/d6a0972a4c5e2017f8a26e6a718a29f61b3ad67f))
* **core:** parse typed lineage metadata onto Clip ([4bb2374](https://github.com/teh-hippo/rs-suno/commit/4bb2374c5a542f5c217ebeaaa439e7f4c7ca7f5d))
* **core:** track per-clip cover artifact state in the manifest ([e55b960](https://github.com/teh-hippo/rs-suno/commit/e55b9604ad5620bfc02edf8a6dc217388aa23092))
* download per-song cover.jpg and opt-in animated cover.webp ([25733c0](https://github.com/teh-hippo/rs-suno/commit/25733c04d4faad92b66bd5559d64320fc28333e1))
* drive album foldering and lineage tags from the resolved graph ([3b437d5](https://github.com/teh-hippo/rs-suno/commit/3b437d56684c3cd6b8c830f66fb8b0554abc3f9b))
* emit and reconcile .m3u8 playlists ([7992bbf](https://github.com/teh-hippo/rs-suno/commit/7992bbf8b53e70b3b5bd27dd3b31eeb7bfcc181a))
* implement the full suno CLI command surface ([4517db8](https://github.com/teh-hippo/rs-suno/commit/4517db80fa7eff3411f34df7b4404481846495b0))
* write per-album folder.jpg and folder cover.webp ([480b1e8](https://github.com/teh-hippo/rs-suno/commit/480b1e88ecd7c5b901af78de38e754f6a427c2a8))


### Bug Fixes

* close three deletion-safety and audit gaps in the sync engine ([9226494](https://github.com/teh-hippo/rs-suno/commit/9226494e64ae30e4316dcba0fa1d18b113919f87))
* **config:** redact parse errors, reject env-prefix collisions, add tests ([0b655ee](https://github.com/teh-hippo/rs-suno/commit/0b655eefcb51d635d4017e457c03783ca69d98e5))
* **executor:** refresh preserve on skip, retry the WAV render flow, harden the fs adapter ([413d4b8](https://github.com/teh-hippo/rs-suno/commit/413d4b8b4cb150429fa75f4c6fd982f1c0fc1af1))
* harden empty-listing waiver, dry-run writes, and concurrency flag ([9fabad7](https://github.com/teh-hippo/rs-suno/commit/9fabad75b843fa197ba6ce0e479a48cf1be31317))
* harden fetch download and transcode paths ([abdd26d](https://github.com/teh-hippo/rs-suno/commit/abdd26d064f8980e4fa4388980a7373d83c3d611))
* harden naming disambiguation ([06f457c](https://github.com/teh-hippo/rs-suno/commit/06f457cb742a85b6e97a38bc965aeecc65347e29))
* harden reconcile delete path against unsafe deletions ([b5fddcd](https://github.com/teh-hippo/rs-suno/commit/b5fddcd38233c177df0b4901a7116902b77bbb08))
* **select:** checked mul for overflow, floor authority over limit, keep unparseable timestamps ([438d02a](https://github.com/teh-hippo/rs-suno/commit/438d02a23f0092bb97640f38aa985402784ef116))
