//! Registers: physical registers of a target, virtual registers of a
//! function, the classes that type them, and sets of physical registers.
//!
//! All four are dense numbers. A [`PReg`] indexes the target's register file
//! ([`Target::regs`](crate::target::Target::regs)), a [`RegClass`] indexes its
//! class table, and a [`VReg`] indexes a function's virtual-register table,
//! which records each virtual register's class. [`Reg`] is either kind,
//! packed into one `u32`, so an operand that names a register is the same
//! size before and after register allocation.

use core::fmt;

/// The most physical registers a target may describe.
///
/// [`RegSet`] is a fixed 128-bit set, so every target's register file fits
/// in one, with room for x86-64's 16 general-purpose and 16 vector registers
/// and AArch64's 31 + 32.
pub const MAX_PREGS: usize = 128;

/// A physical register: an index into the target's register file.
///
/// What the number means (its name, hardware encoding, and class) is the
/// target's business; see [`Target::reg`](crate::target::Target::reg). The
/// x86-64 target names its registers as constants, such as
/// [`x86_64::RAX`](crate::x86_64::RAX).
///
/// # Examples
///
/// ```
/// use backend_lang::x86_64::{self, RAX, RDI};
///
/// assert_eq!(x86_64::TARGET.reg(RAX).name(), "rax");
/// assert!(RAX.index() < RDI.index());
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PReg(u8);

impl PReg {
    /// The physical register with index `index`, or `None` past
    /// [`MAX_PREGS`].
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::PReg;
    ///
    /// assert_eq!(PReg::new(3).map(PReg::index), Some(3));
    /// assert_eq!(PReg::new(128), None);
    /// ```
    #[inline]
    #[must_use]
    pub const fn new(index: usize) -> Option<PReg> {
        if index < MAX_PREGS {
            Some(PReg(index as u8))
        } else {
            None
        }
    }

    /// Builds a register from an index the caller knows is in range (target
    /// tables are checked by tests to stay below [`MAX_PREGS`]).
    pub(crate) const fn from_index(index: u8) -> PReg {
        PReg(index & (MAX_PREGS as u8 - 1))
    }

    /// The register's index in the target's register file.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::RCX;
    ///
    /// assert_eq!(RCX.index(), 1);
    /// ```
    #[inline]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Debug for PReg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "p{}", self.0)
    }
}

/// A virtual register: an index into a function's virtual-register table.
///
/// Virtual registers are created by
/// [`Function::vreg`](crate::mir::Function::vreg), each with a
/// [`RegClass`], and printed as `%N`.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::Function;
/// use backend_lang::x86_64::{GPR, SYSV};
///
/// let mut f = Function::new("f", &SYSV);
/// let v = f.vreg(GPR).unwrap();
/// assert_eq!(v.as_virt().map(|v| v.index()), Some(0));
/// assert_eq!(f.vreg_class(v.as_virt().unwrap()), Some(GPR));
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VReg(u32);

impl VReg {
    /// The most virtual registers one function may have: indexes use 31
    /// bits, because [`Reg`] keeps the top bit to tell the two kinds apart.
    pub const MAX: usize = (1 << 31) - 1;

    pub(crate) const fn from_index(index: u32) -> VReg {
        VReg(index & VIRT_MASK)
    }

    /// The register's index in its function's virtual-register table.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::{GPR, SYSV};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let _ = f.vreg(GPR).unwrap();
    /// let second = f.vreg(GPR).unwrap().as_virt().unwrap();
    /// assert_eq!(second.index(), 1);
    /// ```
    #[inline]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Debug for VReg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "%{}", self.0)
    }
}

const VIRT_BIT: u32 = 1 << 31;
const VIRT_MASK: u32 = VIRT_BIT - 1;

/// A register operand: physical or virtual.
///
/// Before register allocation, instructions name virtual registers (and the
/// physical registers an ABI or an instruction fixes); after it, only
/// physical ones. Emission requires the latter.
///
/// # Examples
///
/// ```
/// use backend_lang::Reg;
/// use backend_lang::x86_64::RAX;
///
/// let r = Reg::from(RAX);
/// assert_eq!(r.as_phys(), Some(RAX));
/// assert!(r.as_virt().is_none());
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Reg(u32);

impl Reg {
    /// The register as a physical register, if it is one.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::Reg;
    /// use backend_lang::x86_64::RSI;
    ///
    /// assert_eq!(Reg::from(RSI).as_phys(), Some(RSI));
    /// ```
    #[inline]
    #[must_use]
    pub const fn as_phys(self) -> Option<PReg> {
        if self.0 & VIRT_BIT == 0 {
            Some(PReg::from_index(self.0 as u8))
        } else {
            None
        }
    }

    /// The register as a virtual register, if it is one.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::Function;
    /// use backend_lang::x86_64::{GPR, SYSV};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let r = f.vreg(GPR).unwrap();
    /// assert!(r.as_virt().is_some());
    /// assert!(r.as_phys().is_none());
    /// ```
    #[inline]
    #[must_use]
    pub const fn as_virt(self) -> Option<VReg> {
        if self.0 & VIRT_BIT != 0 {
            Some(VReg(self.0 & VIRT_MASK))
        } else {
            None
        }
    }

    /// Whether this is a virtual register.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::Reg;
    /// use backend_lang::x86_64::RAX;
    ///
    /// assert!(!Reg::from(RAX).is_virtual());
    /// ```
    #[inline]
    #[must_use]
    pub const fn is_virtual(self) -> bool {
        self.0 & VIRT_BIT != 0
    }
}

impl From<PReg> for Reg {
    #[inline]
    fn from(p: PReg) -> Reg {
        Reg(u32::from(p.0))
    }
}

impl From<VReg> for Reg {
    #[inline]
    fn from(v: VReg) -> Reg {
        Reg(v.0 | VIRT_BIT)
    }
}

impl fmt::Debug for Reg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.as_phys(), self.as_virt()) {
            (Some(p), _) => fmt::Debug::fmt(&p, f),
            (_, Some(v)) => fmt::Debug::fmt(&v, f),
            _ => Ok(()),
        }
    }
}

/// A register class: the set of physical registers a value may live in, and
/// the type of a virtual register. An index into the target's class table.
///
/// # Examples
///
/// ```
/// use backend_lang::x86_64::{self, GPR, RAX, XMM0};
///
/// let gpr = x86_64::TARGET.class(GPR);
/// assert_eq!(gpr.name(), "gpr");
/// assert!(gpr.regs().contains(RAX));
/// assert!(!gpr.regs().contains(XMM0));
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RegClass(u8);

impl RegClass {
    /// The class with index `index` (whether the target has it is checked
    /// where it is used).
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::RegClass;
    /// use backend_lang::x86_64::GPR;
    ///
    /// assert_eq!(RegClass::new(0), GPR);
    /// ```
    #[inline]
    #[must_use]
    pub const fn new(index: u8) -> RegClass {
        RegClass(index)
    }

    /// The class's index in the target's class table.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::VEC;
    ///
    /// assert_eq!(VEC.index(), 1);
    /// ```
    #[inline]
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

impl fmt::Debug for RegClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "class{}", self.0)
    }
}

/// A set of physical registers: 128 bits, one per [`PReg`] index.
///
/// Used for class membership, allocatable sets, callee- and caller-saved
/// sets, and per-instruction clobbers. Every operation is a few word
/// operations; iteration yields registers in index order.
///
/// # Examples
///
/// ```
/// use backend_lang::RegSet;
/// use backend_lang::x86_64::{RAX, RCX, RDX};
///
/// let set = RegSet::EMPTY.with(RAX).with(RDX);
/// assert!(set.contains(RDX));
/// assert!(!set.contains(RCX));
/// assert_eq!(set.iter().collect::<Vec<_>>(), [RAX, RDX]);
/// assert_eq!(set.len(), 2);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct RegSet([u64; 2]);

impl RegSet {
    /// The empty set.
    pub const EMPTY: RegSet = RegSet([0, 0]);

    /// The set holding exactly the registers in `regs`.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::RegSet;
    /// use backend_lang::x86_64::{R11, RCX};
    ///
    /// let set = RegSet::of(&[RCX, R11]);
    /// assert!(set.contains(R11));
    /// ```
    #[must_use]
    pub const fn of(regs: &[PReg]) -> RegSet {
        let mut set = RegSet::EMPTY;
        let mut i = 0;
        while i < regs.len() {
            set = set.with(regs[i]);
            i += 1;
        }
        set
    }

    /// The set with `reg` added.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::RegSet;
    /// use backend_lang::x86_64::RBX;
    ///
    /// assert!(RegSet::EMPTY.with(RBX).contains(RBX));
    /// ```
    #[inline]
    #[must_use]
    pub const fn with(self, reg: PReg) -> RegSet {
        let i = reg.index();
        let mut words = self.0;
        words[i >> 6] |= 1 << (i & 63);
        RegSet(words)
    }

    /// The set with `reg` removed.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::RegSet;
    /// use backend_lang::x86_64::RBX;
    ///
    /// assert!(RegSet::of(&[RBX]).without(RBX).is_empty());
    /// ```
    #[inline]
    #[must_use]
    pub const fn without(self, reg: PReg) -> RegSet {
        let i = reg.index();
        let mut words = self.0;
        words[i >> 6] &= !(1 << (i & 63));
        RegSet(words)
    }

    /// Whether `reg` is in the set.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::RegSet;
    /// use backend_lang::x86_64::{RAX, RBX};
    ///
    /// assert!(RegSet::of(&[RAX]).contains(RAX));
    /// assert!(!RegSet::of(&[RAX]).contains(RBX));
    /// ```
    #[inline]
    #[must_use]
    pub const fn contains(self, reg: PReg) -> bool {
        let i = reg.index();
        self.0[i >> 6] & (1 << (i & 63)) != 0
    }

    /// The registers in either set.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::RegSet;
    /// use backend_lang::x86_64::{RAX, RBX};
    ///
    /// assert_eq!(RegSet::of(&[RAX]).union(RegSet::of(&[RBX])).len(), 2);
    /// ```
    #[inline]
    #[must_use]
    pub const fn union(self, other: RegSet) -> RegSet {
        RegSet([self.0[0] | other.0[0], self.0[1] | other.0[1]])
    }

    /// The registers in both sets.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::RegSet;
    /// use backend_lang::x86_64::{RAX, RBX};
    ///
    /// let a = RegSet::of(&[RAX, RBX]);
    /// assert_eq!(a.intersect(RegSet::of(&[RBX])), RegSet::of(&[RBX]));
    /// ```
    #[inline]
    #[must_use]
    pub const fn intersect(self, other: RegSet) -> RegSet {
        RegSet([self.0[0] & other.0[0], self.0[1] & other.0[1]])
    }

    /// The registers in `self` but not in `other`.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::RegSet;
    /// use backend_lang::x86_64::{RAX, RBX};
    ///
    /// let a = RegSet::of(&[RAX, RBX]);
    /// assert_eq!(a.difference(RegSet::of(&[RAX])), RegSet::of(&[RBX]));
    /// ```
    #[inline]
    #[must_use]
    pub const fn difference(self, other: RegSet) -> RegSet {
        RegSet([self.0[0] & !other.0[0], self.0[1] & !other.0[1]])
    }

    /// Whether the set is empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::RegSet;
    ///
    /// assert!(RegSet::EMPTY.is_empty());
    /// ```
    #[inline]
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0[0] == 0 && self.0[1] == 0
    }

    /// The number of registers in the set.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::x86_64::{self, GPR};
    ///
    /// assert_eq!(x86_64::TARGET.class(GPR).regs().len(), 16);
    /// ```
    #[inline]
    #[must_use]
    pub const fn len(self) -> usize {
        (self.0[0].count_ones() + self.0[1].count_ones()) as usize
    }

    /// The lowest-numbered register in the set.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::RegSet;
    /// use backend_lang::x86_64::{RCX, R9};
    ///
    /// assert_eq!(RegSet::of(&[R9, RCX]).first(), Some(RCX));
    /// assert_eq!(RegSet::EMPTY.first(), None);
    /// ```
    #[inline]
    #[must_use]
    pub const fn first(self) -> Option<PReg> {
        if self.0[0] != 0 {
            Some(PReg::from_index(self.0[0].trailing_zeros() as u8))
        } else if self.0[1] != 0 {
            Some(PReg::from_index(64 + self.0[1].trailing_zeros() as u8))
        } else {
            None
        }
    }

    /// The registers in the set, in index order.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::RegSet;
    /// use backend_lang::x86_64::{RAX, R15};
    ///
    /// let v: Vec<_> = RegSet::of(&[R15, RAX]).iter().collect();
    /// assert_eq!(v, [RAX, R15]);
    /// ```
    #[must_use]
    pub fn iter(self) -> RegSetIter {
        RegSetIter(self)
    }
}

impl fmt::Debug for RegSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl FromIterator<PReg> for RegSet {
    fn from_iter<I: IntoIterator<Item = PReg>>(iter: I) -> RegSet {
        iter.into_iter().fold(RegSet::EMPTY, RegSet::with)
    }
}

/// The registers of a [`RegSet`], in index order. Made by [`RegSet::iter`].
///
/// # Examples
///
/// ```
/// use backend_lang::RegSet;
/// use backend_lang::x86_64::RDX;
///
/// let mut it = RegSet::of(&[RDX]).iter();
/// assert_eq!(it.next(), Some(RDX));
/// assert_eq!(it.next(), None);
/// ```
#[derive(Clone, Debug)]
pub struct RegSetIter(RegSet);

impl Iterator for RegSetIter {
    type Item = PReg;

    #[inline]
    fn next(&mut self) -> Option<PReg> {
        let first = self.0.first()?;
        self.0 = self.0.without(first);
        Some(first)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.0.len();
        (n, Some(n))
    }
}

impl ExactSizeIterator for RegSetIter {}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    #[test]
    fn test_reg_packing_round_trips_both_kinds() {
        for i in [0u8, 1, 15, 127] {
            let p = PReg::from_index(i);
            assert_eq!(Reg::from(p).as_phys(), Some(p));
            assert_eq!(Reg::from(p).as_virt(), None);
        }
        for i in [0u32, 1, 1 << 20, VIRT_MASK] {
            let v = VReg::from_index(i);
            assert_eq!(Reg::from(v).as_virt(), Some(v));
            assert_eq!(Reg::from(v).as_phys(), None);
            assert!(Reg::from(v).is_virtual());
        }
    }

    #[test]
    fn test_regset_high_word_and_iteration_order() {
        let regs: Vec<PReg> = [0u8, 63, 64, 100, 127]
            .iter()
            .map(|&i| PReg::from_index(i))
            .collect();
        let set: RegSet = regs.iter().copied().collect();
        assert_eq!(set.len(), 5);
        assert_eq!(set.iter().collect::<Vec<_>>(), regs);
        assert_eq!(set.iter().len(), 5);
        assert_eq!(set.without(regs[0]).first(), Some(regs[1]));
        let high = RegSet::of(&[PReg::from_index(100)]);
        assert_eq!(high.first(), Some(PReg::from_index(100)));
        assert!(set.difference(set).is_empty());
        assert_eq!(set.intersect(high), high);
    }

    #[test]
    fn test_preg_new_bounds() {
        assert!(PReg::new(127).is_some());
        assert!(PReg::new(128).is_none());
        assert!(PReg::new(usize::MAX).is_none());
    }
}
