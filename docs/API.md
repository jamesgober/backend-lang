# backend-lang &mdash; API Reference

> Complete reference for every public item in `backend-lang`, with examples.
> **Status: 0.2 (pre-1.0).** The surface is designed across the 0.x series
> and frozen at `1.0.0`. See [`../dev/ROADMAP.md`](../dev/ROADMAP.md).

<sub>Copyright &copy; 2026 <strong>James Gober</strong>.</sub>

## Table of contents

- [Overview](#overview)
- [Installation](#installation)
- [Quick start](#quick-start)
- [Concepts](#concepts)
  - [Machine IR](#machine-ir)
  - [Registers and classes](#registers-and-classes)
  - [Operand rules and implicit operands](#operand-rules-and-implicit-operands)
  - [Block parameters and parallel copies](#block-parameters-and-parallel-copies)
  - [Frames](#frames)
  - [Emission](#emission)
- [`emit`](#emit)
- [Registers: `PReg`, `VReg`, `Reg`, `RegClass`, `RegSet`](#registers)
- [`mir`](#mir)
  - [`Function`](#function)
  - [`Block`, `Inst`, `StackSlot`, `Symbol`, `CondCode`](#ids)
  - [`SlotKind`](#slotkind)
  - [`Operand`, `Mem`, `Scale`, `BlockCall`](#operand)
  - [`Effects`, `Implicit`, `ImplicitKind`](#effects)
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
  - [`EmitError`](#emiterror)
- [Feature flags](#feature-flags)
- [Limits](#limits)
- [What 0.2 does not do](#what-02-does-not-do)

## Overview

| Item | Kind | Purpose |
|---|---|---|
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
backend-lang = "0.2"
```

For `no_std` targets (the crate needs only `alloc`):

```toml
[dependencies]
backend-lang = { version = "0.2", default-features = false }
```

## Quick start

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
once. Linear time. Dominance (every use defined on every path) is the
register allocator's precondition and is checked with it in 0.5.

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
`finish` relaxes branches, patches calls by name, and reports names called
but never emitted.

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

## What 0.2 does not do

Recorded with reasons in [`dev/ROADMAP.md`](../dev/ROADMAP.md):

- **Instruction selection** from ir-lang and **SSA register allocation**
  (spilling, rematerialization, stack maps) arrive in 0.5; instruction
  selection waits for ir-lang 2.0 (calls, memory, aggregates).
- **AArch64** and **floating point / SIMD** need isa-lang 0.5's forms.
- **Aggregates by value, variadic calls, and 128-bit integers** in the
  conventions come with call lowering (0.5).
- **Unwind information** (Win64 `.pdata`/`.xdata`, DWARF CFI) and
  **relocations against external symbols** belong with object-lang and
  debuginfo-lang; the `FrameLayout` prologue plan is the input they need.
- **Function alignment** inside one code buffer waits for isa-lang's
  alignment directive.
