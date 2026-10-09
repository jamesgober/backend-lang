# backend-lang &mdash; API Reference

> Complete reference for every public item in `backend-lang`, with examples.
> **Status: 0.3 (pre-1.0).** The surface is designed across the 0.x series
> and frozen at `1.0.0`. See [`../dev/ROADMAP.md`](../dev/ROADMAP.md).

<sub>Copyright &copy; 2026 <strong>James Gober</strong>.</sub>

## Table of contents

- [Overview](#overview)
- [Installation](#installation)
- [Quick start](#quick-start)
- [Concepts](#concepts)
  - [The pipeline](#the-pipeline)
  - [Value representation](#value-representation)
  - [Policies, errors, and traps](#policies-errors-and-traps)
  - [Calls, safepoints, and stack maps](#calls-safepoints-and-stack-maps)
  - [Machine IR](#machine-ir)
  - [Registers and classes](#registers-and-classes)
  - [Operand rules and implicit operands](#operand-rules-and-implicit-operands)
  - [Block parameters and parallel copies](#block-parameters-and-parallel-copies)
  - [Frames](#frames)
  - [Emission](#emission)
- [`compile`](#compile)
  - [`Options`](#options)
  - [`Compiled`, `DataObject`](#compiled)
  - [`CompileError`](#compileerror)
  - [`compile_function`](#compile_function)
- [`isel`](#isel)
  - [`select`, `IselError`](#select)
- [`regalloc`](#regalloc)
  - [`allocate`, `allocate_with`](#allocate)
  - [`Options`, `Allocation`, `Location`](#regalloc-options)
  - [`RegAllocError`](#regallocerror)
- [`emit`](#emit)
- [Registers: `PReg`, `VReg`, `Reg`, `RegClass`, `RegSet`](#registers)
- [`mir`](#mir)
  - [`Function`](#function)
  - [`Block`, `Inst`, `StackSlot`, `Symbol`, `CondCode`](#ids)
  - [`SlotKind`](#slotkind)
  - [`Operand`, `Mem`, `Scale`, `BlockCall`](#operand)
  - [`Effects`, `Implicit`, `ImplicitKind`](#effects)
  - [Call sites: `CallSite`](#callsite)
  - [`Function::verify_ssa`](#functionverify_ssa)
  - [`MirError`](#mirerror)
  - [Text form](#text-form)
- [`target`](#target-module)
  - [`Target`](#target)
  - [`TargetConfig`, `Isa`](#targetconfig)
  - [`RegInfo`, `ClassInfo`](#reginfo-and-classinfo)
  - [`Opcode`, `InstDesc`, `InstKind`](#opcode-and-instdesc)
  - [`OperandRule`, `Accept`, `ImmRange`](#operandrule)
  - [`MoveCaps`](#movecaps)
- [`target::abi`](#abi-module)
  - [`Abi`](#abi)
  - [`RegSeq`, `StackRules`](#regseq-and-stackrules)
  - [`ArgClass`, `ArgLoc`, `AbiError`](#argclass-argloc-abierror)
- [`target::frame`](#frame-module)
  - [`Abi::layout_frame`](#abilayout_frame)
  - [`FrameInput`, `FrameLayout`, `FrameStep`, `FrameError`](#framelayout)
- [`moves`](#moves)
  - [`MoveResolver`](#moveresolver)
  - [`Move`, `Loc`, `Source`, `Step`](#move)
  - [`MoveEnv`, `Resolved`, `MoveError`](#moveenv)
  - [`resolve`](#resolve)
- [`x86_64`](#x86_64)
  - [Registers, classes, conventions](#x86-64-registers)
  - [`op`: the instruction table](#op)
  - [`Emitter`](#emitter)
  - [`Program`, `FunctionInfo`](#program)
  - [`EmittedFunction`, `CallSiteRecord`](#emittedfunction)
  - [`Reloc`, `RelocKind`](#reloc)
  - [`EmitError`](#emiterror)
- [Feature flags](#feature-flags)
- [Limits](#limits)
- [What 0.3 does not do](#what-03-does-not-do)

## Overview

| Item | Kind | Purpose |
|---|---|---|
| [`compile`](#compile) | fn | **Tier 1.** An ir-lang module to x86-64 code, symbols, relocations, and data. |
| [`Options`](#options) | struct | Convention, frame pointer, runtime symbols, switch and allocation settings. |
| [`Compiled`](#compiled) | struct | The compiled module. |
| [`isel::select`](#select) | fn | One ir-lang function to MIR on virtual registers. |
| [`regalloc::allocate`](#allocate) | fn | Virtual registers to physical registers and stack slots. |
| [`emit`](#emit) | fn | Emit one physical-register function to x86-64 machine code. |
| [`mir::Function`](#function) | struct | A machine function: blocks of target instructions. |
| [`target::Target`](#target) | struct | An ISA as data: registers, classes, instructions. |
| [`target::abi::Abi`](#abi) | struct | A calling convention; lays out frames. |
| [`moves::MoveResolver`](#moveresolver) | struct | Sequentializes parallel copies. |
| [`x86_64::Emitter`](#emitter) | struct | Emits several functions into one code buffer. |
| [`x86_64::TARGET`](#x86-64-registers), [`SYSV`](#x86-64-registers), [`WIN64`](#x86-64-registers) | statics | The x86-64 description and its two conventions. |

## Installation

```toml
[dependencies]
backend-lang = "0.3"
```

For `no_std` targets (the crate needs only `alloc`):

```toml
[dependencies]
backend-lang = { version = "0.3", default-features = false }
```

ir-lang is re-exported as `backend_lang::ir_lang`, so the module type the
backend reads is always the one it was built against.

## Quick start

```rust
use backend_lang::Options;
use ir_lang::{Linkage, Module, Overflow, Signature, Type};

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
# Ok::<(), Box<dyn std::error::Error>>(())
```

On MIR you write yourself, physical registers and all:

```rust
use backend_lang::mir::{Effects, Function, Operand};
use backend_lang::x86_64::{op, SYSV, RAX};

let mut f = Function::new("answer", &SYSV);
let b = f.block()?;
f.push(b, op::MOV32RI, &[RAX.into(), Operand::Imm(42)])?;
f.push_with(b, op::RET, &[], Effects::new().uses(&[RAX]))?;
f.omit_frame_pointer();
assert_eq!(backend_lang::emit(&f)?, [0xB8, 42, 0, 0, 0, 0xC3]);
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Concepts

### The pipeline

[`compile`](#compile) validates the module, then for each defined function:

1. **Selection** ([`isel::select`](#select)): ir-lang blocks and block
   parameters map one to one to MIR blocks and parameters; each instruction
   becomes target instructions on virtual registers, with physical registers
   only in short block-local ranges (argument and result registers around
   calls and returns, `rdx:rax` around a division, `rcx` for a shift).
2. **Allocation** ([`regalloc::allocate`](#allocate)): virtual registers to
   physical registers, spilling to stack slots where needed; every edge's
   arguments stay a parallel copy.
3. **Emission** ([`Emitter`](#emitter)): frames, parallel copies, encoding,
   branch relaxation, calls between the module's functions, and relocations
   for everything defined elsewhere.

### Value representation

Every integer, `bool`, `ptr`, and `ref` value lives in one general-purpose
register. A value narrower than 64 bits occupies the low bits with the bits
above its width **unspecified**, exactly as x86-64's 32-bit instructions
leave them; arithmetic on 8- and 16-bit types runs at 32 bits. Wherever the
upper bits matter (comparisons, division, right shifts, overflow checks,
widening conversions, switch values, array indices) the value is extended
first by its type's signedness. `bool` is always exactly 0 or 1; booleans
from outside (parameters, call results, loads) are normalized. At call
boundaries 8- and 16-bit values are extended to 32 bits, as C compilers
expect; 32-bit values follow the platform rule (upper half unspecified).

### Policies, errors, and traps

Every integer operation is selected under every policy ir-lang can carry:

- `wrap` computes modulo the width;
- `trap` branches to a per-function trap block that calls
  [`Options::trap_symbol`](#options) with the OPS code in the first argument
  register (followed by `ud2`, in case the handler returns);
- `error` (only inside a `check` terminator) branches to the error edge,
  passing the OPS code as `err`.

Overflow is read from the flags at 32 and 64 bits and from an exact 32-bit
result plus a range check at 8 and 16 bits; unsigned multiplication checks
the high half. Division tests zero and then `MIN / -1` before it divides, so
the processor never faults. Shifts mask the amount (`mask`) or check it
(`error`: `0 <= n < width`, a negative signed amount failing too).

### Calls, safepoints, and stack maps

A call places stack arguments at `[rsp + offset]` in the caller's outgoing
area, register arguments by one parallel copy, calls, and copies the results
out of the return registers. The convention comes from the signature's tag:
`sysv64` and `win64` as named, `c` and `fast` as [`Options::abi`](#options).

**Stack maps** follow ir-lang's statepoint model (IR.md §9): at a safepoint
(a call or `invoke` that is not `nogc`, or a `safepoint` instruction) the
references live across it are exactly its GC list. Each is stored into a
root slot before the call and reloaded after it as the relocated value, so
no reference is in a register while a collector runs. The call's
[`CallSiteRecord`](#emittedfunction) lists the root slots as offsets from
`rsp` at the call, keyed by the return address; a collector reads and
rewrites the references there.

**`invoke`** records its landing pad on the call site. The unwinder enters
the pad's stub with the frame as it was at the call (callee-saved registers
restored) and the exception payload in `rax`; the stub reloads the relocated
references and jumps to the IR landing pad. This release produces no unwind
tables (see [What 0.3 does not do](#what-03-does-not-do)).

### Machine IR

A [`Function`](#function) is a list of blocks; the first is the entry. Each
block holds instructions of the function's target and ends with exactly one
terminator (`jmp`, `jcc`, `ret`, `ud2`). An instruction is an
[`Opcode`](#opcode-and-instdesc) with explicit operands, definitions first,
plus optional per-instruction implicit operands ([`Effects`](#effects)).
Every table (blocks, instructions, operands, slots, symbols, virtual
registers) is a dense vector indexed by a 32-bit id; nothing is recursive.

Before register allocation, instructions use virtual registers in SSA form:
each is defined once (by an instruction or as a block parameter), which
[`Function::verify`](#functionverify) checks. After allocation every register
is physical and the function can be emitted.

### Registers and classes

A [`PReg`](#registers) is an index into the target's register file, a
[`VReg`](#registers) an index into the function's virtual-register table,
and a [`Reg`](#registers) either one in a `u32`. A [`RegClass`](#registers)
types a virtual register and names the physical registers it may live in.
x86-64 has three classes: `gpr` (the sixteen general-purpose registers,
fourteen allocatable), `vec` (`xmm0`-`xmm15`), and `flags` (the flags
register, never allocated).

### Operand rules and implicit operands

Each instruction's [`InstDesc`](#opcode-and-instdesc) lists an
[`OperandRule`](#operandrule) per explicit operand: a definition of a class,
a use that accepts some shapes ([`Accept`](#operandrule): register, memory,
an immediate range, a symbol), a use **tied** to a definition (the
two-address constraint: `%d = add64 %a, src` encodes `add d, src` with
`d == a`), a condition code, or a branch target.

The registers an instruction touches without naming them are part of its
description: `idiv64` reads `rax`/`rdx` and writes `rax`/`rdx`/flags; a
shift by `cl` reads `rcx`; `syscall` clobbers `rcx`/`r11`. Calls and returns
carry their convention's registers per instruction through [`Effects`](#effects).

### Block parameters and parallel copies

A branch passes arguments to its target's parameters, and on the edge every
parameter receives its argument at once: a parallel copy. `copy` and `pcopy`
are explicit copies inside a block. All three go through the
[`MoveResolver`](#moveresolver), which handles cycles, fan-out, registers and
stack slots mixed, and immediates, with the fewest moves under its rules.

### Frames

[`Abi::layout_frame`](#abilayout_frame) places a function's stack slots,
decides which callee-saved registers to save, keeps the stack 16-aligned at
calls, reserves Win64 shadow space, uses the SysV red zone for small leaves,
and probes frames of a page or more. The result is a
[`FrameLayout`](#framelayout): slot offsets from the stack pointer and the
prologue and epilogue as [`FrameStep`](#framelayout)s.

### Emission

The [`Emitter`](#emitter) checks a function (verified, physical registers
only, tied operands equal, the stack and frame pointer never written by the
body), resolves every parallel copy, lays out the frame, and encodes each
instruction through isa-lang. A conditional branch whose taken edge carries
moves branches around them on the negated condition, so critical edges need
no extra block. Functions call each other by name; [`Emitter::finish`](#emitter)
patches the calls.

## `compile`

```text
pub fn compile(module: &ir_lang::Module, options: &Options) -> Result<Compiled, CompileError>
```

Compiles every defined function of `module` into one code buffer:
validation, instruction selection, register allocation, and emission. Calls
between the module's functions are resolved; imports, globals, and the
runtime entry points the code calls are left as [`Reloc`](#reloc)s (a symbol
the module defines itself is resolved, so a module may provide its own trap
handler). Globals become [`DataObject`](#compiled)s.

**Errors:** [`CompileError`](#compileerror), naming the function.

```rust
use backend_lang::{Options, x86_64::RelocKind};
use ir_lang::{Global, Linkage, Module, Signature, Type};

let mut m = Module::new("m");
let counter = m.declare_global("counter", Global::bytes(&[5, 0, 0, 0], 4).mutable())?;
let f = m.declare_function("get", &Signature::new(&[], &[Type::I32]), Linkage::Export)?;
let mut b = m.build(f)?;
let p = b.global_addr(counter)?;
let v = b.load(Type::I32, p, ir_lang::MemFlags::new(4))?;
b.ret(&[v])?;

let c = backend_lang::compile(&m, &Options::sysv())?;
let r = &c.relocations()[0];
assert_eq!((r.kind, r.symbol.as_str()), (RelocKind::Pc32, "counter"));
assert_eq!(c.data()[0].name(), "counter");
# Ok::<(), Box<dyn std::error::Error>>(())
```

<a id="options"></a>

### `Options`

`#[non_exhaustive]`, `Clone`, `Copy`. Public fields, set with the `with_*`
builders:

| Field | Default | Meaning |
|---|---|---|
| `abi: &'static Abi` | host (`WIN64` on Windows, else `SYSV`) | The platform C convention, used for the `c` and `fast` tags and for runtime calls. |
| `frame_pointer: bool` | `true` | Keep `rbp` as a frame pointer; `false` gives it to the allocator. |
| `trap_symbol` | `"__lexersketch_trap"` | Called by `trap` and trapping policies with the OPS code; never returns. |
| `gc_poll_symbol` | `"__lexersketch_gc_poll"` | Called by `safepoint`; a safepoint with a stack map. |
| `resume_symbol` | `"__lexersketch_resume"` | Called by `resume` with the payload; never returns. |
| `jump_table_min_cases` | `4` | The fewest cases a switch needs for a jump table. |
| `jump_table_max_spread` | `3` | A table is used when the case range is at most this many entries per case (and at most 65,536 entries). |
| `regalloc: regalloc::Options` | default | Allocation settings. |

Constructors: `sysv()`, `win64()`, `host()`, `with_abi(&Abi)`; builders
`with_frame_pointer`, `with_runtime(trap, gc_poll, resume)`,
`with_jump_tables(min_cases, max_spread)`, `with_regalloc`. `Default` is
`host()`.

```rust
use backend_lang::Options;

let o = Options::win64().with_frame_pointer(false).with_jump_tables(u32::MAX, 1);
assert_eq!(o.abi.name(), "win64");
assert_eq!(o.jump_table_min_cases, u32::MAX); // switches always become compare trees
```

<a id="compiled"></a>

### `Compiled`, `DataObject`

`Compiled` (`Clone`, `PartialEq`, `Eq`):

| Method | Returns |
|---|---|
| `code()` | Every function's machine code, back to back, in module order. |
| `program()` | The [`Program`](#program) (code, symbols, functions, relocations). |
| `functions()` | Each [`EmittedFunction`](#emittedfunction): name, offset, size, frame, call-site records. |
| `function(name)` | One of them by name (`O(log n)`). |
| `relocations()` | The code's [`Reloc`](#reloc)s against symbols defined elsewhere, in code order. |
| `data()` | The module's globals (imports excluded), in module order. |
| `imports()` | The symbols the code and data refer to that the module does not define, in name order. |
| `exports()` | The functions and globals with `export` linkage, in name order. |

`DataObject`: `name()`, `size()`, `align()`, `mutable()` (else read-only
data), `exported()`, `bytes()` (`None` for zero-filled), `relocations()`
(`Abs64` relocations at offsets from the object's start, for addresses
stored in it).

```rust
use backend_lang::Options;
use ir_lang::{Global, Module};

let mut m = Module::new("m");
let _ = m.declare_global("answer", Global::bytes(&[42, 0, 0, 0], 4))?;
let c = backend_lang::compile(&m, &Options::sysv())?;
let d = &c.data()[0];
assert_eq!((d.name(), d.size(), d.align(), d.mutable()), ("answer", 4, 4, false));
# Ok::<(), Box<dyn std::error::Error>>(())
```

<a id="compileerror"></a>

### `CompileError`

`#[non_exhaustive]`, `Clone`, `PartialEq`, `Display`, `Error` (with
`source`).

| Variant | Meaning |
|---|---|
| `Invalid(ir_lang::ValidationError)` | The module does not validate. |
| `Select { function, error: IselError }` | Selection failed or refused a construct. |
| `Allocate { function, error: RegAllocError }` | Register allocation failed. |
| `Emit { function: Option<String>, error: EmitError }` | Emission failed (`None` when it belongs to no one function). |

```rust
use backend_lang::{CompileError, Options};
use ir_lang::{Linkage, Module, Signature, Type};

let mut m = Module::new("m");
let f = m.declare_function("f", &Signature::new(&[], &[Type::F64]), Linkage::Export)?;
let mut b = m.build(f)?;
let x = b.f64const(1.5)?;
b.ret(&[x])?;
let err = backend_lang::compile(&m, &Options::sysv()).unwrap_err();
assert!(matches!(err, CompileError::Select { .. }));
assert!(err.to_string().contains("P11")); // floating point waits on isa-lang SSE
# Ok::<(), Box<dyn std::error::Error>>(())
```

<a id="compile_function"></a>

### `compile_function`

```text
pub fn compile_function(module: &Module, func: FuncId, options: &Options) -> Result<mir::Function, CompileError>
```

Selects and allocates one function and returns its MIR on physical
registers, ready for [`emit`](#emit) or an [`Emitter`](#emitter); its text
form shows exactly what was selected and allocated.

```rust
use backend_lang::Options;
use ir_lang::{Linkage, Module, Signature, Type};

let mut m = Module::new("m");
let f = m.declare_function("seven", &Signature::new(&[], &[Type::I32]), Linkage::Export)?;
let mut b = m.build(f)?;
let k = b.iconst(Type::I32, 7)?;
b.ret(&[k])?;
let mir = backend_lang::compile_function(&m, f, &Options::sysv())?;
assert!(mir.to_string().contains("rax"));
# Ok::<(), Box<dyn std::error::Error>>(())
```

## `isel`

<a id="select"></a>

### `select`, `IselError`

```text
pub fn select(module: &Module, func: FuncId, options: &Options) -> Result<mir::Function, IselError>
```

Selects one function of a module (validated first) into a MIR function on
virtual registers, named after it, under the convention its signature's tag
selects. The result satisfies [`regalloc::allocate`](#allocate)'s
precondition.

What is selected:

| ir-lang | Selected as |
|---|---|
| `add sub mul` | `add`/`sub`/`imul` at 32 or 64 bits; overflow from OF/CF, from an exact 32-bit result at 8/16 bits, or from the high half for unsigned multiplication. Commutative operands are ordered so that an operand used only here is the one overwritten (a constant goes right, as an immediate). |
| `div rem floor_div floor_mod` | Zero and `MIN / -1` tested first; `div`/`idiv` through `rdx:rax`; floor variants fixed up with `cmov`. |
| `and or xor not neg abs min max` | ALU ops, `neg` + `cmov` for `abs`, `cmp` + `cmov` for `min`/`max`. |
| `shl shr` | By immediate when constant and in range, else by `cl`, masked or range-checked. |
| `eq ne lt le gt ge`, `select` | `cmp` fused into the branch or `cmov` that uses it once in the same block, else `setcc`. |
| Conversions | `int_cast` (checked by re-extension), `zext`, `sext`, `narrow`, `bitcast`, `bool_to_int`, `ptr_to_int`, `int_to_ptr`, `ref_to_ptr` (free), `char_from_u32` (two compares). |
| `load`, `store`, atomics | `mov`/`movzx`/`movsx` at every width; constant offsets folded into the memory operand; `atomic_load`/`atomic_store` as plain moves (TSO), `seq_cst` stores and `atomic_rmw xchg` as `xchg`, `fence seq_cst` as a locked `xchg` on a private slot. |
| Addresses | `stack_addr` (rounded up at run time for alignment above 16), `global_addr`/`func_addr` (RIP-relative `lea`), `field_addr`, `elem_addr` (scaled `lea` or `imul`), `ptr_offset`. |
| `memcpy memmove memset` | Eight-byte loops then byte loops; `memmove` picks the direction by address order. |
| `call`, `call_indirect`, `tail_call`, `safepoint` | As in [Calls, safepoints, and stack maps](#calls-safepoints-and-stack-maps); an indirect tail call goes through `r11`. |
| `jump br switch ret unreachable trap` | `jmp`, `jcc`; `switch` as a bounds-checked jump table when dense ([`Options`](#options)), else a balanced compare tree; `ret` through the return registers; `ud2`; a call to the trap handler. |
| `check`, `invoke`, `resume` | The operation with its failure branching to the error edge; a call with a landing pad; a call to `resume_symbol`. |

`IselError` (`#[non_exhaustive]`, `Clone`, `PartialEq`, `Display`,
`Error`):

| Variant | Meaning |
|---|---|
| `Invalid(ValidationError)` | The module does not validate. |
| `NoBody(FuncId)` | The handle is not in the module, or it is an import. |
| `Unsupported { func, what }` | A construct this release does not select; `what` names what it waits on. |
| `Mir(MirError)` | Building the MIR failed (a 32-bit table limit). |

Refused today, each as `Unsupported`: floating-point types and operations
(isa-lang SSE, ISSUES P11); `atomic_rmw` other than `xchg`, and `cmpxchg`
(`lock`-prefixed forms); the `aapcs64` convention (the AArch64 target);
32-bit pointer modules; more results than return registers (two under
SysV, one under Win64); a tail call needing more stack-argument space than
its caller received.

```rust
use backend_lang::Options;
use ir_lang::{Linkage, Module, Overflow, Signature, Type};

let mut m = Module::new("m");
let f = m.declare_function("inc", &Signature::new(&[Type::I64], &[Type::I64]), Linkage::Export)?;
let mut b = m.build(f)?;
let x = b.param(0)?;
let one = b.iconst(Type::I64, 1)?;
let y = b.add(x, one, Overflow::Wrap)?;
b.ret(&[y])?;
let mir = backend_lang::isel::select(&m, f, &Options::sysv())?;
assert!(mir.to_string().contains("add64"));
mir.verify_ssa()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## `regalloc`

SSA register allocation, in place: virtual registers to physical registers
and stack slots.

**Input:** a MIR function in SSA form (one definition per virtual register,
every use dominated by its definition: [`verify_ssa`](#functionverify_ssa))
in which physical registers appear only in block-local ranges, plus the
entry block's incoming arguments. [`isel::select`](#select) produces exactly
this. **Output:** the same function on physical registers, ready for
[`emit`](#emit).

**Algorithm.** Liveness by SSA path exploration (from each use outside its
definition's block, walk predecessors up to the definition), budgeted.
Constraints in one backward walk per block: last uses, physical registers
live after each instruction, and every register written while each value is
live (call clobbers, fixed-register definitions), which the value must
avoid. Coloring in dominance order: in strict SSA, interfering values have
one definition dominating the other, so giving each value, at its
definition, a register no live value holds is a correct coloring, with no
interference graph. Coalescing by affinity classes (union-find over copies,
two-address ties, and block parameters with their arguments) and hints
toward the fixed registers values are copied to or from; best effort. When
no register is free at a definition, the lightest of the definition and the
values in usable registers (uses per instruction spanned) is spilled for its
whole life: constants and addresses are rematerialized at each use, other
values stored once after their definition and read from the slot directly
where the instruction takes a memory operand, else through a reload
temporary. The allocation then reruns; after [`MAX_ROUNDS`](#regalloc-options)
rounds every remaining long-lived value is spilled, which always succeeds.
Callee-saved registers are ordinary registers: a value live across a call can
only sit in one, and the frame saves exactly those written.

<a id="allocate"></a>

### `allocate`, `allocate_with`

```text
pub fn allocate(f: &mut Function) -> Result<Allocation, RegAllocError>
pub fn allocate_with(f: &mut Function, options: &Options) -> Result<Allocation, RegAllocError>
```

**Errors:** [`RegAllocError`](#regallocerror); `f` is unspecified after an
error.

```rust
use backend_lang::mir::{Effects, Function};
use backend_lang::regalloc;
use backend_lang::x86_64::{op, GPR, SYSV, RAX, RDI, RSI};

// fn add(a, b) -> a + b, on virtual registers.
let mut f = Function::new("add", &SYSV);
f.omit_frame_pointer();
let b = f.block()?;
f.add_param(b, RDI.into())?;
f.add_param(b, RSI.into())?;
let (x, y, s) = (f.vreg(GPR)?, f.vreg(GPR)?, f.vreg(GPR)?);
f.push(b, op::PCOPY, &[x.into(), y.into(), RDI.into(), RSI.into()])?;
f.push(b, op::ADD64, &[s.into(), x.into(), y.into()])?;
f.push(b, op::COPY, &[RAX.into(), s.into()])?;
f.push_with(b, op::RET, &[], Effects::new().uses(&[RAX]))?;
let a = regalloc::allocate(&mut f)?;
assert_eq!((a.rounds(), a.spilled()), (1, 0));
let code = backend_lang::emit(&f)?;
assert_eq!(code.last(), Some(&0xC3));
# Ok::<(), Box<dyn std::error::Error>>(())
```

<a id="regalloc-options"></a>

### `Options`, `Allocation`, `Location`

`regalloc::Options` (`#[non_exhaustive]`, `Copy`): `live_budget` (default
[`DEFAULT_LIVE_BUDGET`] = 2^24 live-set entries, 64 MiB) and
`keep_assignment` (keep a copy of the final virtual function and its
locations, for checking an allocation independently). Builders
`new()`, `with_live_budget(n)`, `keep_assignment(bool)`.

`Allocation`: `rounds()` (1 when nothing spilled), `spilled()`,
`rematerialized()`, `max_pressure()` (the most values of one class live at
once in the final round), and `assignment()` (with `keep_assignment`: the
final virtual function and each virtual register's `Location`).

`Location` (`#[non_exhaustive]`): `None` (never live), `Reg(PReg)`,
`Slot(StackSlot)`.

`MAX_ROUNDS` = 6: after this many rounds that still spill, every long-lived
value is spilled at once.

```rust
use backend_lang::mir::Operand;
use backend_lang::mir::Function;
use backend_lang::regalloc::{self, Location, Options};
use backend_lang::x86_64::{op, GPR, SYSV};

let mut f = Function::new("f", &SYSV);
let b = f.block()?;
let v = f.vreg(GPR)?;
f.push(b, op::MOV64RI, &[v.into(), Operand::Imm(7)])?;
f.push(b, op::CMP64, &[v.into(), Operand::Imm(1)])?;
f.push(b, op::RET, &[])?;
let a = regalloc::allocate_with(&mut f, &Options::new().keep_assignment(true))?;
let (_, locs) = a.assignment().unwrap();
assert!(matches!(locs[0], Location::Reg(_)));
# Ok::<(), Box<dyn std::error::Error>>(())
```

<a id="regallocerror"></a>

### `RegAllocError`

`#[non_exhaustive]`, `Copy`, `Display`, `Error`.

| Variant | Meaning |
|---|---|
| `Mir(MirError)` | The function is malformed or not in SSA form. |
| `PhysLiveIn { block, reg }` | A physical register is live into a block other than through the entry's parameters. |
| `PhysClobbered { block, reg }` | An instruction writes a physical register holding a live value (two fixed-register ranges overlap). |
| `TooLarge` | The live sets exceed `live_budget`. |
| `NoRegister { class }` | No register of the class can hold a value even after spilling everything spillable. |

```rust
use backend_lang::mir::{Function, Operand};
use backend_lang::regalloc::{self, Options, RegAllocError};
use backend_lang::x86_64::{op, GPR, SYSV};

let mut f = Function::new("f", &SYSV);
let (a, b) = (f.block()?, f.block()?);
let v = f.vreg(GPR)?;
f.push(a, op::MOV64RI, &[v.into(), Operand::Imm(1)])?;
let to_b = f.block_call(b, &[])?;
f.push(a, op::JMP, &[to_b.into()])?;
f.push(b, op::CMP64, &[v.into(), Operand::Imm(0)])?;
f.push(b, op::RET, &[])?;
let err = regalloc::allocate_with(&mut f, &Options::new().with_live_budget(0));
assert_eq!(err.unwrap_err(), RegAllocError::TooLarge);
# Ok::<(), backend_lang::mir::MirError>(())
```

## `emit`

```text
pub fn emit(f: &mir::Function) -> Result<Vec<u8>, x86_64::EmitError>
```

Emits one function and returns its machine code. A function may call itself
by its own name; calls to other functions need an [`Emitter`](#emitter).

**Errors:** [`EmitError`](#emiterror).

```rust
use backend_lang::mir::{Function, Operand};
use backend_lang::x86_64::{op, SYSV, RAX};

let mut f = Function::new("one", &SYSV);
let b = f.block()?;
f.push(b, op::MOV32RI, &[RAX.into(), Operand::Imm(1)])?;
f.push(b, op::RET, &[])?;
// push rbp; mov rbp, rsp; mov eax, 1; pop rbp; ret
assert_eq!(backend_lang::emit(&f)?, [0x55, 0x48, 0x89, 0xE5, 0xB8, 1, 0, 0, 0, 0x5D, 0xC3]);
# Ok::<(), Box<dyn std::error::Error>>(())
```

<a id="registers"></a>

## Registers

| Item | Description |
|---|---|
| `PReg` | A physical register. `PReg::new(index)` (`None` past `MAX_PREGS`), `index()`. |
| `VReg` | A virtual register. `index()`; `VReg::MAX` is the per-function limit (2^31 - 1). |
| `Reg` | Either kind: `From<PReg>`, `From<VReg>`, `as_phys()`, `as_virt()`, `is_virtual()`. |
| `RegClass` | A class: `RegClass::new(index)`, `index()`. |
| `RegSet` | A 128-bit set of physical registers: `EMPTY`, `of(&[..])`, `with`, `without`, `contains`, `union`, `intersect`, `difference`, `is_empty`, `len`, `first`, `iter` (index order). `FromIterator<PReg>`. |
| `RegSetIter` | The iterator of `RegSet::iter`. |
| `MAX_PREGS` | 128: the most physical registers a target may describe. |

```rust
use backend_lang::{Reg, RegSet};
use backend_lang::mir::Function;
use backend_lang::x86_64::{GPR, SYSV, RAX, RDX};

let set = RegSet::of(&[RDX, RAX]);
assert_eq!(set.iter().collect::<Vec<_>>(), [RAX, RDX]);

let mut f = Function::new("f", &SYSV);
let v: Reg = f.vreg(GPR)?;
assert!(v.is_virtual());
assert_eq!(Reg::from(RAX).as_phys(), Some(RAX));
# Ok::<(), backend_lang::mir::MirError>(())
```

## `mir`

### `Function`

```text
impl Function {
    pub fn new(name: &str, abi: &'static Abi) -> Function;
    pub fn with_capacity(name: &str, abi: &'static Abi, insts: usize, operands: usize) -> Function;
    pub fn name(&self) -> &str;
    pub fn abi(&self) -> &'static Abi;
    pub fn target(&self) -> &'static Target;

    pub fn vreg(&mut self, class: RegClass) -> Result<Reg, MirError>;
    pub fn vreg_class(&self, vreg: VReg) -> Option<RegClass>;
    pub fn vreg_count(&self) -> usize;
    pub fn reg_class(&self, reg: Reg) -> Option<RegClass>;

    pub fn block(&mut self) -> Result<Block, MirError>;
    pub fn entry(&self) -> Option<Block>;
    pub fn blocks(&self) -> impl ExactSizeIterator<Item = Block>;
    pub fn block_count(&self) -> usize;
    pub fn add_param(&mut self, block: Block, param: Operand) -> Result<(), MirError>;
    pub fn params(&self, block: Block) -> &[Operand];

    pub fn stack_slot(&mut self, kind: SlotKind) -> Result<StackSlot, MirError>;
    pub fn spill_slot(&mut self, class: RegClass) -> Result<StackSlot, MirError>;
    pub fn slot(&self, slot: StackSlot) -> Option<SlotKind>;
    pub fn slots(&self) -> &[SlotKind];
    pub fn symbol(&mut self, name: &str) -> Result<Symbol, MirError>;
    pub fn symbol_name(&self, symbol: Symbol) -> Option<&str>;

    pub fn block_call(&mut self, block: Block, args: &[Operand]) -> Result<BlockCall, MirError>;
    pub fn args(&self, call: BlockCall) -> &[Operand];
    pub fn push(&mut self, block: Block, opcode: Opcode, operands: &[Operand]) -> Result<Inst, MirError>;
    pub fn push_with(&mut self, block: Block, opcode: Opcode, operands: &[Operand], effects: Effects<'_>) -> Result<Inst, MirError>;

    pub fn insts(&self, block: Block) -> &[Inst];
    pub fn inst_count(&self) -> usize;
    pub fn opcode(&self, inst: Inst) -> Option<Opcode>;
    pub fn inst_block(&self, inst: Inst) -> Option<Block>;
    pub fn operands(&self, inst: Inst) -> &[Operand];
    pub fn implicit(&self, inst: Inst) -> &[Implicit];
    pub fn clobbers(&self, inst: Inst) -> RegSet;

    pub fn reserve_outgoing(&mut self, bytes: u32);
    pub fn outgoing(&self) -> u32;
    pub fn omit_frame_pointer(&mut self);
    pub fn frame_pointer(&self) -> bool;
    pub fn reserved(&self) -> RegSet;
    pub fn has_calls(&self) -> bool;
    pub fn written_regs(&self) -> RegSet;
    pub fn frame_layout(&self) -> Result<FrameLayout, FrameError>;
    pub fn verify(&self) -> Result<(), MirError>;
}
impl Display for Function; // the text form
```

A function belongs to a calling convention, which fixes its target. Ids from
another function are rejected or read as empty, never a panic.

- **`push` / `push_with`** check the operands against the opcode's
  description at once: count, shapes, classes, immediate ranges, memory
  operand limit, branch argument counts and classes against the target
  block's parameters (so add a block's parameters before branches to it;
  `verify` re-checks). Nothing is appended on error.
- **`add_param`** takes a register or a whole stack slot (`Operand::slot`).
  The entry block's parameters are the function's incoming values.
- **`block_call`** takes registers, whole slots, and immediates as arguments.
- **`reserve_outgoing`** keeps the largest outgoing argument area requested;
  stack arguments are stored at `[rsp + offset]` before a call.
- **`omit_frame_pointer`** frees `rbp` for the body (it is then saved like any
  callee-saved register it writes). The frame pointer is kept by default.
- **`written_regs`** is every physical register the body may write: explicit
  and implicit definitions, clobbers, and physical block parameters.

<a id="functionverify"></a>

#### `Function::verify`

Checks every instruction against its opcode, that every block ends with
exactly one terminator, and that every virtual register is defined at most
once. Linear time. Dominance (every use defined on every path) is checked
by [`verify_ssa`](#functionverify_ssa), the register allocator's
precondition.

**Errors:** the first [`MirError`](#mirerror), in block order.

```rust
use backend_lang::mir::{Function, MirError, Operand};
use backend_lang::x86_64::{op, GPR, SYSV};

let mut f = Function::new("f", &SYSV);
let (b0, b1) = (f.block()?, f.block()?);
let v = f.vreg(GPR)?;
f.add_param(b1, v.into())?;
let call = f.block_call(b1, &[Operand::Imm(3)])?;
f.push(b0, op::JMP, &[call.into()])?;
f.push(b1, op::RET, &[])?;
f.verify()?;
// An instruction after the terminator.
f.push(b0, op::NOP, &[])?;
assert!(matches!(f.verify(), Err(MirError::MisplacedTerminator { .. })));
# Ok::<(), MirError>(())
```

<a id="ids"></a>

### `Block`, `Inst`, `StackSlot`, `Symbol`, `CondCode`

Dense ids: `new(u32)` / `index()`, printed `block0`, `inst0`, `ss0`, `sym0`.
`CondCode::new(u8)` / `code()` is a target condition code; on x86-64 build
them with [`x86_64::cond`](#x86-64-registers).

### `SlotKind`

```text
#[non_exhaustive]
pub enum SlotKind {
    Local { size: u32, align: u32 },
    Incoming { offset: u32, size: u32 },
}
```

`Local` is space in the frame (alignment a power of two up to 16).
`Incoming` names a stack argument at `offset` above the canonical frame
address, as [`ArgLoc::Stack`](#argclass-argloc-abierror) reports it.
`size()` returns either's size.

<a id="operand"></a>

### `Operand`, `Mem`, `Scale`, `BlockCall`

```text
#[non_exhaustive]
pub enum Operand {
    Reg(Reg),
    Imm(i64),
    Mem(Mem),
    Slot { slot: StackSlot, offset: i32 },
    Symbol(Symbol),
    Cond(CondCode),
    Block(BlockCall),
}
```

- `Operand::slot(s)` is a whole slot (offset 0), the form parameters,
  arguments, and copies use. `reg()`, `is_mem()`. `From<Reg>`, `From<PReg>`,
  `From<Mem>`, `From<CondCode>`, `From<BlockCall>`, `From<Symbol>`.
- `Mem` is `[base + index * scale + disp]`: `Mem::base(reg)`,
  `Mem::absolute(disp)`, `with_index(reg, scale)`, `with_disp(disp)`,
  `base_reg()`, `index_reg()`, `disp()`. The access width comes from the
  instruction.
- `Scale` is `X1 X2 X4 X8` (`factor()`).
- `BlockCall` is a branch target with arguments, made by
  `Function::block_call`: `block()`, `len()`, `is_empty()`; the arguments are
  read with `Function::args`.

```rust
use backend_lang::mir::{Mem, Operand, Scale};
use backend_lang::x86_64::{RBX, RSI};

let m = Mem::base(RBX.into()).with_index(RSI.into(), Scale::X8).with_disp(16);
assert!(Operand::from(m).is_mem());
assert_eq!(m.index_reg(), Some((RSI.into(), Scale::X8)));
```

<a id="effects"></a>

### `Effects`, `Implicit`, `ImplicitKind`

`Effects` is the builder for an instruction's extra implicit operands:
`Effects::new().uses(&[..]).defs(&[..]).clobbers(set)`, read back with
`uses_regs()`, `defs_regs()`, `clobber_set()`. `Function::implicit(inst)`
returns them as `Implicit { reg, kind }` with `ImplicitKind::Use` or `Def`
(uses first); `Function::clobbers(inst)` the set.

```rust
use backend_lang::mir::{Effects, Function};
use backend_lang::x86_64::{op, SYSV, RAX, RDI};

let mut f = Function::new("caller", &SYSV);
let b = f.block()?;
let g = f.symbol("g")?;
let call = f.push_with(
    b,
    op::CALL,
    &[g.into()],
    Effects::new().uses(&[RDI]).defs(&[RAX]).clobbers(SYSV.call_clobbers()),
)?;
assert_eq!(f.implicit(call).len(), 2);
assert!(f.clobbers(call).contains(RAX));
# Ok::<(), backend_lang::mir::MirError>(())
```

<a id="callsite"></a>

### Call sites: `CallSite`

```text
impl Function {
    pub fn set_call_site(&mut self, call: Inst, gc_slots: &[StackSlot], landing_pad: Option<Block>) -> Result<(), MirError>;
    pub fn call_site(&self, inst: Inst) -> Option<CallSite<'_>>;
    pub fn call_sites(&self) -> impl ExactSizeIterator<Item = CallSite<'_>>;
}
```

A call may carry a record: the stack slots holding its GC roots while it
runs (its stack map) and the landing pad unwinding through it reaches. The
landing pad counts as a successor of the call's block for liveness and
dominance. `CallSite`: `inst()`, `gc_slots()`, `landing_pad()`. The emitter
turns each into a [`CallSiteRecord`](#emittedfunction).

**Errors:** `MirError::NotACall` if `call` is not a call;
`UnknownSlot`/`UnknownBlock` for foreign ids.

```rust
use backend_lang::mir::{Function, SlotKind};
use backend_lang::x86_64::{op, SYSV};

let mut f = Function::new("f", &SYSV);
let root = f.stack_slot(SlotKind::Local { size: 8, align: 8 })?;
let b = f.block()?;
let poll = f.symbol("gc_poll")?;
let call = f.push(b, op::CALL, &[poll.into()])?;
f.push(b, op::RET, &[])?;
f.set_call_site(call, &[root], None)?;
assert_eq!(f.call_site(call).unwrap().gc_slots(), [root]);
# Ok::<(), backend_lang::mir::MirError>(())
```

<a id="functionverify_ssa"></a>

### `Function::verify_ssa`

[`verify`](#functionverify), then SSA dominance: in every block reachable
from the entry, each use of a virtual register is dominated by its
definition (a use in the defining block comes after it; a branch argument
is a use at the end of its block). Linear: dominators by Lengauer and
Tarjan's algorithm (iterative, `O(m log n)` on any graph), then an `O(1)`
interval test per use on the dominator tree.

**Errors:** `verify`'s, then `MirError::UndefinedVReg` or
`MirError::NotDominated`.

```rust
use backend_lang::mir::{Function, MirError};
use backend_lang::x86_64::{op, GPR, SYSV};

let mut f = Function::new("f", &SYSV);
let (a, b) = (f.block()?, f.block()?);
let v = f.vreg(GPR)?;
let to_b = f.block_call(b, &[])?;
f.push(a, op::JMP, &[to_b.into()])?;
let w = f.vreg(GPR)?;
f.push(b, op::MOV64, &[w.into(), v.into()])?; // v is never defined
f.push(b, op::RET, &[])?;
assert!(matches!(f.verify_ssa(), Err(MirError::UndefinedVReg { .. })));
# Ok::<(), MirError>(())
```

### `MirError`

`#[non_exhaustive]`, `Copy`, `Display`, `Error`. Every variant names where
the problem is.

| Variant | Meaning |
|---|---|
| `UnknownOpcode { inst, opcode }` | The target has no such opcode. |
| `OperandCount { inst, expected, found }` | Wrong number of explicit operands (an odd number for `pcopy`). |
| `OperandShape { inst, index }` | An operand shape its position does not take. |
| `RegClass { inst, index }` | A register of the wrong class. |
| `ImmediateRange { inst, index, value }` | An immediate outside its position's range. |
| `TooManyMemOperands { inst }` | More memory operands than the target allows (one on x86-64). |
| `ArgCount { inst, block, expected, found }` | A branch's argument count differs from the target's parameters. |
| `ArgClass { inst, block, index }` | A branch argument of another class than its parameter. |
| `DuplicateDestination { inst }` | A parallel copy writes one location twice. |
| `BadParam { block }` / `BadArg { block }` | A parameter or argument of an unsupported shape. |
| `UnknownVReg`, `UnknownPReg`, `UnknownSlot`, `UnknownBlock`, `UnknownSymbol`, `UnknownClass` | An id the function or target does not have. |
| `MultipleDefs { vreg }` | A virtual register defined twice (SSA). |
| `NotACall { inst }` | A call-site record names an instruction that is not a call. |
| `UndefinedVReg { vreg }` | A virtual register used but never defined (`verify_ssa`). |
| `NotDominated { vreg, inst }` | A use not dominated by its definition (`verify_ssa`). |
| `MissingTerminator { block }` / `MisplacedTerminator { inst }` | Block structure. |
| `NoBlocks` | An empty function. |
| `Limit` | A table would exceed its 32-bit ids. |

### Text form

`Function` implements `Display` deterministically:

```text
function sum (sysv64) {
block0(%0:gpr):
    %4:gpr = mov64ri 0
    jmp block1(%4, %0)
block1(%1:gpr, %2:gpr):
    %5:gpr = add64 %1, %2
    %6:gpr = sub64 %2, 1
    jcc ne, block1(%5, %6), block2(%5)
block2(%3:gpr):
    rax = copy %3
    ret implicit(rax)
}
```

Definitions show their class, memory prints as `[base + index*8 - 16]`,
slots as `ss0` or `ss0+4`, symbols as `@name`, and per-instruction implicit
operands as `implicit(..)`, `implicit-def(..)`, `clobber(..)`. Slots,
symbols, and frame options (`omit-frame-pointer`, `outgoing N`) are listed
first.

<a id="target-module"></a>

## `target`

### `Target`

A target is static data. Accessors: `name`, `isa`, `regs`, `reg(p)` (a
placeholder named `<invalid>` for an index it lacks), `has_reg`, `classes`,
`class(c)` (an empty placeholder for an unknown class), `has_class`, `insts`,
`inst(op)` (`None` for an unknown opcode), `opcode(name)`, `stack_pointer`,
`frame_pointer`, `flags`, `addr_class`, `move_caps`, `max_mem_operands`,
`conds`, `cond_name(cond)`, `class_of(p)`.

`Target::new(name, isa, regs, classes, insts, config)` describes a target;
`Target::generic_insts()` returns the `copy` and `pcopy` descriptions every
instruction table starts with.

```rust
use backend_lang::x86_64::{self, op, GPR, RCX, FLAGS};

let t = &x86_64::TARGET;
assert_eq!(t.class(GPR).allocatable().len(), 14);
let shl = t.inst(op::SHL64CL).unwrap();
assert_eq!(shl.implicit_uses(), [RCX, FLAGS]);
assert_eq!(t.opcode("shl64cl"), Some(op::SHL64CL));
```

<a id="targetconfig"></a>

### `TargetConfig`, `Isa`

`TargetConfig` holds the registers with fixed jobs and target-wide limits
(`stack_pointer`, `frame_pointer`, `flags`, `addr_class`, `move_caps`,
`max_mem_operands`, `conds`); `TargetConfig::of(&target)` copies one to
derive a variant. `Isa` is `#[non_exhaustive]`, with `X86_64`.

The emitter only encodes functions whose target uses this crate's x86-64
register and instruction tables; a variant built on them (another
`MoveCaps`, say) emits, a target with other tables reports
`EmitError::UnsupportedTarget`.

### `RegInfo` and `ClassInfo`

`RegInfo::new(name, class, hw)`: `name()`, `class()`, `hw()` (the hardware
number). `ClassInfo::new(name, regs, allocatable, spill_size, spill_align)`:
`name()`, `regs()`, `allocatable()`, `spill_size()`, `spill_align()`.

<a id="opcode-and-instdesc"></a>

### `Opcode`, `InstDesc`, `InstKind`

`Opcode::new(u16)` / `index()`; `Opcode::COPY` and `Opcode::PCOPY` are
generic. `InstDesc::new(name, kind, operands, implicit_uses, implicit_defs,
clobbers)`: `name()`, `kind()`, `operands()`, `defs()`, `implicit_uses()`,
`implicit_defs()`, `clobbers()`. `InstKind` (`#[non_exhaustive]`): `Plain`,
`Copy`, `ParallelCopy`, `Jump`, `Branch`, `Return`, `Call`, `Trap`;
`is_terminator()`.

<a id="operandrule"></a>

### `OperandRule`, `Accept`, `ImmRange`

```text
#[non_exhaustive]
pub enum OperandRule { Def(RegClass), Tied(u8), Use(RegClass, Accept), Cond, Block }
```

`Accept` is built from `REG`, `MEM`, `RM`, `NONE` with `with_imm(range)` and
`with_symbol()`, read with `reg()`, `mem()`, `symbol()`, `imm()`.
`ImmRange` (`#[non_exhaustive]`): `None`, `I8`, `U8`, `I16`, `I32`, `I64`;
`fits(value)`. Immediates are signed values of the operation's width, as
isa-lang takes them: write `-1` for a 32-bit all-ones value.

```rust
use backend_lang::target::{Accept, ImmRange, OperandRule};
use backend_lang::x86_64::{self, op, GPR};

let add = x86_64::TARGET.inst(op::ADD32).unwrap();
assert_eq!(add.operands()[2], OperandRule::Use(GPR, Accept::RM.with_imm(ImmRange::I32)));
assert!(!ImmRange::I32.fits(1 << 31));
```

### `MoveCaps`

`MoveCaps::new(swap, mem_to_mem, store_imm)`: whether two registers can be
exchanged in one instruction, whether memory moves to memory directly, and
which immediates store to memory directly. x86-64: `xchg` yes, memory to
memory no, stores of sign-extended 32-bit immediates.

<a id="abi-module"></a>

## `target::abi`

### `Abi`

```text
impl Abi {
    pub const fn new(name, target, args: RegSeq, rets: RegSeq, callee_saved: RegSet, stack: StackRules) -> Abi;
    pub fn name(&self) -> &'static str;
    pub fn target(&self) -> &'static Target;
    pub fn args(&self) -> &RegSeq;
    pub fn rets(&self) -> &RegSeq;
    pub fn callee_saved(&self) -> RegSet;
    pub fn stack(&self) -> &StackRules;
    pub fn caller_saved(&self) -> RegSet;
    pub fn call_clobbers(&self) -> RegSet;
    pub fn assign_args(&self, args: &[ArgClass], out: &mut Vec<ArgLoc>) -> u32;
    pub fn assign_rets(&self, rets: &[ArgClass], out: &mut Vec<ArgLoc>) -> Result<(), AbiError>;
    pub fn layout_frame(&self, input: &FrameInput<'_>) -> Result<FrameLayout, FrameError>;
}
```

- **`assign_args`** appends a location per argument and returns the bytes of
  outgoing area the call needs (shadow space plus stack arguments; offsets
  saturate rather than wrap).
- **`assign_rets`** appends a register per return value, or fails with
  `AbiError::TooManyReturns` (appending nothing).
- **`caller_saved`** is every allocatable register the convention does not
  preserve; **`call_clobbers`** adds the flags: the clobber set of a call.

```rust
use backend_lang::target::abi::{ArgClass, ArgLoc};
use backend_lang::x86_64::{SYSV, WIN64, RAX, RDX, RSI, XMM6};

let mut rets = Vec::new();
SYSV.assign_rets(&[ArgClass::Int, ArgClass::Int], &mut rets)?;
assert_eq!(rets, [ArgLoc::Reg(RAX), ArgLoc::Reg(RDX)]);
assert!(SYSV.call_clobbers().contains(RSI));
assert!(!WIN64.call_clobbers().contains(RSI));
assert!(!WIN64.call_clobbers().contains(XMM6));
# Ok::<(), backend_lang::target::abi::AbiError>(())
```

### `RegSeq` and `StackRules`

`RegSeq::new(int, float, positional)`: the integer and float register lists
and whether value `i` may only take entry `i` (Win64) or each list has its
own counter (SysV). `StackRules::new(align, arg_slot, shadow_space,
red_zone, probe_interval)`.

| | SysV (`SYSV`) | Win64 (`WIN64`) |
|---|---|---|
| Integer arguments | `rdi rsi rdx rcx r8 r9` | `rcx rdx r8 r9` (by position) |
| Float arguments | `xmm0`-`xmm7` | `xmm0`-`xmm3` (by position) |
| Results | `rax rdx` / `xmm0 xmm1` | `rax` / `xmm0` |
| Callee-saved | `rbx rbp r12`-`r15` | `rbx rbp rdi rsi r12`-`r15 xmm6`-`xmm15` |
| Stack at calls | 16-aligned | 16-aligned |
| Shadow space | 0 | 32 |
| Red zone | 128 | 0 |
| Probe interval | 4096 (stack-clash protection) | 4096 (required) |

<a id="argclass-argloc-abierror"></a>

### `ArgClass`, `ArgLoc`, `AbiError`

`ArgClass` (`#[non_exhaustive]`): `Int`, `Float`. `ArgLoc`: `Reg(PReg)` or
`Stack(offset)`, the offset from the stack pointer at the call instruction
(the canonical frame address in the callee). `AbiError`
(`#[non_exhaustive]`): `TooManyReturns { index }`.

<a id="frame-module"></a>

## `target::frame`

<a id="abilayout_frame"></a>

### `Abi::layout_frame`

Lays out a frame: callee-saved registers the function writes are pushed
(vector ones saved into 16-byte frame slots), locals are placed largest
alignment first, the outgoing area (with Win64 shadow space when the
function calls) sits at the bottom, and the stack pointer ends 16-aligned
whenever the function calls or allocates. A SysV leaf whose locals fit the
red zone allocates nothing. Allocations of at least the probe interval are
probed page by page.

**Errors:** `FrameError::Alignment` (zero, not a power of two, or above 16),
`FrameError::OverlappingIncoming`, `FrameError::TooLarge` (any offset beyond
`i32`).

```rust
use backend_lang::RegSet;
use backend_lang::mir::{SlotKind, StackSlot};
use backend_lang::target::frame::{FrameInput, FrameStep};
use backend_lang::x86_64::{WIN64, RBP, RSI};

let slots = [SlotKind::Local { size: 24, align: 8 }];
let frame = WIN64.layout_frame(&FrameInput {
    slots: &slots,
    written: RegSet::of(&[RSI]),
    has_calls: true,
    outgoing: 0,
    frame_pointer: true,
})?;
assert_eq!(frame.prologue()[..3], [FrameStep::Push(RBP), FrameStep::SetFramePointer, FrameStep::Push(RSI)]);
// 32 bytes of shadow space below the local.
assert_eq!(frame.slot_offset(StackSlot::new(0)), Some(32));
# Ok::<(), backend_lang::target::frame::FrameError>(())
```

<a id="framelayout"></a>

### `FrameInput`, `FrameLayout`, `FrameStep`, `FrameError`

- `FrameInput { slots, written, has_calls, outgoing, frame_pointer }`:
  everything the layout depends on. `Function::frame_layout` fills it in.
- `FrameLayout`: `slot_offset(slot)` (from the stack pointer after the
  prologue; negative in the red zone), `pushed()`, `vec_saves()`,
  `prologue()`, `epilogue()`, `alloc()`, `cfa_offset()`, `frame_pointer()`,
  `uses_red_zone()`.
- `FrameStep` (`#[non_exhaustive]`): `Push(r)`, `SetFramePointer`,
  `Alloc { bytes, probe }`, `SaveVec { reg, offset }`,
  `RestoreVec { reg, offset }`, `Free(bytes)`, `Pop(r)`.
- `FrameError` (`#[non_exhaustive]`): `TooLarge`,
  `Alignment { slot, align }`, `OverlappingIncoming { slot }`.

## `moves`

### `MoveResolver`

```text
impl MoveResolver {
    pub fn new() -> MoveResolver;
    pub fn resolve(&mut self, target: &Target, moves: &[Move], env: &MoveEnv, out: &mut Vec<Step>) -> Result<Resolved, MoveError>;
}
```

Appends steps that perform the parallel copy: every destination ends with
its source's value from before the copy; every other location is unchanged
except the free registers and the scratch and victim slots. Moves whose
source is their destination are dropped (their register is still treated as
live). The resolver:

1. emits each move once nothing pending reads its destination (trees drain
   leaves first);
2. breaks each remaining cycle by reading a value already copied out of the
   cycle (free), by `k - 1` swaps when every location is a register and the
   target swaps, or through a scratch location (a free register of the class,
   else the scratch slot): `k + 1` moves;
3. makes each move legal: memory-to-memory moves and unstorable immediate
   stores go through a free register, else a borrowed register saved to the
   victim slot and restored.

Numbering locations costs one sort (`O(n log n)`); the rest is linear.
Buffers are reused across calls.

**Errors:** [`MoveError`](#moveenv).

```rust
use backend_lang::moves::{Move, MoveEnv, MoveResolver, Step};
use backend_lang::x86_64::{self, GPR, RAX, RBX, RCX};

let rotate = [Move::new(RAX, RBX, GPR), Move::new(RBX, RCX, GPR), Move::new(RCX, RAX, GPR)];
let mut out = Vec::new();
let used = MoveResolver::new().resolve(&x86_64::TARGET, &rotate, &MoveEnv::new(), &mut out)?;
assert_eq!(out.len(), 2); // two swaps
assert!(matches!(out[0], Step::Swap { .. }));
assert!(!used.scratch_slot);
# Ok::<(), backend_lang::moves::MoveError>(())
```

<a id="move"></a>

### `Move`, `Loc`, `Source`, `Step`

- `Move { dst: Loc, src: Source, class: RegClass }`; `Move::new(dst, src, class)`
  takes anything convertible (`PReg`, `StackSlot`, `i64`).
- `Loc`: `Reg(PReg)` or `Slot(StackSlot)`; `is_mem()`.
- `Source`: `Loc(Loc)` or `Imm(i64)`.
- `Step`: `Move { dst, src, class }` or `Swap { a, b, class }`.

<a id="moveenv"></a>

### `MoveEnv`, `Resolved`, `MoveError`

- `MoveEnv { free, scratch_slot, victim_slot, swap }`; `MoveEnv::new()` (no
  free registers, no slots, swaps allowed), `with_free`, `with_scratch_slot`,
  `with_victim_slot`, `with_swap`. Free registers named by a move are
  ignored.
- `Resolved { steps, scratch_slot, victim_slot }` (`#[non_exhaustive]`): what
  a resolution appended and used.
- `MoveError` (`#[non_exhaustive]`): `DuplicateDestination { index }`,
  `ClassMismatch { index }`, `NoScratch { class }`, `NoTemp { class }`,
  `TooMany`.

### `resolve`

`moves::resolve(target, moves, env) -> Result<Vec<Step>, MoveError>`: the
one-call path with a fresh resolver.

```rust
use backend_lang::mir::StackSlot;
use backend_lang::moves::{self, Move, MoveEnv, MoveError};
use backend_lang::x86_64::{self, GPR};

let (a, b) = (StackSlot::new(0), StackSlot::new(1));
let swap = [Move::new(a, b, GPR), Move::new(b, a, GPR)];
// A cycle through memory needs somewhere to put a value.
assert_eq!(moves::resolve(&x86_64::TARGET, &swap, &MoveEnv::new()), Err(MoveError::NoScratch { class: GPR }));
let env = MoveEnv::new().with_scratch_slot(StackSlot::new(2)).with_victim_slot(StackSlot::new(3));
assert!(moves::resolve(&x86_64::TARGET, &swap, &env).is_ok());
```

## `x86_64`

<a id="x86-64-registers"></a>

### Registers, classes, conventions

- Registers: `RAX RCX RDX RBX RSP RBP RSI RDI R8`-`R15` (indices 0-15, the
  hardware numbers), `XMM0`-`XMM15` (16-31), `FLAGS` (32).
- Classes: `GPR`, `VEC`, `FLAGS_CLASS`.
- `TARGET`: the description. `SYSV`, `WIN64`: the conventions.
- `cond(isa_lang::x86_64::Cond) -> CondCode`.

Vector registers are described so the conventions are complete; isa-lang
0.2 encodes no SSE instructions, so emitting a vector move or a Win64
vector save reports `EmitError::Unencodable`.

### `op`

The opcodes, each a constant whose documentation gives its MIR form. Rows
are one operation at one width; definitions come first; `%a` in a two-address
form is tied to the definition.

| Family | Opcodes |
|---|---|
| Copies | `COPY`, `PCOPY` |
| Moves and loads | `MOV32RI MOV64RI MOV32 MOV64 MOVZX8 MOVZX16 MOVSX8TO32 MOVSX8TO64 MOVSX16TO32 MOVSX16TO64 MOVSX32TO64` |
| Stores | `STORE8 STORE16 STORE32 STORE64` |
| Addresses | `LEA32 LEA64` (`LEA64` of a symbol: a function's address) |
| Arithmetic | `ADD SUB AND OR XOR ADC SBB` × `32 64` (register, memory, or 32-bit immediate source) |
| Compare | `CMP32 CMP64 TEST32 TEST64 BT32 BT64` |
| Unary | `INC DEC NEG NOT` × `32 64` |
| Multiply | `IMUL32 IMUL64 IMUL32RI IMUL64RI`; widening `MUL32 MUL64 IMULW32 IMULW64` |
| Divide | `DIV32 DIV64 IDIV32 IDIV64`; `CDQ CQO CDQE` |
| Shifts | `SHL SHR SAR ROL ROR` × `32 64`, by immediate or `…CL` |
| Conditions | `SETCC CMOV32 CMOV64` |
| Control | `JMP JCC CALL RET UD2 INT3 NOP SYSCALL` |
| Tail calls, tables, pads | `TAILCALL` (epilogue, then a jump to a symbol or register), `JMPTBL` (`%t = jmptbl %i, b0, b1, …`: a table of offsets after the function; `%i` already in range; `%t` written before `%i` is last read), `LANDINGPAD` (emits nothing; defines `rax`, where the payload arrives) |
| Atomics | `XCHG8 XCHG16 XCHG32 XCHG64` (with memory: implicitly locked, a full barrier) |

`SYSCALL` is emitted as the bytes `0f 05` (isa-lang 0.2 has no form for it).
`push` and `pop` are not MIR instructions: the stack pointer is fixed
between prologue and epilogue.

### `Emitter`

```text
impl Emitter {
    pub fn new() -> Emitter;
    pub fn with_capacity(bytes: usize) -> Emitter;
    pub fn function(&mut self, f: &Function) -> Result<FunctionInfo, EmitError>;
    pub fn finish(self) -> Result<Program, EmitError>;
}
```

`function` verifies and emits one function at the current end of the code;
`declare_external(name)` marks a name as defined elsewhere, so calls, tail
calls, and addresses of it become [`Reloc`](#reloc)s; `finish` relaxes
branches, patches calls and jump tables, and reports names called but
neither emitted nor declared external.

```rust
use backend_lang::mir::{Effects, Function, Operand};
use backend_lang::x86_64::{op, Emitter, SYSV, RAX};

let mut seven = Function::new("seven", &SYSV);
let b = seven.block()?;
seven.push(b, op::MOV32RI, &[RAX.into(), Operand::Imm(7)])?;
seven.push_with(b, op::RET, &[], Effects::new().uses(&[RAX]))?;

let mut main = Function::new("main", &SYSV);
let b = main.block()?;
let callee = main.symbol("seven")?;
main.push_with(b, op::CALL, &[callee.into()], Effects::new().defs(&[RAX]).clobbers(SYSV.call_clobbers()))?;
main.push_with(b, op::RET, &[], Effects::new().uses(&[RAX]))?;

let mut e = Emitter::new();
e.function(&main)?;
e.function(&seven)?;
let program = e.finish()?;
assert_eq!(program.symbol("main"), Some(0));
assert!(program.symbol("seven").unwrap() > 0);
# Ok::<(), Box<dyn std::error::Error>>(())
```

<a id="program"></a>

### `Program`, `FunctionInfo`

`Program`: `bytes()`, `into_bytes()`, `symbol(name)` (offset), `symbols()`
(name order). `FunctionInfo`: `frame()`, the laid-out frame (including any
hidden slots the edge moves used, numbered after the function's own).

<a id="emittedfunction"></a>

### `EmittedFunction`, `CallSiteRecord`

`Program::functions()` lists each `EmittedFunction` in emission order:
`name()`, `offset()`, `size()`, `frame()` (its [`FrameLayout`](#framelayout)),
and `call_sites()`. A `CallSiteRecord` has `return_offset()` (the offset of
the instruction after the call, from the start of the code: the key a stack
walker looks up), `gc_slots()` (the stack map: one 8-byte reference per
entry, at byte offsets from the caller's `rsp` at the call, before the return
address is pushed), and `landing_pad()` (the pad's offset, if any).

```rust
use backend_lang::mir::{Function, SlotKind};
use backend_lang::x86_64::{op, Emitter, SYSV};

let mut f = Function::new("f", &SYSV);
let root = f.stack_slot(SlotKind::Local { size: 8, align: 8 })?;
let b = f.block()?;
let g = f.symbol("f")?;
let call = f.push(b, op::CALL, &[g.into()])?;
f.push(b, op::RET, &[])?;
f.set_call_site(call, &[root], None)?;
let mut e = Emitter::new();
e.function(&f)?;
let p = e.finish()?;
let site = &p.functions()[0].call_sites()[0];
assert_eq!(site.gc_slots().len(), 1);
assert!(site.return_offset() > 0);
# Ok::<(), Box<dyn std::error::Error>>(())
```

<a id="reloc"></a>

### `Reloc`, `RelocKind`

`Reloc` (`#[non_exhaustive]`, public fields `offset`, `kind`, `symbol`,
`addend`): the bytes at `offset` receive what `kind` computes from the
symbol's address `S`, the addend `A`, and the patched place `P`; they hold
zero until linked. `RelocKind` (`#[non_exhaustive]`), with `size()`:

| Kind | Value | ELF | object-lang 0.2 |
|---|---|---|---|
| `Call32` | `S + A - P`, 32 bits (a `call`/`jmp` displacement) | `R_X86_64_PLT32` | `X86_64Reloc::Plt32` |
| `Pc32` | `S + A - P`, 32 bits (a RIP-relative address) | `R_X86_64_PC32` | `X86_64Reloc::Pc32` |
| `Abs64` | `S + A`, 64 bits (an address stored in data) | `R_X86_64_64` | `X86_64Reloc::Abs64` |

```rust
use backend_lang::mir::Function;
use backend_lang::x86_64::{op, Emitter, RelocKind, SYSV};

let mut f = Function::new("f", &SYSV);
f.omit_frame_pointer();
let b = f.block()?;
let puts = f.symbol("puts")?;
f.push(b, op::CALL, &[puts.into()])?;
f.push(b, op::RET, &[])?;
let mut e = Emitter::new();
e.declare_external("puts")?;
e.function(&f)?;
let p = e.finish()?;
let r = &p.relocations()[0];
assert_eq!((r.kind, r.symbol.as_str(), r.addend), (RelocKind::Call32, "puts", -4));
# Ok::<(), Box<dyn std::error::Error>>(())
```

### `EmitError`

`#[non_exhaustive]`, `Clone`, `Display`, `Error` (with `source`).

| Variant | Meaning |
|---|---|
| `Mir(MirError)` | The function does not verify. |
| `Frame(FrameError)` | The frame cannot be laid out. |
| `Moves(MoveError)` | A parallel copy cannot be resolved. |
| `UnsupportedTarget` | The target's tables are not this emitter's x86-64 tables. |
| `VirtualRegister { block, inst }` | A virtual register remains (`inst: None` for a parameter). |
| `TiedMismatch { inst }` | A tied use is not its definition's register. |
| `ReservedWrite { block, inst }` | The body writes the stack pointer or the kept frame pointer. |
| `Unencodable { inst }` | A vector-register operation isa-lang 0.2 cannot encode. |
| `Encode { inst, error }` | isa-lang refused an instruction. |
| `Asm(AsmError)` | The assembler failed (code beyond 2 GiB). |
| `UndefinedSymbol(name)` / `DuplicateSymbol(name)` | Linking by name. |
| `ProbeScratch` | A probe loop needs `r10` or `r11`, and both carry entry values. |
| `Constraint { inst }` | A table branch's temporary is its index register, or a tail call's register target is one the epilogue restores. |
| `ExternalConflict(name)` | A name was both declared external and emitted. |

## Feature flags

| Feature | Default | Effect |
|---|---|---|
| `std` | on | Enables isa-lang's `std` feature; no other effect. The crate is `no_std` + `alloc` without it. |

## Limits

| Limit | Value | On overflow |
|---|---|---|
| Physical registers per target | 128 | `PReg::new` returns `None` |
| Virtual registers per function | 2^31 - 1 | `MirError::Limit` |
| Blocks, instructions, operands, arguments, slots, symbols | 2^32 - 2 each | `MirError::Limit` |
| Frame and slot offsets | `i32` | `FrameError::TooLarge` |
| Slot alignment | 16 | `FrameError::Alignment` |
| Code size | 2 GiB (isa-lang) | `EmitError::Asm` |
| Allocator live sets | `regalloc::Options::live_budget` (default 2^24 entries) | `RegAllocError::TooLarge` |
| Allocation rounds | `MAX_ROUNDS` + 1 (the last spills every long-lived value) | always allocates, or `NoRegister` |
| Jump-table entries | 65,536 | a compare tree instead |

## What 0.3 does not do

Recorded with reasons in [`dev/ROADMAP.md`](../dev/ROADMAP.md):

- **Floating point and SIMD**, and the vector half of the conventions: they
  need isa-lang's SSE forms (ISSUES P11). Refused as
  `IselError::Unsupported`.
- **`atomic_rmw` other than `xchg`, and `cmpxchg`**: they need
  `lock`-prefixed forms isa-lang 0.2 does not encode. Refused.
- **`shift = saturate`** (OPS v2): ir-lang 2.0.0-alpha.1 cannot express it
  (its `Shift` has `error` and `mask`), so there is nothing to select yet.
- **Unwind tables**: no Win64 `.pdata`/`.xdata`, DWARF CFI, or LSDA, and no
  personality routine. `invoke` records its landing pad per call site and
  the pads work under an unwinder that restores the frame as the records
  and `FrameLayout` describe (the tests carry a minimal one), but no system
  unwinder can find them yet.
- **Position-independent code**: data is addressed PC-relative and calls are
  direct `rel32`; there is no GOT or PLT, so code linked into a shared
  object cannot reach symbols another object defines.
- **AArch64** and the `aapcs64` convention; **32-bit pointer** modules.
- **Aggregates by value, sret, varargs, i128**, and more results than
  return registers; a tail call needing more stack-argument space than its
  caller received.
- **Function alignment** inside one code buffer waits for isa-lang's
  alignment directive.
