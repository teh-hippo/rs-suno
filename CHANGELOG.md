# Changelog

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
