//! # backend_lang
//!
//! The LexerSketch native backend: instruction selection from ir-lang 2.0,
//! SSA register allocation with exact stack maps, and emission to x86-64
//! machine code through [`isa_lang`], on top of a machine IR over virtual and
//! physical registers, target descriptions that state everything the backend
//! needs to know about an ISA as data, SysV and Win64 calling conventions and
//! frame layout, and a parallel-move resolver. No Cranelift, no LLVM, no
//! system assembler.
//!
//! This release (0.3) compiles the integer, control-flow, memory, call, and
//! checked-arithmetic subset of ir-lang. Floating point waits on isa-lang's
//! SSE forms, AArch64 on its AArch64 forms, and unwind tables on object
//! files that carry them; each is refused with an error that says so, never
//! miscompiled.
//!
//! ## Quick start
//!
//! [`compile`] is the one-call path: an ir-lang module in, machine code with
//! symbols and relocations out.
//!
//! ```
//! use backend_lang::Options;
//! use ir_lang::{Linkage, Module, Overflow, Signature, Type};
//!
//! // fn add(x: i64, y: i64) -> i64 { x + y }
//! let mut m = Module::new("demo");
//! let sig = Signature::new(&[Type::I64, Type::I64], &[Type::I64]);
//! let f = m.declare_function("add", &sig, Linkage::Export)?;
//! let mut b = m.build(f)?;
//! let (x, y) = (b.param(0)?, b.param(1)?);
//! let s = b.add(x, y, Overflow::Wrap)?;
//! b.ret(&[s])?;
//!
//! let c = backend_lang::compile(&m, &Options::sysv().with_frame_pointer(false))?;
//! // SysV: x arrives in rdi, y in rsi; the sum leaves in rax.
//! // add rdi, rsi; mov rax, rdi; ret
//! assert_eq!(c.code(), [0x48, 0x01, 0xF7, 0x48, 0x89, 0xF8, 0xC3]);
//! assert_eq!(c.function("add").map(|f| f.offset()), Some(0));
//! assert_eq!(c.exports(), ["add"]);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! [`emit`] is the lower-level path: a MIR function on physical registers
//! (written by hand, or after [`regalloc::allocate`]) in, machine code out.
//!
//! ```
//! use backend_lang::mir::{Effects, Function};
//! use backend_lang::x86_64::{op, SYSV, RAX, RDI};
//!
//! // fn double(x) -> x + x, SysV: x arrives in rdi, the result leaves in rax.
//! let mut f = Function::new("double", &SYSV);
//! let entry = f.block()?;
//! f.add_param(entry, RDI.into())?;
//! f.push(entry, op::MOV64, &[RAX.into(), RDI.into()])?;
//! f.push(entry, op::ADD64, &[RAX.into(), RAX.into(), RDI.into()])?;
//! f.push_with(entry, op::RET, &[], Effects::new().uses(&[RAX]))?;
//! f.omit_frame_pointer();
//!
//! let code = backend_lang::emit(&f)?;
//! // mov rax, rdi; add rax, rdi; ret
//! assert_eq!(code, [0x48, 0x89, 0xF8, 0x48, 0x01, 0xF8, 0xC3]);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ## The pieces
//!
//! - [`compile`], [`Options`], [`Compiled`]: the whole pipeline for a module.
//! - [`isel`]: instruction selection, one ir-lang function to MIR on
//!   virtual registers.
//! - [`regalloc`]: SSA register allocation, virtual registers to physical
//!   registers and stack slots.
//! - [`mir`]: [`Function`](mir::Function)s of [`Block`](mir::Block)s of
//!   target instructions, with typed virtual registers, stack slots, symbols,
//!   explicit implicit operands, call-site records, and a deterministic text
//!   form.
//! - [`target`]: the [`Target`](target::Target) description (registers,
//!   classes, instructions with their operand rules and implicit operands),
//!   [`abi`](target::abi) (argument and return registers, callee-saved sets,
//!   stack alignment, shadow space, red zone, probing), and
//!   [`frame`](target::frame) (frame layout and prologue/epilogue plans).
//! - [`moves`]: the parallel-move resolver every parallel copy goes through.
//! - [`x86_64`]: the x86-64 description, SysV and Win64, and the
//!   [`Emitter`](x86_64::Emitter).
//!
//! ## Block parameters, not phi nodes
//!
//! Values cross control-flow edges as block parameters: a branch passes
//! arguments, and the target block's parameters receive them all at once.
//! This is the same information as phi nodes, kept at the edge where it is
//! used: the parallel copy a phi implies on each incoming edge is written
//! down on that edge. The register allocator assigns parameters and
//! arguments independently and resolves each edge with the parallel-move
//! resolver; ir-lang uses block parameters too, so instruction selection
//! maps them one to one; and there is no per-block phi list to keep in sync
//! with predecessor order.
//!
//! ## Guarantees
//!
//! - **Compiled code computes what the IR says.** Random ir-lang programs
//!   (every integer operation under every policy, error edges, loops,
//!   switches, memory, calls, tail calls) are compiled and run against
//!   ir-lang's reference interpreter, in an x86-64 interpreter under SysV and
//!   Win64 and natively on x86-64 Linux and Windows.
//! - **Stack maps are exact.** At every safepoint the recorded root slots are
//!   exactly its GC list (IR.md §9), and no reference is in a register while
//!   the collector runs; a moving collector in the tests walks real frames by
//!   these records.
//! - **Parallel copies are exact.** Every edge, `copy`, and `pcopy` goes
//!   through [`moves::MoveResolver`], which handles any permutation, cycles
//!   of any length, fan-out, registers and stack slots mixed, and
//!   immediates, with the fewest moves under its rules.
//! - **Unsupported is an error, not bad code.** What this release does not
//!   select is reported as [`isel::IselError::Unsupported`] naming what it
//!   waits on.
//! - **No reachable panics, no recursion.** Every table is a dense vector,
//!   every walk a loop, every fallible path a `Result`. Passes are linear in
//!   the function (one sort here and there) except liveness, whose cost is
//!   the size of the live sets and has a budget
//!   ([`regalloc::Options::live_budget`]).
//!
//! ## Features
//!
//! - `std` (default): no effect beyond isa-lang's and ir-lang's; the crate is
//!   `no_std` and needs only `alloc`.

#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(unused_must_use)]
#![deny(unused_results)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::todo)]
#![deny(clippy::unimplemented)]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
#![deny(clippy::unreachable)]

extern crate alloc;

mod compile;
pub mod isel;
pub mod mir;
pub mod moves;
pub mod regalloc;
mod regs;
pub mod target;
pub mod x86_64;

pub use compile::{CompileError, Compiled, DataObject, Options, compile, compile_function};
pub use ir_lang;
pub use isa_lang;
pub use regs::{MAX_PREGS, PReg, Reg, RegClass, RegSet, RegSetIter, VReg};

use alloc::vec::Vec;

/// Emits one function to machine code: the one-call path.
///
/// The function must be on physical registers (after register allocation,
/// or written that way). It is verified, its frame is laid out under its
/// calling convention, and every instruction is encoded. Calls to other
/// functions need [`x86_64::Emitter`], which places several functions
/// together; a function may call itself by its own name here.
///
/// # Errors
///
/// An [`x86_64::EmitError`] naming the problem.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::{Function, Operand};
/// use backend_lang::x86_64::{op, SYSV, RAX};
///
/// let mut f = Function::new("one", &SYSV);
/// let b = f.block()?;
/// f.push(b, op::MOV32RI, &[RAX.into(), Operand::Imm(1)])?;
/// f.push(b, op::RET, &[])?;
/// let code = backend_lang::emit(&f)?;
/// assert_eq!(code.last(), Some(&0xC3));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn emit(f: &mir::Function) -> Result<Vec<u8>, x86_64::EmitError> {
    let mut e = x86_64::Emitter::new();
    let _info = e.function(f)?;
    Ok(e.finish()?.into_bytes())
}

/// Compiles and runs the `rust` code blocks in `README.md` and `docs/API.md` as
/// part of `cargo test`, so the published examples cannot drift from the API.
///
/// Present only while collecting doctests (`#[cfg(doctest)]`); it is not part of
/// the public surface and does not appear in the built library or its docs.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
#[doc = include_str!("../docs/API.md")]
pub struct MarkdownDocTests;
