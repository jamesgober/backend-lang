//! SSA register allocation: virtual registers to physical registers and
//! stack slots.
//!
//! The input is a MIR function in SSA form (one definition per virtual
//! register, every use dominated by its definition) in which physical
//! registers appear only in short, block-local ranges: the copies into and
//! out of argument and result registers around calls and returns, the
//! operands of fixed-register instructions (`idiv`'s `rdx:rax`, a shift's
//! `cl`), and the entry block's incoming arguments. Instruction selection
//! ([`isel`](crate::isel)) produces exactly this. The output is the same
//! function on physical registers, ready for [`emit`](crate::emit).
//!
//! # Algorithm
//!
//! 1. **Liveness** by SSA path exploration: from each use outside its
//!    definition's block, walk predecessors up to the definition, marking
//!    the value live-in on the way. Cost is the size of the live sets, with
//!    an explicit budget.
//! 2. **Constraints** in one backward walk per block: which uses are last
//!    uses, which physical registers are live after each instruction, and,
//!    for each value, every register written while it is live (a call's
//!    clobbers, a fixed-register definition), so the value never sits in one
//!    of them.
//! 3. **Coloring in dominance order.** In strict SSA, two values that
//!    interfere have one definition dominating the other, and the first is
//!    live where the second is defined; so walking blocks in reverse
//!    postorder and giving each value at its definition a register no live
//!    value holds is a correct coloring, with no interference graph.
//!    Block parameters get registers at their block's start; the parallel
//!    copy on each edge is resolved at emission by the move resolver.
//! 4. **Coalescing.** Values joined by a copy, a two-address tie, or a
//!    block parameter and its arguments form affinity classes (union-find);
//!    a value takes its class's register, its tied operand's register, or the
//!    physical register it is copied to or from, whenever that register is
//!    free, so the copy, the tie, or the edge move disappears. It is best
//!    effort: a value that interferes with its class takes another register
//!    and keeps its move.
//! 5. **Spilling** when no register is free at a definition: the value with
//!    the lowest weight (uses per instruction spanned) among the definition
//!    and the values occupying a usable register is spilled for its whole
//!    life (spill everywhere: no live-range splitting). A spilled constant
//!    or address is **rematerialized** at each use instead of stored; any
//!    other value is stored to a spill slot after its definition and
//!    reloaded into a short-lived temporary before each use that cannot read
//!    the slot directly (x86-64 reads one memory operand per instruction,
//!    and branch arguments and copies read slots as they are). A spilled
//!    block parameter simply lives in its slot. The allocation then reruns;
//!    temporaries are never spilled, so each round spills at least one more
//!    long-lived value, and after [`MAX_ROUNDS`] every remaining long-lived
//!    value is spilled, which always succeeds.
//!
//! **Callee-saved registers** are used like any other; a value live across a
//! call can only sit in one (the call clobbers the rest), and the frame
//! saves exactly the callee-saved registers the function writes.
//!
//! **Stack maps** need nothing from the allocator: instruction selection
//! stores each GC reference of a safepoint into a dedicated root slot before
//! the call and reloads the relocated value after it, so no reference is in
//! a register while the collector runs, and the slots are recorded on the
//! call ([`Function::set_call_site`]).

mod color;
mod liveness;
mod rewrite;

use alloc::vec::Vec;
use core::fmt;

use crate::mir::cfg::Cfg;
use crate::mir::{Block, Function, MirError, StackSlot};
use crate::regs::{PReg, RegClass, VReg};

/// After this many rounds that still spill, every long-lived value is
/// spilled at once, which always allocates.
pub const MAX_ROUNDS: u32 = 6;

/// The default budget on live-set entries (one per value live into a
/// block): 16 Mi entries, 64 MiB.
pub const DEFAULT_LIVE_BUDGET: usize = 1 << 24;

/// Why a function could not be allocated.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::{Function, Operand};
/// use backend_lang::regalloc::{self, RegAllocError};
/// use backend_lang::x86_64::{op, GPR, SYSV};
///
/// let mut f = Function::new("f", &SYSV);
/// let b = f.block()?;
/// let v = f.vreg(GPR)?;
/// let w = f.vreg(GPR)?;
/// f.push(b, op::MOV64, &[w.into(), v.into()])?; // v is never defined
/// f.push(b, op::RET, &[])?;
/// assert!(matches!(regalloc::allocate(&mut f), Err(RegAllocError::Mir(_))));
/// # Ok::<(), backend_lang::mir::MirError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RegAllocError {
    /// The function is malformed or not in SSA form
    /// ([`Function::verify_ssa`]).
    Mir(MirError),
    /// A physical register is live into a block other than through the
    /// entry block's parameters. Physical registers must be block-local:
    /// copy values out of them into virtual registers.
    PhysLiveIn {
        /// The block.
        block: Block,
        /// The register.
        reg: PReg,
    },
    /// An instruction writes a physical register that holds a live value
    /// (two fixed-register ranges overlap).
    PhysClobbered {
        /// The block.
        block: Block,
        /// The register.
        reg: PReg,
    },
    /// The live sets exceed the budget.
    TooLarge,
    /// No register of the class can hold a value even after spilling
    /// everything spillable (the class has no allocatable registers, or a
    /// single instruction needs more than the class has).
    NoRegister {
        /// The class.
        class: RegClass,
    },
}

impl fmt::Display for RegAllocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegAllocError::Mir(e) => fmt::Display::fmt(e, f),
            RegAllocError::PhysLiveIn { block, reg } => write!(
                f,
                "physical register {} is live into {block}; copy it to a virtual register first",
                reg.index()
            ),
            RegAllocError::PhysClobbered { block, reg } => write!(
                f,
                "in {block}, physical register {} is written while it holds a live value",
                reg.index()
            ),
            RegAllocError::TooLarge => f.write_str("the live sets exceed the allocator's budget"),
            RegAllocError::NoRegister { class } => write!(
                f,
                "no register of class {} can be found for a value that cannot be spilled",
                class.index()
            ),
        }
    }
}

impl core::error::Error for RegAllocError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            RegAllocError::Mir(e) => Some(e),
            _ => None,
        }
    }
}

impl From<MirError> for RegAllocError {
    fn from(e: MirError) -> RegAllocError {
        RegAllocError::Mir(e)
    }
}

/// Where a virtual register lives for its whole life after allocation.
///
/// # Examples
///
/// ```
/// use backend_lang::regalloc::Location;
/// use backend_lang::x86_64::RAX;
///
/// assert_eq!(Location::Reg(RAX), Location::Reg(RAX));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Location {
    /// Not live anywhere reachable (never used, or only in dead code).
    None,
    /// A physical register.
    Reg(PReg),
    /// A stack slot (a spilled block parameter).
    Slot(StackSlot),
}

/// Allocation settings.
///
/// # Examples
///
/// ```
/// use backend_lang::regalloc::Options;
///
/// let o = Options::new().with_live_budget(1 << 20).keep_assignment(true);
/// assert_eq!(o.live_budget, 1 << 20);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Options {
    /// The most live-set entries (values live into a block, summed over
    /// blocks) the allocator builds before giving up with
    /// [`RegAllocError::TooLarge`].
    pub live_budget: usize,
    /// Whether to keep the final virtual function and its assignment in the
    /// [`Allocation`] (for checking an allocation independently; it costs a
    /// copy of the function).
    pub keep_assignment: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options::new()
    }
}

impl Options {
    /// The defaults: [`DEFAULT_LIVE_BUDGET`], no assignment kept.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::regalloc::Options;
    ///
    /// assert!(!Options::new().keep_assignment);
    /// ```
    #[must_use]
    pub const fn new() -> Options {
        Options {
            live_budget: DEFAULT_LIVE_BUDGET,
            keep_assignment: false,
        }
    }

    /// Sets the live-set budget.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::regalloc::Options;
    ///
    /// assert_eq!(Options::new().with_live_budget(10).live_budget, 10);
    /// ```
    #[must_use]
    pub const fn with_live_budget(mut self, entries: usize) -> Options {
        self.live_budget = entries;
        self
    }

    /// Keeps the final virtual function and assignment.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::regalloc::Options;
    ///
    /// assert!(Options::new().keep_assignment(true).keep_assignment);
    /// ```
    #[must_use]
    pub const fn keep_assignment(mut self, keep: bool) -> Options {
        self.keep_assignment = keep;
        self
    }
}

/// What the allocator did.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::{Effects, Function, Operand};
/// use backend_lang::regalloc;
/// use backend_lang::x86_64::{op, GPR, SYSV, RAX};
///
/// let mut f = Function::new("f", &SYSV);
/// let b = f.block()?;
/// let v = f.vreg(GPR)?;
/// f.push(b, op::MOV64RI, &[v.into(), Operand::Imm(7)])?;
/// f.push(b, op::COPY, &[RAX.into(), v.into()])?;
/// f.push_with(b, op::RET, &[], Effects::new().uses(&[RAX]))?;
/// let a = regalloc::allocate(&mut f)?;
/// assert_eq!(a.spilled(), 0);
/// // The value was given rax, the register it is copied to: no move left.
/// assert_eq!(backend_lang::emit(&f)?, [0x55, 0x48, 0x89, 0xE5, 0xB8, 7, 0, 0, 0, 0x5D, 0xC3]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone, Debug)]
pub struct Allocation {
    rounds: u32,
    spilled: u32,
    rematerialized: u32,
    max_pressure: u32,
    assignment: Option<(Function, Vec<Location>)>,
}

impl Allocation {
    /// The coloring rounds run (1 when nothing spilled).
    ///
    /// # Examples
    ///
    /// See [`Allocation`].
    #[inline]
    #[must_use]
    pub fn rounds(&self) -> u32 {
        self.rounds
    }

    /// The values spilled to stack slots.
    ///
    /// # Examples
    ///
    /// See [`Allocation`].
    #[inline]
    #[must_use]
    pub fn spilled(&self) -> u32 {
        self.spilled
    }

    /// The spilled values rematerialized at their uses instead of stored.
    ///
    /// # Examples
    ///
    /// See [`Allocation`].
    #[inline]
    #[must_use]
    pub fn rematerialized(&self) -> u32 {
        self.rematerialized
    }

    /// The most values of one class live at once in the final round.
    ///
    /// # Examples
    ///
    /// See [`Allocation`].
    #[inline]
    #[must_use]
    pub fn max_pressure(&self) -> u32 {
        self.max_pressure
    }

    /// With [`Options::keep_assignment`]: the virtual function the final
    /// round colored (after spill code was inserted), and each of its virtual
    /// registers' locations, indexed by [`VReg::index`].
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Function, Operand};
    /// use backend_lang::regalloc::{self, Location, Options};
    /// use backend_lang::x86_64::{op, GPR, SYSV};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let b = f.block()?;
    /// let v = f.vreg(GPR)?;
    /// f.push(b, op::MOV64RI, &[v.into(), Operand::Imm(7)])?;
    /// f.push(b, op::CMP64, &[v.into(), Operand::Imm(1)])?;
    /// f.push(b, op::RET, &[])?;
    /// let a = regalloc::allocate_with(&mut f, &Options::new().keep_assignment(true))?;
    /// let (virt, locs) = a.assignment().unwrap();
    /// assert_eq!(virt.vreg_count(), locs.len());
    /// assert!(matches!(locs[0], Location::Reg(_)));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    #[must_use]
    pub fn assignment(&self) -> Option<(&Function, &[Location])> {
        self.assignment.as_ref().map(|(f, l)| (f, l.as_slice()))
    }
}

/// Allocates registers for `f` in place with the default [`Options`].
///
/// # Errors
///
/// A [`RegAllocError`]; `f` is unspecified after an error.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::{Effects, Function, Operand};
/// use backend_lang::regalloc;
/// use backend_lang::x86_64::{op, GPR, SYSV, RAX, RDI, RSI};
///
/// // fn add(a, b) -> a + b, on virtual registers.
/// let mut f = Function::new("add", &SYSV);
/// f.omit_frame_pointer();
/// let b = f.block()?;
/// f.add_param(b, RDI.into())?;
/// f.add_param(b, RSI.into())?;
/// let (x, y, s) = (f.vreg(GPR)?, f.vreg(GPR)?, f.vreg(GPR)?);
/// f.push(b, op::PCOPY, &[x.into(), y.into(), RDI.into(), RSI.into()])?;
/// f.push(b, op::ADD64, &[s.into(), x.into(), y.into()])?;
/// f.push(b, op::COPY, &[RAX.into(), s.into()])?;
/// f.push_with(b, op::RET, &[], Effects::new().uses(&[RAX]))?;
/// regalloc::allocate(&mut f)?;
/// let code = backend_lang::emit(&f)?;
/// assert_eq!(code.last(), Some(&0xC3));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn allocate(f: &mut Function) -> Result<Allocation, RegAllocError> {
    allocate_with(f, &Options::new())
}

/// Allocates registers for `f` in place.
///
/// # Errors
///
/// A [`RegAllocError`]; `f` is unspecified after an error.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::{Function, Operand};
/// use backend_lang::regalloc::{self, Options, RegAllocError};
/// use backend_lang::x86_64::{op, GPR, SYSV};
///
/// let mut f = Function::new("f", &SYSV);
/// let (a, b) = (f.block()?, f.block()?);
/// let v = f.vreg(GPR)?;
/// f.push(a, op::MOV64RI, &[v.into(), Operand::Imm(1)])?;
/// let to_b = f.block_call(b, &[])?;
/// f.push(a, op::JMP, &[to_b.into()])?;
/// f.push(b, op::CMP64, &[v.into(), Operand::Imm(0)])?;
/// f.push(b, op::RET, &[])?;
/// // v is live into b: one live-set entry, over a budget of zero.
/// let err = regalloc::allocate_with(&mut f, &Options::new().with_live_budget(0));
/// assert_eq!(err.unwrap_err(), RegAllocError::TooLarge);
/// # Ok::<(), backend_lang::mir::MirError>(())
/// ```
pub fn allocate_with(f: &mut Function, options: &Options) -> Result<Allocation, RegAllocError> {
    // The control-flow graph never changes during allocation (spill code
    // and dead-code removal touch no branch), so it is built once.
    f.verify()?;
    let cfg = Cfg::new(f);
    f.check_dominance(&cfg)?;
    let mut state = rewrite::SpillState::new(f);
    rewrite::remove_dead_pure(f, &cfg, &mut state)?;
    let mut rounds = 0u32;
    loop {
        rounds += 1;
        let last = rounds > MAX_ROUNDS;
        if last {
            rewrite::spill_all(f, &cfg, &mut state)?;
        }
        let analysis = liveness::analyze(f, &cfg, &state, options.live_budget)?;
        let colored = color::color(f, &analysis, &state)?;
        if colored.spill.is_empty() {
            let assignment = if options.keep_assignment {
                Some((f.clone(), colored.locations.clone()))
            } else {
                None
            };
            rewrite::apply(f, &analysis, &colored.locations)?;
            return Ok(Allocation {
                rounds,
                spilled: state.spilled,
                rematerialized: state.rematerialized,
                max_pressure: analysis.max_pressure,
                assignment,
            });
        }
        if last {
            let class = colored
                .spill
                .first()
                .map_or(RegClass::new(0), |&v| class_of(f, v));
            return Err(RegAllocError::NoRegister { class });
        }
        rewrite::spill(f, &cfg, &mut state, &colored.spill)?;
    }
}

/// A virtual register's class, for a register known to the function.
fn class_of(f: &Function, v: VReg) -> RegClass {
    f.vreg_class(v).unwrap_or(RegClass::new(0))
}
