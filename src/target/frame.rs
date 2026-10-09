//! Stack-frame layout: where each stack slot lives, which registers the
//! prologue saves, how much stack it allocates, and the prologue and
//! epilogue as a list of steps.
//!
//! The model is the x86-64 push frame, shared by SysV and Win64:
//!
//! ```text
//!   higher addresses
//!   ┌──────────────────────────┐
//!   │ incoming stack arguments │  CFA + offset   (SlotKind::Incoming)
//!   ├──────────────────────────┤  <- CFA (the stack pointer before the call)
//!   │ return address           │
//!   │ saved frame pointer      │  push rbp; mov rbp, rsp   (with a frame pointer)
//!   │ saved callee-saved GPRs  │  push rbx ...
//!   │ padding to 16            │
//!   │ locals and spill slots   │  largest alignment first
//!   │ saved callee-saved XMMs  │  (Win64 xmm6-xmm15; 16-byte slots)
//!   │ outgoing argument area   │  shadow space + stack arguments of calls
//!   └──────────────────────────┘  <- SP after the prologue, 16-aligned
//! ```
//!
//! The stack pointer does not move between the prologue and the epilogue, so
//! every slot has a fixed offset from it, and stack arguments of calls are
//! stored at `[sp + offset]` directly. A function that makes no calls and
//! needs no more than the ABI's red zone keeps its locals below the stack
//! pointer and allocates nothing. A frame of at least the ABI's probe
//! interval is allocated one page at a time, touching each page, so the
//! guard page below the stack is never jumped over (Windows requires this;
//! on Linux it is stack-clash protection).
//!
//! AArch64's frame (`stp`/`ldp` pairs, frame record of `fp` and `lr`) is a
//! different shape and arrives with the AArch64 target (backend-lang 0.5).

use alloc::vec::Vec;
use core::fmt;

use super::abi::Abi;
use crate::mir::{SlotKind, StackSlot};
use crate::regs::{PReg, RegSet};

/// Bytes of a return address and of one pushed register on x86-64.
const WORD: u64 = 8;

/// Everything about a function the frame depends on.
///
/// [`Function::frame_layout`](crate::mir::Function::frame_layout) fills this
/// in from a function; build one by hand to lay out a frame for code that is
/// not MIR.
///
/// # Examples
///
/// ```
/// use backend_lang::RegSet;
/// use backend_lang::mir::SlotKind;
/// use backend_lang::target::frame::FrameInput;
/// use backend_lang::x86_64::{SYSV, RBX};
///
/// let slots = [SlotKind::Local { size: 8, align: 8 }];
/// let input = FrameInput {
///     slots: &slots,
///     written: RegSet::of(&[RBX]),
///     has_calls: true,
///     outgoing: 0,
///     frame_pointer: true,
/// };
/// let frame = SYSV.layout_frame(&input)?;
/// assert_eq!(frame.pushed(), [RBX]);
/// # Ok::<(), backend_lang::target::frame::FrameError>(())
/// ```
#[derive(Clone, Copy, Debug)]
pub struct FrameInput<'a> {
    /// The function's stack slots, indexed by [`StackSlot::index`].
    pub slots: &'a [SlotKind],
    /// The registers the function writes; the callee-saved ones among them
    /// are saved and restored.
    pub written: RegSet,
    /// Whether the function calls anything (then the stack is 16-aligned at
    /// each call, the red zone is off, and Win64 shadow space is reserved).
    pub has_calls: bool,
    /// Bytes of outgoing argument area the function's calls need (the
    /// largest [`Abi::assign_args`] result over its calls).
    pub outgoing: u32,
    /// Whether to keep a frame pointer.
    pub frame_pointer: bool,
}

/// Why a frame could not be laid out.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::{Function, SlotKind};
/// use backend_lang::target::frame::FrameError;
/// use backend_lang::x86_64::SYSV;
///
/// let mut f = Function::new("f", &SYSV);
/// let slot = f.stack_slot(SlotKind::Local { size: 32, align: 32 })?;
/// assert_eq!(f.frame_layout().unwrap_err(), FrameError::Alignment { slot, align: 32 });
/// # Ok::<(), backend_lang::mir::MirError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FrameError {
    /// The frame, or an offset into it, exceeds the 32-bit displacement
    /// instructions address it with. Move large data to the heap.
    TooLarge,
    /// A slot's alignment is zero, not a power of two, or larger than the
    /// ABI's stack alignment (16), which the fixed frame cannot provide
    /// without realigning the stack.
    Alignment {
        /// The slot.
        slot: StackSlot,
        /// Its alignment.
        align: u32,
    },
    /// Two incoming slots overlap; each names a distinct stack argument.
    OverlappingIncoming {
        /// The second of the two slots.
        slot: StackSlot,
    },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::TooLarge => {
                f.write_str("the stack frame exceeds the 2 GiB a 32-bit displacement reaches")
            }
            FrameError::Alignment { slot, align } => write!(
                f,
                "stack slot ss{} asks for alignment {align}; slots take a power of two up to the stack alignment",
                slot.index()
            ),
            FrameError::OverlappingIncoming { slot } => write!(
                f,
                "incoming stack slot ss{} overlaps another incoming slot",
                slot.index()
            ),
        }
    }
}

impl core::error::Error for FrameError {}

/// One step of a prologue or epilogue, in execution order.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::{Function, SlotKind};
/// use backend_lang::target::frame::FrameStep;
/// use backend_lang::x86_64::{WIN64, RBP};
///
/// let mut f = Function::new("f", &WIN64);
/// let _big = f.stack_slot(SlotKind::Local { size: 10000, align: 8 })?;
/// let frame = f.frame_layout().unwrap();
/// assert_eq!(frame.prologue()[0], FrameStep::Push(RBP));
/// assert_eq!(frame.prologue()[1], FrameStep::SetFramePointer);
/// // Win64 probes frames of a page or more.
/// assert!(matches!(frame.prologue()[2], FrameStep::Alloc { probe: 4096, .. }));
/// # Ok::<(), backend_lang::mir::MirError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FrameStep {
    /// Push a register (`push r`).
    Push(PReg),
    /// Point the frame pointer at the stack pointer (`mov rbp, rsp`).
    SetFramePointer,
    /// Lower the stack pointer by `bytes`. With `probe` non-zero, one page of
    /// `probe` bytes at a time, touching each.
    Alloc {
        /// Bytes to allocate.
        bytes: u32,
        /// The probe interval, or `0` for one adjustment.
        probe: u32,
    },
    /// Store a vector register into the frame at `[sp + offset]`.
    SaveVec {
        /// The register.
        reg: PReg,
        /// Its save slot's offset from the stack pointer.
        offset: i32,
    },
    /// Reload a vector register saved by [`FrameStep::SaveVec`].
    RestoreVec {
        /// The register.
        reg: PReg,
        /// Its save slot's offset from the stack pointer.
        offset: i32,
    },
    /// Raise the stack pointer by `bytes` (`add rsp, bytes`).
    Free(u32),
    /// Pop a register (`pop r`).
    Pop(PReg),
}

/// A laid-out frame: slot offsets and the prologue and epilogue.
///
/// Offsets are from the stack pointer after the prologue; they are negative
/// for locals kept in the red zone.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::{Function, SlotKind};
/// use backend_lang::x86_64::SYSV;
///
/// let mut f = Function::new("leaf", &SYSV);
/// let a = f.stack_slot(SlotKind::Local { size: 8, align: 8 })?;
/// let frame = f.frame_layout().unwrap();
/// // A leaf with 8 bytes of locals uses the red zone: nothing is allocated.
/// assert!(frame.uses_red_zone());
/// assert_eq!(frame.alloc(), 0);
/// assert!(frame.slot_offset(a).unwrap() < 0);
/// # Ok::<(), backend_lang::mir::MirError>(())
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameLayout {
    offsets: Vec<i32>,
    pushed: Vec<PReg>,
    vec_saves: Vec<(PReg, i32)>,
    prologue: Vec<FrameStep>,
    epilogue: Vec<FrameStep>,
    alloc: u32,
    cfa_offset: u32,
    frame_pointer: bool,
    red_zone: bool,
}

impl FrameLayout {
    /// A slot's offset from the stack pointer after the prologue, or `None`
    /// for a slot the layout does not have.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Function, SlotKind};
    /// use backend_lang::x86_64::SYSV;
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let arg = f.stack_slot(SlotKind::Incoming { offset: 0, size: 8 })?;
    /// let frame = f.frame_layout().unwrap();
    /// // rbp is pushed: [rsp] = saved rbp, [rsp+8] = return address, [rsp+16] = argument.
    /// assert_eq!(frame.slot_offset(arg), Some(16));
    /// # Ok::<(), backend_lang::mir::MirError>(())
    /// ```
    #[inline]
    #[must_use]
    pub fn slot_offset(&self, slot: StackSlot) -> Option<i32> {
        self.offsets.get(slot.index()).copied()
    }

    /// The general-purpose callee-saved registers pushed after the frame
    /// pointer, in push order.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// assert!(Function::new("f", &SYSV).frame_layout().unwrap().pushed().is_empty());
    /// ```
    #[inline]
    #[must_use]
    pub fn pushed(&self) -> &[PReg] {
        &self.pushed
    }

    /// Vector callee-saved registers saved into the frame, with their save
    /// slots' offsets.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// assert!(Function::new("f", &SYSV).frame_layout().unwrap().vec_saves().is_empty());
    /// ```
    #[inline]
    #[must_use]
    pub fn vec_saves(&self) -> &[(PReg, i32)] {
        &self.vec_saves
    }

    /// The prologue, in execution order.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::target::frame::FrameStep;
    /// use backend_lang::x86_64::{SYSV, RBP};
    ///
    /// let frame = Function::new("f", &SYSV).frame_layout().unwrap();
    /// assert_eq!(frame.prologue(), [FrameStep::Push(RBP), FrameStep::SetFramePointer]);
    /// ```
    #[inline]
    #[must_use]
    pub fn prologue(&self) -> &[FrameStep] {
        &self.prologue
    }

    /// The epilogue, in execution order (the return itself is not part of
    /// it).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::target::frame::FrameStep;
    /// use backend_lang::x86_64::{SYSV, RBP};
    ///
    /// let frame = Function::new("f", &SYSV).frame_layout().unwrap();
    /// assert_eq!(frame.epilogue(), [FrameStep::Pop(RBP)]);
    /// ```
    #[inline]
    #[must_use]
    pub fn epilogue(&self) -> &[FrameStep] {
        &self.epilogue
    }

    /// Bytes the prologue subtracts from the stack pointer after its pushes.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// assert_eq!(Function::new("f", &SYSV).frame_layout().unwrap().alloc(), 0);
    /// ```
    #[inline]
    #[must_use]
    pub const fn alloc(&self) -> u32 {
        self.alloc
    }

    /// The canonical frame address (the stack pointer before the call) minus
    /// the stack pointer after the prologue: return address, pushes, and
    /// allocation.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// // Return address plus the saved rbp.
    /// assert_eq!(Function::new("f", &SYSV).frame_layout().unwrap().cfa_offset(), 16);
    /// ```
    #[inline]
    #[must_use]
    pub const fn cfa_offset(&self) -> u32 {
        self.cfa_offset
    }

    /// Whether the frame keeps a frame pointer.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// assert!(Function::new("f", &SYSV).frame_layout().unwrap().frame_pointer());
    /// ```
    #[inline]
    #[must_use]
    pub const fn frame_pointer(&self) -> bool {
        self.frame_pointer
    }

    /// Whether the locals live in the red zone below the stack pointer.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::SYSV;
    ///
    /// assert!(!Function::new("f", &SYSV).frame_layout().unwrap().uses_red_zone());
    /// ```
    #[inline]
    #[must_use]
    pub const fn uses_red_zone(&self) -> bool {
        self.red_zone
    }
}

/// The smallest `n >= at_least` with `n ≡ residue (mod align)`.
fn round_to_residue(at_least: u64, residue: u64, align: u64) -> u64 {
    let rem = at_least % align;
    let add = (residue + align - rem) % align;
    at_least + add
}

fn fits_i32(v: u64) -> Result<i32, FrameError> {
    i32::try_from(v).map_err(|_| FrameError::TooLarge)
}

impl Abi {
    /// Lays out a frame under this convention.
    ///
    /// # Errors
    ///
    /// [`FrameError::Alignment`] for a slot alignment that is not a power of
    /// two up to the stack alignment, [`FrameError::OverlappingIncoming`] for
    /// incoming slots that overlap, and [`FrameError::TooLarge`] when any
    /// offset would not fit a 32-bit displacement.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::RegSet;
    /// use backend_lang::mir::SlotKind;
    /// use backend_lang::target::frame::{FrameInput, FrameStep};
    /// use backend_lang::x86_64::{SYSV, RBP, R12};
    ///
    /// let slots = [SlotKind::Local { size: 4, align: 4 }, SlotKind::Local { size: 16, align: 16 }];
    /// let frame = SYSV.layout_frame(&FrameInput {
    ///     slots: &slots,
    ///     written: RegSet::of(&[R12]),
    ///     has_calls: true,
    ///     outgoing: 0,
    ///     frame_pointer: true,
    /// })?;
    /// // ret + rbp + r12 = 24 bytes above; 20 bytes of locals round to 24 so the
    /// // stack pointer ends 16-aligned.
    /// assert_eq!(frame.alloc(), 24);
    /// assert_eq!(frame.prologue(), [
    ///     FrameStep::Push(RBP),
    ///     FrameStep::SetFramePointer,
    ///     FrameStep::Push(R12),
    ///     FrameStep::Alloc { bytes: 24, probe: 0 },
    /// ]);
    /// // The 16-aligned slot comes first, at the bottom.
    /// assert_eq!(frame.slot_offset(backend_lang::mir::StackSlot::new(1)), Some(0));
    /// assert_eq!(frame.slot_offset(backend_lang::mir::StackSlot::new(0)), Some(16));
    /// # Ok::<(), backend_lang::target::frame::FrameError>(())
    /// ```
    pub fn layout_frame(&self, input: &FrameInput<'_>) -> Result<FrameLayout, FrameError> {
        let target = self.target();
        let align = u64::from(self.stack().align()).max(1);
        let sp = target.stack_pointer();
        let fp = target.frame_pointer();

        // Which registers to preserve: the callee-saved ones the body writes.
        // The frame pointer is handled by its own push when it is kept.
        let mut saved = input.written.intersect(self.callee_saved()).without(sp);
        if input.frame_pointer {
            saved = saved.without(fp);
        }
        let push_class = target.addr_class();
        let mut pushed = Vec::new();
        let mut vec_regs = Vec::new();
        for reg in saved.iter() {
            if target.class_of(reg) == Some(push_class) {
                pushed.push(reg);
            } else {
                vec_regs.push(reg);
            }
        }
        let pushes = pushed.len() as u64 + u64::from(input.frame_pointer);
        let above = WORD + WORD * pushes;

        // Locals (and vector save slots), largest alignment first, then by
        // slot number, so the packing is tight and the order deterministic.
        let mut locals: Vec<(u32, u64, usize)> = Vec::new(); // (align, size, id); id >= slots.len() is a save slot
        for (i, kind) in input.slots.iter().enumerate() {
            if let SlotKind::Local { size, align: a } = *kind {
                if a == 0 || !a.is_power_of_two() || u64::from(a) > align {
                    return Err(FrameError::Alignment {
                        slot: StackSlot::from_index(i),
                        align: a,
                    });
                }
                locals.push((a, u64::from(size), i));
            }
        }
        for (k, &reg) in vec_regs.iter().enumerate() {
            let class = target.class(target.class_of(reg).unwrap_or(push_class));
            let a = class.spill_align().clamp(1, 16);
            locals.push((a, u64::from(class.spill_size()), input.slots.len() + k));
        }
        locals.sort_unstable_by(|x, y| y.0.cmp(&x.0).then(x.2.cmp(&y.2)));
        let mut local_off = Vec::with_capacity(locals.len());
        let mut cursor = 0u64;
        for &(a, size, id) in &locals {
            let a = u64::from(a);
            cursor = cursor.div_ceil(a) * a;
            local_off.push((id, cursor));
            cursor += size;
        }
        let locals_size = cursor;

        let mut outgoing = u64::from(input.outgoing);
        if input.has_calls {
            outgoing = outgoing.max(u64::from(self.stack().shadow_space()));
        }
        let outgoing = outgoing.div_ceil(align) * align;
        // Residue the allocation must have so the stack pointer ends aligned
        // (the CFA is aligned: the caller's stack pointer at the call was).
        let residue = (align - above % align) % align;

        let red_zone_bytes = round_to_residue(locals_size, residue, align);
        let (alloc, base, red_zone) = if !input.has_calls
            && outgoing == 0
            && locals_size > 0
            && red_zone_bytes <= u64::from(self.stack().red_zone())
        {
            // Locals sit below the stack pointer: base offset is negative.
            (0u64, -(red_zone_bytes as i64), true)
        } else if !input.has_calls && outgoing == 0 && locals_size == 0 {
            (0, 0, false)
        } else {
            (
                round_to_residue(outgoing + locals_size, residue, align),
                outgoing as i64,
                false,
            )
        };
        if alloc > i32::MAX as u64 {
            return Err(FrameError::TooLarge);
        }
        let cfa = alloc + above;
        let cfa_i = fits_i32(cfa)?;

        let mut offsets = alloc::vec![0i32; input.slots.len()];
        let mut vec_saves = Vec::with_capacity(vec_regs.len());
        for &(id, off) in &local_off {
            let at = base + off as i64;
            let at = i32::try_from(at).map_err(|_| FrameError::TooLarge)?;
            match offsets.get_mut(id) {
                Some(o) => *o = at,
                None => {
                    if let Some(&reg) = vec_regs.get(id - input.slots.len()) {
                        vec_saves.push((reg, at));
                    }
                }
            }
        }
        // Incoming slots: CFA + offset. Checked for overlap by sorting.
        let mut incoming: Vec<(u64, u64, usize)> = Vec::new();
        for (i, kind) in input.slots.iter().enumerate() {
            if let SlotKind::Incoming { offset, size } = *kind {
                let at = cfa + u64::from(offset);
                offsets[i] = fits_i32(at)?;
                let _end = fits_i32(at + u64::from(size))?;
                incoming.push((u64::from(offset), u64::from(size.max(1)), i));
            }
        }
        incoming.sort_unstable();
        for pair in incoming.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            if a.0 + a.1 > b.0 {
                return Err(FrameError::OverlappingIncoming {
                    slot: StackSlot::from_index(b.2),
                });
            }
        }

        let alloc = alloc as u32;
        let probe = self.stack().probe_interval();
        let probe = if probe > 0 && alloc >= probe {
            probe
        } else {
            0
        };
        let mut prologue = Vec::with_capacity(pushed.len() + vec_saves.len() + 3);
        let mut epilogue = Vec::with_capacity(pushed.len() + vec_saves.len() + 2);
        if input.frame_pointer {
            prologue.push(FrameStep::Push(fp));
            prologue.push(FrameStep::SetFramePointer);
        }
        prologue.extend(pushed.iter().map(|&r| FrameStep::Push(r)));
        if alloc > 0 {
            prologue.push(FrameStep::Alloc {
                bytes: alloc,
                probe,
            });
        }
        prologue.extend(
            vec_saves
                .iter()
                .map(|&(reg, offset)| FrameStep::SaveVec { reg, offset }),
        );
        epilogue.extend(
            vec_saves
                .iter()
                .map(|&(reg, offset)| FrameStep::RestoreVec { reg, offset }),
        );
        if alloc > 0 {
            epilogue.push(FrameStep::Free(alloc));
        }
        epilogue.extend(pushed.iter().rev().map(|&r| FrameStep::Pop(r)));
        if input.frame_pointer {
            epilogue.push(FrameStep::Pop(fp));
        }
        Ok(FrameLayout {
            offsets,
            pushed,
            vec_saves,
            prologue,
            epilogue,
            alloc,
            cfa_offset: cfa_i as u32,
            frame_pointer: input.frame_pointer,
            red_zone,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;
    use alloc::vec;

    use super::*;
    use crate::x86_64::*;

    fn input(slots: &[SlotKind], written: RegSet, calls: bool, fp: bool) -> FrameInput<'_> {
        FrameInput {
            slots,
            written,
            has_calls: calls,
            outgoing: 0,
            frame_pointer: fp,
        }
    }

    /// Simulates the prologue from an aligned CFA and checks the invariants
    /// every frame must hold.
    fn check(abi: &Abi, frame: &FrameLayout, slots: &[SlotKind], calls: bool) {
        let cfa: i64 = 0x10000;
        let mut sp = cfa - 8; // return address
        for step in frame.prologue() {
            match *step {
                FrameStep::Push(_) => sp -= 8,
                FrameStep::Alloc { bytes, .. } => sp -= i64::from(bytes),
                _ => {}
            }
        }
        assert_eq!(cfa - sp, i64::from(frame.cfa_offset()));
        if calls || frame.alloc() > 0 {
            assert_eq!(sp % 16, 0, "stack pointer aligned after the prologue");
        }
        // Locals: aligned, disjoint, inside the frame (or the red zone), and
        // clear of the outgoing area and the saved registers.
        let mut spans = vec![];
        for (i, s) in slots.iter().enumerate() {
            let off = i64::from(
                frame
                    .slot_offset(StackSlot::from_index(i))
                    .unwrap_or(i32::MIN),
            );
            let addr = sp + off;
            match *s {
                SlotKind::Local { size, align } => {
                    assert_eq!(addr % i64::from(align), 0, "slot {i} aligned");
                    let lo = if frame.uses_red_zone() {
                        sp - i64::from(abi.stack().red_zone())
                    } else {
                        sp
                    };
                    assert!(addr >= lo);
                    let pushes_bottom = cfa
                        - 8
                        - 8 * (frame.pushed().len() as i64 + i64::from(frame.frame_pointer()));
                    assert!(addr + i64::from(size) <= pushes_bottom);
                    spans.push((addr, addr + i64::from(size)));
                }
                SlotKind::Incoming { offset, .. } => assert_eq!(addr, cfa + i64::from(offset)),
            }
        }
        spans.sort_unstable();
        for w in spans.windows(2) {
            assert!(w[0].1 <= w[1].0, "locals overlap");
        }
        // The epilogue undoes the prologue.
        let mut back = sp;
        for step in frame.epilogue() {
            match *step {
                FrameStep::Pop(_) => back += 8,
                FrameStep::Free(b) => back += i64::from(b),
                _ => {}
            }
        }
        assert_eq!(back, cfa - 8);
    }

    #[test]
    fn test_layout_invariants_over_mixed_shapes() {
        let shapes: &[&[SlotKind]] = &[
            &[],
            &[SlotKind::Local { size: 1, align: 1 }],
            &[
                SlotKind::Local { size: 3, align: 1 },
                SlotKind::Local {
                    size: 16,
                    align: 16,
                },
                SlotKind::Local { size: 8, align: 8 },
                SlotKind::Incoming { offset: 0, size: 8 },
                SlotKind::Local { size: 2, align: 2 },
            ],
            &[SlotKind::Local {
                size: 200,
                align: 8,
            }],
            &[SlotKind::Local {
                size: 9000,
                align: 16,
            }],
        ];
        let writes = [
            RegSet::EMPTY,
            RegSet::of(&[RBX]),
            RegSet::of(&[RBX, R12, R15]),
            RegSet::of(&[RBX, RSI, RDI, XMM6, XMM9]),
        ];
        for abi in [&SYSV, &WIN64] {
            for slots in shapes {
                for &written in &writes {
                    for calls in [false, true] {
                        for fp in [false, true] {
                            let frame = abi.layout_frame(&input(slots, written, calls, fp));
                            let frame = frame.unwrap_or_else(|e| panic!("{e}"));
                            check(abi, &frame, slots, calls);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn test_red_zone_only_for_sysv_leaves_that_fit() {
        let small = [SlotKind::Local {
            size: 120,
            align: 8,
        }];
        let f = SYSV
            .layout_frame(&input(&small, RegSet::EMPTY, false, true))
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(f.uses_red_zone());
        assert_eq!(f.alloc(), 0);
        let f = SYSV
            .layout_frame(&input(&small, RegSet::EMPTY, true, true))
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(!f.uses_red_zone());
        let f = WIN64
            .layout_frame(&input(&small, RegSet::EMPTY, false, true))
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(!f.uses_red_zone());
        let big = [SlotKind::Local {
            size: 129,
            align: 8,
        }];
        let f = SYSV
            .layout_frame(&input(&big, RegSet::EMPTY, false, true))
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(!f.uses_red_zone());
    }

    #[test]
    fn test_win64_shadow_space_and_vector_saves() {
        let f = WIN64
            .layout_frame(&input(&[], RegSet::of(&[XMM6, RSI]), true, false))
            .unwrap_or_else(|e| panic!("{e}"));
        // rsi pushed; xmm6 saved in a 16-byte slot above the 32-byte shadow area.
        assert_eq!(f.pushed(), [RSI]);
        assert_eq!(f.vec_saves(), [(XMM6, 32)]);
        assert!(f.alloc() >= 48);
        assert!(f.prologue().contains(&FrameStep::SaveVec {
            reg: XMM6,
            offset: 32
        }));
        // SysV preserves no vector registers.
        let f = SYSV
            .layout_frame(&input(&[], RegSet::of(&[XMM6]), true, false))
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(f.vec_saves().is_empty());
    }

    #[test]
    fn test_probe_threshold() {
        let page = [SlotKind::Local {
            size: 4096,
            align: 8,
        }];
        for abi in [&SYSV, &WIN64] {
            let f = abi
                .layout_frame(&input(&page, RegSet::EMPTY, true, true))
                .unwrap_or_else(|e| panic!("{e}"));
            assert!(matches!(
                f.prologue().last(),
                Some(FrameStep::Alloc { probe: 4096, .. })
            ));
        }
        let small = [SlotKind::Local {
            size: 4000,
            align: 8,
        }];
        let f = WIN64
            .layout_frame(&input(&small, RegSet::EMPTY, true, true))
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(
            f.prologue().last(),
            Some(FrameStep::Alloc { probe: 0, .. })
        ));
    }

    #[test]
    fn test_errors() {
        let bad = [SlotKind::Local { size: 8, align: 3 }];
        assert_eq!(
            SYSV.layout_frame(&input(&bad, RegSet::EMPTY, false, true)),
            Err(FrameError::Alignment {
                slot: StackSlot::from_index(0),
                align: 3
            })
        );
        let zero = [SlotKind::Local { size: 8, align: 0 }];
        assert!(
            SYSV.layout_frame(&input(&zero, RegSet::EMPTY, false, true))
                .is_err()
        );
        let huge = [SlotKind::Local {
            size: u32::MAX,
            align: 8,
        }];
        assert_eq!(
            SYSV.layout_frame(&input(&huge, RegSet::EMPTY, false, true)),
            Err(FrameError::TooLarge)
        );
        let many = vec![
            SlotKind::Local {
                size: 1 << 30,
                align: 8
            };
            3
        ];
        assert_eq!(
            SYSV.layout_frame(&input(&many, RegSet::EMPTY, false, true)),
            Err(FrameError::TooLarge)
        );
        let far = [SlotKind::Incoming {
            offset: u32::MAX - 4,
            size: 8,
        }];
        assert_eq!(
            SYSV.layout_frame(&input(&far, RegSet::EMPTY, false, true)),
            Err(FrameError::TooLarge)
        );
        let overlap = [
            SlotKind::Incoming { offset: 0, size: 8 },
            SlotKind::Incoming { offset: 4, size: 8 },
        ];
        assert_eq!(
            SYSV.layout_frame(&input(&overlap, RegSet::EMPTY, false, true)),
            Err(FrameError::OverlappingIncoming {
                slot: StackSlot::from_index(1)
            })
        );
        assert!(FrameError::TooLarge.to_string().contains("2 GiB"));
    }
}
