//! Calling conventions: where arguments and return values go, which
//! registers a call preserves, and the stack rules a frame must follow.
//!
//! An [`Abi`] is data, like a [`Target`]: argument and return registers per
//! value class, the callee-saved set, stack alignment at calls, the stack
//! space reserved for the callee (Win64 shadow space), the space below the
//! stack pointer a leaf may use (SysV red zone), and the stack-probe
//! interval. [`Abi::assign_args`] and [`Abi::assign_rets`] apply the rules to
//! a signature; [`frame`](super::frame) lays out a function's frame by them.
//!
//! Scalar integer and floating-point values are covered. Aggregates passed
//! by value (SysV classification, Win64 by-reference rules), variadic calls
//! (SysV `al`, Win64 register duplication), and 128-bit integers come with
//! call lowering from ir-lang 2.0 (backend-lang 0.5).

use alloc::vec::Vec;
use core::fmt;

use super::Target;
use crate::regs::{PReg, RegSet};

/// The kind of a scalar argument or return value, which picks its register
/// sequence.
///
/// # Examples
///
/// ```
/// use backend_lang::target::abi::{ArgClass, ArgLoc};
/// use backend_lang::x86_64::{SYSV, RDI, XMM0};
///
/// let mut locs = Vec::new();
/// SYSV.assign_args(&[ArgClass::Int, ArgClass::Float], &mut locs);
/// assert_eq!(locs, [ArgLoc::Reg(RDI), ArgLoc::Reg(XMM0)]);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ArgClass {
    /// An integer or pointer of at most 64 bits.
    Int,
    /// A 32- or 64-bit floating-point value.
    Float,
}

/// Where one argument or return value lives at the call.
///
/// # Examples
///
/// ```
/// use backend_lang::target::abi::{ArgClass, ArgLoc};
/// use backend_lang::x86_64::WIN64;
///
/// let mut locs = Vec::new();
/// WIN64.assign_args(&[ArgClass::Int; 5], &mut locs);
/// // The fifth argument goes above the 32 bytes of shadow space.
/// assert_eq!(locs[4], ArgLoc::Stack(32));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArgLoc {
    /// In a register.
    Reg(PReg),
    /// On the stack, at this byte offset from the stack pointer at the call
    /// instruction (before the return address is pushed). In the callee it
    /// is the same offset from the canonical frame address; an incoming
    /// stack slot ([`SlotKind::Incoming`]) names it.
    ///
    /// [`SlotKind::Incoming`]: crate::mir::SlotKind::Incoming
    Stack(u32),
}

/// Why return values could not be assigned.
///
/// # Examples
///
/// ```
/// use backend_lang::target::abi::{AbiError, ArgClass};
/// use backend_lang::x86_64::WIN64;
///
/// let mut locs = Vec::new();
/// let err = WIN64.assign_rets(&[ArgClass::Int, ArgClass::Int], &mut locs).unwrap_err();
/// assert_eq!(err, AbiError::TooManyReturns { index: 1 });
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AbiError {
    /// Return value `index` has no register left. Values that do not fit
    /// the return registers are returned through memory the caller provides
    /// (a hidden pointer argument), which is part of aggregate lowering
    /// (backend-lang 0.5); return fewer scalars until then.
    TooManyReturns {
        /// The first value that did not fit.
        index: usize,
    },
}

impl fmt::Display for AbiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AbiError::TooManyReturns { index } => write!(
                f,
                "return value {index} does not fit the return registers; return it through memory"
            ),
        }
    }
}

impl core::error::Error for AbiError {}

/// The integer and float register sequences of one direction (arguments or
/// returns), and whether they are consumed positionally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegSeq {
    int: &'static [PReg],
    float: &'static [PReg],
    positional: bool,
}

impl RegSeq {
    /// Integers take `int` in order and floats take `float` in order. With
    /// `positional`, value `i` may only take entry `i` of its list (Win64),
    /// so an integer in position 1 uses `rdx` even when position 0 was a
    /// float; otherwise each list has its own counter (SysV).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::target::abi::RegSeq;
    /// use backend_lang::x86_64::{RAX, XMM0};
    ///
    /// let rets = RegSeq::new(&[RAX], &[XMM0], true);
    /// assert_eq!(rets.int(), [RAX]);
    /// assert!(rets.positional());
    /// ```
    #[must_use]
    pub const fn new(int: &'static [PReg], float: &'static [PReg], positional: bool) -> RegSeq {
        RegSeq {
            int,
            float,
            positional,
        }
    }

    /// The integer registers, in order.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{SYSV, RDI};
    ///
    /// assert_eq!(SYSV.args().int()[0], RDI);
    /// ```
    #[inline]
    #[must_use]
    pub const fn int(&self) -> &'static [PReg] {
        self.int
    }

    /// The floating-point registers, in order.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{SYSV, XMM7};
    ///
    /// assert_eq!(SYSV.args().float().last(), Some(&XMM7));
    /// ```
    #[inline]
    #[must_use]
    pub const fn float(&self) -> &'static [PReg] {
        self.float
    }

    /// Whether value `i` may only take entry `i` of its list.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{SYSV, WIN64};
    ///
    /// assert!(WIN64.args().positional());
    /// assert!(!SYSV.args().positional());
    /// ```
    #[inline]
    #[must_use]
    pub const fn positional(&self) -> bool {
        self.positional
    }

    /// The register for value `index` of `class`, given how many values of
    /// each list came before it; `None` when the list is used up.
    fn pick(
        &self,
        class: ArgClass,
        index: usize,
        ints: &mut usize,
        floats: &mut usize,
    ) -> Option<PReg> {
        let (list, counter) = match class {
            ArgClass::Int => (self.int, ints),
            ArgClass::Float => (self.float, floats),
        };
        let slot = if self.positional { index } else { *counter };
        let reg = list.get(slot).copied();
        if reg.is_some() {
            *counter += 1;
        }
        reg
    }
}

/// Stack rules: alignment at calls, the callee's reserved area, the red
/// zone, the slot size of stack arguments, and the probe interval.
///
/// # Examples
///
/// ```
/// use backend_lang::x86_64::{SYSV, WIN64};
///
/// assert_eq!(SYSV.stack().red_zone(), 128);
/// assert_eq!(WIN64.stack().shadow_space(), 32);
/// assert_eq!(WIN64.stack().align(), 16);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StackRules {
    align: u32,
    arg_slot: u32,
    shadow_space: u32,
    red_zone: u32,
    probe_interval: u32,
}

impl StackRules {
    /// Stack rules: the stack pointer is a multiple of `align` at every call
    /// instruction; each stack argument takes `arg_slot` bytes; every call
    /// reserves `shadow_space` bytes for the callee below the stack
    /// arguments; a leaf function may use `red_zone` bytes below the stack
    /// pointer without moving it; and a frame of `probe_interval` bytes or
    /// more is allocated one probed page at a time (`0` disables probing).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::target::abi::StackRules;
    ///
    /// let rules = StackRules::new(16, 8, 0, 0, 4096);
    /// assert_eq!(rules.probe_interval(), 4096);
    /// ```
    #[must_use]
    pub const fn new(
        align: u32,
        arg_slot: u32,
        shadow_space: u32,
        red_zone: u32,
        probe_interval: u32,
    ) -> StackRules {
        StackRules {
            align,
            arg_slot,
            shadow_space,
            red_zone,
            probe_interval,
        }
    }

    /// The stack pointer's alignment at a call instruction, in bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::SYSV;
    ///
    /// assert_eq!(SYSV.stack().align(), 16);
    /// ```
    #[inline]
    #[must_use]
    pub const fn align(&self) -> u32 {
        self.align
    }

    /// Bytes per stack argument.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::SYSV;
    ///
    /// assert_eq!(SYSV.stack().arg_slot(), 8);
    /// ```
    #[inline]
    #[must_use]
    pub const fn arg_slot(&self) -> u32 {
        self.arg_slot
    }

    /// Bytes every caller reserves at the bottom of its outgoing area for the
    /// callee (Win64: 32, the home of the four register arguments).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::SYSV;
    ///
    /// assert_eq!(SYSV.stack().shadow_space(), 0);
    /// ```
    #[inline]
    #[must_use]
    pub const fn shadow_space(&self) -> u32 {
        self.shadow_space
    }

    /// Bytes below the stack pointer a function that makes no calls may use
    /// without moving the stack pointer (SysV: 128).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::WIN64;
    ///
    /// assert_eq!(WIN64.stack().red_zone(), 0);
    /// ```
    #[inline]
    #[must_use]
    pub const fn red_zone(&self) -> u32 {
        self.red_zone
    }

    /// Frames of at least this many bytes touch each page as they allocate
    /// it, so the guard page below the stack is never skipped; `0` means no
    /// probing.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::WIN64;
    ///
    /// assert_eq!(WIN64.stack().probe_interval(), 4096);
    /// ```
    #[inline]
    #[must_use]
    pub const fn probe_interval(&self) -> u32 {
        self.probe_interval
    }
}

/// A calling convention on a target.
///
/// # Examples
///
/// ```
/// use backend_lang::target::abi::{ArgClass, ArgLoc};
/// use backend_lang::x86_64::{SYSV, RAX, RBX, RCX, RDX, RSI, RDI, R8, R9};
///
/// let mut locs = Vec::new();
/// let stack = SYSV.assign_args(&[ArgClass::Int; 8], &mut locs);
/// assert_eq!(&locs[..6], [RDI, RSI, RDX, RCX, R8, R9].map(ArgLoc::Reg));
/// assert_eq!(&locs[6..], [ArgLoc::Stack(0), ArgLoc::Stack(8)]);
/// assert_eq!(stack, 16);
/// assert!(SYSV.callee_saved().contains(RBX));
/// assert!(SYSV.call_clobbers().contains(RAX));
/// ```
#[derive(Debug)]
pub struct Abi {
    name: &'static str,
    target: &'static Target,
    args: RegSeq,
    rets: RegSeq,
    callee_saved: RegSet,
    stack: StackRules,
}

impl Abi {
    /// Describes a calling convention on `target`.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::target::abi::{Abi, RegSeq, StackRules};
    /// use backend_lang::x86_64::{self, RAX, RDI, RBX};
    /// use backend_lang::RegSet;
    ///
    /// static TINY: Abi = Abi::new(
    ///     "tiny",
    ///     &x86_64::TARGET,
    ///     RegSeq::new(&[RDI], &[], false),
    ///     RegSeq::new(&[RAX], &[], false),
    ///     RegSet::of(&[RBX]),
    ///     StackRules::new(16, 8, 0, 0, 0),
    /// );
    /// assert_eq!(TINY.name(), "tiny");
    /// ```
    #[must_use]
    pub const fn new(
        name: &'static str,
        target: &'static Target,
        args: RegSeq,
        rets: RegSeq,
        callee_saved: RegSet,
        stack: StackRules,
    ) -> Abi {
        Abi {
            name,
            target,
            args,
            rets,
            callee_saved,
            stack,
        }
    }

    /// The convention's name, as the MIR printer writes it.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::WIN64;
    ///
    /// assert_eq!(WIN64.name(), "win64");
    /// ```
    #[inline]
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The target the convention belongs to.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::SYSV;
    ///
    /// assert_eq!(SYSV.target().name(), "x86-64");
    /// ```
    #[inline]
    #[must_use]
    pub const fn target(&self) -> &'static Target {
        self.target
    }

    /// The argument registers.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{WIN64, RCX};
    ///
    /// assert_eq!(WIN64.args().int()[0], RCX);
    /// ```
    #[inline]
    #[must_use]
    pub const fn args(&self) -> &RegSeq {
        &self.args
    }

    /// The return registers.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{SYSV, RAX, RDX};
    ///
    /// assert_eq!(SYSV.rets().int(), [RAX, RDX]);
    /// ```
    #[inline]
    #[must_use]
    pub const fn rets(&self) -> &RegSeq {
        &self.rets
    }

    /// The registers a callee must preserve (restore before returning).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{WIN64, RSI, XMM6};
    ///
    /// assert!(WIN64.callee_saved().contains(RSI));
    /// assert!(WIN64.callee_saved().contains(XMM6));
    /// ```
    #[inline]
    #[must_use]
    pub const fn callee_saved(&self) -> RegSet {
        self.callee_saved
    }

    /// The stack rules.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::SYSV;
    ///
    /// assert_eq!(SYSV.stack().align(), 16);
    /// ```
    #[inline]
    #[must_use]
    pub const fn stack(&self) -> &StackRules {
        &self.stack
    }

    /// The caller-saved registers: every allocatable register the convention
    /// does not preserve. A value live across a call must not be in one (or
    /// must be saved around the call).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{SYSV, WIN64, FLAGS, R11, RDI};
    ///
    /// assert!(SYSV.caller_saved().contains(RDI));
    /// assert!(!WIN64.caller_saved().contains(RDI));
    /// assert!(SYSV.caller_saved().contains(R11));
    /// assert!(!SYSV.caller_saved().contains(FLAGS));
    /// ```
    #[must_use]
    pub fn caller_saved(&self) -> RegSet {
        self.target
            .classes()
            .iter()
            .fold(RegSet::EMPTY, |acc, c| acc.union(c.allocatable()))
            .difference(self.callee_saved)
    }

    /// The registers a call may change: every allocatable register the
    /// convention does not preserve, plus the flags. This is the clobber set
    /// call lowering attaches to a call instruction.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{SYSV, WIN64, FLAGS, R11, RSI, RBX, XMM6};
    ///
    /// assert!(SYSV.call_clobbers().contains(RSI));
    /// assert!(!WIN64.call_clobbers().contains(RSI));
    /// assert!(SYSV.call_clobbers().contains(XMM6));
    /// assert!(!WIN64.call_clobbers().contains(XMM6));
    /// assert!(SYSV.call_clobbers().contains(FLAGS) && SYSV.call_clobbers().contains(R11));
    /// assert!(!SYSV.call_clobbers().contains(RBX));
    /// ```
    #[must_use]
    pub fn call_clobbers(&self) -> RegSet {
        match self.target.flags() {
            Some(flags) => self.caller_saved().with(flags),
            None => self.caller_saved(),
        }
    }

    /// Assigns a location to each argument, in order, appending them to
    /// `out`, and returns the bytes of outgoing stack area the call needs:
    /// the shadow space (if the convention has one) plus the stack
    /// arguments. A caller's frame reserves the largest such area over its
    /// calls ([`Function::reserve_outgoing`]).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::target::abi::{ArgClass, ArgLoc};
    /// use backend_lang::x86_64::{WIN64, RCX, XMM1, R8};
    ///
    /// let mut locs = Vec::new();
    /// let area = WIN64.assign_args(&[ArgClass::Int, ArgClass::Float, ArgClass::Int], &mut locs);
    /// // Positional: the float in slot 1 takes xmm1, the int in slot 2 takes r8.
    /// assert_eq!(locs, [ArgLoc::Reg(RCX), ArgLoc::Reg(XMM1), ArgLoc::Reg(R8)]);
    /// assert_eq!(area, 32);
    /// ```
    ///
    /// [`Function::reserve_outgoing`]: crate::mir::Function::reserve_outgoing
    pub fn assign_args(&self, args: &[ArgClass], out: &mut Vec<ArgLoc>) -> u32 {
        out.reserve(args.len());
        let (mut ints, mut floats) = (0, 0);
        let mut offset = self.stack.shadow_space;
        for (i, &class) in args.iter().enumerate() {
            match self.args.pick(class, i, &mut ints, &mut floats) {
                Some(reg) => out.push(ArgLoc::Reg(reg)),
                None => {
                    out.push(ArgLoc::Stack(offset));
                    offset = offset.saturating_add(self.stack.arg_slot);
                }
            }
        }
        offset
    }

    /// Assigns a register to each return value, in order, appending them to
    /// `out`.
    ///
    /// # Errors
    ///
    /// [`AbiError::TooManyReturns`] when a value has no register left;
    /// nothing is appended in that case.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::target::abi::{ArgClass, ArgLoc};
    /// use backend_lang::x86_64::{SYSV, RAX, XMM0};
    ///
    /// let mut locs = Vec::new();
    /// SYSV.assign_rets(&[ArgClass::Int, ArgClass::Float], &mut locs)?;
    /// assert_eq!(locs, [ArgLoc::Reg(RAX), ArgLoc::Reg(XMM0)]);
    /// # Ok::<(), backend_lang::target::abi::AbiError>(())
    /// ```
    pub fn assign_rets(&self, rets: &[ArgClass], out: &mut Vec<ArgLoc>) -> Result<(), AbiError> {
        let start = out.len();
        let (mut ints, mut floats) = (0, 0);
        for (i, &class) in rets.iter().enumerate() {
            match self.rets.pick(class, i, &mut ints, &mut floats) {
                Some(reg) => out.push(ArgLoc::Reg(reg)),
                None => {
                    out.truncate(start);
                    return Err(AbiError::TooManyReturns { index: i });
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;
    use alloc::vec;

    use super::*;
    use crate::x86_64::*;

    fn args(abi: &Abi, classes: &[ArgClass]) -> (Vec<ArgLoc>, u32) {
        let mut out = Vec::new();
        let n = abi.assign_args(classes, &mut out);
        (out, n)
    }

    #[test]
    fn test_sysv_args_interleaved_int_and_float_counters_are_independent() {
        use ArgClass::{Float as F, Int as I};
        let (locs, stack) = args(&SYSV, &[F, I, F, I, I, I, I, I, I, F]);
        let regs = |r: &[PReg]| r.iter().map(|&r| ArgLoc::Reg(r)).collect::<Vec<_>>();
        assert_eq!(
            locs,
            [
                regs(&[XMM0, RDI, XMM1, RSI, RDX, RCX, R8, R9]),
                vec![ArgLoc::Stack(0), ArgLoc::Reg(XMM2)]
            ]
            .concat()
        );
        assert_eq!(stack, 8);
    }

    #[test]
    fn test_sysv_float_args_spill_after_eight() {
        let (locs, stack) = args(&SYSV, &[ArgClass::Float; 10]);
        assert_eq!(locs[7], ArgLoc::Reg(XMM7));
        assert_eq!(locs[8], ArgLoc::Stack(0));
        assert_eq!(locs[9], ArgLoc::Stack(8));
        assert_eq!(stack, 16);
    }

    #[test]
    fn test_win64_positional_and_shadow_space() {
        use ArgClass::{Float as F, Int as I};
        let (locs, stack) = args(&WIN64, &[F, F, I, F, I, F]);
        assert_eq!(
            locs,
            [
                ArgLoc::Reg(XMM0),
                ArgLoc::Reg(XMM1),
                ArgLoc::Reg(R8),
                ArgLoc::Reg(XMM3),
                ArgLoc::Stack(32),
                ArgLoc::Stack(40)
            ]
        );
        assert_eq!(stack, 48);
        // No arguments still reserves the shadow space.
        assert_eq!(args(&WIN64, &[]).1, 32);
        assert_eq!(args(&SYSV, &[]).1, 0);
    }

    #[test]
    fn test_returns_per_convention() {
        let mut out = Vec::new();
        assert_eq!(SYSV.assign_rets(&[ArgClass::Int; 2], &mut out), Ok(()));
        assert_eq!(out, [ArgLoc::Reg(RAX), ArgLoc::Reg(RDX)]);
        out.clear();
        assert_eq!(
            SYSV.assign_rets(&[ArgClass::Int; 3], &mut out),
            Err(AbiError::TooManyReturns { index: 2 })
        );
        assert!(out.is_empty());
        assert_eq!(WIN64.assign_rets(&[ArgClass::Float], &mut out), Ok(()));
        assert_eq!(out, [ArgLoc::Reg(XMM0)]);
        out.clear();
        assert_eq!(
            WIN64.assign_rets(&[ArgClass::Float, ArgClass::Int], &mut out),
            Err(AbiError::TooManyReturns { index: 1 })
        );
        assert_eq!(
            AbiError::TooManyReturns { index: 1 }.to_string(),
            "return value 1 does not fit the return registers; return it through memory"
        );
    }

    #[test]
    fn test_callee_saved_sets_match_the_published_conventions() {
        let sysv: Vec<_> = SYSV.callee_saved().iter().collect();
        assert_eq!(sysv, [RBX, RBP, R12, R13, R14, R15]);
        let win: Vec<_> = WIN64.callee_saved().iter().collect();
        assert_eq!(
            win,
            [
                RBX, RBP, RSI, RDI, R12, R13, R14, R15, XMM6, XMM7, XMM8, XMM9, XMM10, XMM11,
                XMM12, XMM13, XMM14, XMM15
            ]
        );
        // Clobbers: everything allocatable and not preserved, plus flags; never
        // the stack pointer.
        for abi in [&SYSV, &WIN64] {
            let c = abi.call_clobbers();
            assert!(!c.contains(RSP));
            assert!(c.contains(FLAGS));
            assert!(c.intersect(abi.callee_saved()).is_empty());
        }
        assert_eq!(SYSV.call_clobbers().len(), 9 + 16 + 1);
        assert_eq!(WIN64.call_clobbers().len(), 7 + 6 + 1);
    }

    #[test]
    fn test_stack_offsets_saturate_instead_of_wrapping() {
        // A hostile signature cannot wrap the offset arithmetic.
        let many = vec![ArgClass::Int; 1 << 16];
        let (locs, stack) = args(&SYSV, &many);
        assert_eq!(locs.len(), 1 << 16);
        assert_eq!(stack, 8 * ((1 << 16) - 6));
    }
}
