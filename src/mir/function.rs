//! A machine function: blocks of target instructions over virtual and
//! physical registers, with block parameters, stack slots, and symbols.

use alloc::string::String;
use alloc::vec::Vec;

use super::entity::{Block, Inst, StackSlot, Symbol};
use super::operand::{BlockCall, Implicit, ImplicitKind, Operand};
use super::verify::MirError;
use crate::regs::{PReg, Reg, RegClass, RegSet, VReg};
use crate::target::abi::Abi;
use crate::target::frame::{FrameError, FrameInput, FrameLayout};
use crate::target::{InstKind, Opcode, OperandRule, Target};

/// What a stack slot is.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::{Function, SlotKind};
/// use backend_lang::x86_64::SYSV;
///
/// let mut f = Function::new("f", &SYSV);
/// let local = f.stack_slot(SlotKind::Local { size: 24, align: 8 })?;
/// let arg = f.stack_slot(SlotKind::Incoming { offset: 0, size: 8 })?;
/// assert_eq!(f.slot(local), Some(SlotKind::Local { size: 24, align: 8 }));
/// assert!(matches!(f.slot(arg), Some(SlotKind::Incoming { .. })));
/// # Ok::<(), backend_lang::mir::MirError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SlotKind {
    /// Space in this function's frame.
    Local {
        /// Bytes.
        size: u32,
        /// Alignment in bytes: a power of two up to the stack alignment.
        align: u32,
    },
    /// An argument the caller passed on the stack, at `offset` bytes above
    /// the canonical frame address (see
    /// [`ArgLoc::Stack`](crate::target::abi::ArgLoc::Stack)).
    Incoming {
        /// Offset from the canonical frame address.
        offset: u32,
        /// Bytes.
        size: u32,
    },
}

impl SlotKind {
    /// The slot's size in bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::SlotKind;
    ///
    /// assert_eq!(SlotKind::Incoming { offset: 8, size: 8 }.size(), 8);
    /// ```
    #[inline]
    #[must_use]
    pub const fn size(self) -> u32 {
        match self {
            SlotKind::Local { size, .. } | SlotKind::Incoming { size, .. } => size,
        }
    }
}

/// Extra implicit operands for one instruction: registers it reads and
/// writes without naming them, and registers it clobbers, beyond what its
/// opcode always has. Used for calls (argument registers, return registers,
/// the convention's clobbers), returns (result registers), and system calls.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::Effects;
/// use backend_lang::x86_64::{SYSV, RAX, RDI};
///
/// let e = Effects::new().uses(&[RDI]).defs(&[RAX]).clobbers(SYSV.call_clobbers());
/// assert_eq!(e.uses_regs(), [RDI]);
/// assert!(e.clobber_set().contains(RAX));
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effects<'a> {
    uses: &'a [PReg],
    defs: &'a [PReg],
    clobbers: RegSet,
}

impl<'a> Effects<'a> {
    /// No extra operands.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Effects;
    ///
    /// assert!(Effects::new().uses_regs().is_empty());
    /// ```
    #[must_use]
    pub const fn new() -> Effects<'a> {
        Effects {
            uses: &[],
            defs: &[],
            clobbers: RegSet::EMPTY,
        }
    }

    /// Registers read.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Effects;
    /// use backend_lang::x86_64::RSI;
    ///
    /// assert_eq!(Effects::new().uses(&[RSI]).uses_regs(), [RSI]);
    /// ```
    #[must_use]
    pub const fn uses(self, regs: &'a [PReg]) -> Effects<'a> {
        Effects { uses: regs, ..self }
    }

    /// Registers written with defined values.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Effects;
    /// use backend_lang::x86_64::RAX;
    ///
    /// assert_eq!(Effects::new().defs(&[RAX]).defs_regs(), [RAX]);
    /// ```
    #[must_use]
    pub const fn defs(self, regs: &'a [PReg]) -> Effects<'a> {
        Effects { defs: regs, ..self }
    }

    /// Registers left with unspecified contents.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::RegSet;
    /// use backend_lang::mir::Effects;
    /// use backend_lang::x86_64::R11;
    ///
    /// assert!(Effects::new().clobbers(RegSet::of(&[R11])).clobber_set().contains(R11));
    /// ```
    #[must_use]
    pub const fn clobbers(self, regs: RegSet) -> Effects<'a> {
        Effects {
            clobbers: regs,
            ..self
        }
    }

    /// The registers read.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Effects;
    ///
    /// assert!(Effects::new().uses_regs().is_empty());
    /// ```
    #[must_use]
    pub const fn uses_regs(&self) -> &'a [PReg] {
        self.uses
    }

    /// The registers written.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Effects;
    ///
    /// assert!(Effects::new().defs_regs().is_empty());
    /// ```
    #[must_use]
    pub const fn defs_regs(&self) -> &'a [PReg] {
        self.defs
    }

    /// The registers clobbered.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Effects;
    ///
    /// assert!(Effects::new().clobber_set().is_empty());
    /// ```
    #[must_use]
    pub const fn clobber_set(&self) -> RegSet {
        self.clobbers
    }
}

/// One instruction's record. Operands, implicit operands, and clobber sets
/// live in the function's shared pools; the record holds ranges into them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InstData {
    pub(crate) opcode: Opcode,
    pub(crate) block: Block,
    pub(crate) ops: u32,
    pub(crate) nops: u32,
    pub(crate) imp: u32,
    pub(crate) nimp: u32,
    /// Index into `clobbers`, or `NO_CLOBBERS`.
    pub(crate) clobber: u32,
}

pub(crate) const NO_CLOBBERS: u32 = u32::MAX;

#[derive(Clone, Debug, Default)]
pub(crate) struct BlockData {
    pub(crate) params: Vec<Operand>,
    pub(crate) insts: Vec<Inst>,
}

/// The most entries any one table of a function may hold (instructions,
/// operands, blocks, slots, symbols): ids are 32-bit.
pub(crate) const MAX_ENTRIES: usize = u32::MAX as usize - 1;

/// A machine function.
///
/// Blocks hold target instructions in order; the first block is the entry.
/// Instructions name virtual registers (each typed by a register class) or
/// physical registers, stack slots, and other functions by symbol. Control
/// flow passes values between blocks through **block parameters**: a branch
/// names its target with arguments, and on the edge every parameter receives
/// its argument at once (a parallel copy). Block parameters are SSA's phi
/// nodes written at the edge, which is the form an SSA register allocator
/// works on and the form ir-lang already uses; see the crate docs for the
/// rationale.
///
/// Every table is a dense vector indexed by its id type; nothing is
/// recursive.
///
/// # Examples
///
/// Sum `1..=n` with virtual registers:
///
/// ```
/// use backend_lang::mir::{Function, Operand};
/// use backend_lang::x86_64::{self, op, GPR, SYSV};
/// use isa_lang::x86_64::Cond;
///
/// let mut f = Function::new("sum", &SYSV);
/// let entry = f.block()?;
/// let head = f.block()?;
/// let exit = f.block()?;
/// let n = f.vreg(GPR)?;
/// f.add_param(entry, n.into())?;
/// // Parameters come first: branches are checked against them.
/// let (acc, i) = (f.vreg(GPR)?, f.vreg(GPR)?);
/// f.add_param(head, acc.into())?;
/// f.add_param(head, i.into())?;
/// let result = f.vreg(GPR)?;
/// f.add_param(exit, result.into())?;
///
/// let zero = f.vreg(GPR)?;
/// f.push(entry, op::MOV64RI, &[zero.into(), Operand::Imm(0)])?;
/// let to_head = f.block_call(head, &[zero.into(), n.into()])?;
/// f.push(entry, op::JMP, &[to_head.into()])?;
///
/// let (acc2, i2) = (f.vreg(GPR)?, f.vreg(GPR)?);
/// f.push(head, op::ADD64, &[acc2.into(), acc.into(), i.into()])?;
/// f.push(head, op::SUB64, &[i2.into(), i.into(), Operand::Imm(1)])?;
/// let again = f.block_call(head, &[acc2.into(), i2.into()])?;
/// let done = f.block_call(exit, &[acc2.into()])?;
/// f.push(head, op::JCC, &[x86_64::cond(Cond::Ne).into(), again.into(), done.into()])?;
///
/// f.push(exit, op::RET, &[])?;
///
/// f.verify()?;
/// assert!(f.to_string().contains("%5:gpr = add64 %1, %2"));
/// # Ok::<(), backend_lang::mir::MirError>(())
/// ```
#[derive(Clone, Debug)]
pub struct Function {
    name: String,
    abi: &'static Abi,
    pub(crate) vregs: Vec<RegClass>,
    pub(crate) blocks: Vec<BlockData>,
    pub(crate) insts: Vec<InstData>,
    pub(crate) operands: Vec<Operand>,
    pub(crate) args: Vec<Operand>,
    pub(crate) implicits: Vec<Implicit>,
    pub(crate) clobbers: Vec<RegSet>,
    pub(crate) slots: Vec<SlotKind>,
    pub(crate) symbols: Vec<String>,
    outgoing: u32,
    frame_pointer: bool,
}

/// Fails unless a table of `len` entries has room for one more id.
fn room(len: usize) -> Result<(), MirError> {
    next_id(len).map(|_| ())
}

fn next_id(len: usize) -> Result<u32, MirError> {
    if len < MAX_ENTRIES {
        Ok(len as u32)
    } else {
        Err(MirError::Limit)
    }
}

impl Function {
    /// An empty function named `name`, under calling convention `abi` (which
    /// also fixes the target).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::WIN64;
    ///
    /// let f = Function::new("main", &WIN64);
    /// assert_eq!(f.name(), "main");
    /// assert_eq!(f.abi().name(), "win64");
    /// ```
    #[must_use]
    pub fn new(name: &str, abi: &'static Abi) -> Function {
        Function {
            name: String::from(name),
            abi,
            vregs: Vec::new(),
            blocks: Vec::new(),
            insts: Vec::new(),
            operands: Vec::new(),
            args: Vec::new(),
            implicits: Vec::new(),
            clobbers: Vec::new(),
            slots: Vec::new(),
            symbols: Vec::new(),
            outgoing: 0,
            frame_pointer: true,
        }
    }

    /// An empty function with room for `insts` instructions and `operands`
    /// operands, so building a large function does not reallocate.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// let f = Function::with_capacity("big", &SYSV, 100_000, 300_000);
    /// assert_eq!(f.inst_count(), 0);
    /// ```
    #[must_use]
    pub fn with_capacity(name: &str, abi: &'static Abi, insts: usize, operands: usize) -> Function {
        let mut f = Function::new(name, abi);
        f.insts.reserve(insts);
        f.operands.reserve(operands);
        f
    }

    /// The function's name, which is also the symbol other functions call it
    /// by.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// assert_eq!(Function::new("f", &SYSV).name(), "f");
    /// ```
    #[inline]
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The calling convention.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// assert_eq!(Function::new("f", &SYSV).abi().name(), "sysv64");
    /// ```
    #[inline]
    #[must_use]
    pub fn abi(&self) -> &'static Abi {
        self.abi
    }

    /// The target (the calling convention's).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// assert_eq!(Function::new("f", &SYSV).target().name(), "x86-64");
    /// ```
    #[inline]
    #[must_use]
    pub fn target(&self) -> &'static Target {
        self.abi.target()
    }

    /// A new virtual register of `class`.
    ///
    /// # Errors
    ///
    /// [`MirError::UnknownClass`] if the target has no such class,
    /// [`MirError::Limit`] past [`VReg::MAX`] registers.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::RegClass;
    /// use backend_lang::mir::{Function, MirError};
    /// use backend_lang::x86_64::{SYSV, VEC};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// assert!(f.vreg(VEC)?.is_virtual());
    /// assert_eq!(f.vreg(RegClass::new(7)), Err(MirError::UnknownClass(RegClass::new(7))));
    /// # Ok::<(), MirError>(())
    /// ```
    pub fn vreg(&mut self, class: RegClass) -> Result<Reg, MirError> {
        if !self.target().has_class(class) {
            return Err(MirError::UnknownClass(class));
        }
        if self.vregs.len() >= VReg::MAX {
            return Err(MirError::Limit);
        }
        let v = VReg::from_index(self.vregs.len() as u32);
        self.vregs.push(class);
        Ok(v.into())
    }

    /// The class of a virtual register, or `None` if the function does not
    /// have it.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::{GPR, SYSV};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let v = f.vreg(GPR)?.as_virt().unwrap();
    /// assert_eq!(f.vreg_class(v), Some(GPR));
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    #[inline]
    #[must_use]
    pub fn vreg_class(&self, vreg: VReg) -> Option<RegClass> {
        self.vregs.get(vreg.index()).copied()
    }

    /// The number of virtual registers.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::{GPR, SYSV};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let _ = f.vreg(GPR)?;
    /// assert_eq!(f.vreg_count(), 1);
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    #[inline]
    #[must_use]
    pub fn vreg_count(&self) -> usize {
        self.vregs.len()
    }

    /// The class of any register: a virtual register's declared class, or
    /// a physical register's class in the target.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::{GPR, SYSV, VEC, XMM2};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let v = f.vreg(GPR)?;
    /// assert_eq!(f.reg_class(v), Some(GPR));
    /// assert_eq!(f.reg_class(XMM2.into()), Some(VEC));
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    #[must_use]
    pub fn reg_class(&self, reg: Reg) -> Option<RegClass> {
        match (reg.as_phys(), reg.as_virt()) {
            (Some(p), _) => self.target().class_of(p),
            (_, Some(v)) => self.vreg_class(v),
            _ => None,
        }
    }

    /// A new, empty block. The first block is the entry.
    ///
    /// # Errors
    ///
    /// [`MirError::Limit`] past `u32::MAX - 1` blocks.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let entry = f.block()?;
    /// assert_eq!(f.entry(), Some(entry));
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    pub fn block(&mut self) -> Result<Block, MirError> {
        let id = next_id(self.blocks.len())?;
        self.blocks.push(BlockData::default());
        Ok(Block::new(id))
    }

    /// The entry block, if any block exists.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// assert_eq!(Function::new("f", &SYSV).entry(), None);
    /// ```
    #[inline]
    #[must_use]
    pub fn entry(&self) -> Option<Block> {
        if self.blocks.is_empty() {
            None
        } else {
            Some(Block::new(0))
        }
    }

    /// The blocks, in layout order (the order emission lays them out).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let a = f.block()?;
    /// let b = f.block()?;
    /// assert_eq!(f.blocks().collect::<Vec<_>>(), [a, b]);
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    pub fn blocks(&self) -> impl ExactSizeIterator<Item = Block> + '_ {
        (0..self.blocks.len()).map(Block::from_index)
    }

    /// The number of blocks.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// assert_eq!(Function::new("f", &SYSV).block_count(), 0);
    /// ```
    #[inline]
    #[must_use]
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Appends a parameter to `block`: a register (virtual before
    /// allocation, physical after) or a whole stack slot
    /// ([`Operand::slot`]). The entry block's parameters are the function's
    /// incoming arguments.
    ///
    /// # Errors
    ///
    /// [`MirError::UnknownBlock`] for a block the function does not have,
    /// [`MirError::BadParam`] for any other operand shape (including a slot
    /// operand with an offset), [`MirError::UnknownVReg`],
    /// [`MirError::UnknownPReg`], or [`MirError::UnknownSlot`] for a register
    /// or slot the function or target does not have.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Function, Operand};
    /// use backend_lang::x86_64::{RDI, SYSV};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let entry = f.block()?;
    /// f.add_param(entry, RDI.into())?;
    /// assert_eq!(f.params(entry), [Operand::from(RDI)]);
    /// assert!(f.add_param(entry, Operand::Imm(1)).is_err());
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    pub fn add_param(&mut self, block: Block, param: Operand) -> Result<(), MirError> {
        match param {
            Operand::Reg(r) => self.check_reg(r)?,
            Operand::Slot { slot, offset: 0 } => self.check_slot(slot)?,
            _ => return Err(MirError::BadParam { block }),
        }
        let data = self
            .blocks
            .get_mut(block.index())
            .ok_or(MirError::UnknownBlock(block))?;
        room(data.params.len())?;
        data.params.push(param);
        Ok(())
    }

    /// A block's parameters (empty for a block the function does not have).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let b = f.block()?;
    /// assert!(f.params(b).is_empty());
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    #[inline]
    #[must_use]
    pub fn params(&self, block: Block) -> &[Operand] {
        self.blocks
            .get(block.index())
            .map_or(&[], |b| b.params.as_slice())
    }

    /// A new stack slot.
    ///
    /// # Errors
    ///
    /// [`MirError::Limit`] past `u32::MAX - 1` slots. Sizes and alignments
    /// are checked when the frame is laid out.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Function, SlotKind};
    /// use backend_lang::x86_64::SYSV;
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let s = f.stack_slot(SlotKind::Local { size: 16, align: 16 })?;
    /// assert_eq!(f.slot(s).map(SlotKind::size), Some(16));
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    pub fn stack_slot(&mut self, kind: SlotKind) -> Result<StackSlot, MirError> {
        let id = next_id(self.slots.len())?;
        self.slots.push(kind);
        Ok(StackSlot::new(id))
    }

    /// A new stack slot sized and aligned to hold one value of `class`.
    ///
    /// # Errors
    ///
    /// [`MirError::UnknownClass`] for a class the target does not have,
    /// [`MirError::Limit`] past `u32::MAX - 1` slots.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Function, SlotKind};
    /// use backend_lang::x86_64::{SYSV, VEC};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let s = f.spill_slot(VEC)?;
    /// assert_eq!(f.slot(s), Some(SlotKind::Local { size: 16, align: 16 }));
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    pub fn spill_slot(&mut self, class: RegClass) -> Result<StackSlot, MirError> {
        if !self.target().has_class(class) {
            return Err(MirError::UnknownClass(class));
        }
        let info = self.target().class(class);
        self.stack_slot(SlotKind::Local {
            size: info.spill_size(),
            align: info.spill_align(),
        })
    }

    /// A slot's kind, or `None` if the function does not have it.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Function, StackSlot};
    /// use backend_lang::x86_64::SYSV;
    ///
    /// assert_eq!(Function::new("f", &SYSV).slot(StackSlot::new(0)), None);
    /// ```
    #[inline]
    #[must_use]
    pub fn slot(&self, slot: StackSlot) -> Option<SlotKind> {
        self.slots.get(slot.index()).copied()
    }

    /// Every slot's kind, indexed by [`StackSlot::index`].
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// assert!(Function::new("f", &SYSV).slots().is_empty());
    /// ```
    #[inline]
    #[must_use]
    pub fn slots(&self) -> &[SlotKind] {
        &self.slots
    }

    /// A new reference to the function named `name`.
    ///
    /// # Errors
    ///
    /// [`MirError::Limit`] past `u32::MAX - 1` symbols.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let callee = f.symbol("g")?;
    /// assert_eq!(f.symbol_name(callee), Some("g"));
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    pub fn symbol(&mut self, name: &str) -> Result<Symbol, MirError> {
        let id = next_id(self.symbols.len())?;
        self.symbols.push(String::from(name));
        Ok(Symbol::new(id))
    }

    /// A symbol's name, or `None` if the function does not have it.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Function, Symbol};
    /// use backend_lang::x86_64::SYSV;
    ///
    /// assert_eq!(Function::new("f", &SYSV).symbol_name(Symbol::new(0)), None);
    /// ```
    #[inline]
    #[must_use]
    pub fn symbol_name(&self, symbol: Symbol) -> Option<&str> {
        self.symbols.get(symbol.index()).map(String::as_str)
    }

    /// A branch target: `block` with `args` for its parameters, as an
    /// operand for a jump or branch. The arguments are registers, whole
    /// stack slots, or immediates.
    ///
    /// # Errors
    ///
    /// [`MirError::UnknownBlock`], [`MirError::BadArg`] for another operand
    /// shape, the `Unknown*` errors for registers or slots the function does
    /// not have, and [`MirError::Limit`] when the argument pool is full.
    /// Argument counts and classes are checked when the branch is pushed.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Function, Operand};
    /// use backend_lang::x86_64::{op, SYSV};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let entry = f.block()?;
    /// let target = f.block()?;
    /// let call = f.block_call(target, &[])?;
    /// f.push(entry, op::JMP, &[call.into()])?;
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    pub fn block_call(&mut self, block: Block, args: &[Operand]) -> Result<BlockCall, MirError> {
        if block.index() >= self.blocks.len() {
            return Err(MirError::UnknownBlock(block));
        }
        for &a in args {
            match a {
                Operand::Reg(r) => self.check_reg(r)?,
                Operand::Slot { slot, offset: 0 } => self.check_slot(slot)?,
                Operand::Imm(_) => {}
                _ => return Err(MirError::BadArg { block }),
            }
        }
        let start = next_id(self.args.len())?;
        let len = u32::try_from(args.len()).map_err(|_| MirError::Limit)?;
        room(self.args.len().saturating_add(args.len()))?;
        self.args.extend_from_slice(args);
        Ok(BlockCall::new(block, start, len))
    }

    /// The arguments of a branch target (empty for one this function did not
    /// make).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Function, Operand};
    /// use backend_lang::x86_64::SYSV;
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let b = f.block()?;
    /// let call = f.block_call(b, &[Operand::Imm(7)])?;
    /// assert_eq!(f.args(call), [Operand::Imm(7)]);
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    #[inline]
    #[must_use]
    pub fn args(&self, call: BlockCall) -> &[Operand] {
        self.args.get(call.range()).unwrap_or(&[])
    }

    /// Appends an instruction to `block`.
    ///
    /// The operands are checked against the opcode's description at once:
    /// their count, each one's shape and class, immediate ranges, branch
    /// argument counts against the target block's parameters (so a block's
    /// parameters must be added before branches to it), and the target's
    /// memory-operand limit.
    ///
    /// # Errors
    ///
    /// A [`MirError`] naming the problem; nothing is appended then.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Function, MirError, Operand};
    /// use backend_lang::x86_64::{op, GPR, SYSV};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let b = f.block()?;
    /// let (a, c) = (f.vreg(GPR)?, f.vreg(GPR)?);
    /// f.push(b, op::MOV64RI, &[a.into(), Operand::Imm(40)])?;
    /// f.push(b, op::ADD64, &[c.into(), a.into(), Operand::Imm(2)])?;
    /// // add64 takes a 32-bit immediate at most.
    /// let err = f.push(b, op::ADD64, &[c.into(), a.into(), Operand::Imm(1 << 40)]).unwrap_err();
    /// assert!(matches!(err, MirError::ImmediateRange { .. }));
    /// # Ok::<(), MirError>(())
    /// ```
    pub fn push(
        &mut self,
        block: Block,
        opcode: Opcode,
        operands: &[Operand],
    ) -> Result<Inst, MirError> {
        self.push_with(block, opcode, operands, Effects::new())
    }

    /// Appends an instruction with extra implicit operands (see
    /// [`Effects`]).
    ///
    /// # Errors
    ///
    /// As for [`Function::push`], plus [`MirError::UnknownPReg`] for an
    /// implicit register the target does not have.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Effects, Function, Operand};
    /// use backend_lang::x86_64::{op, SYSV, RAX, RDI};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let b = f.block()?;
    /// f.push(b, op::MOV64RI, &[RAX.into(), Operand::Imm(60)])?;
    /// f.push_with(b, op::SYSCALL, &[], Effects::new().uses(&[RDI]))?;
    /// f.push(b, op::UD2, &[])?;
    /// assert!(f.to_string().contains("syscall implicit(rdi)"));
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    pub fn push_with(
        &mut self,
        block: Block,
        opcode: Opcode,
        operands: &[Operand],
        effects: Effects<'_>,
    ) -> Result<Inst, MirError> {
        if block.index() >= self.blocks.len() {
            return Err(MirError::UnknownBlock(block));
        }
        let inst = Inst::new(next_id(self.insts.len())?);
        self.check_inst(inst, opcode, operands)?;
        for &r in effects.uses.iter().chain(effects.defs) {
            if !self.target().has_reg(r) {
                return Err(MirError::UnknownPReg(r));
            }
        }
        let ops = next_id(self.operands.len())?;
        room(self.operands.len().saturating_add(operands.len()))?;
        let imp = next_id(self.implicits.len())?;
        let nimp = effects.uses.len() + effects.defs.len();
        room(self.implicits.len().saturating_add(nimp))?;
        let clobber = if effects.clobbers.is_empty() {
            NO_CLOBBERS
        } else {
            let id = next_id(self.clobbers.len())?;
            self.clobbers.push(effects.clobbers);
            id
        };
        self.operands.extend_from_slice(operands);
        self.implicits
            .extend(effects.uses.iter().map(|&reg| Implicit {
                reg,
                kind: ImplicitKind::Use,
            }));
        self.implicits
            .extend(effects.defs.iter().map(|&reg| Implicit {
                reg,
                kind: ImplicitKind::Def,
            }));
        self.insts.push(InstData {
            opcode,
            block,
            ops,
            nops: operands.len() as u32,
            imp,
            nimp: nimp as u32,
            clobber,
        });
        if let Some(b) = self.blocks.get_mut(block.index()) {
            b.insts.push(inst);
        }
        Ok(inst)
    }

    /// The instructions of `block`, in order (empty for a block the function
    /// does not have).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::{op, SYSV};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let b = f.block()?;
    /// let r = f.push(b, op::RET, &[])?;
    /// assert_eq!(f.insts(b), [r]);
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    #[inline]
    #[must_use]
    pub fn insts(&self, block: Block) -> &[Inst] {
        self.blocks
            .get(block.index())
            .map_or(&[], |b| b.insts.as_slice())
    }

    /// The number of instructions in the function.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// assert_eq!(Function::new("f", &SYSV).inst_count(), 0);
    /// ```
    #[inline]
    #[must_use]
    pub fn inst_count(&self) -> usize {
        self.insts.len()
    }

    /// An instruction's opcode, or `None` for one the function does not have.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::{op, SYSV};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let b = f.block()?;
    /// let r = f.push(b, op::RET, &[])?;
    /// assert_eq!(f.opcode(r), Some(op::RET));
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    #[inline]
    #[must_use]
    pub fn opcode(&self, inst: Inst) -> Option<Opcode> {
        self.insts.get(inst.index()).map(|d| d.opcode)
    }

    /// The block an instruction is in.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::{op, SYSV};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let b = f.block()?;
    /// let r = f.push(b, op::RET, &[])?;
    /// assert_eq!(f.inst_block(r), Some(b));
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    #[inline]
    #[must_use]
    pub fn inst_block(&self, inst: Inst) -> Option<Block> {
        self.insts.get(inst.index()).map(|d| d.block)
    }

    /// An instruction's explicit operands (empty for one the function does
    /// not have).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Function, Operand};
    /// use backend_lang::x86_64::{op, SYSV, RAX};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let b = f.block()?;
    /// let i = f.push(b, op::MOV64RI, &[RAX.into(), Operand::Imm(5)])?;
    /// assert_eq!(f.operands(i), [Operand::from(RAX), Operand::Imm(5)]);
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    #[inline]
    #[must_use]
    pub fn operands(&self, inst: Inst) -> &[Operand] {
        self.insts
            .get(inst.index())
            .and_then(|d| {
                self.operands
                    .get(d.ops as usize..d.ops as usize + d.nops as usize)
            })
            .unwrap_or(&[])
    }

    /// An instruction's extra implicit operands, uses first (empty for one
    /// the function does not have). The opcode's own implicit operands are in
    /// its [`InstDesc`](crate::target::InstDesc).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Effects, Function};
    /// use backend_lang::x86_64::{op, SYSV, RAX};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let b = f.block()?;
    /// let r = f.push_with(b, op::RET, &[], Effects::new().uses(&[RAX]))?;
    /// assert_eq!(f.implicit(r).len(), 1);
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    #[inline]
    #[must_use]
    pub fn implicit(&self, inst: Inst) -> &[Implicit] {
        self.insts
            .get(inst.index())
            .and_then(|d| {
                self.implicits
                    .get(d.imp as usize..d.imp as usize + d.nimp as usize)
            })
            .unwrap_or(&[])
    }

    /// An instruction's extra clobbers (empty for one the function does not
    /// have).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Effects, Function};
    /// use backend_lang::x86_64::{op, SYSV};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let b = f.block()?;
    /// let callee = f.symbol("g")?;
    /// let c = f.push_with(b, op::CALL, &[callee.into()], Effects::new().clobbers(SYSV.call_clobbers()))?;
    /// assert_eq!(f.clobbers(c), SYSV.call_clobbers());
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    #[inline]
    #[must_use]
    pub fn clobbers(&self, inst: Inst) -> RegSet {
        self.insts
            .get(inst.index())
            .and_then(|d| self.clobbers.get(d.clobber as usize))
            .copied()
            .unwrap_or(RegSet::EMPTY)
    }

    /// Reserves `bytes` of outgoing argument area at the bottom of the frame
    /// (the largest over all calls; see
    /// [`Abi::assign_args`](crate::target::abi::Abi::assign_args)). Stack
    /// arguments are stored to `[sp + offset]` before a call. Calling it
    /// again keeps the larger value.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// f.reserve_outgoing(24);
    /// f.reserve_outgoing(8);
    /// assert_eq!(f.outgoing(), 24);
    /// ```
    pub fn reserve_outgoing(&mut self, bytes: u32) {
        self.outgoing = self.outgoing.max(bytes);
    }

    /// The outgoing argument area reserved.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// assert_eq!(Function::new("f", &SYSV).outgoing(), 0);
    /// ```
    #[inline]
    #[must_use]
    pub fn outgoing(&self) -> u32 {
        self.outgoing
    }

    /// Lays the frame out without a frame pointer, which frees the frame
    /// pointer register for the body (it is then saved like any other
    /// callee-saved register it writes). The frame pointer is kept by
    /// default.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// f.omit_frame_pointer();
    /// assert!(!f.frame_pointer());
    /// assert!(f.frame_layout().unwrap().prologue().is_empty());
    /// ```
    pub fn omit_frame_pointer(&mut self) {
        self.frame_pointer = false;
    }

    /// Whether the frame keeps a frame pointer.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// assert!(Function::new("f", &SYSV).frame_pointer());
    /// ```
    #[inline]
    #[must_use]
    pub fn frame_pointer(&self) -> bool {
        self.frame_pointer
    }

    /// The registers reserved for the frame, which the body may read but not
    /// write: the stack pointer, and the frame pointer when it is kept.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::{SYSV, RBP, RSP};
    ///
    /// let f = Function::new("f", &SYSV);
    /// assert!(f.reserved().contains(RSP) && f.reserved().contains(RBP));
    /// ```
    #[must_use]
    pub fn reserved(&self) -> RegSet {
        let t = self.target();
        let set = RegSet::of(&[t.stack_pointer()]);
        if self.frame_pointer {
            set.with(t.frame_pointer())
        } else {
            set
        }
    }

    /// Whether any instruction is a call.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// assert!(!Function::new("f", &SYSV).has_calls());
    /// ```
    #[must_use]
    pub fn has_calls(&self) -> bool {
        let t = self.target();
        self.insts.iter().any(|d| {
            t.inst(d.opcode)
                .is_some_and(|desc| desc.kind() == InstKind::Call)
        })
    }

    /// Every physical register the function's instructions may write:
    /// explicit and implicit definitions, clobbers, and physical block
    /// parameters (written by the moves on the edges into them). The
    /// callee-saved registers among them are what the prologue saves.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Function, Operand};
    /// use backend_lang::x86_64::{op, SYSV, RBX, RDX, RAX, FLAGS};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let b = f.block()?;
    /// f.push(b, op::MOV64RI, &[RBX.into(), Operand::Imm(1)])?;
    /// f.push(b, op::CQO, &[])?;
    /// let w = f.written_regs();
    /// assert!(w.contains(RBX) && w.contains(RDX));
    /// assert!(!w.contains(RAX));
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    #[must_use]
    pub fn written_regs(&self) -> RegSet {
        let t = self.target();
        let mut set = RegSet::EMPTY;
        for (i, d) in self.insts.iter().enumerate() {
            let inst = Inst::from_index(i);
            let Some(desc) = t.inst(d.opcode) else {
                continue;
            };
            for p in desc.implicit_defs() {
                set = set.with(*p);
            }
            set = set.union(desc.clobbers()).union(self.clobbers(inst));
            for imp in self.implicit(inst) {
                if imp.kind == ImplicitKind::Def {
                    set = set.with(imp.reg);
                }
            }
            let ops = self.operands(inst);
            let defs = match desc.kind() {
                InstKind::Copy => ops.len().min(1),
                InstKind::ParallelCopy => ops.len() / 2,
                _ => desc
                    .operands()
                    .iter()
                    .take_while(|r| matches!(r, OperandRule::Def(_)))
                    .count(),
            };
            for op in ops.iter().take(defs) {
                if let Some(p) = op.reg().and_then(Reg::as_phys) {
                    set = set.with(p);
                }
            }
        }
        for b in &self.blocks {
            for p in &b.params {
                if let Some(r) = p.reg().and_then(Reg::as_phys) {
                    set = set.with(r);
                }
            }
        }
        set
    }

    /// Lays out the function's frame under its calling convention.
    ///
    /// # Errors
    ///
    /// [`FrameError`] for a slot alignment the frame cannot provide,
    /// overlapping incoming slots, or a frame beyond 32-bit offsets.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Function, SlotKind};
    /// use backend_lang::x86_64::WIN64;
    ///
    /// let mut f = Function::new("f", &WIN64);
    /// let s = f.stack_slot(SlotKind::Local { size: 8, align: 8 })?;
    /// let frame = f.frame_layout().unwrap();
    /// // Win64 has no red zone; after the rbp push the stack pointer is
    /// // 16-aligned, so 8 bytes of locals round up to 16.
    /// assert_eq!(frame.alloc(), 16);
    /// assert_eq!(frame.slot_offset(s), Some(0));
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    pub fn frame_layout(&self) -> Result<FrameLayout, FrameError> {
        self.abi.layout_frame(&FrameInput {
            slots: &self.slots,
            written: self.written_regs(),
            has_calls: self.has_calls(),
            outgoing: self.outgoing,
            frame_pointer: self.frame_pointer,
        })
    }

    pub(crate) fn check_reg(&self, reg: Reg) -> Result<(), MirError> {
        match (reg.as_phys(), reg.as_virt()) {
            (Some(p), _) if !self.target().has_reg(p) => Err(MirError::UnknownPReg(p)),
            (_, Some(v)) if v.index() >= self.vregs.len() => Err(MirError::UnknownVReg(v)),
            _ => Ok(()),
        }
    }

    pub(crate) fn check_slot(&self, slot: StackSlot) -> Result<(), MirError> {
        if slot.index() < self.slots.len() {
            Ok(())
        } else {
            Err(MirError::UnknownSlot(slot))
        }
    }
}
