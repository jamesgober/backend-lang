//! Machine IR: target instructions on virtual and physical registers.
//!
//! A [`Function`] is a list of [`Block`]s; each block holds instructions of
//! the function's target ([`Opcode`](crate::target::Opcode)s with
//! [`Operand`]s) and ends with one terminator. Values flow between blocks
//! through block parameters: a branch passes arguments, and every parameter
//! of the target block receives its argument at once, as a parallel copy.
//!
//! Before register allocation, instructions define and use virtual registers
//! ([`Function::vreg`]), each typed by a register class, in SSA form (one
//! definition each; [`Function::verify`] checks it). Physical registers
//! appear where an ABI or an instruction fixes them, and as the extra
//! implicit operands of calls and returns ([`Effects`]). After allocation
//! every register is physical, block parameters and arguments may be stack
//! slots, and the function is ready for emission
//! ([`emit`](crate::emit)).
//!
//! [`Function`] implements `Display` in the deterministic text form below.
//!
//! # Text form
//!
//! ```text
//! function sum (sysv64) {
//! block0(%0:gpr):
//!     %4:gpr = mov64ri 0
//!     jmp block1(%4, %0)
//! block1(%1:gpr, %2:gpr):
//!     %5:gpr = add64 %1, %2
//!     %6:gpr = sub64 %2, 1
//!     jcc ne, block1(%5, %6), block2(%5)
//! block2(%3:gpr):
//!     rax = copy %3
//!     ret implicit(rax)
//! }
//! ```
//!
//! Virtual registers carry their class where they are defined (`%5:gpr`, and
//! in block parameters), physical registers print by name, memory as
//! `[base + index*scale + disp]`, stack slots as `ssN` (`ssN+8` with an
//! offset), symbols as `@name`, condition codes by name, and an
//! instruction's extra implicit operands after its explicit ones as
//! `implicit(...)`, `implicit-def(...)`, and `clobber(...)`. Slots, symbols,
//! and frame options are listed first. The text depends only on the
//! function, so tests compare it exactly.

mod entity;
mod function;
mod operand;
mod print;
mod verify;

pub use entity::{Block, CondCode, Inst, StackSlot, Symbol};
pub use function::{Effects, Function, SlotKind};
pub use operand::{BlockCall, Implicit, ImplicitKind, Mem, Operand, Scale};
pub use verify::MirError;
