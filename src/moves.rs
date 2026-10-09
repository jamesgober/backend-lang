//! The parallel-move resolver: turns a parallel copy into a sequence of
//! moves (and swaps) that a machine can execute one at a time.
//!
//! A parallel copy `d0, d1, ... := s0, s1, ...` reads every source before it
//! writes any destination. Control-flow edges with block arguments, register
//! allocation (moving values between registers and spill slots at block
//! boundaries), and call lowering (placing arguments in ABI registers) all
//! produce them, and all three share this resolver. Getting it wrong is a
//! silent miscompile: a loop back edge `jump head(b, a)` that swaps its two
//! parameters turns into `a ← b; b ← a`, and both end up holding `b`
//! (codegen-lang's H01).
//!
//! # Algorithm
//!
//! Locations (registers and stack slots) are numbered densely by sorting the
//! moves' locations, then the moves form a graph in which every location has
//! at most one writer (destinations are distinct) but any number of readers
//! (one source may feed several destinations). Such a graph is a set of
//! trees, each hanging off at most one cycle.
//!
//! 1. A move is **ready** when no pending move still reads its destination.
//!    Ready moves are emitted; emitting one can make the move that writes its
//!    source ready. This drains every tree, leaves first.
//! 2. What remains is disjoint simple cycles. Each is broken in the cheapest
//!    of three ways:
//!    - **Alias:** if a value on the cycle was already copied to a tree
//!      destination (a fan-out), that destination holds it and will not
//!      change, so the cycle reads it from there: no extra move at all.
//!    - **Swap:** if every location on the cycle is a register and the
//!      target can exchange two registers (x86-64 `xchg`), a cycle of `k`
//!      registers takes `k - 1` swaps.
//!    - **Scratch:** otherwise one value is saved to a scratch location (a
//!      free register of its class, else the caller's scratch stack slot),
//!      the cycle drains as a chain, and the saved value lands last: `k + 1`
//!      moves.
//!
//! The result is minimal in the number of instructions under the rules
//! above: every destination is written once, a cycle costs nothing extra
//! when it can be aliased, `k - 1` swaps when it can be swapped, and one
//! extra move otherwise.
//!
//! Then every move is made legal for the target: a memory-to-memory move,
//! or a store of an immediate the target cannot store directly, goes through
//! a temporary register: a free register of the class if the caller named
//! one, else a **borrowed** register, saved to the caller's victim slot,
//! used, and restored. Borrowing is always sound because the borrowed
//! register is restored and the move it serves touches only memory.
//!
//! Numbering costs `O(n log n)` for `n` moves; everything after it is
//! linear. Buffers live in the [`MoveResolver`] and are reused, so a
//! resolver that has seen its largest copy allocates nothing more.

use alloc::vec::Vec;
use core::fmt;

use crate::mir::StackSlot;
use crate::regs::{PReg, RegClass, RegSet};
use crate::target::Target;

/// A place a value lives: a physical register or a stack slot.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::StackSlot;
/// use backend_lang::moves::Loc;
/// use backend_lang::x86_64::RAX;
///
/// assert_eq!(Loc::from(RAX), Loc::Reg(RAX));
/// assert_eq!(Loc::from(StackSlot::new(1)), Loc::Slot(StackSlot::new(1)));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Loc {
    /// A physical register.
    Reg(PReg),
    /// A whole stack slot (sized for the move's class).
    Slot(StackSlot),
}

impl Loc {
    fn key(self) -> u64 {
        match self {
            Loc::Reg(r) => r.index() as u64,
            Loc::Slot(s) => (1 << 32) | s.index() as u64,
        }
    }

    /// Whether this is a stack slot.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::StackSlot;
    /// use backend_lang::moves::Loc;
    ///
    /// assert!(Loc::Slot(StackSlot::new(0)).is_mem());
    /// ```
    #[inline]
    #[must_use]
    pub const fn is_mem(self) -> bool {
        matches!(self, Loc::Slot(_))
    }
}

impl From<PReg> for Loc {
    #[inline]
    fn from(r: PReg) -> Loc {
        Loc::Reg(r)
    }
}

impl From<StackSlot> for Loc {
    #[inline]
    fn from(s: StackSlot) -> Loc {
        Loc::Slot(s)
    }
}

/// Where a moved value comes from: a location or an immediate.
///
/// # Examples
///
/// ```
/// use backend_lang::moves::{Loc, Source};
/// use backend_lang::x86_64::RCX;
///
/// assert_eq!(Source::from(RCX), Source::Loc(Loc::Reg(RCX)));
/// assert_eq!(Source::from(7), Source::Imm(7));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// The value in a location.
    Loc(Loc),
    /// A constant.
    Imm(i64),
}

impl From<Loc> for Source {
    #[inline]
    fn from(l: Loc) -> Source {
        Source::Loc(l)
    }
}

impl From<PReg> for Source {
    #[inline]
    fn from(r: PReg) -> Source {
        Source::Loc(Loc::Reg(r))
    }
}

impl From<StackSlot> for Source {
    #[inline]
    fn from(s: StackSlot) -> Source {
        Source::Loc(Loc::Slot(s))
    }
}

impl From<i64> for Source {
    #[inline]
    fn from(v: i64) -> Source {
        Source::Imm(v)
    }
}

/// One copy of a parallel copy: `dst := src`, of register class `class`.
///
/// # Examples
///
/// ```
/// use backend_lang::moves::Move;
/// use backend_lang::x86_64::{GPR, RAX, RBX};
///
/// let m = Move::new(RAX, RBX, GPR);
/// assert_eq!(m.class, GPR);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Move {
    /// The destination.
    pub dst: Loc,
    /// The source.
    pub src: Source,
    /// The class of the value moved, which picks scratch and temporary
    /// registers and the width of the move.
    pub class: RegClass,
}

impl Move {
    /// `dst := src` of `class`.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::StackSlot;
    /// use backend_lang::moves::{Loc, Move, Source};
    /// use backend_lang::x86_64::{GPR, RSI};
    ///
    /// let spill = Move::new(StackSlot::new(0), RSI, GPR);
    /// assert_eq!(spill.dst, Loc::Slot(StackSlot::new(0)));
    /// assert_eq!(spill.src, Source::from(RSI));
    /// ```
    #[must_use]
    pub fn new(dst: impl Into<Loc>, src: impl Into<Source>, class: RegClass) -> Move {
        Move {
            dst: dst.into(),
            src: src.into(),
            class,
        }
    }
}

/// One step of the resolved sequence, each executable by one target
/// instruction.
///
/// # Examples
///
/// ```
/// use backend_lang::moves::{Move, MoveEnv, MoveResolver, Step};
/// use backend_lang::x86_64::{self, GPR, RAX, RBX};
///
/// // Swap two registers: one xchg on x86-64.
/// let swap = [Move::new(RAX, RBX, GPR), Move::new(RBX, RAX, GPR)];
/// let mut out = Vec::new();
/// MoveResolver::new().resolve(&x86_64::TARGET, &swap, &MoveEnv::new(), &mut out)?;
/// assert_eq!(out, [Step::Swap { a: RAX, b: RBX, class: GPR }]);
/// # Ok::<(), backend_lang::moves::MoveError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Step {
    /// `dst := src`. Never memory to memory on a target that cannot, and
    /// never a store of an immediate the target cannot store.
    Move {
        /// The destination.
        dst: Loc,
        /// The source.
        src: Source,
        /// The class of the value.
        class: RegClass,
    },
    /// Exchange two registers of `class`.
    Swap {
        /// One register.
        a: PReg,
        /// The other.
        b: PReg,
        /// Their class.
        class: RegClass,
    },
}

/// What the resolver may use besides the moves' own locations.
///
/// # Examples
///
/// ```
/// use backend_lang::RegSet;
/// use backend_lang::mir::StackSlot;
/// use backend_lang::moves::MoveEnv;
/// use backend_lang::x86_64::R11;
///
/// let env = MoveEnv::new()
///     .with_free(RegSet::of(&[R11]))
///     .with_scratch_slot(StackSlot::new(4))
///     .with_victim_slot(StackSlot::new(5));
/// assert!(env.free.contains(R11));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MoveEnv {
    /// Registers the caller guarantees are dead here and may be overwritten.
    /// Registers that are a source or destination of a move are ignored.
    pub free: RegSet,
    /// A stack slot (of the largest class's spill size) for a cycle's saved
    /// value when no free register of its class exists.
    pub scratch_slot: Option<StackSlot>,
    /// A stack slot to save a borrowed register in, for a memory-to-memory
    /// move when no free register exists.
    pub victim_slot: Option<StackSlot>,
    /// Whether register cycles may be resolved with swaps (honoured only on
    /// targets that have them). On by default.
    pub swap: bool,
}

impl Default for MoveEnv {
    fn default() -> MoveEnv {
        MoveEnv::new()
    }
}

impl MoveEnv {
    /// No free registers, no slots, swaps allowed.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::moves::MoveEnv;
    ///
    /// assert!(MoveEnv::new().free.is_empty());
    /// assert!(MoveEnv::new().swap);
    /// ```
    #[must_use]
    pub const fn new() -> MoveEnv {
        MoveEnv {
            free: RegSet::EMPTY,
            scratch_slot: None,
            victim_slot: None,
            swap: true,
        }
    }

    /// With `free` as the free registers.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::RegSet;
    /// use backend_lang::moves::MoveEnv;
    /// use backend_lang::x86_64::R10;
    ///
    /// assert!(MoveEnv::new().with_free(RegSet::of(&[R10])).free.contains(R10));
    /// ```
    #[must_use]
    pub const fn with_free(self, free: RegSet) -> MoveEnv {
        MoveEnv { free, ..self }
    }

    /// With `slot` as the scratch slot.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::StackSlot;
    /// use backend_lang::moves::MoveEnv;
    ///
    /// let env = MoveEnv::new().with_scratch_slot(StackSlot::new(0));
    /// assert_eq!(env.scratch_slot, Some(StackSlot::new(0)));
    /// ```
    #[must_use]
    pub const fn with_scratch_slot(self, slot: StackSlot) -> MoveEnv {
        MoveEnv {
            scratch_slot: Some(slot),
            ..self
        }
    }

    /// With `slot` as the victim slot.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::StackSlot;
    /// use backend_lang::moves::MoveEnv;
    ///
    /// let env = MoveEnv::new().with_victim_slot(StackSlot::new(1));
    /// assert_eq!(env.victim_slot, Some(StackSlot::new(1)));
    /// ```
    #[must_use]
    pub const fn with_victim_slot(self, slot: StackSlot) -> MoveEnv {
        MoveEnv {
            victim_slot: Some(slot),
            ..self
        }
    }

    /// With swaps allowed or not.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::moves::MoveEnv;
    ///
    /// assert!(!MoveEnv::new().with_swap(false).swap);
    /// ```
    #[must_use]
    pub const fn with_swap(self, swap: bool) -> MoveEnv {
        MoveEnv { swap, ..self }
    }
}

/// Why a parallel copy could not be resolved.
///
/// # Examples
///
/// ```
/// use backend_lang::moves::{Move, MoveEnv, MoveError, MoveResolver};
/// use backend_lang::x86_64::{self, GPR, RAX, RBX};
///
/// let twice = [Move::new(RAX, RBX, GPR), Move::new(RAX, 1, GPR)];
/// let err = MoveResolver::new()
///     .resolve(&x86_64::TARGET, &twice, &MoveEnv::new(), &mut Vec::new())
///     .unwrap_err();
/// assert_eq!(err, MoveError::DuplicateDestination { index: 1 });
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MoveError {
    /// Two moves write the same location; a parallel copy's destinations are
    /// distinct. `index` is the later of the two.
    DuplicateDestination {
        /// The move.
        index: usize,
    },
    /// A register in move `index` is not in the move's class (or the target
    /// has no such class or register).
    ClassMismatch {
        /// The move.
        index: usize,
    },
    /// A cycle of class `class` needs a scratch location: no free register
    /// of the class, no swap, and no scratch slot was provided.
    NoScratch {
        /// The class.
        class: RegClass,
    },
    /// A memory-to-memory move (or an immediate store) of class `class`
    /// needs a temporary register: no free register of the class, and no
    /// victim slot to borrow one with (or the class has no register to
    /// borrow).
    NoTemp {
        /// The class.
        class: RegClass,
    },
    /// More than `u32::MAX - 2` moves in one parallel copy.
    TooMany,
}

impl fmt::Display for MoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MoveError::DuplicateDestination { index } => write!(
                f,
                "move {index} writes a destination an earlier move already writes"
            ),
            MoveError::ClassMismatch { index } => {
                write!(f, "move {index} names a register outside its class")
            }
            MoveError::NoScratch { class } => write!(
                f,
                "a cycle of class {} needs a free register or a scratch slot",
                class.index()
            ),
            MoveError::NoTemp { class } => write!(
                f,
                "a memory-to-memory move of class {} needs a free register or a victim slot",
                class.index()
            ),
            MoveError::TooMany => f.write_str("too many moves in one parallel copy"),
        }
    }
}

impl core::error::Error for MoveError {}

/// What a resolution used beyond the moves' own locations.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::StackSlot;
/// use backend_lang::moves::{Move, MoveEnv, MoveResolver};
/// use backend_lang::x86_64::{self, GPR};
///
/// let (a, b) = (StackSlot::new(0), StackSlot::new(1));
/// let swap = [Move::new(a, b, GPR), Move::new(b, a, GPR)];
/// let env = MoveEnv::new().with_scratch_slot(StackSlot::new(2)).with_victim_slot(StackSlot::new(3));
/// let used = MoveResolver::new().resolve(&x86_64::TARGET, &swap, &env, &mut Vec::new())?;
/// assert!(used.scratch_slot && used.victim_slot);
/// # Ok::<(), backend_lang::moves::MoveError>(())
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Resolved {
    /// Steps appended.
    pub steps: usize,
    /// Whether the scratch slot was used.
    pub scratch_slot: bool,
    /// Whether the victim slot was used.
    pub victim_slot: bool,
}

const NONE: u32 = u32::MAX;
/// A move's source is the scratch location.
const SCRATCH: u32 = u32::MAX - 1;
/// A move's source is its immediate.
const IMM: u32 = u32::MAX - 2;

/// Resolves parallel copies. Keeps its buffers between calls.
///
/// # Examples
///
/// A rotation of three registers and a register into a slot, with a free
/// register to break the cycle when swaps are off:
///
/// ```
/// use backend_lang::RegSet;
/// use backend_lang::mir::StackSlot;
/// use backend_lang::moves::{Loc, Move, MoveEnv, MoveResolver, Source, Step};
/// use backend_lang::x86_64::{self, GPR, RAX, RBX, RCX, R11};
///
/// let moves = [
///     Move::new(RAX, RBX, GPR),
///     Move::new(RBX, RCX, GPR),
///     Move::new(RCX, RAX, GPR),
///     Move::new(StackSlot::new(0), RAX, GPR),
/// ];
/// let env = MoveEnv::new().with_free(RegSet::of(&[R11])).with_swap(false);
/// let mut steps = Vec::new();
/// MoveResolver::new().resolve(&x86_64::TARGET, &moves, &env, &mut steps)?;
/// // The slot copy of rax doubles as the cycle's saved value: four moves.
/// assert_eq!(steps.len(), 4);
/// assert_eq!(steps[0], Step::Move { dst: Loc::Slot(StackSlot::new(0)), src: Source::from(RAX), class: GPR });
/// # Ok::<(), backend_lang::moves::MoveError>(())
/// ```
#[derive(Clone, Debug, Default)]
pub struct MoveResolver {
    keys: Vec<(u64, u64)>,
    locs: Vec<Loc>,
    writer: Vec<u32>,
    readers: Vec<u32>,
    alias: Vec<u32>,
    dst: Vec<u32>,
    src: Vec<u32>,
    imm: Vec<i64>,
    class: Vec<RegClass>,
    origin: Vec<u32>,
    ready: Vec<u32>,
    cycle: Vec<u32>,
}

/// The state of one resolution that the legalizer needs.
struct Legal<'a> {
    target: &'a Target,
    env: &'a MoveEnv,
    /// Registers touched by the moves; never used as free registers.
    involved: RegSet,
    /// The scratch location while it holds a cycle's value.
    scratch: Option<Loc>,
    used: Resolved,
}

impl Legal<'_> {
    /// A free register of `class`, not involved and not the busy scratch.
    fn free_reg(&self, class: RegClass) -> Option<PReg> {
        let mut free = self
            .env
            .free
            .intersect(self.target.class(class).regs())
            .difference(self.involved);
        if let Some(Loc::Reg(r)) = self.scratch {
            free = free.without(r);
        }
        free.first()
    }

    fn push(&mut self, out: &mut Vec<Step>, step: Step) {
        out.push(step);
        self.used.steps += 1;
    }

    /// Emits `dst := src` legally for the target.
    fn emit(
        &mut self,
        out: &mut Vec<Step>,
        dst: Loc,
        src: Source,
        class: RegClass,
    ) -> Result<(), MoveError> {
        let caps = self.target.move_caps();
        let needs_temp = dst.is_mem()
            && match src {
                Source::Loc(l) => l.is_mem() && !caps.mem_to_mem(),
                Source::Imm(v) => !caps.store_imm().fits(v),
            };
        if !needs_temp {
            self.push(out, Step::Move { dst, src, class });
            return Ok(());
        }
        if let Some(t) = self.free_reg(class) {
            self.push(
                out,
                Step::Move {
                    dst: Loc::Reg(t),
                    src,
                    class,
                },
            );
            self.push(
                out,
                Step::Move {
                    dst,
                    src: Source::Loc(Loc::Reg(t)),
                    class,
                },
            );
            return Ok(());
        }
        // Borrow a register: any allocatable one but the busy scratch. Its
        // value is saved and restored around a move that touches only
        // memory, so it is preserved whatever it held.
        let slot = self.env.victim_slot.ok_or(MoveError::NoTemp { class })?;
        let mut pool = self.target.class(class).allocatable();
        if let Some(Loc::Reg(r)) = self.scratch {
            pool = pool.without(r);
        }
        let v = pool.first().ok_or(MoveError::NoTemp { class })?;
        let (vl, sl) = (Loc::Reg(v), Loc::Slot(slot));
        self.used.victim_slot = true;
        self.push(
            out,
            Step::Move {
                dst: sl,
                src: Source::Loc(vl),
                class,
            },
        );
        self.push(
            out,
            Step::Move {
                dst: vl,
                src,
                class,
            },
        );
        self.push(
            out,
            Step::Move {
                dst,
                src: Source::Loc(vl),
                class,
            },
        );
        self.push(
            out,
            Step::Move {
                dst: vl,
                src: Source::Loc(sl),
                class,
            },
        );
        Ok(())
    }
}

impl MoveResolver {
    /// A resolver with empty buffers.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::moves::MoveResolver;
    ///
    /// let _r = MoveResolver::new();
    /// ```
    #[must_use]
    pub fn new() -> MoveResolver {
        MoveResolver::default()
    }

    /// Resolves the parallel copy `moves` on `target`, appending the steps to
    /// `out`.
    ///
    /// Moves whose source is their destination are dropped. Executing the
    /// appended steps in order leaves every destination holding its source's
    /// value from before the copy, and every other location unchanged except
    /// `env.free` registers (which are dead) and the scratch and victim
    /// slots.
    ///
    /// # Errors
    ///
    /// [`MoveError::DuplicateDestination`], [`MoveError::ClassMismatch`],
    /// [`MoveError::NoScratch`] or [`MoveError::NoTemp`] when a scratch or
    /// temporary is needed and `env` provides none, [`MoveError::TooMany`].
    /// On error, `out` may hold a partial sequence; discard it.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::StackSlot;
    /// use backend_lang::moves::{Loc, Move, MoveEnv, MoveResolver, Source, Step};
    /// use backend_lang::x86_64::{self, GPR, RAX, R11};
    /// use backend_lang::RegSet;
    ///
    /// // Slot to slot on x86-64 goes through a register.
    /// let copy = [Move::new(StackSlot::new(1), StackSlot::new(0), GPR)];
    /// let env = MoveEnv::new().with_free(RegSet::of(&[R11]));
    /// let mut out = Vec::new();
    /// MoveResolver::new().resolve(&x86_64::TARGET, &copy, &env, &mut out)?;
    /// assert_eq!(out, [
    ///     Step::Move { dst: Loc::Reg(R11), src: Source::from(StackSlot::new(0)), class: GPR },
    ///     Step::Move { dst: Loc::Slot(StackSlot::new(1)), src: Source::from(R11), class: GPR },
    /// ]);
    /// # Ok::<(), backend_lang::moves::MoveError>(())
    /// ```
    pub fn resolve(
        &mut self,
        target: &Target,
        moves: &[Move],
        env: &MoveEnv,
        out: &mut Vec<Step>,
    ) -> Result<Resolved, MoveError> {
        if moves.len() > (u32::MAX - 3) as usize {
            return Err(MoveError::TooMany);
        }
        let involved = self.number(target, moves)?;
        let mut legal = Legal {
            target,
            env,
            involved,
            scratch: None,
            used: Resolved::default(),
        };
        let count = self.dst.len();
        // Seed in reverse so the stack pops in input order: a copy with no
        // conflicts comes out exactly as written.
        self.ready.clear();
        for m in (0..count).rev() {
            if self.readers[self.dst[m] as usize] == 0 {
                self.ready.push(m as u32);
            }
        }
        let mut cursor = 0usize;
        loop {
            self.drain(&mut legal, out)?;
            while cursor < count && self.writer[self.dst[cursor] as usize] != cursor as u32 {
                cursor += 1;
            }
            if cursor == count {
                break;
            }
            self.break_cycle(cursor as u32, &mut legal, out)?;
        }
        Ok(legal.used)
    }

    /// Numbers the locations of the non-trivial moves and builds the graph.
    /// Returns the registers involved.
    fn number(&mut self, target: &Target, moves: &[Move]) -> Result<RegSet, MoveError> {
        self.keys.clear();
        self.dst.clear();
        self.src.clear();
        self.imm.clear();
        self.class.clear();
        self.origin.clear();
        let mut involved = RegSet::EMPTY;
        for (i, m) in moves.iter().enumerate() {
            if !target.has_class(m.class) {
                return Err(MoveError::ClassMismatch { index: i });
            }
            let regs = target.class(m.class).regs();
            let in_class = |l: Loc| match l {
                Loc::Reg(r) => target.has_reg(r) && regs.contains(r),
                Loc::Slot(_) => true,
            };
            if !in_class(m.dst) || matches!(m.src, Source::Loc(l) if !in_class(l)) {
                return Err(MoveError::ClassMismatch { index: i });
            }
            if m.src == Source::Loc(m.dst) {
                // Still a destination: two writers of it are an error even if
                // one of them is a no-op. And its value is live, so it is
                // never a free register.
                self.keys.push((m.dst.key(), (i as u64) << 1));
                if let Loc::Reg(r) = m.dst {
                    involved = involved.with(r);
                }
                continue;
            }
            let k = self.dst.len() as u32;
            self.keys.push((m.dst.key(), (i as u64) << 1));
            self.dst.push(NONE);
            self.class.push(m.class);
            self.origin.push(i as u32);
            match m.src {
                Source::Loc(l) => {
                    self.keys.push((l.key(), (u64::from(k) << 1) | 1));
                    self.src.push(NONE);
                    self.imm.push(0);
                }
                Source::Imm(v) => {
                    self.src.push(IMM);
                    self.imm.push(v);
                }
            }
            for l in [
                Some(m.dst),
                if let Source::Loc(l) = m.src {
                    Some(l)
                } else {
                    None
                },
            ]
            .into_iter()
            .flatten()
            {
                if let Loc::Reg(r) = l {
                    involved = involved.with(r);
                }
            }
        }
        // Destination entries carry the input index (bit 0 clear), source
        // entries the compact move index (bit 0 set). Sorting groups each
        // location's entries together; a second destination entry in a group
        // is a duplicate, and the sort puts the later input second.
        self.keys.sort_unstable();
        // Input index -> compact index (`origin` is ascending).
        let compact_of =
            |input: u32, origin: &[u32]| -> Option<usize> { origin.binary_search(&input).ok() };
        self.locs.clear();
        let mut prev_key = u64::MAX;
        let mut prev_was_dst = false;
        for &(key, tag) in &self.keys {
            if key != prev_key || self.locs.is_empty() {
                prev_key = key;
                prev_was_dst = false;
                self.locs.push(if key >> 32 == 0 {
                    Loc::Reg(PReg::from_index(key as u8))
                } else {
                    Loc::Slot(StackSlot::from_index((key & 0xFFFF_FFFF) as usize))
                });
            }
            let id = (self.locs.len() - 1) as u32;
            if tag & 1 == 0 {
                let input = (tag >> 1) as u32;
                if prev_was_dst {
                    return Err(MoveError::DuplicateDestination {
                        index: input as usize,
                    });
                }
                prev_was_dst = true;
                if let Some(m) = compact_of(input, &self.origin) {
                    self.dst[m] = id;
                }
            } else {
                self.src[(tag >> 1) as usize] = id;
            }
        }
        let n = self.locs.len();
        self.writer.clear();
        self.writer.resize(n, NONE);
        self.readers.clear();
        self.readers.resize(n, 0);
        self.alias.clear();
        self.alias.resize(n, NONE);
        for m in 0..self.dst.len() {
            self.writer[self.dst[m] as usize] = m as u32;
            let s = self.src[m];
            if s < IMM {
                self.readers[s as usize] += 1;
            }
        }
        Ok(involved)
    }

    /// Emits every ready move, and every move each emission makes ready.
    fn drain(&mut self, legal: &mut Legal<'_>, out: &mut Vec<Step>) -> Result<(), MoveError> {
        while let Some(m) = self.ready.pop() {
            let m = m as usize;
            let d = self.dst[m];
            let class = self.class[m];
            let src = match self.src[m] {
                IMM => Source::Imm(self.imm[m]),
                SCRATCH => match legal.scratch.take() {
                    Some(l) => Source::Loc(l),
                    None => return Err(MoveError::NoScratch { class }),
                },
                s => Source::Loc(self.locs[s as usize]),
            };
            legal.emit(out, self.locs[d as usize], src, class)?;
            self.writer[d as usize] = NONE;
            let s = self.src[m];
            if s < IMM {
                // `d` now holds `s`'s original value and nothing will write
                // it again: remember it, preferring a register copy.
                let a = &mut self.alias[s as usize];
                if *a == NONE
                    || (self.locs[*a as usize].is_mem() && !self.locs[d as usize].is_mem())
                {
                    *a = d;
                }
                self.release(s);
            }
        }
        Ok(())
    }

    /// One pending read of `s` is done; if it was the last, the pending move
    /// writing `s` becomes ready.
    fn release(&mut self, s: u32) {
        let r = &mut self.readers[s as usize];
        *r -= 1;
        if *r == 0 {
            let w = self.writer[s as usize];
            if w != NONE {
                self.ready.push(w);
            }
        }
    }

    /// Breaks the cycle through pending move `start`. Every pending move is
    /// on a cycle here (trees were drained), each location on it has exactly
    /// one pending reader, and following a move's source to that source's
    /// writer walks the cycle.
    fn break_cycle(
        &mut self,
        start: u32,
        legal: &mut Legal<'_>,
        out: &mut Vec<Step>,
    ) -> Result<(), MoveError> {
        self.cycle.clear();
        let mut m = start;
        loop {
            self.cycle.push(m);
            let s = self.src[m as usize];
            let next = self.writer.get(s as usize).copied().unwrap_or(NONE);
            if next == start || next == NONE {
                break;
            }
            m = next;
        }
        // Alias: a fan-out copy of a cycle value already sits in a location
        // nothing will overwrite. Read it from there instead.
        for &m in &self.cycle {
            let s = self.src[m as usize];
            let a = self.alias[s as usize];
            if a != NONE {
                self.src[m as usize] = a;
                self.readers[a as usize] += 1;
                self.release(s);
                return Ok(());
            }
        }
        let class = self.class[start as usize];
        let all_regs_one_class = self.cycle.iter().all(|&m| {
            !self.locs[self.dst[m as usize] as usize].is_mem() && self.class[m as usize] == class
        });
        if all_regs_one_class && legal.env.swap && legal.target.move_caps().swap() {
            // cycle[i]: D_i := D_{i+1}; swap(D_0, D_1), swap(D_1, D_2), ...
            for w in 0..self.cycle.len().saturating_sub(1) {
                let a = self.locs[self.dst[self.cycle[w] as usize] as usize];
                let b = self.locs[self.dst[self.cycle[w + 1] as usize] as usize];
                if let (Loc::Reg(a), Loc::Reg(b)) = (a, b) {
                    legal.push(out, Step::Swap { a, b, class });
                }
            }
            for &m in &self.cycle {
                let d = self.dst[m as usize] as usize;
                self.writer[d] = NONE;
                self.readers[d] = 0;
            }
            return Ok(());
        }
        // Scratch: save one source, prefer a move whose source and
        // destination are registers (cheapest if the scratch is a slot).
        let pick = self
            .cycle
            .iter()
            .copied()
            .find(|&m| {
                !self.locs[self.src[m as usize] as usize].is_mem()
                    && !self.locs[self.dst[m as usize] as usize].is_mem()
            })
            .unwrap_or(start);
        let class = self.class[pick as usize];
        let s = self.src[pick as usize];
        let scratch = match legal.free_reg(class) {
            Some(r) => Loc::Reg(r),
            None => {
                let slot = legal
                    .env
                    .scratch_slot
                    .ok_or(MoveError::NoScratch { class })?;
                legal.used.scratch_slot = true;
                Loc::Slot(slot)
            }
        };
        legal.emit(out, scratch, Source::Loc(self.locs[s as usize]), class)?;
        legal.scratch = Some(scratch);
        self.src[pick as usize] = SCRATCH;
        self.release(s);
        Ok(())
    }
}

/// Resolves `moves` with a fresh [`MoveResolver`]: the one-call path.
///
/// # Errors
///
/// As for [`MoveResolver::resolve`].
///
/// # Examples
///
/// ```
/// use backend_lang::moves::{self, Move, MoveEnv, Step};
/// use backend_lang::x86_64::{self, GPR, RAX, RBX, RCX};
///
/// // rax <- rbx <- rcx <- rax: a 3-cycle is two swaps.
/// let rot = [Move::new(RAX, RBX, GPR), Move::new(RBX, RCX, GPR), Move::new(RCX, RAX, GPR)];
/// let steps = moves::resolve(&x86_64::TARGET, &rot, &MoveEnv::new())?;
/// assert_eq!(steps, [
///     Step::Swap { a: RAX, b: RBX, class: GPR },
///     Step::Swap { a: RBX, b: RCX, class: GPR },
/// ]);
/// # Ok::<(), backend_lang::moves::MoveError>(())
/// ```
pub fn resolve(target: &Target, moves: &[Move], env: &MoveEnv) -> Result<Vec<Step>, MoveError> {
    let mut out = Vec::with_capacity(moves.len() + 1);
    let _used = MoveResolver::new().resolve(target, moves, env, &mut out)?;
    Ok(out)
}
