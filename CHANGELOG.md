# Changelog

## [0.6.1](https://github.com/Druidder-Labs/witnessed/compare/v0.6.0...v0.6.1) (2026-10-24)


### Bug Fixes

* **assurance:** keep evidence file paths out of the policy input ([b80896f](https://github.com/Druidder-Labs/witnessed/commit/b80896ff495083adbe9d321e852940793a16221f))

## [0.6.0](https://github.com/Druidder-Labs/witnessed/compare/v0.5.0...v0.6.0) (2026-10-23)


### Features

* **assurance:** verify agent changes against signed evidence and policy ([8cc5a7b](https://github.com/Druidder-Labs/witnessed/commit/8cc5a7b429a96dd96c52884de1d47726ca5356f1))
* **assurance:** verify agent changes against signed evidence and policy ([10d0cd5](https://github.com/Druidder-Labs/witnessed/commit/10d0cd54819d8f5c6bdaec765dea4c68bb4e5f15))


### Bug Fixes

* **review:** end approval waits when the permission dialog closes ([2468725](https://github.com/Druidder-Labs/witnessed/commit/24687259ab45c4c9dfcb85d44ed89e253f456e72))
* **review:** measure approval waits from the tool run and the transcript ([35e1be6](https://github.com/Druidder-Labs/witnessed/commit/35e1be6f7adcdb09c23f9e3eca85a94cb9aa732a))

## [0.5.0](https://github.com/Druidder-Labs/witnessed/compare/v0.4.0...v0.5.0) (2026-10-23)


### Features

* rename agentrecorder to witnessed and add decision review ([c5a430d](https://github.com/Druidder-Labs/witnessed/commit/c5a430de2b1603e1a3e0f7534d92c161cf5d98ca))


### Bug Fixes

* **cli:** drop libc conversions that are no-ops on Linux ([e143229](https://github.com/Druidder-Labs/witnessed/commit/e14322932d169e97ef6c6853035bc2112a282daf))
* **supervisor:** cast TIOCSCTTY instead of into() for Linux clippy ([b078f9c](https://github.com/Druidder-Labs/witnessed/commit/b078f9ced41ac2b2bf97ec199d7a48ee9af843be))

## [0.4.0](https://github.com/Druidder-Labs/agentrecorder/compare/v0.3.0...v0.4.0) (2026-10-22)


### Features

* **bundle:** add an HR overview, evaluation guide and verified code diff ([e3bf927](https://github.com/Druidder-Labs/agentrecorder/commit/e3bf927d4378ce9fb2693ff8b43a21956e4a18cd))
* **bundle:** separate approval wait from agent time in the prompts panel ([9757bcd](https://github.com/Druidder-Labs/agentrecorder/commit/9757bcd9c17ebe85f45b5b6cf817f70fd4f0e058))
* **bundle:** show the prompts in order with wait, agent time, tools ([d1c738a](https://github.com/Druidder-Labs/agentrecorder/commit/d1c738a049d3f4d0428b582ad14241ec37e87887))


### Bug Fixes

* **bundle:** keep redacted secrets out of binary diffs ([1935e3a](https://github.com/Druidder-Labs/agentrecorder/commit/1935e3abe9f92aabe0adaf95d88f3d4e66b33d7a))
* **store:** name the versions and the update command when the store is newer ([4a4a2a0](https://github.com/Druidder-Labs/agentrecorder/commit/4a4a2a025d7795df8549f57fa09427b211b89f75))

## [0.3.0](https://github.com/Druidder-Labs/agentrecorder/compare/v0.2.0...v0.3.0) (2026-10-21)


### Features

* **bundle:** verification panel, token totals and clearer verify results ([fba29b2](https://github.com/Druidder-Labs/agentrecorder/commit/fba29b227f87094ef79cd55fa6a93bbaf6d0729a))
* **bundle:** write zip bundles and bind the hash chain to a reviewer token ([10af4b5](https://github.com/Druidder-Labs/agentrecorder/commit/10af4b5c15899c4f7488c6de5f03c01d1be7f589))
* **inspect:** report tool calls, token usage and files changed ([89001ce](https://github.com/Druidder-Labs/agentrecorder/commit/89001ce7dae0d1d409d9f2410f478cbcec263b32))
* **inspect:** split token usage per prompt with opt-in prompt text ([f1c17c6](https://github.com/Druidder-Labs/agentrecorder/commit/f1c17c6572bec73472bfa0b5967b78a3532713ee))

## [0.2.0](https://github.com/Druidder-Labs/agentrecorder/compare/v0.1.0...v0.2.0) (2026-10-21)


### Features

* **bundle:** share sessions as a verifiable dashboard bundle ([ba4dbcf](https://github.com/Druidder-Labs/agentrecorder/commit/ba4dbcfa6489e0aa9aaebed7e5e796859ee75df6))
* **cli:** developer experience pass for sharing sessions ([2ffc708](https://github.com/Druidder-Labs/agentrecorder/commit/2ffc70855301f2a5323aa6c9fd7993eaf662222a))
* **hook:** record claude code hook events in the session ([36f420e](https://github.com/Druidder-Labs/agentrecorder/commit/36f420e320ce74ba09af14971a0d8dfade100ca6))
* **inspect:** count shell commands and failures from OSC 133 marks ([f608fdb](https://github.com/Druidder-Labs/agentrecorder/commit/f608fdbe3317fea1acc8a00aba1852b8666f8f35))
* **inspect:** report agent state from hook events ([251a981](https://github.com/Druidder-Labs/agentrecorder/commit/251a981c93efd31607fa4e6020c965a677e28a07))
* **mark:** record user markers and replay from them ([9a5da6f](https://github.com/Druidder-Labs/agentrecorder/commit/9a5da6f7e29dd3349456b3aee8f8cb813b8cbd11))
* **recover:** confirm a claude code resume from its SessionStart hook ([2c7dda1](https://github.com/Druidder-Labs/agentrecorder/commit/2c7dda18d87f58646995fc35a040a6694a33f3b1))
* **replay:** follow a running session with --follow ([0656fe7](https://github.com/Druidder-Labs/agentrecorder/commit/0656fe7cbfbc26943100a70cf73e02b8ff2b0f4e))
* **run:** pre-assign claude code session ids for exact resume ([52c04af](https://github.com/Druidder-Labs/agentrecorder/commit/52c04af4a2b5a4ef83578b550e3f383fc2ea0b62))
* **supervisor:** redraw a slow interactive client instead of detaching it ([bcd9b1a](https://github.com/Druidder-Labs/agentrecorder/commit/bcd9b1a70d58a09c706f5484faa9139f49a971d7))


### Bug Fixes

* **inspect:** abbreviate injected hook settings in the command line ([cb32214](https://github.com/Druidder-Labs/agentrecorder/commit/cb32214acf391640884174e149013e381ffb5711))
* return freed pages on rm and stabilize the macos test suite ([e0d5f12](https://github.com/Druidder-Labs/agentrecorder/commit/e0d5f12c7414f833d8640e986aa0ace517dcaab2))
* **run:** deliver output produced before the first attach ([2a5f28f](https://github.com/Druidder-Labs/agentrecorder/commit/2a5f28f9f9cd02c91a97e549e2a24ca66b95458d))

## 0.1.0 (2026-10-21)


### Features

* Add initial proposal ([cd91be7](https://github.com/Druidder-Labs/agentrecorder/commit/cd91be7f26d14fce79b59104ea3d727de7d81532))
