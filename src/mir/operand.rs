//! Operands: registers, immediates, memory references, stack slots, function
//! symbols, condition codes, and branch targets with arguments.

use core::fmt;

use super::entity::{Block, CondCode, StackSlot, Symbol};
use crate::regs::{PReg, Reg};

/// The scale of a memory operand's index register.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::Scale;
///
/// assert_eq!(Scale::X8.factor(), 8);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scale {
    /// `index * 1`.
    X1,
    /// `index * 2`.
    X2,
    /// `index * 4`.
    X4,
    /// `index * 8`.
    X8,
}

impl Scale {
    /// The multiplier: 1, 2, 4, or 8.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Scale;
    ///
    /// assert_eq!(Scale::X2.factor(), 2);
    /// ```
    #[inline]
    #[must_use]
    pub const fn factor(self) -> u8 {
        match self {
            Scale::X1 => 1,
            Scale::X2 => 2,
            Scale::X4 => 4,
            Scale::X8 => 8,
        }
    }
}

/// Marks an absent base or index register; never a valid register (the
/// largest virtual-register index is one less).
const NO_REG: u32 = u32::MAX;

/// A memory reference: `[base + index * scale + disp]`, each part optional.
///
/// The access width comes from the instruction (`mov64` reads eight bytes).
/// Base and index are registers of the target's address class. A stack slot
/// is its own operand, [`Operand::Slot`], because its address is only known
/// once the frame is laid out.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::{Mem, Scale};
/// use backend_lang::x86_64::{RDI, RSI};
///
/// let m = Mem::base(RDI.into()).with_index(RSI.into(), Scale::X8).with_disp(16);
/// assert_eq!(m.base_reg(), Some(RDI.into()));
/// assert_eq!(m.index_reg(), Some((RSI.into(), Scale::X8)));
/// assert_eq!(m.disp(), 16);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Mem {
    base: u32,
    index: u32,
    disp: i32,
    scale: Scale,
}

impl Mem {
    /// `[base]`.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Mem;
    /// use backend_lang::x86_64::RBX;
    ///
    /// assert_eq!(Mem::base(RBX.into()).disp(), 0);
    /// ```
    #[inline]
    #[must_use]
    pub fn base(base: Reg) -> Mem {
        Mem {
            base: reg_bits(base),
            index: NO_REG,
            disp: 0,
            scale: Scale::X1,
        }
    }

    /// `[disp]`: an absolute address, with no registers.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Mem;
    ///
    /// let m = Mem::absolute(0x1000);
    /// assert_eq!(m.base_reg(), None);
    /// assert_eq!(m.disp(), 0x1000);
    /// ```
    #[inline]
    #[must_use]
    pub const fn absolute(disp: i32) -> Mem {
        Mem {
            base: NO_REG,
            index: NO_REG,
            disp,
            scale: Scale::X1,
        }
    }

    /// The same reference with an index register.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Mem, Scale};
    /// use backend_lang::x86_64::{RAX, RCX};
    ///
    /// let m = Mem::base(RAX.into()).with_index(RCX.into(), Scale::X4);
    /// assert_eq!(m.index_reg(), Some((RCX.into(), Scale::X4)));
    /// ```
    #[inline]
    #[must_use]
    pub fn with_index(self, index: Reg, scale: Scale) -> Mem {
        Mem {
            index: reg_bits(index),
            scale,
            ..self
        }
    }

    /// The same reference with base register `base`.
    #[inline]
    pub(crate) fn with_base(self, base: Reg) -> Mem {
        Mem {
            base: reg_bits(base),
            ..self
        }
    }

    /// The same reference with displacement `disp`.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Mem;
    /// use backend_lang::x86_64::RAX;
    ///
    /// assert_eq!(Mem::base(RAX.into()).with_disp(-8).disp(), -8);
    /// ```
    #[inline]
    #[must_use]
    pub const fn with_disp(self, disp: i32) -> Mem {
        Mem { disp, ..self }
    }

    /// The base register, if any.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Mem;
    ///
    /// assert_eq!(Mem::absolute(0).base_reg(), None);
    /// ```
    #[inline]
    #[must_use]
    pub fn base_reg(&self) -> Option<Reg> {
        bits_reg(self.base)
    }

    /// The index register and its scale, if any.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Mem;
    /// use backend_lang::x86_64::RAX;
    ///
    /// assert_eq!(Mem::base(RAX.into()).index_reg(), None);
    /// ```
    #[inline]
    #[must_use]
    pub fn index_reg(&self) -> Option<(Reg, Scale)> {
        bits_reg(self.index).map(|r| (r, self.scale))
    }

    /// The displacement.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Mem;
    ///
    /// assert_eq!(Mem::absolute(12).disp(), 12);
    /// ```
    #[inline]
    #[must_use]
    pub const fn disp(&self) -> i32 {
        self.disp
    }

    /// The registers the reference reads: its base, then its index.
    pub(crate) fn regs(&self) -> impl Iterator<Item = Reg> {
        self.base_reg()
            .into_iter()
            .chain(self.index_reg().map(|(r, _)| r))
    }
}

fn reg_bits(r: Reg) -> u32 {
    match (r.as_phys(), r.as_virt()) {
        (Some(p), _) => p.index() as u32,
        (_, Some(v)) => (v.index() as u32) | (1 << 31),
        _ => NO_REG,
    }
}

fn bits_reg(bits: u32) -> Option<Reg> {
    if bits == NO_REG {
        None
    } else if bits & (1 << 31) != 0 {
        Some(Reg::from(crate::regs::VReg::from_index(bits & !(1 << 31))))
    } else {
        Some(Reg::from(PReg::from_index(bits as u8)))
    }
}

impl fmt::Debug for Mem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mem")
            .field("base", &self.base_reg())
            .field("index", &self.index_reg())
            .field("disp", &self.disp)
            .finish()
    }
}

/// A branch target and the arguments passed to its block parameters. Made by
/// [`Function::block_call`](crate::mir::Function::block_call), which stores
/// the arguments in the function; read them back with
/// [`Function::args`](crate::mir::Function::args).
///
/// # Examples
///
/// ```
/// use backend_lang::mir::{Function, Operand};
/// use backend_lang::x86_64::{SYSV, RAX};
///
/// let mut f = Function::new("f", &SYSV);
/// let _entry = f.block()?;
/// let next = f.block()?;
/// let call = f.block_call(next, &[Operand::Reg(RAX.into())])?;
/// assert_eq!(call.block(), next);
/// assert_eq!(f.args(call), [Operand::Reg(RAX.into())]);
/// # Ok::<(), backend_lang::mir::MirError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockCall {
    block: Block,
    start: u32,
    len: u32,
}

impl BlockCall {
    pub(crate) const fn new(block: Block, start: u32, len: u32) -> BlockCall {
        BlockCall { block, start, len }
    }

    /// The target block.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let b = f.block()?;
    /// assert_eq!(f.block_call(b, &[])?.block(), b);
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    #[inline]
    #[must_use]
    pub const fn block(&self) -> Block {
        self.block
    }

    /// The number of arguments.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Function, Operand};
    /// use backend_lang::x86_64::SYSV;
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let b = f.block()?;
    /// assert_eq!(f.block_call(b, &[Operand::Imm(1), Operand::Imm(2)])?.len(), 2);
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether there are no arguments.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let b = f.block()?;
    /// assert!(f.block_call(b, &[])?.is_empty());
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(crate) const fn range(&self) -> core::ops::Range<usize> {
        self.start as usize..self.start as usize + self.len as usize
    }
}

/// One operand of a machine instruction.
///
/// Which shapes an operand position takes is the instruction's
/// [`OperandRule`](crate::target::OperandRule); [`Function::push`] checks
/// them.
///
/// [`Function::push`]: crate::mir::Function::push
///
/// # Examples
///
/// ```
/// use backend_lang::mir::{Mem, Operand};
/// use backend_lang::x86_64::RDI;
///
/// let ops = [
///     Operand::Reg(RDI.into()),
///     Operand::Imm(-1),
///     Operand::Mem(Mem::base(RDI.into()).with_disp(8)),
/// ];
/// assert!(matches!(ops[1], Operand::Imm(-1)));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Operand {
    /// A register, virtual or physical.
    Reg(Reg),
    /// An immediate: a signed value of the operation's width.
    Imm(i64),
    /// Memory at an address computed from registers.
    Mem(Mem),
    /// Memory in a stack slot, `offset` bytes into it.
    Slot {
        /// The slot.
        slot: StackSlot,
        /// Bytes from the slot's start.
        offset: i32,
    },
    /// A function, by name: a direct call target, or (as a memory operand of
    /// `lea`) the function's address.
    Symbol(Symbol),
    /// A condition code.
    Cond(CondCode),
    /// A branch target with arguments.
    Block(BlockCall),
}

impl Operand {
    /// The stack slot itself (offset 0): the form block parameters, branch
    /// arguments, and copies use.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Operand, StackSlot};
    ///
    /// let s = StackSlot::new(2);
    /// assert_eq!(Operand::slot(s), Operand::Slot { slot: s, offset: 0 });
    /// ```
    #[inline]
    #[must_use]
    pub const fn slot(slot: StackSlot) -> Operand {
        Operand::Slot { slot, offset: 0 }
    }

    /// The register, if this is a register operand.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Operand;
    /// use backend_lang::x86_64::RAX;
    ///
    /// assert_eq!(Operand::from(RAX).reg(), Some(RAX.into()));
    /// assert_eq!(Operand::Imm(0).reg(), None);
    /// ```
    #[inline]
    #[must_use]
    pub const fn reg(&self) -> Option<Reg> {
        match self {
            Operand::Reg(r) => Some(*r),
            _ => None,
        }
    }

    /// Whether the operand is memory: an address or a stack slot.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Mem, Operand, StackSlot};
    ///
    /// assert!(Operand::slot(StackSlot::new(0)).is_mem());
    /// assert!(Operand::Mem(Mem::absolute(0)).is_mem());
    /// assert!(!Operand::Imm(0).is_mem());
    /// ```
    #[inline]
    #[must_use]
    pub const fn is_mem(&self) -> bool {
        matches!(self, Operand::Mem(_) | Operand::Slot { .. })
    }
}

impl From<Reg> for Operand {
    #[inline]
    fn from(r: Reg) -> Operand {
        Operand::Reg(r)
    }
}

impl From<PReg> for Operand {
    #[inline]
    fn from(r: PReg) -> Operand {
        Operand::Reg(r.into())
    }
}

impl From<Mem> for Operand {
    #[inline]
    fn from(m: Mem) -> Operand {
        Operand::Mem(m)
    }
}

impl From<CondCode> for Operand {
    #[inline]
    fn from(c: CondCode) -> Operand {
        Operand::Cond(c)
    }
}

impl From<BlockCall> for Operand {
    #[inline]
    fn from(b: BlockCall) -> Operand {
        Operand::Block(b)
    }
}

impl From<Symbol> for Operand {
    #[inline]
    fn from(s: Symbol) -> Operand {
        Operand::Symbol(s)
    }
}

/// Whether an extra implicit register operand is read or written.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::{Implicit, ImplicitKind};
/// use backend_lang::x86_64::RDI;
///
/// let i = Implicit { reg: RDI, kind: ImplicitKind::Use };
/// assert_eq!(i.kind, ImplicitKind::Use);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ImplicitKind {
    /// The instruction reads the register.
    Use,
    /// The instruction writes the register with a defined value.
    Def,
}

/// An implicit register operand attached to one instruction, beyond those
/// its opcode always has: a call's argument and return registers, a
/// return's result registers, a system call's arguments.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::{Effects, Function, ImplicitKind, Operand};
/// use backend_lang::x86_64::{op, SYSV, RAX};
///
/// let mut f = Function::new("f", &SYSV);
/// let b = f.block()?;
/// let ret = f.push_with(b, op::RET, &[], Effects::new().uses(&[RAX]))?;
/// assert_eq!(f.implicit(ret)[0].reg, RAX);
/// assert_eq!(f.implicit(ret)[0].kind, ImplicitKind::Use);
/// # Ok::<(), backend_lang::mir::MirError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Implicit {
    /// The register.
    pub reg: PReg,
    /// Read or written.
    pub kind: ImplicitKind,
}
