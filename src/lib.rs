//! # backend_lang
//!
//! The core of the LexerSketch native backend: a machine IR over virtual and
//! physical registers, target descriptions that say everything the backend
//! needs to know about an ISA as data, calling conventions and frame layout,
//! a parallel-move resolver, and emission to machine code through
//! [`isa_lang`]. No Cranelift, no LLVM, no system assembler.
//!
//! This release (0.2) is the foundation. Instruction selection from ir-lang
//! and SSA register allocation build on it in 0.5; until then MIR is built
//! directly, and emission takes functions whose registers are already
//! physical.
//!
//! ## Quick start
//!
//! [`emit`] is the one-call path: a MIR function on physical registers in,
//! x86-64 machine code out.
//!
//! ```
//! use backend_lang::mir::{Effects, Function, Operand};
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
//! - [`mir`]: [`Function`](mir::Function)s of [`Block`](mir::Block)s of
//!   target instructions, with typed virtual registers, stack slots, symbols,
//!   explicit implicit operands, and a deterministic text form.
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
//! down on that edge. An SSA register allocator (0.5) assigns parameters and
//! arguments independently and resolves each edge with the parallel-move
//! resolver; ir-lang already uses block parameters, so instruction selection
//! maps them one to one; and there is no per-block phi list to keep in sync
//! with predecessor order.
//!
//! ## Guarantees
//!
//! - **Parallel copies are exact.** Every edge, `copy`, and `pcopy` goes
//!   through [`moves::MoveResolver`], which handles any permutation, cycles
//!   of any length, fan-out, registers and stack slots mixed, and
//!   immediates, with the fewest moves under its rules. Property tests run
//!   it against a parallel-assignment reference, and emitted edges against
//!   an interpreter of the emitted machine code.
//! - **Emission is checked.** A function is verified before emission;
//!   virtual registers, mismatched tied operands, and writes to the stack or
//!   frame pointer are errors, not bad code.
//! - **No reachable panics, no recursion.** Every table is a dense vector,
//!   every walk a loop, every fallible path a `Result`.
//!
//! ## Features
//!
//! - `std` (default): no effect beyond isa-lang's; the crate is `no_std`
//!   and needs only `alloc`.

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

pub mod mir;
pub mod moves;
mod regs;
pub mod target;
pub mod x86_64;

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
