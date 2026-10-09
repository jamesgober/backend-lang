<h1 align="center">
    <img width="90px" height="auto" src="https://raw.githubusercontent.com/jamesgober/jamesgober/main/media/icons/hexagon-3.svg" alt="Triple Hexagon">
    <br><b>CHANGELOG</b>
</h1>
<p>
  All notable changes to <code>backend-lang</code> will be documented in this file. The format is based on <a href="https://keepachangelog.com/en/1.1.0/">Keep a Changelog</a>,
  and this project adheres to <a href="https://semver.org/spec/v2.0.0.html/">Semantic Versioning</a>.
</p>

---

## [Unreleased]

### Added

### Changed

### Fixed

### Security

---

## [0.3.0] - 2026-10-09

ir-lang 2.0 to running x86-64 code: instruction selection for the integer,
control-flow, memory, call, and checked-arithmetic subset, SSA register
allocation with spilling, rematerialization, and exact stack maps, and
emission of whole modules with symbols and relocations, differential-tested
against ir-lang's reference interpreter.

### Added

- The Tier-1 path: `compile(module, options)` and `compile_function`,
  `Options` (`sysv`, `win64`, `host`, `with_abi`, `with_frame_pointer`,
  `with_runtime`, `with_jump_tables`, `with_regalloc`), `Compiled` (`code`,
  `program`, `functions`, `function`, `relocations`, `data`, `imports`,
  `exports`), `DataObject`, and `CompileError` (`#[non_exhaustive]`).
- `isel`: `select` and `IselError` (`#[non_exhaustive]`). Every OPS integer
  operation under every policy ir-lang 2.0.0-alpha.1 carries (`wrap`, `trap`
  through a per-function trap block, `error` through `check` edges with the
  OPS code as `err`); comparisons fused into branches and `select`;
  conversions; loads, stores, stack slots (over-aligned ones too), field and
  element addressing, globals, function addresses, block memory operations,
  the x86-64 atomic subset; direct, indirect, and tail calls under SysV and
  Win64; `switch` as a jump table or a compare tree by density; `safepoint`;
  `invoke` with landing pads and `resume` (no unwind tables yet).
- `regalloc`: `allocate`, `allocate_with`, `Options`, `Allocation`,
  `Location`, `RegAllocError` (`#[non_exhaustive]`), `MAX_ROUNDS`,
  `DEFAULT_LIVE_BUDGET`. SSA liveness by path exploration (budgeted),
  dominance-order coloring without an interference graph, affinity
  coalescing and fixed-register hints, spill-everywhere with
  rematerialization of constants and addresses, edge copies through the
  move resolver, callee-saved registers by convention.
- Stack maps per IR.md §9: GC references stored to root slots around each
  safepoint and reloaded as the relocated values; the slots are recorded per
  call site.
- `mir`: `Function::verify_ssa` (dominance by Lengauer–Tarjan, linear),
  `CallSite`, `Function::{set_call_site, call_site, call_sites}`, and
  `MirError::{NotACall, UndefinedVReg, NotDominated}`.
- `target`: `InstKind::{TailCall, Switch}`.
- `x86_64`: rows `tailcall`, `jmptbl`, `landingpad`, `xchg8`, `xchg16`,
  `xchg32`, `xchg64`; `Emitter::declare_external`; `Reloc`, `RelocKind`
  (`Call32`, `Pc32`, `Abs64`), `CallSiteRecord`, `EmittedFunction`,
  `Program::{functions, relocations}`; `EmitError::{Constraint,
  ExternalConflict}`.
- A dependency on `ir-lang` `=2.0.0-alpha.1` (crates.io), re-exported as
  `backend_lang::ir_lang`; its `interp` feature as a dev-dependency.
- Tests: a random generator of valid ir-lang 2.0 programs, run against
  ir-lang's interpreter in the x86-64 test interpreter (SysV and Win64) and
  natively in-process on x86-64 Linux and Windows; OPS conformance for every
  op × type × policy on edge values; allocator assignments checked by an
  independent liveness; stack maps under a moving collector that walks real
  frames; landing pads through a minimal unwinder; ELF relocatables and a
  static executable through object-lang.
- Benchmarks: compile throughput on 100,000-instruction functions (whole
  pipeline and each phase, and a high-pressure variant), and compiled code
  against the interpreter. Example `compile`.

### Changed

- Version 0.3.0; the crate description names instruction selection and
  register allocation.
- Dominators in `mir` are computed by Lengauer–Tarjan instead of the
  Cooper–Harvey–Kennedy iteration, which is quadratic when one block has
  many predecessors deep in the dominator tree.

### Fixed

- Register allocation: a rematerialized `mov32ri` constant substituted as an
  immediate now keeps its zero extension (`-1` there is `0xFFFF_FFFF`, not
  all ones).
- Register allocation: a value a terminator defines (a jump table's
  temporary) is never chosen for spilling, since no store can follow it.

---

## [0.2.0] - 2026-10-08

The foundation of the native backend: machine IR, the backend half of the
target description, SysV and Win64 calling conventions and frame layout, the
parallel-move resolver, and emission to x86-64 machine code through
isa-lang, proven end to end into a running Linux process.

### Added

- `mir`: `Function` (blocks of target instructions over typed virtual and
  physical registers, block parameters, stack slots, symbols, per-instruction
  implicit uses/defs/clobbers via `Effects`), `Block`, `Inst`, `StackSlot`,
  `Symbol`, `CondCode`, `SlotKind`, `Operand`, `Mem`, `Scale`, `BlockCall`,
  `Implicit`, `ImplicitKind`, and `MirError` (`#[non_exhaustive]`, every
  variant locating the problem). Operands are checked at construction;
  `Function::verify` checks structure and SSA single definition in linear
  time. A deterministic text form (`Display`).
- `PReg`, `VReg`, `Reg`, `RegClass`, `RegSet` (128-bit), `RegSetIter`,
  `MAX_PREGS`.
- `target`: `Target`, `TargetConfig`, `Isa`, `RegInfo`, `ClassInfo`, `Opcode`
  (with the generic `COPY` and `PCOPY`), `InstDesc`, `InstKind`,
  `OperandRule` (definitions, uses, two-address ties, conditions, branch
  targets), `Accept`, `ImmRange`, `MoveCaps`. The flags are a physical
  register that producers define and consumers use.
- `target::abi`: `Abi` (`assign_args`, `assign_rets`, `caller_saved`, `call_clobbers`,
  `layout_frame`), `RegSeq`, `StackRules`, `ArgClass`, `ArgLoc`, `AbiError`.
- `target::frame`: `FrameInput`, `FrameLayout`, `FrameStep`, `FrameError`:
  callee-saved pushes (vector registers saved into frame slots), locals by
  alignment, outgoing area with Win64 shadow space, 16-byte alignment at
  calls, the SysV red zone for small leaves, and page-by-page stack probes.
- `moves`: `MoveResolver`, `Move`, `Loc`, `Source`, `Step`, `MoveEnv`,
  `Resolved`, `MoveError`, `resolve`: parallel copies of any shape (cycles of
  every length, fan-out, registers and slots mixed, immediates) with cycles
  broken by an existing fan-out copy, by swaps, or through a scratch
  location, and memory-to-memory moves through a free or borrowed register.
- `x86_64`: the description (`TARGET`; 33 registers; `GPR`, `VEC`,
  `FLAGS_CLASS`; 91 instruction rows in `op`, each with operand rules and
  implicit operands), `SYSV` and `WIN64`, `cond`, `Emitter`, `Program`,
  `FunctionInfo`, and `EmitError` (`#[non_exhaustive]`).
- `emit`: the Tier-1 one-call path from a physical-register function to
  machine code.
- A dependency on `isa-lang` 0.2 (crates.io); `object-lang` 0.2 as a
  dev-dependency for the end-to-end executable test.
- Tests: an x86-64 interpreter over isa-lang's decoder; resolver property
  tests against a parallel-assignment reference with minimal-count checks;
  emitted jump and branch edges executed and checked; every opcode emitted
  over 913 operand shapes and cross-checked against isa-lang's flag effects
  and fixed registers; frame layout properties; hostile construction; and
  three MIR programs run as static Linux executables.
- Criterion benchmarks: 100,000-instruction build/verify/emit/print and the
  resolver from 4 to 100,000 moves; examples `emit` and `moves`.

### Changed

- Version 0.2.0; the crate description now names what this release does.

---

## [0.1.0] - 2026-10-08

Initial scaffold and repository bootstrap. No domain logic yet &mdash; this release establishes the structure, tooling, and quality gates the implementation will be built on.

### Added

- `Cargo.toml` with crate metadata, Rust 2024 edition, MSRV 1.85.
- Dual `Apache-2.0 OR MIT` license files.
- `README.md`, `CHANGELOG.md`, and a documentation skeleton.
- `REPS.md` compliance baseline.
- `.github/workflows/ci.yml` CI matrix; `deny.toml`, `clippy.toml`, `rustfmt.toml`.
- `dev/DIRECTIVES.md` and `dev/ROADMAP.md` (committed engineering standards + plan).

[Unreleased]: https://github.com/jamesgober/backend-lang/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/jamesgober/backend-lang/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/jamesgober/backend-lang/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/jamesgober/backend-lang/releases/tag/v0.1.0
