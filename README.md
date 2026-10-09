<h1 align="center">
    <img width="99" alt="Rust logo" src="https://raw.githubusercontent.com/jamesgober/rust-collection/72baabd71f00e14aa9184efcb16fa3deddda3a0a/assets/rust-logo.svg">
    <br>
    <b>backend-lang</b>
    <br>
    <sub><sup>NATIVE BACKEND CORE</sup></sub>
</h1>

<div align="center">
    <a href="https://crates.io/crates/backend-lang"><img alt="Crates.io" src="https://img.shields.io/crates/v/backend-lang"></a>
    <a href="https://crates.io/crates/backend-lang"><img alt="Downloads" src="https://img.shields.io/crates/d/backend-lang?color=%230099ff"></a>
    <a href="https://docs.rs/backend-lang"><img alt="docs.rs" src="https://img.shields.io/docsrs/backend-lang"></a>
    <a href="https://github.com/jamesgober/backend-lang/actions"><img alt="CI" src="https://github.com/jamesgober/backend-lang/actions/workflows/ci.yml/badge.svg"></a>
    <a href="https://github.com/rust-lang/rfcs/blob/master/text/2495-min-rust-version.md"><img alt="MSRV" src="https://img.shields.io/badge/MSRV-1.85%2B-blue"></a>
</div>

<br>

<div align="left">
    <p>
        <strong>backend-lang</strong> is the LexerSketch native backend: it takes an <a href="https://crates.io/crates/ir-lang"><code>ir-lang</code></a> 2.0 module through instruction selection, SSA register allocation with exact stack maps, and emission to x86-64 machine code through <a href="https://crates.io/crates/isa-lang"><code>isa-lang</code></a>, producing code, symbols, call-site records, data, and the relocations a linker needs. Underneath sit a machine IR over virtual and physical registers, target descriptions that state everything the backend needs to know about an ISA as data, SysV and Win64 calling conventions and frame layout, and a parallel-move resolver. It replaces rented backends (Cranelift, LLVM) in the LexerSketch toolchain; nothing on its path is a system assembler, linker, or C library.
    </p>
    <p>
        This release compiles the <strong>integer, control-flow, memory, call, and checked-arithmetic</strong> subset of ir-lang: every OPS integer operation under every policy ir-lang carries (with <code>check</code> error edges), comparisons, conversions, loads and stores, stack slots, field and element addressing, globals, block memory operations, calls and tail calls under SysV and Win64, switches as jump tables or compare trees, GC safepoints with stack maps, and <code>invoke</code> with landing pads (no unwind tables yet). Floating point waits on isa-lang's SSE forms, AArch64 on its AArch64 forms; anything not selected is refused with an error naming what it waits on (see the <a href="./dev/ROADMAP.md">roadmap</a>). Compiled code is checked against ir-lang's reference interpreter on random programs, in an x86-64 interpreter on every platform and natively on x86-64 Linux and Windows.
    </p>
    <br>
    <hr>
    <p>
        <strong>MSRV is 1.85+</strong> (Rust 2024 edition). <code>no_std</code>-compatible (needs only <code>alloc</code>), <code>#![forbid(unsafe_code)]</code>, two dependencies, both from the family: <a href="https://crates.io/crates/isa-lang"><code>isa-lang</code></a> 0.2 and <a href="https://crates.io/crates/ir-lang"><code>ir-lang</code></a> <code>=2.0.0-alpha.1</code> (pinned exactly while it is a pre-release).
    </p>
    <blockquote>
        <strong>Status: pre-1.0.</strong> The public API is designed across the 0.x series and frozen at <code>1.0.0</code>, after jit-lang 2.0 and aot-lang 2.0 run on it. See <a href="./CHANGELOG.md"><code>CHANGELOG.md</code></a>.
    </blockquote>
</div>

<hr>
<br>

## The model

- **[`compile`](./docs/API.md#compile)** takes an ir-lang [`Module`](https://docs.rs/ir-lang) to a [`Compiled`](./docs/API.md#compiled): the machine code of every function back to back, each function's offset, frame, and call-site records (return address, stack map, landing pad), the relocations against symbols defined elsewhere, and the module's data. Internal calls are already resolved.
- **[`isel`](./docs/API.md#isel)** selects one ir-lang function into MIR on virtual registers; **[`regalloc`](./docs/API.md#regalloc)** assigns physical registers and stack slots.
- A **[`Function`](./docs/API.md#function)** is blocks of target instructions. Instructions name **virtual registers** (each typed by a register class), **physical registers**, **stack slots**, and other functions by **symbol**. Values cross edges as **block parameters**: a branch passes arguments, and the target's parameters receive them all at once.
- A **[`Target`](./docs/API.md#target)** describes an ISA as data: its registers, its register classes, and its instructions, each with operand rules (definitions, uses, two-address ties, accepted shapes and immediate ranges) and the registers it reads, writes, and clobbers without naming them (`idiv` reads `rdx:rax`, a shift by `cl` reads `rcx`, flag producers define the flags register).
- An **[`Abi`](./docs/API.md#abi)** is a calling convention: argument and return registers, callee-saved registers, stack alignment, shadow space, red zone, and stack probing. [`layout_frame`](./docs/API.md#abilayout_frame) turns a function's slots and saved registers into offsets and a prologue/epilogue plan.
- The **[`MoveResolver`](./docs/API.md#moveresolver)** turns any parallel copy into moves and swaps a machine can execute one at a time.
- The **[`Emitter`](./docs/API.md#emitter)** encodes physical-register functions through isa-lang, resolving every edge's parallel copy on the way.

<br>

What it guarantees, and how each guarantee is checked:

| Guarantee | How it is held |
|---|---|
| Compiled code computes exactly what the IR specifies, results and trap codes alike. | Random valid ir-lang programs (every integer op under every policy, `check` error edges, loops whose back edges permute their parameters, dense and sparse switches, slots and globals through field and element addresses, block memory operations, direct, indirect, and tail calls, high register pressure) are compiled and run against ir-lang's reference interpreter: in the x86-64 test interpreter under SysV and Win64 on every platform, and natively in-process on x86-64 Linux and Windows. Arguments arrive with junk above their width; callee-saved registers and the stack pointer are checked after every return. |
| Every OPS integer operation, on every type, under every policy. | Each op × type × policy on the OPS edge values (`0, 1, -1, MIN, MAX, MIN+1, MAX-1`, shift amounts around the width) compiled and compared with the interpreter. |
| The register assignment is interference-free. | On every function selected from random programs and on hand-built ones with cycles, ties, and fixed registers, an independent liveness checks point by point: no two live values share a register, no value is live across an instruction that writes its register, block parameters avoid each other and every live-in value. |
| Stack maps are exact. | Every safepoint's recorded root slots are exactly its GC list. A moving collector in the tests walks the real machine stack by the call-site records and frame layouts, moves every object a stack map names, and poisons the old copies, so a missed root or a missed reload changes the result. |
| Parallel copies are exact: any permutation, cycles of every length, fan-out, registers and stack slots mixed, immediates. | Property tests run the resolver against a parallel-assignment reference over random copies built from explicit cycles, chains, fan-out, and immediates; every step is checked to be one x86-64 can execute; every location that is not a destination is checked unchanged. Cycles of every length from 2 to 14 registers and to 64 slots; a 100,000-slot permutation. |
| Moves are minimal under the resolver's rules. | For register-only copies, the step count is checked equal to the minimum: one per copy, `k - 1` swaps per `k`-cycle (or `k + 1` moves without swaps), nothing extra for a cycle whose value is also copied out of it. |
| Emitted edges perform their parallel copy. | Random jump and branch edges over all fourteen allocatable registers and up to six stack slots are emitted, executed by an x86-64 interpreter in the test suite, and checked location by location. |
| Every instruction row encodes, and its implicit operands agree with isa-lang. | 913 operand shapes over every opcode are emitted and decoded back; the flags each row declares as read and written are checked against isa-lang's flag effects, and fixed registers against isa-lang's forms. |
| Frames are aligned and disjoint, under SysV and Win64. | Property tests over random slots, saved registers, calls, and outgoing areas simulate each prologue: the stack is 16-aligned at calls, and slots are aligned, disjoint, and clear of the saved registers and the outgoing area. |
| MIR → isa-lang → object-lang → a running process. | Three hand-built programs (a Fibonacci loop with parallel-copy edges, recursive factorial across two functions, and a mix of the instruction table with an indirect call) are wrapped into static ELF executables and run on Linux x86-64 with their `exit` status checked, and run in the interpreter on every platform. |
| No panics on hostile construction. | Random opcodes with random valid, foreign, and ill-shaped operands are built, verified, printed, laid out, and emitted; every failure is an error value. |

<hr>
<br>

## Installation

```toml
[dependencies]
backend-lang = "0.3"
```

Without the standard library:

```toml
[dependencies]
backend-lang = { version = "0.3", default-features = false }
```

<hr>
<br>

## Quick start

`compile` is the one-call path: an ir-lang module in, x86-64 machine code with symbols and relocations out.

```rust
use backend_lang::Options;
use ir_lang::{Linkage, Module, Overflow, Signature, Type};

// fn add(x: i64, y: i64) -> i64 { x + y }
let mut m = Module::new("demo");
let sig = Signature::new(&[Type::I64, Type::I64], &[Type::I64]);
let f = m.declare_function("add", &sig, Linkage::Export)?;
let mut b = m.build(f)?;
let (x, y) = (b.param(0)?, b.param(1)?);
let s = b.add(x, y, Overflow::Wrap)?;
b.ret(&[s])?;

let c = backend_lang::compile(&m, &Options::sysv().with_frame_pointer(false))?;
// add rdi, rsi; mov rax, rdi; ret
assert_eq!(c.code(), [0x48, 0x01, 0xF7, 0x48, 0x89, 0xF8, 0xC3]);
assert_eq!(c.exports(), ["add"]);
# Ok::<(), Box<dyn std::error::Error>>(())
```

### Errors become edges, traps become calls

A `check` terminator branches to its error edge with the OPS error code; a `trap` policy calls the runtime's trap handler, which the module imports by name:

```rust
use backend_lang::Options;
use ir_lang::{BinaryOp, BlockArg, BlockCall, DivZero, InstData, Linkage, Module, Overflow, Policy, Signature, Type};

let mut m = Module::new("demo");
let sig = Signature::new(&[Type::I32, Type::I32], &[Type::I32]);
let f = m.declare_function("safe_div", &sig, Linkage::Export)?;
let mut b = m.build(f)?;
let (x, y) = (b.param(0)?, b.param(1)?);
let ok = b.create_block(&[Type::I32])?;
let bad = b.create_block(&[Type::U32])?;
let policy = Policy::overflow(Overflow::Wrap).with_div_zero(DivZero::Error);
let div = InstData::Binary { op: BinaryOp::Div, policy, args: [x, y] };
b.check(
    div,
    BlockCall::with_args(ok, &[BlockArg::Result(0)]),
    BlockCall::with_args(bad, &[BlockArg::ErrorCode]),
)?;
b.switch_to(ok)?;
let q = b.block_param(ok, 0)?;
b.ret(&[q])?;
b.switch_to(bad)?;
let minus_one = b.iconst(Type::I32, -1)?;
b.ret(&[minus_one])?;

let c = backend_lang::compile(&m, &Options::win64())?;
assert!(c.imports().is_empty()); // nothing traps: no runtime call needed
assert!(c.function("safe_div").is_some());
# Ok::<(), Box<dyn std::error::Error>>(())
```

### Machine IR directly

`emit` is the lower-level path: a function on physical registers in, x86-64 machine code out.

```rust
use backend_lang::mir::{Effects, Function};
use backend_lang::x86_64::{op, SYSV, RAX, RDI};

// fn double(x) -> x + x under SysV: x arrives in rdi, the result leaves in rax.
let mut f = Function::new("double", &SYSV);
let entry = f.block()?;
f.add_param(entry, RDI.into())?;
f.push(entry, op::MOV64, &[RAX.into(), RDI.into()])?;
f.push(entry, op::ADD64, &[RAX.into(), RAX.into(), RDI.into()])?;
f.push_with(entry, op::RET, &[], Effects::new().uses(&[RAX]))?;
f.omit_frame_pointer();

let code = backend_lang::emit(&f)?;
// mov rax, rdi; add rax, rdi; ret
assert_eq!(code, [0x48, 0x89, 0xF8, 0x48, 0x01, 0xF8, 0xC3]);
# Ok::<(), Box<dyn std::error::Error>>(())
```

### Edges are parallel copies

A loop whose back edge swaps its two values. The resolver sees the 2-cycle and emits one `xchg`; a cycle through memory would borrow a register instead:

```rust
use backend_lang::mir::{Effects, Function, Operand};
use backend_lang::x86_64::{self, op, SYSV, RAX, RCX};
use isa_lang::x86_64::{decode_all, Cond};

let mut f = Function::new("swap_loop", &SYSV);
f.omit_frame_pointer();
let (entry, head, exit) = (f.block()?, f.block()?, f.block()?);
f.add_param(head, RAX.into())?;
f.add_param(head, RCX.into())?;
let start = f.block_call(head, &[Operand::Imm(1), Operand::Imm(2)])?;
f.push(entry, op::JMP, &[start.into()])?;
f.push(head, op::CMP64, &[RAX.into(), Operand::Imm(2)])?;
let again = f.block_call(head, &[RCX.into(), RAX.into()])?; // swapped
let done = f.block_call(exit, &[])?;
f.push(head, op::JCC, &[x86_64::cond(Cond::Ne).into(), again.into(), done.into()])?;
f.push_with(exit, op::RET, &[], Effects::new().uses(&[RAX]))?;

let code = backend_lang::emit(&f)?;
let text: Vec<String> = decode_all(&code)
    .filter_map(Result::ok)
    .map(|(_, d)| d.inst().to_string())
    .collect();
assert!(text.contains(&"xchg rax, rcx".to_string()));
# Ok::<(), Box<dyn std::error::Error>>(())
```

### Calling conventions and frames

```rust
use backend_lang::mir::{Function, SlotKind};
use backend_lang::target::abi::{ArgClass, ArgLoc};
use backend_lang::x86_64::{SYSV, WIN64, RCX, RDI, XMM1};

let mut locs = Vec::new();
SYSV.assign_args(&[ArgClass::Int, ArgClass::Float, ArgClass::Float], &mut locs);
assert_eq!(locs[0], ArgLoc::Reg(RDI));
assert_eq!(locs[2], ArgLoc::Reg(XMM1));

locs.clear();
let outgoing = WIN64.assign_args(&[ArgClass::Int; 6], &mut locs);
assert_eq!(locs[0], ArgLoc::Reg(RCX));
assert_eq!(locs[4], ArgLoc::Stack(32)); // above the 32 bytes of shadow space
assert_eq!(outgoing, 48);

// A SysV leaf with a small frame keeps it in the red zone below rsp.
let mut leaf = Function::new("leaf", &SYSV);
let slot = leaf.stack_slot(SlotKind::Local { size: 16, align: 8 })?;
let frame = leaf.frame_layout()?;
assert!(frame.uses_red_zone());
assert!(frame.slot_offset(slot).unwrap() < 0);
# Ok::<(), Box<dyn std::error::Error>>(())
```

<hr>
<br>

## Examples

Runnable programs in [`examples/`](./examples):

| Example | What it shows |
|---|---|
| [`compile`](./examples/compile.rs) | The Tier-1 path: a module with a loop, a checked division whose error edge returns a sentinel, and a caller, compiled for SysV; each function's allocated MIR and disassembly. `cargo run --example compile` |
| [`emit`](./examples/emit.rs) | Build a loop in MIR, print it, emit it, and disassemble the result. `cargo run --example emit` |
| [`moves`](./examples/moves.rs) | The parallel-move resolver on a copy mixing a register rotation, fan-out to a slot, a slot swap, and an immediate, with and without a free register. `cargo run --example moves` |

<hr>
<br>

## Performance

Library-only numbers (no process start, no object file), measured with the benchmarks in [`benches/`](./benches) on Windows x86_64, Rust stable, release profile. **The 0.3 numbers were taken while other builds shared the machine**: the unchanged `mir_100k/build` benchmark ran about 2.9× slower than its 0.2 measurement in the same session, so treat the table as an upper bound.

| Benchmark | What it measures | 0.2 | 0.3 (loaded machine) |
|---|---|---:|---:|
| `mir_100k/build` | Build a 100,000-instruction function (2,000 blocks, each ending in a branch with two four-value edges). | ~4.3 ms | ~12.6 ms |
| `mir_100k/verify` | Verify it. | ~1.3 ms | ~2.0 ms |
| `mir_100k/emit` | All of `emit`: verify, lay out the frame, resolve every edge, encode, relax branches. | ~10.5 ms | ~12.4 ms |
| `moves/permutation/4` | A random permutation of four registers. | ~150 ns | ~152 ns |
| `moves/edges_100k_x4` | 100,000 four-move edges through one reused resolver. | ~17 ms | ~22 ms |
| `compile_100k/compile` | `compile` on a 100,000-instruction ir-lang function (loops, branches, calls, memory; spills): validation, selection, allocation, emission. | — | ~175 ms (~0.57 M IR instructions/s) |
| `compile_100k/isel`, `/regalloc`, `/emit` | Each phase alone on the same function. | — | ~36 / ~174 / ~39 ms (measured separately; that the parts exceed the whole is the noise) |
| `compile_pressure/compile` | A generated 20,000-statement function under heavy register pressure. | — | ~512 ms |
| `quality/native` vs `quality/interp` | Compiled code against ir-lang's interpreter: recursive `fib(25)`, a 1,000,000-iteration loop, a 1 KiB memory kernel ×20. | — | 0.38 ms vs 73 ms; 0.53 ms vs 154 ms; 28 µs vs 4.2 ms |

On the same machine with less load, the ignored timing test (`cargo test --release --test differential timing_realistic -- --ignored --nocapture`) measured the whole pipeline at 0.88–0.99 M IR instructions/s on the 100,000-instruction function. The 1 M/s target is therefore not demonstrated; the allocator, which reruns liveness and coloring once per spill round, is most of the time.

Everything is linear except one sort when the resolver numbers a copy's locations (`O(n log n)`); a reused resolver allocates nothing once it has seen its largest copy.

```bash
cargo bench --bench bench
```

<hr>
<br>

## Design notes

- **Narrow integers stay narrow only where it matters.** An `i8`/`i16`/`i32` value lives in the low bits of a 64-bit register with the upper bits unspecified, exactly as x86-64's 32-bit instructions leave them; wherever the upper bits matter (comparisons, division, right shifts, overflow checks, widening, switch values, array indices, call boundaries) it is extended first by its signedness. `bool` is always exactly 0 or 1.
- **Division never faults.** Zero and `MIN / -1` are tested before `idiv` runs, so every OPS policy (wrap, trap, error edge) is honoured instead of a processor exception.
- **Coloring without an interference graph.** In strict SSA, interfering values have one definition dominating the other, so giving each value, in dominance order, a register no live value holds is a correct coloring. Spilling is spill-everywhere with rematerialization; constants and addresses are recomputed instead of stored.
- **Stack maps fall out of the IR.** ir-lang's statepoints list every reference live across a safepoint (IR.md §9), so the backend stores exactly those into root slots before the call and reloads the relocated values after it. No reference is ever in a register while a collector runs, and no backend liveness analysis of references is needed.
- **Block parameters, not phi nodes.** The same information as phis, kept on the edge where it is used: each edge's parallel copy is written down on that edge. ir-lang already uses block parameters, an SSA register allocator resolves each edge with the same resolver, and there is no phi list to keep in sync with predecessor order.
- **Implicit operands are data.** isa-lang 0.2 describes each form's explicit operands only. A register allocator needs every register an instruction touches, so this crate's instruction table records implicit uses, definitions, and clobbers per row, and calls and returns carry their ABI registers per instruction. The flags are a physical register: producers define it, consumers use it, and an instruction that may leave some flags unchanged (`inc`, a shift by zero) also uses it, so a flag value never appears to come from an instruction that might not have produced it.
- **The stack pointer does not move in the body.** Slots have fixed offsets from it; stack arguments are stores to `[rsp + offset]`; `push` and `pop` belong to prologues and epilogues only.
- **Edges split themselves.** A conditional branch whose taken edge carries moves branches on the negated condition around them, so critical edges never need an extra block.
- **No rented parts.** Encoding is isa-lang's and object files are object-lang's; both are this family's own.

<hr>
<br>

## Testing

```bash
cargo test                       # unit + integration + property + doctests
cargo test --no-default-features # no_std + alloc
cargo clippy --all-targets --all-features -- -D warnings
cargo bench --bench bench
```

The integration tests carry a small x86-64 interpreter ([`tests/common`](./tests/common/mod.rs)) that executes the bytes the emitter produces, written against isa-lang's decoder alone, so emitted programs and edges are checked on every platform, and a random generator of valid ir-lang programs ([`tests/common/irgen.rs`](./tests/common/irgen.rs)) whose compiled code is compared with ir-lang's reference interpreter ([`tests/differential.rs`](./tests/differential.rs)); on x86-64 Linux and Windows the same programs also run natively in-process ([`tests/native.rs`](./tests/native.rs)), and on Linux [`tests/executable.rs`](./tests/executable.rs) and [`tests/link.rs`](./tests/link.rs) run static executables as real processes. `BACKEND_DIFF_CASES=n` sets the number of programs per differential sweep. Every `rust` example in this README and in [`docs/API.md`](./docs/API.md) is compiled and run as a doctest.

<hr>
<br>

## Contributing

See [`REPS.md`](./REPS.md) for the engineering standards every change is held to, [`dev/DIRECTIVES.md`](./dev/DIRECTIVES.md) for the definition of done, and [`dev/ROADMAP.md`](./dev/ROADMAP.md) for the roadmap. Before a PR: `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, and `cargo test --all-features` must be clean.

<br>

<div id="license">
    <h2>License</h2>
    <p>Licensed under either of</p>
    <ul>
        <li><b>Apache License, Version 2.0</b> &mdash; <a href="./LICENSE-APACHE">LICENSE-APACHE</a></li>
        <li><b>MIT License</b> &mdash; <a href="./LICENSE-MIT">LICENSE-MIT</a></li>
    </ul>
    <p>at your option.</p>
</div>

<div align="center">
  <h2></h2>
  <sup>COPYRIGHT <small>&copy;</small> 2026 <strong>James Gober.</strong></sup>
</div>
