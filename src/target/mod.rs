//! Target descriptions: the backend's half of `TARGET-DESC.md`.
//!
//! A [`Target`] is data: its register file ([`RegInfo`]), its register
//! classes ([`ClassInfo`]), and its machine instructions ([`InstDesc`]),
//! each instruction with its operand rules, its implicit register uses and
//! definitions, and the registers it clobbers. The generic machinery in this
//! crate (MIR verification, the parallel-move resolver, frame layout) reads
//! only this data; what is specific to an ISA is the description itself and
//! the encoder that turns instructions into bytes (for x86-64, isa-lang).
//!
//! Implicit operands live here, not in isa-lang: as of isa-lang 0.2 the
//! encoder's tables describe the explicit operands of each form only (the
//! fixed `CL` of a shift is visible, `MUL`'s `RDX:RAX` is not). The register
//! allocator needs all of them, so this layer records them per instruction;
//! it can be cross-checked against isa-lang's own data once isa-lang carries
//! it (isa-lang ROADMAP v0.5).
//!
//! [`abi`] adds calling conventions and [`frame`] the stack-frame model.

pub mod abi;
pub mod frame;

use core::fmt;

use crate::regs::{PReg, RegClass, RegSet};

/// The instruction-set architecture a [`Target`] describes, which selects the
/// encoder emission uses.
///
/// # Examples
///
/// ```
/// use backend_lang::target::Isa;
/// use backend_lang::x86_64;
///
/// assert_eq!(x86_64::TARGET.isa(), Isa::X86_64);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Isa {
    /// x86-64 (AMD64 / Intel 64), encoded by `isa-lang`.
    X86_64,
}

/// One physical register: its name, its class, and its hardware number.
///
/// # Examples
///
/// ```
/// use backend_lang::x86_64::{self, GPR, R12};
///
/// let r12 = x86_64::TARGET.reg(R12);
/// assert_eq!(r12.name(), "r12");
/// assert_eq!(r12.class(), GPR);
/// assert_eq!(r12.hw(), 12);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegInfo {
    name: &'static str,
    class: RegClass,
    hw: u8,
}

impl RegInfo {
    /// Describes a register named `name` in `class`, encoded as `hw` (the
    /// number the ISA's instruction fields carry).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::RegClass;
    /// use backend_lang::target::RegInfo;
    ///
    /// let r = RegInfo::new("r0", RegClass::new(0), 0);
    /// assert_eq!(r.name(), "r0");
    /// ```
    #[must_use]
    pub const fn new(name: &'static str, class: RegClass, hw: u8) -> RegInfo {
        RegInfo { name, class, hw }
    }

    /// The register's assembly name.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{self, XMM3};
    ///
    /// assert_eq!(x86_64::TARGET.reg(XMM3).name(), "xmm3");
    /// ```
    #[inline]
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The register's class.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{self, VEC, XMM3};
    ///
    /// assert_eq!(x86_64::TARGET.reg(XMM3).class(), VEC);
    /// ```
    #[inline]
    #[must_use]
    pub const fn class(&self) -> RegClass {
        self.class
    }

    /// The register's hardware number.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{self, RSP};
    ///
    /// assert_eq!(x86_64::TARGET.reg(RSP).hw(), 4);
    /// ```
    #[inline]
    #[must_use]
    pub const fn hw(&self) -> u8 {
        self.hw
    }
}

/// The register returned by [`Target::reg`] for an index the target does not
/// have: named `<invalid>`, in no class a target uses.
const INVALID_REG: RegInfo = RegInfo::new("<invalid>", RegClass::new(u8::MAX), u8::MAX);

/// A register class: its members, the members an allocator may hand out, and
/// the size and alignment of a spill slot for one of its values.
///
/// # Examples
///
/// ```
/// use backend_lang::x86_64::{self, GPR, RSP};
///
/// let gpr = x86_64::TARGET.class(GPR);
/// assert!(gpr.regs().contains(RSP));
/// assert!(!gpr.allocatable().contains(RSP));
/// assert_eq!((gpr.spill_size(), gpr.spill_align()), (8, 8));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClassInfo {
    name: &'static str,
    regs: RegSet,
    allocatable: RegSet,
    spill_size: u32,
    spill_align: u32,
}

impl ClassInfo {
    /// Describes a class named `name` with members `regs`, of which
    /// `allocatable` may be handed out by an allocator, spilled to slots of
    /// `spill_size` bytes aligned to `spill_align`.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::RegSet;
    /// use backend_lang::target::ClassInfo;
    ///
    /// let c = ClassInfo::new("int", RegSet::EMPTY, RegSet::EMPTY, 8, 8);
    /// assert_eq!(c.name(), "int");
    /// ```
    #[must_use]
    pub const fn new(
        name: &'static str,
        regs: RegSet,
        allocatable: RegSet,
        spill_size: u32,
        spill_align: u32,
    ) -> ClassInfo {
        ClassInfo {
            name,
            regs,
            allocatable,
            spill_size,
            spill_align,
        }
    }

    /// The class's name, as the MIR printer writes it.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{self, VEC};
    ///
    /// assert_eq!(x86_64::TARGET.class(VEC).name(), "vec");
    /// ```
    #[inline]
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Every register of the class.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{self, VEC};
    ///
    /// assert_eq!(x86_64::TARGET.class(VEC).regs().len(), 16);
    /// ```
    #[inline]
    #[must_use]
    pub const fn regs(&self) -> RegSet {
        self.regs
    }

    /// The registers an allocator may assign: the class minus the stack
    /// pointer and the frame pointer. (Without a frame pointer the frame
    /// pointer register is allocatable too; see
    /// [`Function::omit_frame_pointer`](crate::mir::Function::omit_frame_pointer).)
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{self, GPR, RBP};
    ///
    /// let gpr = x86_64::TARGET.class(GPR);
    /// assert_eq!(gpr.allocatable().len(), 14);
    /// assert!(!gpr.allocatable().contains(RBP));
    /// ```
    #[inline]
    #[must_use]
    pub const fn allocatable(&self) -> RegSet {
        self.allocatable
    }

    /// Bytes in a spill slot for one value of the class.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{self, VEC};
    ///
    /// assert_eq!(x86_64::TARGET.class(VEC).spill_size(), 16);
    /// ```
    #[inline]
    #[must_use]
    pub const fn spill_size(&self) -> u32 {
        self.spill_size
    }

    /// Required alignment of a spill slot for the class.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{self, VEC};
    ///
    /// assert_eq!(x86_64::TARGET.class(VEC).spill_align(), 16);
    /// ```
    #[inline]
    #[must_use]
    pub const fn spill_align(&self) -> u32 {
        self.spill_align
    }
}

/// The class returned by [`Target::class`] for an index the target does not
/// have: no registers.
const INVALID_CLASS: ClassInfo = ClassInfo::new("<invalid>", RegSet::EMPTY, RegSet::EMPTY, 0, 1);

/// A machine opcode: an index into the target's instruction table.
///
/// Every target's table begins with the two generic pseudo-instructions
/// [`Opcode::COPY`] and [`Opcode::PCOPY`]; the rest are the target's own
/// (for x86-64, the constants in [`x86_64::op`](crate::x86_64::op)).
///
/// # Examples
///
/// ```
/// use backend_lang::target::Opcode;
/// use backend_lang::x86_64::{self, op};
///
/// assert_eq!(x86_64::TARGET.inst(Opcode::COPY).unwrap().name(), "copy");
/// assert_eq!(x86_64::TARGET.inst(op::ADD64).unwrap().name(), "add64");
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Opcode(u16);

impl Opcode {
    /// `dst = copy src`: one move between registers, stack slots, or from an
    /// immediate. Lowered through the parallel-move resolver, so a
    /// slot-to-slot copy gets a temporary register as the target requires.
    /// A copy's width is its register's class; a copy with no register
    /// (slot to slot, or an immediate to a slot) moves a value of the
    /// target's address class (8 bytes on x86-64).
    pub const COPY: Opcode = Opcode(0);
    /// `d0, d1, ... = pcopy s0, s1, ...`: a parallel copy. Every source is
    /// read before any destination is written, as on a control-flow edge.
    /// Lowered through the parallel-move resolver.
    pub const PCOPY: Opcode = Opcode(1);

    /// The opcode with index `index` in a target's table.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::target::Opcode;
    ///
    /// assert_eq!(Opcode::new(0), Opcode::COPY);
    /// ```
    #[inline]
    #[must_use]
    pub const fn new(index: u16) -> Opcode {
        Opcode(index)
    }

    /// The opcode's index in the target's instruction table.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::target::Opcode;
    ///
    /// assert_eq!(Opcode::PCOPY.index(), 1);
    /// ```
    #[inline]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Debug for Opcode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Opcode({})", self.0)
    }
}

/// What an instruction does to control flow, and which generic handling it
/// gets.
///
/// # Examples
///
/// ```
/// use backend_lang::target::InstKind;
/// use backend_lang::x86_64::{self, op};
///
/// assert_eq!(x86_64::TARGET.inst(op::JMP).unwrap().kind(), InstKind::Jump);
/// assert!(InstKind::Jump.is_terminator());
/// assert!(!InstKind::Call.is_terminator());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum InstKind {
    /// An ordinary instruction; control falls through.
    Plain,
    /// [`Opcode::COPY`].
    Copy,
    /// [`Opcode::PCOPY`].
    ParallelCopy,
    /// An unconditional branch to one block, passing arguments.
    Jump,
    /// A two-way conditional branch: a condition, then the taken and the
    /// not-taken block, each with arguments.
    Branch,
    /// A return from the function (the epilogue runs first).
    Return,
    /// A call; control comes back to the next instruction.
    Call,
    /// An instruction that never falls through and never returns (`ud2`).
    Trap,
}

impl InstKind {
    /// Whether the instruction ends a block: jumps, branches, returns, traps.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::target::InstKind;
    ///
    /// assert!(InstKind::Return.is_terminator());
    /// assert!(!InstKind::Plain.is_terminator());
    /// ```
    #[inline]
    #[must_use]
    pub const fn is_terminator(self) -> bool {
        matches!(
            self,
            InstKind::Jump | InstKind::Branch | InstKind::Return | InstKind::Trap
        )
    }
}

/// The values an immediate operand may take.
///
/// Immediates are signed values of the operation's width, as isa-lang takes
/// them (TARGET-DESC §3.1): for a 32-bit operation, all ones is `-1`.
///
/// # Examples
///
/// ```
/// use backend_lang::target::ImmRange;
///
/// assert!(ImmRange::I8.fits(-128));
/// assert!(!ImmRange::I8.fits(128));
/// assert!(ImmRange::U8.fits(255));
/// assert!(!ImmRange::None.fits(0));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ImmRange {
    /// No immediate is accepted.
    None,
    /// `-128..=127`.
    I8,
    /// `0..=255`.
    U8,
    /// `-32768..=32767`.
    I16,
    /// `-2^31..=2^31-1`.
    I32,
    /// Any 64-bit value.
    I64,
}

impl ImmRange {
    /// Whether `value` is in the range.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::target::ImmRange;
    ///
    /// assert!(ImmRange::I32.fits(i64::from(i32::MIN)));
    /// assert!(!ImmRange::I32.fits(1 << 31));
    /// assert!(ImmRange::I64.fits(i64::MIN));
    /// ```
    #[inline]
    #[must_use]
    pub const fn fits(self, value: i64) -> bool {
        match self {
            ImmRange::None => false,
            ImmRange::I8 => value >= -128 && value <= 127,
            ImmRange::U8 => value >= 0 && value <= 255,
            ImmRange::I16 => value >= i16::MIN as i64 && value <= i16::MAX as i64,
            ImmRange::I32 => value >= i32::MIN as i64 && value <= i32::MAX as i64,
            ImmRange::I64 => true,
        }
    }
}

/// Which operand shapes a use accepts: a register, memory (an address or a
/// stack slot), an immediate in a range, a function symbol.
///
/// # Examples
///
/// ```
/// use backend_lang::target::{Accept, ImmRange};
///
/// let rmi = Accept::RM.with_imm(ImmRange::I32);
/// assert!(rmi.reg() && rmi.mem());
/// assert_eq!(rmi.imm(), ImmRange::I32);
/// assert!(!Accept::REG.mem());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Accept {
    reg: bool,
    mem: bool,
    symbol: bool,
    imm: ImmRange,
}

impl Accept {
    /// A register only.
    pub const REG: Accept = Accept {
        reg: true,
        mem: false,
        symbol: false,
        imm: ImmRange::None,
    };
    /// Memory only.
    pub const MEM: Accept = Accept {
        reg: false,
        mem: true,
        symbol: false,
        imm: ImmRange::None,
    };
    /// A register or memory.
    pub const RM: Accept = Accept {
        reg: true,
        mem: true,
        symbol: false,
        imm: ImmRange::None,
    };
    /// Nothing; extend with the `with_*` methods.
    pub const NONE: Accept = Accept {
        reg: false,
        mem: false,
        symbol: false,
        imm: ImmRange::None,
    };

    /// Also accepts immediates in `range`.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::target::{Accept, ImmRange};
    ///
    /// assert_eq!(Accept::REG.with_imm(ImmRange::U8).imm(), ImmRange::U8);
    /// ```
    #[must_use]
    pub const fn with_imm(self, range: ImmRange) -> Accept {
        Accept { imm: range, ..self }
    }

    /// Also accepts a function symbol (a direct call target, or the address
    /// of a function as a RIP-relative memory operand).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::target::Accept;
    ///
    /// assert!(Accept::MEM.with_symbol().symbol());
    /// ```
    #[must_use]
    pub const fn with_symbol(self) -> Accept {
        Accept {
            symbol: true,
            ..self
        }
    }

    /// Whether a register is accepted.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::target::Accept;
    ///
    /// assert!(Accept::RM.reg());
    /// ```
    #[inline]
    #[must_use]
    pub const fn reg(self) -> bool {
        self.reg
    }

    /// Whether memory (an address or a stack slot) is accepted.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::target::Accept;
    ///
    /// assert!(Accept::MEM.mem());
    /// ```
    #[inline]
    #[must_use]
    pub const fn mem(self) -> bool {
        self.mem
    }

    /// Whether a function symbol is accepted.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::target::Accept;
    ///
    /// assert!(!Accept::RM.symbol());
    /// ```
    #[inline]
    #[must_use]
    pub const fn symbol(self) -> bool {
        self.symbol
    }

    /// The immediates accepted.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::target::{Accept, ImmRange};
    ///
    /// assert_eq!(Accept::RM.imm(), ImmRange::None);
    /// ```
    #[inline]
    #[must_use]
    pub const fn imm(self) -> ImmRange {
        self.imm
    }
}

/// The rule for one explicit operand of an instruction.
///
/// Operands are listed definitions first, then uses, as the MIR printer
/// shows them (`%2 = add64 %0, %1`).
///
/// # Examples
///
/// ```
/// use backend_lang::target::{Accept, ImmRange, OperandRule};
/// use backend_lang::x86_64::{self, op, GPR};
///
/// let add = x86_64::TARGET.inst(op::ADD64).unwrap();
/// assert_eq!(add.operands()[0], OperandRule::Def(GPR));
/// assert_eq!(add.operands()[1], OperandRule::Tied(0));
/// assert_eq!(add.operands()[2], OperandRule::Use(GPR, Accept::RM.with_imm(ImmRange::I32)));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OperandRule {
    /// A register the instruction writes, of this class.
    Def(RegClass),
    /// A use that must be the same register as definition `n`: the
    /// two-address constraint (`add d, s` reads and writes `d`). In SSA form
    /// the definition and the tied use are different virtual registers; the
    /// register allocator gives them one physical register.
    Tied(u8),
    /// A value the instruction reads, of this class (for a register; the
    /// class of a memory operand's base and index is the target's address
    /// class), in the shapes `Accept` allows.
    Use(RegClass, Accept),
    /// A condition code.
    Cond,
    /// A branch target with its arguments.
    Block,
}

/// One machine instruction's description.
///
/// # Examples
///
/// ```
/// use backend_lang::target::InstKind;
/// use backend_lang::x86_64::{self, op, RAX, RDX, FLAGS};
///
/// let div = x86_64::TARGET.inst(op::IDIV64).unwrap();
/// assert_eq!(div.name(), "idiv64");
/// assert_eq!(div.kind(), InstKind::Plain);
/// assert_eq!(div.implicit_uses(), [RAX, RDX]);
/// assert_eq!(div.implicit_defs(), [RAX, RDX, FLAGS]);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InstDesc {
    name: &'static str,
    kind: InstKind,
    operands: &'static [OperandRule],
    implicit_uses: &'static [PReg],
    implicit_defs: &'static [PReg],
    clobbers: RegSet,
}

impl InstDesc {
    /// Describes an instruction.
    ///
    /// `implicit_uses` are registers read without being named as operands,
    /// `implicit_defs` registers written that way (results the code may read
    /// afterwards, such as `idiv`'s quotient in `rax`), and `clobbers`
    /// registers left with unspecified contents.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::RegSet;
    /// use backend_lang::target::{InstDesc, InstKind};
    ///
    /// let nop = InstDesc::new("nop", InstKind::Plain, &[], &[], &[], RegSet::EMPTY);
    /// assert!(nop.operands().is_empty());
    /// ```
    #[must_use]
    pub const fn new(
        name: &'static str,
        kind: InstKind,
        operands: &'static [OperandRule],
        implicit_uses: &'static [PReg],
        implicit_defs: &'static [PReg],
        clobbers: RegSet,
    ) -> InstDesc {
        InstDesc {
            name,
            kind,
            operands,
            implicit_uses,
            implicit_defs,
            clobbers,
        }
    }

    /// The instruction's MIR name.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{self, op};
    ///
    /// assert_eq!(x86_64::TARGET.inst(op::MOVZX8).unwrap().name(), "movzx8");
    /// ```
    #[inline]
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// What the instruction does to control flow.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::target::InstKind;
    /// use backend_lang::x86_64::{self, op};
    ///
    /// assert_eq!(x86_64::TARGET.inst(op::RET).unwrap().kind(), InstKind::Return);
    /// ```
    #[inline]
    #[must_use]
    pub const fn kind(&self) -> InstKind {
        self.kind
    }

    /// The explicit operands, definitions first. Empty for
    /// [`Opcode::COPY`] and [`Opcode::PCOPY`], whose operands are checked
    /// by their own rule.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{self, op};
    ///
    /// assert_eq!(x86_64::TARGET.inst(op::CMP64).unwrap().operands().len(), 2);
    /// ```
    #[inline]
    #[must_use]
    pub const fn operands(&self) -> &'static [OperandRule] {
        self.operands
    }

    /// The number of explicit definitions (the leading `Def` operands).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{self, op};
    ///
    /// assert_eq!(x86_64::TARGET.inst(op::ADD64).unwrap().defs(), 1);
    /// assert_eq!(x86_64::TARGET.inst(op::CMP64).unwrap().defs(), 0);
    /// ```
    #[must_use]
    pub fn defs(&self) -> usize {
        self.operands
            .iter()
            .take_while(|r| matches!(r, OperandRule::Def(_)))
            .count()
    }

    /// Registers the instruction reads without naming them.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{self, op, RAX};
    ///
    /// assert_eq!(x86_64::TARGET.inst(op::CQO).unwrap().implicit_uses(), [RAX]);
    /// ```
    #[inline]
    #[must_use]
    pub const fn implicit_uses(&self) -> &'static [PReg] {
        self.implicit_uses
    }

    /// Registers the instruction writes without naming them, with defined
    /// results.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{self, op, RDX};
    ///
    /// assert_eq!(x86_64::TARGET.inst(op::CQO).unwrap().implicit_defs(), [RDX]);
    /// ```
    #[inline]
    #[must_use]
    pub const fn implicit_defs(&self) -> &'static [PReg] {
        self.implicit_defs
    }

    /// Registers the instruction leaves with unspecified contents.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{self, op, RCX, R11};
    ///
    /// let clobbers = x86_64::TARGET.inst(op::SYSCALL).unwrap().clobbers();
    /// assert!(clobbers.contains(RCX) && clobbers.contains(R11));
    /// ```
    #[inline]
    #[must_use]
    pub const fn clobbers(&self) -> RegSet {
        self.clobbers
    }
}

/// How a target can move values, which picks the parallel-move resolver's
/// strategy (see [`moves`](crate::moves)).
///
/// # Examples
///
/// ```
/// use backend_lang::target::ImmRange;
/// use backend_lang::x86_64;
///
/// let caps = x86_64::TARGET.move_caps();
/// assert!(caps.swap());
/// assert!(!caps.mem_to_mem());
/// assert_eq!(caps.store_imm(), ImmRange::I32);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MoveCaps {
    swap: bool,
    mem_to_mem: bool,
    store_imm: ImmRange,
}

impl MoveCaps {
    /// Describes a target's moves: whether it can exchange two registers in
    /// one instruction, move memory to memory directly, and which immediates
    /// it can store to memory directly.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::target::{ImmRange, MoveCaps};
    ///
    /// let caps = MoveCaps::new(false, false, ImmRange::None);
    /// assert!(!caps.swap());
    /// ```
    #[must_use]
    pub const fn new(swap: bool, mem_to_mem: bool, store_imm: ImmRange) -> MoveCaps {
        MoveCaps {
            swap,
            mem_to_mem,
            store_imm,
        }
    }

    /// Whether two registers of a class can be exchanged by one instruction
    /// (x86-64 `xchg`).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64;
    ///
    /// assert!(x86_64::TARGET.move_caps().swap());
    /// ```
    #[inline]
    #[must_use]
    pub const fn swap(self) -> bool {
        self.swap
    }

    /// Whether memory can be copied to memory without a register.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64;
    ///
    /// assert!(!x86_64::TARGET.move_caps().mem_to_mem());
    /// ```
    #[inline]
    #[must_use]
    pub const fn mem_to_mem(self) -> bool {
        self.mem_to_mem
    }

    /// The immediates that can be stored to memory in one instruction.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::target::ImmRange;
    /// use backend_lang::x86_64;
    ///
    /// assert!(x86_64::TARGET.move_caps().store_imm().fits(-1));
    /// ```
    #[inline]
    #[must_use]
    pub const fn store_imm(self) -> ImmRange {
        self.store_imm
    }
}

/// A target: register file, classes, instructions, and the registers with
/// fixed jobs.
///
/// Targets are static data; [`x86_64::TARGET`](crate::x86_64::TARGET) is the
/// one this release ships.
///
/// # Examples
///
/// ```
/// use backend_lang::x86_64::{self, RSP, RBP, FLAGS};
///
/// let t = &x86_64::TARGET;
/// assert_eq!(t.name(), "x86-64");
/// assert_eq!(t.stack_pointer(), RSP);
/// assert_eq!(t.frame_pointer(), RBP);
/// assert_eq!(t.flags(), Some(FLAGS));
/// ```
#[derive(Debug)]
pub struct Target {
    name: &'static str,
    isa: Isa,
    regs: &'static [RegInfo],
    classes: &'static [ClassInfo],
    insts: &'static [InstDesc],
    stack_pointer: PReg,
    frame_pointer: PReg,
    flags: Option<PReg>,
    addr_class: RegClass,
    move_caps: MoveCaps,
    max_mem_operands: u8,
    conds: &'static [&'static str],
}

/// The registers with fixed jobs, and the target-wide limits, for
/// [`Target::new`].
///
/// # Examples
///
/// ```
/// use backend_lang::target::TargetConfig;
/// use backend_lang::x86_64::{self, RSP};
///
/// let cfg = TargetConfig::of(&x86_64::TARGET);
/// assert_eq!(cfg.stack_pointer, RSP);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetConfig {
    /// The stack pointer.
    pub stack_pointer: PReg,
    /// The frame pointer.
    pub frame_pointer: PReg,
    /// The condition-flags register, if the ISA has one.
    pub flags: Option<PReg>,
    /// The class of a memory operand's base and index registers.
    pub addr_class: RegClass,
    /// How values move.
    pub move_caps: MoveCaps,
    /// The most memory operands one instruction may have.
    pub max_mem_operands: u8,
    /// The condition codes' names, indexed by
    /// [`CondCode::code`](crate::mir::CondCode::code) (at most 255).
    pub conds: &'static [&'static str],
}

impl TargetConfig {
    /// The configuration of an existing target, to derive a variant from.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::target::TargetConfig;
    /// use backend_lang::x86_64;
    ///
    /// assert_eq!(TargetConfig::of(&x86_64::TARGET).conds.len(), 16);
    /// ```
    #[must_use]
    pub const fn of(target: &Target) -> TargetConfig {
        TargetConfig {
            stack_pointer: target.stack_pointer,
            frame_pointer: target.frame_pointer,
            flags: target.flags,
            addr_class: target.addr_class,
            move_caps: target.move_caps,
            max_mem_operands: target.max_mem_operands,
            conds: target.conds,
        }
    }
}

impl Target {
    /// Describes a target.
    ///
    /// `insts` must begin with the [`Opcode::COPY`] and [`Opcode::PCOPY`]
    /// descriptions (see [`Target::generic_insts`]); registers' classes must
    /// index `classes`; at most [`MAX_PREGS`](crate::MAX_PREGS) registers.
    /// The crate's own targets are checked by its tests; a hand-made one
    /// that breaks these rules is caught by [`Function::verify`] as operands
    /// that fit no class.
    ///
    /// # Examples
    ///
    /// A variant of x86-64 without `xchg`, to make the parallel-move
    /// resolver break cycles through a scratch register instead:
    ///
    /// ```
    /// use backend_lang::target::{MoveCaps, ImmRange, Target, TargetConfig};
    /// use backend_lang::x86_64;
    ///
    /// let base = &x86_64::TARGET;
    /// let mut cfg = TargetConfig::of(base);
    /// cfg.move_caps = MoveCaps::new(false, false, ImmRange::I32);
    /// let no_swap = Target::new("x86-64-noswap", base.isa(), base.regs(), base.classes(), base.insts(), cfg);
    /// assert!(!no_swap.move_caps().swap());
    /// ```
    ///
    /// [`Function::verify`]: crate::mir::Function::verify
    #[must_use]
    pub const fn new(
        name: &'static str,
        isa: Isa,
        regs: &'static [RegInfo],
        classes: &'static [ClassInfo],
        insts: &'static [InstDesc],
        config: TargetConfig,
    ) -> Target {
        Target {
            name,
            isa,
            regs,
            classes,
            insts,
            stack_pointer: config.stack_pointer,
            frame_pointer: config.frame_pointer,
            flags: config.flags,
            addr_class: config.addr_class,
            move_caps: config.move_caps,
            max_mem_operands: config.max_mem_operands,
            conds: config.conds,
        }
    }

    /// The two generic descriptions every instruction table starts with,
    /// [`Opcode::COPY`] then [`Opcode::PCOPY`].
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::target::{InstKind, Target};
    ///
    /// let [copy, pcopy] = Target::generic_insts();
    /// assert_eq!(copy.kind(), InstKind::Copy);
    /// assert_eq!(pcopy.name(), "pcopy");
    /// ```
    #[must_use]
    pub const fn generic_insts() -> [InstDesc; 2] {
        [
            InstDesc::new("copy", InstKind::Copy, &[], &[], &[], RegSet::EMPTY),
            InstDesc::new(
                "pcopy",
                InstKind::ParallelCopy,
                &[],
                &[],
                &[],
                RegSet::EMPTY,
            ),
        ]
    }

    /// The target's name.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64;
    ///
    /// assert_eq!(x86_64::TARGET.name(), "x86-64");
    /// ```
    #[inline]
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The ISA, which selects the encoder.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::target::Isa;
    /// use backend_lang::x86_64;
    ///
    /// assert_eq!(x86_64::TARGET.isa(), Isa::X86_64);
    /// ```
    #[inline]
    #[must_use]
    pub const fn isa(&self) -> Isa {
        self.isa
    }

    /// The register file, indexed by [`PReg::index`].
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64;
    ///
    /// assert_eq!(x86_64::TARGET.regs().len(), 33);
    /// ```
    #[inline]
    #[must_use]
    pub const fn regs(&self) -> &'static [RegInfo] {
        self.regs
    }

    /// One register's description. An index the target does not have gets a
    /// placeholder named `<invalid>`, in no class.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::PReg;
    /// use backend_lang::x86_64::{self, RDI};
    ///
    /// assert_eq!(x86_64::TARGET.reg(RDI).name(), "rdi");
    /// assert_eq!(x86_64::TARGET.reg(PReg::new(100).unwrap()).name(), "<invalid>");
    /// ```
    #[inline]
    #[must_use]
    pub fn reg(&self, reg: PReg) -> &RegInfo {
        self.regs.get(reg.index()).unwrap_or(&INVALID_REG)
    }

    /// Whether the target has a register with this index.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::PReg;
    /// use backend_lang::x86_64::{self, RAX};
    ///
    /// assert!(x86_64::TARGET.has_reg(RAX));
    /// assert!(!x86_64::TARGET.has_reg(PReg::new(100).unwrap()));
    /// ```
    #[inline]
    #[must_use]
    pub fn has_reg(&self, reg: PReg) -> bool {
        reg.index() < self.regs.len()
    }

    /// The class table, indexed by [`RegClass::index`].
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64;
    ///
    /// assert_eq!(x86_64::TARGET.classes().len(), 3);
    /// ```
    #[inline]
    #[must_use]
    pub const fn classes(&self) -> &'static [ClassInfo] {
        self.classes
    }

    /// One class's description. A class the target does not have gets an
    /// empty placeholder named `<invalid>`.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::RegClass;
    /// use backend_lang::x86_64::{self, GPR};
    ///
    /// assert_eq!(x86_64::TARGET.class(GPR).name(), "gpr");
    /// assert!(x86_64::TARGET.class(RegClass::new(9)).regs().is_empty());
    /// ```
    #[inline]
    #[must_use]
    pub fn class(&self, class: RegClass) -> &ClassInfo {
        self.classes.get(class.index()).unwrap_or(&INVALID_CLASS)
    }

    /// Whether the target has this class.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::RegClass;
    /// use backend_lang::x86_64::{self, VEC};
    ///
    /// assert!(x86_64::TARGET.has_class(VEC));
    /// assert!(!x86_64::TARGET.has_class(RegClass::new(9)));
    /// ```
    #[inline]
    #[must_use]
    pub fn has_class(&self, class: RegClass) -> bool {
        class.index() < self.classes.len()
    }

    /// The instruction table, indexed by [`Opcode::index`].
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64;
    ///
    /// assert!(x86_64::TARGET.insts().len() > 80);
    /// ```
    #[inline]
    #[must_use]
    pub const fn insts(&self) -> &'static [InstDesc] {
        self.insts
    }

    /// One instruction's description, or `None` for an opcode the target
    /// does not have.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::target::Opcode;
    /// use backend_lang::x86_64::{self, op};
    ///
    /// assert!(x86_64::TARGET.inst(op::LEA64).is_some());
    /// assert!(x86_64::TARGET.inst(Opcode::new(60000)).is_none());
    /// ```
    #[inline]
    #[must_use]
    pub fn inst(&self, opcode: Opcode) -> Option<&'static InstDesc> {
        self.insts.get(opcode.index())
    }

    /// The opcode whose MIR name is `name`, by a scan of the table.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{self, op};
    ///
    /// assert_eq!(x86_64::TARGET.opcode("sub32"), Some(op::SUB32));
    /// assert_eq!(x86_64::TARGET.opcode("frobnicate"), None);
    /// ```
    #[must_use]
    pub fn opcode(&self, name: &str) -> Option<Opcode> {
        self.insts
            .iter()
            .position(|d| d.name == name)
            .and_then(|i| u16::try_from(i).ok())
            .map(Opcode)
    }

    /// The stack pointer.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{self, RSP};
    ///
    /// assert_eq!(x86_64::TARGET.stack_pointer(), RSP);
    /// ```
    #[inline]
    #[must_use]
    pub const fn stack_pointer(&self) -> PReg {
        self.stack_pointer
    }

    /// The frame pointer.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{self, RBP};
    ///
    /// assert_eq!(x86_64::TARGET.frame_pointer(), RBP);
    /// ```
    #[inline]
    #[must_use]
    pub const fn frame_pointer(&self) -> PReg {
        self.frame_pointer
    }

    /// The condition-flags register, modelled as a physical register so that
    /// flag producers and consumers are ordinary implicit definitions and
    /// uses.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{self, FLAGS};
    ///
    /// assert_eq!(x86_64::TARGET.flags(), Some(FLAGS));
    /// ```
    #[inline]
    #[must_use]
    pub const fn flags(&self) -> Option<PReg> {
        self.flags
    }

    /// The class of memory operands' base and index registers.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{self, GPR};
    ///
    /// assert_eq!(x86_64::TARGET.addr_class(), GPR);
    /// ```
    #[inline]
    #[must_use]
    pub const fn addr_class(&self) -> RegClass {
        self.addr_class
    }

    /// How values move on this target.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64;
    ///
    /// assert!(x86_64::TARGET.move_caps().swap());
    /// ```
    #[inline]
    #[must_use]
    pub const fn move_caps(&self) -> MoveCaps {
        self.move_caps
    }

    /// The most memory operands one instruction may have (one on x86-64).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64;
    ///
    /// assert_eq!(x86_64::TARGET.max_mem_operands(), 1);
    /// ```
    #[inline]
    #[must_use]
    pub const fn max_mem_operands(&self) -> u8 {
        self.max_mem_operands
    }

    /// The number of condition codes; a `Cond` operand is below it.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64;
    ///
    /// assert_eq!(x86_64::TARGET.conds(), 16);
    /// ```
    #[inline]
    #[must_use]
    pub const fn conds(&self) -> u8 {
        if self.conds.len() > u8::MAX as usize {
            u8::MAX
        } else {
            self.conds.len() as u8
        }
    }

    /// A condition code's name (`<invalid>` for one the target does not
    /// have).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::CondCode;
    /// use backend_lang::x86_64;
    ///
    /// assert_eq!(x86_64::TARGET.cond_name(CondCode::new(4)), "e");
    /// assert_eq!(x86_64::TARGET.cond_name(CondCode::new(99)), "<invalid>");
    /// ```
    #[inline]
    #[must_use]
    pub fn cond_name(&self, cond: crate::mir::CondCode) -> &'static str {
        self.conds
            .get(usize::from(cond.code()))
            .copied()
            .unwrap_or("<invalid>")
    }

    /// The class of a physical register, or `None` if the target does not
    /// have it.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{self, GPR, R8};
    ///
    /// assert_eq!(x86_64::TARGET.class_of(R8), Some(GPR));
    /// ```
    #[inline]
    #[must_use]
    pub fn class_of(&self, reg: PReg) -> Option<RegClass> {
        self.regs.get(reg.index()).map(RegInfo::class)
    }
}
