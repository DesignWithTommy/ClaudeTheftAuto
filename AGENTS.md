# Notes for coding agents working on M1LK

M1LK is an Xbox Series compatibility runtime for Windows, tuned for low-end NVIDIA PCs
(8 GB VRAM, 16 GB RAM). Rust workspace: `apps/victory` (the launcher) and `crates/*`
(`victory-gpu` = D3D12 renderer, `victory-hle` = system emulation, `victory-shader` = shader
translation, `victory-core` = shared code, including the runtime switch registry).

## Hard rules
- Never name any commercial game, studio, character or place in code, comments, commit
  messages, file names, PR text or test fixtures. Describe things generically ("the title").
- Never add game files, assets, keys, tables derived from a game, executables or download links.
- NVIDIA-only: don't add AMD-specific code paths.
- Never push to `main`. Work on a branch; a human reviews and merges.
- Every behaviour change goes behind its own switch, default OFF, read through the switch
  registry in `crates/victory-core/src/switches.rs` (on branch `test/round2` until it is
  merged; branch from it if `main` doesn't have it yet) (env `M1LK_<NAME>`, `VICTORY_<NAME>` also
  accepted). Mark it RUNTIME-SAFE only if flipping it between frames can't corrupt state;
  otherwise START-ONLY. Add a one-line reason.
- Credit code ported from other GPL forks in the commit message (repo + commit).
- Add unit tests for pure logic. Keep `cargo fmt` style (`rustfmt.toml`) and clippy clean.

## Building
CI builds Windows x64 on push to `test/**` branches (`.github/workflows/build.yml`, diag
profile with the `diagnostics` feature). A runner without Windows can't build the full app;
prefer small, reviewable diffs and say in the PR what you could not check.

## PR description
State what changed, the switch name and default, RUNTIME-SAFE or START-ONLY, what it should
improve (which frame stage or RAM), and the risk.
