# backend-lang - Roadmap

> Path from scaffold to a stable 1.0. Hard parts are front-loaded; each phase has hard exit criteria.
> Master plan: ../_lexersketch/ROADMAP.md and ../_lexersketch/NEW-LIBS.md
>
> **Anti-deferral rule:** no listed hard task moves to a later phase unless this file records the move and the reason.

## v0.1.0 - Scaffold (DONE)
Compiles, CI green, structure correct, no domain logic.
- [x] Manifest, README, CHANGELOG, REPS, dual license, CI, deny, clippy, rustfmt, DIRECTIVES, ROADMAP.

## v0.2.0 - Foundation
- [ ] Machine IR, target-description model, ABI/frame model; parallel-move resolver with property tests.

## v0.5.0 - Implementation
- [ ] Instruction selection from ir-lang 2.0; SSA register allocation (spill, rematerialize, stack maps); x86-64 and AArch64 through isa-lang.

## v0.9.0 - Hardening
- [ ] Differential testing vs the VM; fuzzing; compile-speed benchmarks.

## v1.0.0 - Stable
- [ ] Frozen after jit-lang 2.0 and aot-lang 2.0 run on it (D18).
