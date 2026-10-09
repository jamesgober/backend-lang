# backend-lang - Roadmap

> Path from scaffold to a stable 1.0. Hard parts are front-loaded; each phase has hard exit criteria.
> Master plan: ../_lexersketch/ROADMAP.md and ../_lexersketch/NEW-LIBS.md
>
> **Anti-deferral rule:** no listed hard task moves to a later phase unless this file records the move and the reason.

## v0.1.0 - Scaffold (DONE)
Compiles, CI green, structure correct, no domain logic.
- [x] Manifest, README, CHANGELOG, REPS, dual license, CI, deny, clippy, rustfmt, DIRECTIVES, ROADMAP.

## v0.2.0 - Foundation (DONE)
- [x] Machine IR, target-description model, ABI/frame model; parallel-move resolver with property tests.
- [x] Emission of physical-register MIR through isa-lang, proven end to end (MIR → isa-lang →
      object-lang → a running Linux process).

Delivered:
- **Machine IR** (`mir`): `Function` of blocks of target instructions; virtual registers typed
  by class, physical registers, operands (register, immediate, `[base + index*scale + disp]`,
  stack slot with offset, function symbol, condition code, branch target with arguments);
  per-instruction implicit uses, definitions, and clobbers (`Effects`); block parameters; local
  and incoming stack slots; dense 32-bit ids for every table; construction-time operand checks;
  `verify` (terminators, branch arities and classes, SSA single definition) in linear time; a
  deterministic text form. Nothing recursive.
- **Block parameters, not phis** (decided here): the same information as phi nodes, kept on the
  edge as the parallel copy it is. ir-lang already uses block parameters (isel maps them one to
  one), an SSA register allocator resolves each edge with the same resolver, and there is no phi
  list to keep in sync with predecessor order.
- **Target description, backend half** (`target`): registers, classes (with allocatable sets and
  spill sizes), and the instruction table as data: per row the operand rules (definitions, uses
  with accepted shapes and immediate ranges, two-address ties, conditions, branch targets), the
  implicit uses/definitions, and clobbers. The flags are a physical register; an instruction
  that may leave some flags unchanged (`inc`/`dec`, shifts by a possibly-zero count, `bt`) is
  modelled as also reading them. isa-lang 0.2 does not carry implicit operands yet (its ROADMAP
  v0.5), so this layer records them; a test cross-checks every row's flag effects and fixed
  registers against isa-lang's own data.
- **x86-64 description** (`x86_64`): 33 registers (16 GPR, 16 XMM, flags), classes `gpr`/`vec`/
  `flags`, 91 instruction rows (moves, loads, stores, `lea`, the ALU at 32 and 64 bits, compare,
  test, `bt`, unary ops, multiply/divide with `rdx:rax`, sign extension, shifts by immediate and
  `cl`, `setcc`, `cmov`, `jmp`, `jcc`, `call`, `ret`, `ud2`, `int3`, `nop`, `syscall`), each
  mapped to the isa-lang instruction it encodes as.
- **ABIs** (`target::abi`): SysV AMD64 and Win64: argument and return registers (separate
  counters vs positional), callee-saved sets including Win64's `xmm6`-`xmm15`, call clobbers,
  stack alignment, shadow space, red zone, probe interval; `assign_args`/`assign_rets`.
- **Frame model** (`target::frame`): callee-saved pushes (vector registers into 16-byte frame
  slots), locals by alignment, outgoing area with shadow space, 16-byte alignment at calls, the
  SysV red zone for small leaves, page-by-page probes (unrolled up to four pages, a counted loop
  beyond), and the prologue/epilogue as a step plan.
- **Parallel-move resolver** (`moves`): any permutation, cycles of every length, fan-out,
  register↔slot, immediates; cycles broken for free by an existing fan-out copy, by `k - 1` swaps
  (x86-64 `xchg`), or through a scratch location (a free register, else a scratch slot);
  memory-to-memory moves and unstorable immediates through a free register, else a borrowed one
  saved to a victim slot. Minimal step counts checked against a reference; `O(n log n)` (one
  sort), buffers reused.
- **Emission** (`x86_64::Emitter`, Tier-1 `emit`): verified physical-register functions, frame
  prologues/epilogues, every edge's parallel copy (critical edges split by branching around the
  taken edge's moves), calls between functions by name, function addresses by RIP-relative
  `lea`, branch relaxation by isa-lang.
- **Proof:** an x86-64 interpreter over isa-lang's decoder in the test suite runs emitted
  programs and random jump/branch edges on every platform; three hand-built programs (Fibonacci
  with parallel-copy edges and a stack-slot parameter, recursive factorial across two functions,
  a mix of the table with an indirect call) are wrapped by object-lang 0.2 into static ELF
  executables and run on Linux x86-64 (exit status checked, ETXTBSY retried); every opcode over
  913 operand shapes; frame and resolver properties; hostile construction never panics.
- Benchmarks at 100,000 instructions (build, verify, emit, print) and the resolver from 4 to
  100,000 moves.

Dependency wiring (decided here, recorded per the anti-deferral rule):
- **isa-lang 0.2: wired** (crates.io). Every byte of the encoding is isa-lang's; the emitter
  drives its `Assembler` (labels, relaxation, call and RIP fixups).
- **object-lang 0.2: dev-dependency only.** The library produces code bytes; wrapping them into
  an executable is the AOT driver's job (aot-lang 2.0). The end-to-end test uses it.
- **ir-lang: not wired.** Instruction selection consumes ir-lang 2.0 (calls, memory,
  aggregates); 1.x lacks them (ISSUES H05), so isel waits for it (see v0.5.0 below).
- **pager-lang: not wired.** Making code executable is the JIT's job (jit-lang 2.0).

Moved out of v0.2.0, with reasons:
- **Instruction selection from ir-lang** → v0.5.0: it needs ir-lang 2.0, which is not released.
  The task for 0.2 scoped it out explicitly; MIR is built directly until then.
- **Dominance checking in `verify`** → v0.5.0, with the SSA register allocator whose
  precondition it is (it needs the dominator tree the allocator builds anyway).
- **Vector-register code (XMM moves, Win64 `xmm6`-`xmm15` saves) and floating point** → v0.5.0:
  isa-lang 0.2 encodes no SSE. The registers, classes, ABIs, and frame plan already include them;
  emission reports `EmitError::Unencodable`.
- **`syscall` as an isa-lang form** → isa-lang 0.5 (requested there): emitted as raw bytes now.
- **Implicit operands as isa-lang data** → isa-lang 0.5; then the cross-check here compares the
  full implicit sets, not only flags and fixed registers.
- **Aggregates by value, variadic calls, 128-bit integers in the ABIs** → v0.5.0, with call
  lowering from ir-lang 2.0's aggregates.
- **Unwind data** (Win64 `.pdata`/`.xdata`, DWARF CFI) generated from `FrameLayout` → v0.5.0,
  when object-lang carries those sections; external-symbol relocations and function alignment
  need isa-lang 0.5's assembler relocations and alignment directive.
- **The NOML reader for target descriptions** (TARGET-DESC §13) → v0.5.0, with the pattern
  tables that are its first second consumer.

## v0.5.0 - Implementation
- [ ] Instruction selection from ir-lang 2.0; SSA register allocation (spill, rematerialize, stack maps); x86-64 and AArch64 through isa-lang.
- [ ] Dominance verification; XMM/floating-point rows and Win64 vector saves (isa-lang 0.5 SSE);
      aggregates, varargs, and i128 in the ABIs; unwind data from `FrameLayout`; the NOML target
      reader; AArch64 frame model (`stp`/`ldp`, frame record).

## v0.9.0 - Hardening
- [ ] Differential testing vs the VM; fuzzing; compile-speed benchmarks.

## v1.0.0 - Stable
- [ ] Frozen after jit-lang 2.0 and aot-lang 2.0 run on it (D18).
