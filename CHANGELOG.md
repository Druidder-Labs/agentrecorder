# Changelog

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
