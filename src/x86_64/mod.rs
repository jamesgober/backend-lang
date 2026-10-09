//! The x86-64 target: registers, classes, calling conventions (SysV and
//! Win64), the machine-instruction table, and emission through isa-lang.
//!
//! The register file is the sixteen general-purpose registers (indices
//! 0-15, in hardware-number order, so a register's index is its encoding),
//! the sixteen vector registers `xmm0`-`xmm15` (16-31), and the flags
//! (32). Instructions are in [`op`]; each is one machine operation at one
//! width, with its implicit register operands spelled out (see
//! [`InstDesc`](crate::target::InstDesc)).
//!
//! Vector registers are described so that the calling conventions are
//! complete (SysV passes floats in `xmm0`-`xmm7`; Win64 preserves
//! `xmm6`-`xmm15`), but isa-lang 0.2 encodes no SSE instructions, so
//! emitting a vector move, or a Win64 prologue that must save a vector
//! register, reports [`EmitError::Unencodable`]. That lifts with isa-lang's
//! floating-point forms (isa-lang 0.5).

mod emit;
mod table;

pub use emit::{EmitError, Emitter, FunctionInfo, Program};
#[cfg(test)]
use table::ENCODINGS;
pub use table::op;

use crate::mir::CondCode;
use crate::regs::{PReg, RegClass, RegSet};
use crate::target::abi::{Abi, RegSeq, StackRules};
use crate::target::{ClassInfo, ImmRange, Isa, MoveCaps, RegInfo, Target, TargetConfig};

macro_rules! pregs {
    ($($name:ident = $idx:expr, $text:literal;)*) => {
        $(
            #[doc = concat!("The `", $text, "` register.")]
            pub const $name: PReg = PReg::from_index($idx);
        )*
    };
}

pregs! {
    RAX = 0, "rax"; RCX = 1, "rcx"; RDX = 2, "rdx"; RBX = 3, "rbx";
    RSP = 4, "rsp"; RBP = 5, "rbp"; RSI = 6, "rsi"; RDI = 7, "rdi";
    R8 = 8, "r8"; R9 = 9, "r9"; R10 = 10, "r10"; R11 = 11, "r11";
    R12 = 12, "r12"; R13 = 13, "r13"; R14 = 14, "r14"; R15 = 15, "r15";
    XMM0 = 16, "xmm0"; XMM1 = 17, "xmm1"; XMM2 = 18, "xmm2"; XMM3 = 19, "xmm3";
    XMM4 = 20, "xmm4"; XMM5 = 21, "xmm5"; XMM6 = 22, "xmm6"; XMM7 = 23, "xmm7";
    XMM8 = 24, "xmm8"; XMM9 = 25, "xmm9"; XMM10 = 26, "xmm10"; XMM11 = 27, "xmm11";
    XMM12 = 28, "xmm12"; XMM13 = 29, "xmm13"; XMM14 = 30, "xmm14"; XMM15 = 31, "xmm15";
}

/// The condition-flags register (`rflags`), as a physical register: flag
/// producers define it and flag consumers use it.
pub const FLAGS: PReg = PReg::from_index(32);

/// The general-purpose class: `rax`-`r15`, 64-bit values. Operations of
/// smaller widths use the low bits; 32-bit operations zero the upper half.
pub const GPR: RegClass = RegClass::new(0);
/// The vector class: `xmm0`-`xmm15`, for floating-point values.
pub const VEC: RegClass = RegClass::new(1);
/// The flags class: only [`FLAGS`], never allocated.
pub const FLAGS_CLASS: RegClass = RegClass::new(2);

static REGS: [RegInfo; 33] = [
    RegInfo::new("rax", GPR, 0),
    RegInfo::new("rcx", GPR, 1),
    RegInfo::new("rdx", GPR, 2),
    RegInfo::new("rbx", GPR, 3),
    RegInfo::new("rsp", GPR, 4),
    RegInfo::new("rbp", GPR, 5),
    RegInfo::new("rsi", GPR, 6),
    RegInfo::new("rdi", GPR, 7),
    RegInfo::new("r8", GPR, 8),
    RegInfo::new("r9", GPR, 9),
    RegInfo::new("r10", GPR, 10),
    RegInfo::new("r11", GPR, 11),
    RegInfo::new("r12", GPR, 12),
    RegInfo::new("r13", GPR, 13),
    RegInfo::new("r14", GPR, 14),
    RegInfo::new("r15", GPR, 15),
    RegInfo::new("xmm0", VEC, 0),
    RegInfo::new("xmm1", VEC, 1),
    RegInfo::new("xmm2", VEC, 2),
    RegInfo::new("xmm3", VEC, 3),
    RegInfo::new("xmm4", VEC, 4),
    RegInfo::new("xmm5", VEC, 5),
    RegInfo::new("xmm6", VEC, 6),
    RegInfo::new("xmm7", VEC, 7),
    RegInfo::new("xmm8", VEC, 8),
    RegInfo::new("xmm9", VEC, 9),
    RegInfo::new("xmm10", VEC, 10),
    RegInfo::new("xmm11", VEC, 11),
    RegInfo::new("xmm12", VEC, 12),
    RegInfo::new("xmm13", VEC, 13),
    RegInfo::new("xmm14", VEC, 14),
    RegInfo::new("xmm15", VEC, 15),
    RegInfo::new("flags", FLAGS_CLASS, 0),
];

const GPRS: RegSet = RegSet::of(&[
    RAX, RCX, RDX, RBX, RSP, RBP, RSI, RDI, R8, R9, R10, R11, R12, R13, R14, R15,
]);
const XMMS: RegSet = RegSet::of(&[
    XMM0, XMM1, XMM2, XMM3, XMM4, XMM5, XMM6, XMM7, XMM8, XMM9, XMM10, XMM11, XMM12, XMM13, XMM14,
    XMM15,
]);

static CLASSES: [ClassInfo; 3] = [
    ClassInfo::new("gpr", GPRS, GPRS.without(RSP).without(RBP), 8, 8),
    ClassInfo::new("vec", XMMS, XMMS, 16, 16),
    ClassInfo::new("flags", RegSet::of(&[FLAGS]), RegSet::EMPTY, 8, 8),
];

/// Condition-code names, by the `cc` field of `Jcc`/`SETcc`/`CMOVcc`.
static CONDS: [&str; 16] = [
    "o", "no", "b", "ae", "e", "ne", "be", "a", "s", "ns", "p", "np", "l", "ge", "le", "g",
];

/// The x86-64 target.
///
/// # Examples
///
/// ```
/// use backend_lang::x86_64::{self, op, GPR};
///
/// let t = &x86_64::TARGET;
/// assert_eq!(t.class(GPR).regs().len(), 16);
/// assert_eq!(t.inst(op::SHL64CL).unwrap().implicit_uses(), [x86_64::RCX, x86_64::FLAGS]);
/// ```
pub static TARGET: Target = Target::new(
    "x86-64",
    Isa::X86_64,
    &REGS,
    &CLASSES,
    &table::DESCS,
    TargetConfig {
        stack_pointer: RSP,
        frame_pointer: RBP,
        flags: Some(FLAGS),
        addr_class: GPR,
        // `xchg reg, reg` swaps; no memory-to-memory `mov`; stores take a
        // sign-extended 32-bit immediate.
        move_caps: MoveCaps::new(true, false, ImmRange::I32),
        max_mem_operands: 1,
        conds: &CONDS,
    },
);

/// The System V AMD64 calling convention (Linux, macOS, the BSDs).
///
/// Integer arguments in `rdi rsi rdx rcx r8 r9`, floats in `xmm0`-`xmm7`,
/// each sequence counted separately, the rest on the stack in 8-byte slots;
/// integer results in `rax rdx`, floats in `xmm0 xmm1`; `rbx rbp r12`-`r15`
/// preserved; the stack 16-aligned at calls; a 128-byte red zone.
///
/// # Examples
///
/// ```
/// use backend_lang::x86_64::{SYSV, RDI};
///
/// assert_eq!(SYSV.args().int()[0], RDI);
/// assert_eq!(SYSV.stack().red_zone(), 128);
/// ```
pub static SYSV: Abi = Abi::new(
    "sysv64",
    &TARGET,
    RegSeq::new(
        &[RDI, RSI, RDX, RCX, R8, R9],
        &[XMM0, XMM1, XMM2, XMM3, XMM4, XMM5, XMM6, XMM7],
        false,
    ),
    RegSeq::new(&[RAX, RDX], &[XMM0, XMM1], false),
    RegSet::of(&[RBX, RBP, R12, R13, R14, R15]),
    // Probing at a page is stack-clash protection here (GCC's
    // -fstack-clash-protection, on by default in major Linux distributions).
    StackRules::new(16, 8, 0, 128, 4096),
);

/// The Microsoft x64 calling convention (Windows).
///
/// The first four arguments by position in `rcx rdx r8 r9` or `xmm0`-`xmm3`,
/// the rest on the stack above 32 bytes of shadow space every caller
/// reserves; one result in `rax` or `xmm0`; `rbx rbp rdi rsi r12`-`r15` and
/// `xmm6`-`xmm15` preserved; the stack 16-aligned at calls; no red zone;
/// frames of a page or more probed.
///
/// # Examples
///
/// ```
/// use backend_lang::x86_64::{WIN64, RCX};
///
/// assert_eq!(WIN64.args().int()[0], RCX);
/// assert_eq!(WIN64.stack().shadow_space(), 32);
/// ```
pub static WIN64: Abi = Abi::new(
    "win64",
    &TARGET,
    RegSeq::new(&[RCX, RDX, R8, R9], &[XMM0, XMM1, XMM2, XMM3], true),
    RegSeq::new(&[RAX], &[XMM0], true),
    RegSet::of(&[
        RBX, RBP, RDI, RSI, R12, R13, R14, R15, XMM6, XMM7, XMM8, XMM9, XMM10, XMM11, XMM12, XMM13,
        XMM14, XMM15,
    ]),
    StackRules::new(16, 8, 32, 0, 4096),
);

/// The MIR condition code for an isa-lang condition.
///
/// # Examples
///
/// ```
/// use backend_lang::x86_64;
/// use isa_lang::x86_64::Cond;
///
/// assert_eq!(x86_64::cond(Cond::E).code(), 4);
/// assert_eq!(x86_64::TARGET.cond_name(x86_64::cond(Cond::Ge)), "ge");
/// ```
#[inline]
#[must_use]
pub const fn cond(c: isa_lang::x86_64::Cond) -> CondCode {
    CondCode::new(c.code())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regs::MAX_PREGS;
    use crate::target::{InstKind, Opcode};

    #[test]
    fn test_register_file_is_consistent() {
        assert!(TARGET.regs().len() <= MAX_PREGS);
        for (i, r) in TARGET.regs().iter().enumerate() {
            let p = PReg::from_index(i as u8);
            assert!(TARGET.has_class(r.class()));
            assert!(
                TARGET.class(r.class()).regs().contains(p),
                "{} in its class",
                r.name()
            );
            if r.class() != FLAGS_CLASS {
                assert_eq!(usize::from(r.hw()), i % 16);
            }
        }
        // Classes partition the register file.
        let mut all = RegSet::EMPTY;
        for c in TARGET.classes() {
            assert!(all.intersect(c.regs()).is_empty());
            all = all.union(c.regs());
            assert_eq!(c.allocatable().difference(c.regs()), RegSet::EMPTY);
        }
        assert_eq!(all.len(), TARGET.regs().len());
    }

    #[test]
    fn test_inst_table_starts_with_the_generic_pair_and_names_are_unique() {
        let insts = TARGET.insts();
        assert_eq!(insts[0].kind(), InstKind::Copy);
        assert_eq!(insts[1].kind(), InstKind::ParallelCopy);
        assert_eq!(insts.len(), ENCODINGS.len());
        for (i, d) in insts.iter().enumerate() {
            assert_eq!(
                TARGET.opcode(d.name()),
                Some(Opcode::new(i as u16)),
                "{}",
                d.name()
            );
        }
    }

    #[test]
    fn test_abis_belong_to_the_target() {
        for abi in [&SYSV, &WIN64] {
            assert!(core::ptr::eq(abi.target(), &TARGET));
            for r in abi.args().int().iter().chain(abi.rets().int()) {
                assert_eq!(TARGET.class_of(*r), Some(GPR));
            }
            for r in abi.args().float().iter().chain(abi.rets().float()) {
                assert_eq!(TARGET.class_of(*r), Some(VEC));
            }
        }
    }
}
