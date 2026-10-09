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
  The task for 0.2 scoped it out explicitly; MIR is built directly until then. *Delivered in
  v0.3.0, against ir-lang 2.0.0-alpha.1.*
- **Dominance checking in `verify`** → v0.5.0, with the SSA register allocator whose
  precondition it is (it needs the dominator tree the allocator builds anyway). *Delivered in
  v0.3.0 as `Function::verify_ssa`.*
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

## v0.3.0 - Instruction selection, register allocation, emission (DONE)
ir-lang 2.0 → MIR → x86-64 bytes for the integer, control-flow, memory, call, and
checked-arithmetic subset, differential-tested against ir-lang's interpreter.
Exit criteria:
- [x] Instruction selection for every OPS integer operation under every policy ir-lang
      2.0.0-alpha.1 can carry, comparisons, select, conversions, memory, calls, tail calls,
      switches, `check`, `invoke`, and `resume`.
- [x] SSA register allocation with spilling, rematerialization, coalescing, edge parallel
      copies through the resolver, per-ABI callee-saved handling, and exact stack maps.
- [x] A module compiles to one code buffer with symbols, call-site records, data, and the
      relocations object-lang 0.2 writes; internal calls resolved.
- [x] Differential harness (test interpreter everywhere, native on x86-64 Linux and
      Windows), allocator properties, stack-map exactness, compile and code-quality benches.

Delivered:
- **Tier-1 path** (`compile`, `compile_function`, `Options`, `Compiled`, `DataObject`,
  `CompileError`): validate the module, select, allocate, and emit every defined function into
  one buffer; globals become `DataObject`s with their `Abs64` relocations; imports, globals, and
  the runtime entry points (`trap_symbol`, `gc_poll_symbol`, `resume_symbol`) stay relocations;
  `imports()`/`exports()` list the symbols a linker needs.
- **Instruction selection** (`isel::select`, `IselError`): blocks and block parameters map one
  to one. Narrow values live in the low bits of a 64-bit register with the upper bits
  unspecified, extended by their signedness wherever the upper bits matter; `bool` is always
  0 or 1. Every integer operation (`add sub mul div rem floor_div floor_mod and or xor shl shr
  min max neg abs not`) under `wrap`, `trap` (a per-function trap block per OPS code), and
  `error` (inside `check`, branching to the error edge with the OPS code as `err`): flag
  checks at 32/64 bits, exact 32-bit arithmetic plus a range check at 8/16 bits, `u32`/`u64`
  multiply overflow from the high half, division guarded for zero and `MIN / -1` before the
  processor can fault, floor variants by `cmov` fix-ups, shifts masked or range-checked.
  Comparisons fuse into their branch or `select` (`cmov`) when used once in the same block.
  Conversions `int_cast` (checked), `zext`, `sext`, `narrow`, `bitcast`, `bool_to_int`,
  `ptr_to_int`/`int_to_ptr`/`ref_to_ptr`, and `char_from_u32` (`check` only). Memory: loads and
  stores of every integer width (alignment needs no instruction on x86-64), stack slots
  (alignment above 16 by rounding the address up inside an enlarged slot), `field_addr`,
  `elem_addr` (scaled `lea` or a multiply), `ptr_offset`, `global_addr`, `func_addr`, constant
  offsets folded into memory operands, `memcpy`, `memmove` (direction by address order),
  `memset`, atomic loads and stores (`xchg` for `seq_cst` stores), `atomic_rmw xchg`, and
  `fence seq_cst` (a locked `xchg`). Calls direct and indirect, with stack arguments, results
  in registers, and 8/16-bit values extended at the boundary; tail calls (direct and through
  `r11`) whose stack arguments fit the caller's incoming area; `switch` as a bounds-checked jump
  table when the cases are dense (`Options::jump_table_min_cases`, `jump_table_max_spread`),
  else a balanced compare tree; `safepoint` as a call to the GC poll; the conventions `fast`,
  `c`, `sysv64`, and `win64`.
- **`invoke` and landing pads, minimal**: the call records its landing pad (MIR `CallSite`,
  emitted as `CallSiteRecord::landing_pad`); the unwinder is expected to enter the pad's stub
  with the frame as it was at the call (callee-saved registers restored) and the payload in
  `rax`; the stub (`landingpad`) reloads the relocated references from their root slots and
  jumps to the IR landing pad with `exn` and `resI`. `resume` calls `resume_symbol`. What is
  missing is listed under v0.5.0.
- **SSA register allocation** (`regalloc::allocate`, `allocate_with`, `Options`,
  `Allocation`, `Location`, `RegAllocError`): liveness by SSA path exploration with a budget;
  constraints from one backward walk (last uses, physical registers live after each
  instruction, registers written during each value's life); coloring in dominance order with no
  interference graph; coalescing by affinity classes over copies, ties, and edges, plus hints
  toward fixed registers (selection orders commutative operands so that one used only there is
  the tied one);
  spill-everywhere with constants and addresses rematerialized at their uses, other values
  stored once and read from the slot directly where the instruction takes a memory operand,
  else through a short-lived reload temporary; iteration to a fixed point with a guaranteed
  final round (`MAX_ROUNDS`). Callee-saved registers are ordinary registers; a value live
  across a call can only take one, and the frame saves exactly those written.
- **Stack maps** (IR.md §9 statepoints): each GC reference in a safepoint's list is stored into
  a root slot before the call and reloaded after it as the relocated value, so no reference is
  in a register while a collector runs; the slots are the call's stack map
  (`CallSiteRecord::gc_slots`, offsets from `rsp` at the call).
- **MIR**: `Function::verify_ssa` (dominance in linear time: dominators by Lengauer–Tarjan,
  then O(1) interval tests on the dominator tree), call-site records (`set_call_site`,
  `call_site`, `call_sites`, `CallSite`), `MirError::{NotACall, UndefinedVReg, NotDominated}`;
  x86-64 rows `tailcall`, `jmptbl`, `landingpad`, `xchg8/16/32/64`;
  `InstKind::{TailCall, Switch}`.
- **Emission**: `Emitter::declare_external` and relocations (`Reloc`, `RelocKind::{Call32,
  Pc32, Abs64}`) for symbols defined elsewhere; `Program::{functions, relocations}`;
  `EmittedFunction` (offset, size, frame, call-site records); jump tables placed after their
  function; `EmitError::{Constraint, ExternalConflict}`.
- **Proof**: a random generator of valid, UB-free ir-lang 2.0 programs (every integer op and
  policy, checks with error edges, loops whose back edges permute their parameters, dense and
  sparse switches, slots and globals through field and element addresses, block memory
  operations, direct and indirect calls, tail calls, high pressure) compiled and run against
  ir-lang's interpreter, in the test interpreter under SysV and Win64 everywhere and natively
  in-process on x86-64 Linux and Windows; OPS conformance on edge values for every op × type ×
  policy; allocator assignments checked point by point by an independent liveness; a moving
  collector that walks real frames by the call-site records; a minimal unwinder for landing
  pads; modules written as ELF relocatables by object-lang and linked into a static executable
  that runs on Linux (ETXTBSY retried).

Dependency wiring (decided here, recorded per the anti-deferral rule):
- **ir-lang `=2.0.0-alpha.1`: wired** (crates.io, pinned exactly because it is a pre-release).
  Instruction selection reads it; `ir_lang` is re-exported. Its `interp` feature is a
  dev-dependency only (the oracle).
- **isa-lang 0.2: wired**, unchanged.
- **object-lang 0.2: dev-dependency only**: the relocations `Compiled` reports are the ones an
  ELF relocatable carries; the link test writes and reads them back.

Moved out of v0.3.0, with reasons:
- **`shift = saturate`** → when ir-lang carries it: OPS v2 defines it, but ir-lang
  2.0.0-alpha.1's `Shift` has only `error` and `mask`, so no ir-lang program can ask for it and
  no oracle can check it. The shift lowering already range-checks the amount; saturation is a
  `cmov` on the same compare.
- **Floating point and SIMD** (types, operations, conversions, arguments, loads) → v0.5.0:
  isa-lang 0.2 encodes no SSE (ISSUES P11). Refused as `IselError::Unsupported`.
- **`atomic_rmw` other than `xchg`, and `cmpxchg`** → v0.5.0: they need `lock`-prefixed forms
  isa-lang 0.2 does not encode (an addition to ISSUES P11). Refused.
- **Unwind tables** (Win64 `.pdata`/`.xdata`, DWARF CFI, an LSDA mapping call sites to landing
  pads) and a personality routine → v0.5.0: object-lang 0.2 carries no such sections. The
  call-site records and `FrameLayout` are their input; the tests use their own minimal unwinder.
- **More results than return registers** (three under SysV, two under Win64) → v0.5.0 with
  aggregate and sret lowering. Refused.
- **Tail calls needing more stack-argument space than the caller received** → v0.5.0 (the
  `fast` convention reserving a fixed area). Refused.
- **Compile throughput (target ≥1M IR instructions/s)**: not met reliably on this machine;
  measured numbers are in the release notes. The allocator reruns liveness and coloring for
  every spill round (two or three on large functions); an incremental later round is v0.9.0
  work.

## v0.5.0 - Implementation
- [ ] AArch64: isa-lang AArch64 forms, the AArch64 description, AAPCS64 (`aapcs64`), the
      frame model (`stp`/`ldp`, frame record), selection, and i-cache coherence for the JIT.
- [ ] Floating point and SIMD: XMM rows and Win64 vector saves (isa-lang 0.5 SSE, ISSUES
      P11), float selection (OPS §4 exactly: `min`/`max` NaN and signed-zero rules, `round`
      ties away, `float_to_int` saturate/error), float arguments and results.
- [ ] Unwinding: Win64 `.pdata`/`.xdata` and DWARF CFI from `FrameLayout`, an LSDA from the
      call-site records, a personality routine; `invoke` and landing pads complete.
- [ ] PIC: GOT-relative addressing of imported data and PLT calls (`R_X86_64_GOTPCREL`,
      `PLT32`), so modules link into shared objects; today data is reached PC-relative and
      calls are direct `rel32`.
- [ ] `lock`-prefixed atomics (`atomic_rmw` beyond `xchg`, `cmpxchg`).
- [ ] Aggregates by value, sret, more results than return registers, varargs, and i128 in
      the ABIs; the NOML target reader.

## v0.9.0 - Hardening
- [ ] Differential testing vs the VM; fuzzing; compile-speed benchmarks.

## v1.0.0 - Stable
- [ ] Frozen after jit-lang 2.0 and aot-lang 2.0 run on it (D18).
