//! The x86-64 machine-instruction table: one row per MIR opcode, with its
//! operand rules, implicit operands, and the isa-lang instruction it encodes
//! as.
//!
//! Each row is one operation at one width. Operands are listed definitions
//! first; a two-address instruction lists its tied use right after the
//! definition (`%d = add64 %a, src` encodes `add d, src` with `d == a` after
//! register allocation). The encoding column names the isa-lang mnemonic and
//! which MIR operand goes in each isa-lang position at which width; isa-lang
//! then picks the shortest form (`add r/m64, imm8` before `imm32`), and the
//! test suite checks that every row encodes for every operand shape it
//! accepts and that its flag effects agree with isa-lang's.
//!
//! Flags are the physical register [`FLAGS`]. An instruction defines it when
//! it writes or leaves undefined any flag; it also uses it when it reads a
//! flag or may leave some flags unchanged (`inc`/`dec` keep the carry; a
//! shift by zero changes nothing), so that a flag value never appears to come
//! from an instruction that might not have produced it.
//!
//! `push` and `pop` are not MIR instructions: the stack pointer is fixed
//! between prologue and epilogue so that stack slots have fixed offsets.
//! 8- and 16-bit arithmetic is done in 32-bit registers by instruction
//! selection; the string instructions, `lock`-prefixed read-modify-writes,
//! `cmpxchg`, and `mfence` wait on isa-lang forms (ISSUES P11).

use isa_lang::x86_64::{Mnemonic as M, Reg as IReg, Size};

use super::{FLAGS, GPR, R11, RAX, RCX, RDX};
use crate::regs::{PReg, RegSet};
use crate::target::{Accept, ImmRange, InstDesc, InstKind, OperandRule, Target};

/// Where an isa-lang operand comes from.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Src {
    /// MIR operand `n`, at this width (registers are named at the width;
    /// memory operands get it as their access size).
    Op(u8, Size),
    /// A fixed register the form names (`cl` for shifts).
    Fixed(IReg),
}

/// The isa-lang mnemonic, or a condition family whose condition comes from a
/// MIR operand.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Fam {
    Plain(M),
    Setcc,
    Cmovcc,
}

/// How a MIR instruction is emitted.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Enc {
    /// Through the parallel-move resolver.
    Copy,
    /// Through the parallel-move resolver.
    Pcopy,
    /// One isa-lang instruction. `cond` is the index of the `Cond` operand
    /// for the condition families.
    Isa {
        fam: Fam,
        ops: &'static [Src],
        cond: u8,
    },
    /// `mov r64, imm`, shortened to `mov r32, imm32` below 2^32.
    MovImm64,
    /// Edge moves, then `jmp` (or fall through).
    Jmp,
    /// `jcc`, edge moves, `jmp` (see the emitter).
    Jcc,
    /// `call label` for a symbol, `call r/m64` otherwise.
    Call,
    /// The epilogue, then `ret`.
    Ret,
    /// The epilogue, then `jmp` to a symbol or register.
    TailCall,
    /// `lea t, [rip + table]; add t, [t + i*8]; jmp t`, with the table of
    /// target offsets placed after the function.
    JumpTable,
    /// Bytes isa-lang has no form for.
    Raw(&'static [u8]),
}

const NO_COND: u8 = u8::MAX;
const BY: Size = Size::Byte;
const WD: Size = Size::Word;
const DW: Size = Size::Dword;
const QW: Size = Size::Qword;

const fn op(index: u8, size: Size) -> Src {
    Src::Op(index, size)
}

const fn isa(m: M, ops: &'static [Src]) -> Enc {
    Enc::Isa {
        fam: Fam::Plain(m),
        ops,
        cond: NO_COND,
    }
}

const fn plain(
    name: &'static str,
    ops: &'static [OperandRule],
    uses: &'static [PReg],
    defs: &'static [PReg],
) -> InstDesc {
    InstDesc::new(name, InstKind::Plain, ops, uses, defs, RegSet::EMPTY)
}

const D: OperandRule = OperandRule::Def(GPR);
const T0: OperandRule = OperandRule::Tied(0);
const RM: OperandRule = OperandRule::Use(GPR, Accept::RM);
const RMI32: OperandRule = OperandRule::Use(GPR, Accept::RM.with_imm(ImmRange::I32));
const RI8: OperandRule = OperandRule::Use(GPR, Accept::REG.with_imm(ImmRange::I8));
const RI16: OperandRule = OperandRule::Use(GPR, Accept::REG.with_imm(ImmRange::I16));
const RI32: OperandRule = OperandRule::Use(GPR, Accept::REG.with_imm(ImmRange::I32));
const RU8: OperandRule = OperandRule::Use(GPR, Accept::REG.with_imm(ImmRange::U8));
const MEM: OperandRule = OperandRule::Use(GPR, Accept::MEM);
const MEM_SYM: OperandRule = OperandRule::Use(GPR, Accept::MEM.with_symbol());
const RM_SYM: OperandRule = OperandRule::Use(GPR, Accept::RM.with_symbol());
const REG: OperandRule = OperandRule::Use(GPR, Accept::REG);
const REG_SYM: OperandRule = OperandRule::Use(GPR, Accept::REG.with_symbol());
const IMM32: OperandRule = OperandRule::Use(GPR, Accept::NONE.with_imm(ImmRange::I32));
const IMM64: OperandRule = OperandRule::Use(GPR, Accept::NONE.with_imm(ImmRange::I64));
const COUNT: OperandRule = OperandRule::Use(GPR, Accept::NONE.with_imm(ImmRange::U8));
const COND: OperandRule = OperandRule::Cond;
const BLOCK: OperandRule = OperandRule::Block;

const NO: &[PReg] = &[];
const F: &[PReg] = &[FLAGS];
const RAX_: &[PReg] = &[RAX];
const RDX_: &[PReg] = &[RDX];
const RAX_RDX: &[PReg] = &[RAX, RDX];
const RAX_RDX_F: &[PReg] = &[RAX, RDX, FLAGS];
const RCX_F: &[PReg] = &[RCX, FLAGS];

macro_rules! table {
    ($($id:ident: $doc:literal => $desc:expr, $enc:expr;)*) => {
        #[allow(non_camel_case_types, clippy::upper_case_acronyms, dead_code)]
        #[repr(u16)]
        enum Id {
            Copy_,
            Pcopy_,
            $($id,)*
        }

        /// The x86-64 MIR opcodes, one constant per row of the instruction
        /// table. Each constant's documentation shows its MIR form.
        ///
        /// # Examples
        ///
        /// ```
        /// use backend_lang::x86_64::{self, op};
        ///
        /// let add = x86_64::TARGET.inst(op::ADD64).unwrap();
        /// assert_eq!(add.name(), "add64");
        /// ```
        pub mod op {
            use crate::target::Opcode;

            /// `%d = copy src`: the generic copy ([`Opcode::COPY`]).
            pub const COPY: Opcode = Opcode::COPY;
            /// `%d0, %d1 = pcopy s0, s1`: the generic parallel copy
            /// ([`Opcode::PCOPY`]).
            pub const PCOPY: Opcode = Opcode::PCOPY;
            $(
                #[doc = $doc]
                pub const $id: Opcode = Opcode::new(super::Id::$id as u16);
            )*
        }

        const COUNT_ROWS: usize = 2 + [$(stringify!($id)),*].len();

        const GENERIC: [InstDesc; 2] = Target::generic_insts();

        pub(crate) static DESCS: [InstDesc; COUNT_ROWS] = [GENERIC[0], GENERIC[1], $($desc,)*];

        pub(crate) static ENCODINGS: [Enc; COUNT_ROWS] = [Enc::Copy, Enc::Pcopy, $($enc,)*];
    };
}

table! {
    MOV32RI: "`%d = mov32ri imm`: load a 32-bit immediate, zero-extended to 64 bits."
        => plain("mov32ri", &[D, IMM32], NO, NO),
        isa(M::Mov, &[op(0, DW), op(1, DW)]);
    MOV64RI: "`%d = mov64ri imm`: load any 64-bit immediate (encoded as `mov r32, imm32` when it is below 2^32, else the shortest 64-bit form)."
        => plain("mov64ri", &[D, IMM64], NO, NO),
        Enc::MovImm64;
    MOV32: "`%d = mov32 src`: copy or load 32 bits, zero-extended to 64."
        => plain("mov32", &[D, RM], NO, NO),
        isa(M::Mov, &[op(0, DW), op(1, DW)]);
    MOV64: "`%d = mov64 src`: copy or load 64 bits."
        => plain("mov64", &[D, RM], NO, NO),
        isa(M::Mov, &[op(0, QW), op(1, QW)]);
    MOVZX8: "`%d = movzx8 src`: a byte (the low byte of a register, or memory), zero-extended to 64 bits."
        => plain("movzx8", &[D, RM], NO, NO),
        isa(M::Movzx, &[op(0, DW), op(1, BY)]);
    MOVZX16: "`%d = movzx16 src`: 16 bits, zero-extended to 64."
        => plain("movzx16", &[D, RM], NO, NO),
        isa(M::Movzx, &[op(0, DW), op(1, WD)]);
    MOVSX8TO32: "`%d = movsx8to32 src`: a byte sign-extended to 32 bits (upper 32 zeroed)."
        => plain("movsx8to32", &[D, RM], NO, NO),
        isa(M::Movsx, &[op(0, DW), op(1, BY)]);
    MOVSX8TO64: "`%d = movsx8to64 src`: a byte sign-extended to 64 bits."
        => plain("movsx8to64", &[D, RM], NO, NO),
        isa(M::Movsx, &[op(0, QW), op(1, BY)]);
    MOVSX16TO32: "`%d = movsx16to32 src`: 16 bits sign-extended to 32 (upper 32 zeroed)."
        => plain("movsx16to32", &[D, RM], NO, NO),
        isa(M::Movsx, &[op(0, DW), op(1, WD)]);
    MOVSX16TO64: "`%d = movsx16to64 src`: 16 bits sign-extended to 64."
        => plain("movsx16to64", &[D, RM], NO, NO),
        isa(M::Movsx, &[op(0, QW), op(1, WD)]);
    MOVSX32TO64: "`%d = movsx32to64 src`: 32 bits sign-extended to 64."
        => plain("movsx32to64", &[D, RM], NO, NO),
        isa(M::Movsxd, &[op(0, QW), op(1, DW)]);
    STORE8: "`store8 mem, src`: store the low 8 bits of a register, or an 8-bit immediate."
        => plain("store8", &[MEM, RI8], NO, NO),
        isa(M::Mov, &[op(0, BY), op(1, BY)]);
    STORE16: "`store16 mem, src`: store the low 16 bits of a register, or a 16-bit immediate."
        => plain("store16", &[MEM, RI16], NO, NO),
        isa(M::Mov, &[op(0, WD), op(1, WD)]);
    STORE32: "`store32 mem, src`: store the low 32 bits of a register, or a 32-bit immediate."
        => plain("store32", &[MEM, RI32], NO, NO),
        isa(M::Mov, &[op(0, DW), op(1, DW)]);
    STORE64: "`store64 mem, src`: store the low 64 bits of a register, or a sign-extended 32-bit immediate."
        => plain("store64", &[MEM, RI32], NO, NO),
        isa(M::Mov, &[op(0, QW), op(1, QW)]);
    LEA32: "`%d = lea32 mem`: the low 32 bits of an address, zero-extended."
        => plain("lea32", &[D, MEM], NO, NO),
        isa(M::Lea, &[op(0, DW), op(1, DW)]);
    LEA64: "`%d = lea64 mem`: an address; with a symbol operand, the address of that function (RIP-relative)."
        => plain("lea64", &[D, MEM_SYM], NO, NO),
        isa(M::Lea, &[op(0, QW), op(1, QW)]);
    ADD32: "`%d = add32 %a, src`: 32-bit add; `%a` is tied to `%d`; `src` is a register, memory, or a 32-bit immediate."
        => plain("add32", &[D, T0, RMI32], NO, F),
        isa(M::Add, &[op(0, DW), op(2, DW)]);
    ADD64: "`%d = add64 %a, src`: 64-bit add; `%a` is tied to `%d`; `src` is a register, memory, or a 32-bit immediate."
        => plain("add64", &[D, T0, RMI32], NO, F),
        isa(M::Add, &[op(0, QW), op(2, QW)]);
    SUB32: "`%d = sub32 %a, src`: 32-bit subtract; `%a` is tied to `%d`; `src` is a register, memory, or a 32-bit immediate."
        => plain("sub32", &[D, T0, RMI32], NO, F),
        isa(M::Sub, &[op(0, DW), op(2, DW)]);
    SUB64: "`%d = sub64 %a, src`: 64-bit subtract; `%a` is tied to `%d`; `src` is a register, memory, or a 32-bit immediate."
        => plain("sub64", &[D, T0, RMI32], NO, F),
        isa(M::Sub, &[op(0, QW), op(2, QW)]);
    AND32: "`%d = and32 %a, src`: 32-bit bitwise and; `%a` is tied to `%d`; `src` is a register, memory, or a 32-bit immediate."
        => plain("and32", &[D, T0, RMI32], NO, F),
        isa(M::And, &[op(0, DW), op(2, DW)]);
    AND64: "`%d = and64 %a, src`: 64-bit bitwise and; `%a` is tied to `%d`; `src` is a register, memory, or a 32-bit immediate."
        => plain("and64", &[D, T0, RMI32], NO, F),
        isa(M::And, &[op(0, QW), op(2, QW)]);
    OR32: "`%d = or32 %a, src`: 32-bit bitwise or; `%a` is tied to `%d`; `src` is a register, memory, or a 32-bit immediate."
        => plain("or32", &[D, T0, RMI32], NO, F),
        isa(M::Or, &[op(0, DW), op(2, DW)]);
    OR64: "`%d = or64 %a, src`: 64-bit bitwise or; `%a` is tied to `%d`; `src` is a register, memory, or a 32-bit immediate."
        => plain("or64", &[D, T0, RMI32], NO, F),
        isa(M::Or, &[op(0, QW), op(2, QW)]);
    XOR32: "`%d = xor32 %a, src`: 32-bit bitwise xor; `%a` is tied to `%d`; `src` is a register, memory, or a 32-bit immediate."
        => plain("xor32", &[D, T0, RMI32], NO, F),
        isa(M::Xor, &[op(0, DW), op(2, DW)]);
    XOR64: "`%d = xor64 %a, src`: 64-bit bitwise xor; `%a` is tied to `%d`; `src` is a register, memory, or a 32-bit immediate."
        => plain("xor64", &[D, T0, RMI32], NO, F),
        isa(M::Xor, &[op(0, QW), op(2, QW)]);
    ADC32: "`%d = adc32 %a, src`: 32-bit add with carry; `%a` is tied to `%d`; `src` is a register, memory, or a 32-bit immediate."
        => plain("adc32", &[D, T0, RMI32], F, F),
        isa(M::Adc, &[op(0, DW), op(2, DW)]);
    ADC64: "`%d = adc64 %a, src`: 64-bit add with carry; `%a` is tied to `%d`; `src` is a register, memory, or a 32-bit immediate."
        => plain("adc64", &[D, T0, RMI32], F, F),
        isa(M::Adc, &[op(0, QW), op(2, QW)]);
    SBB32: "`%d = sbb32 %a, src`: 32-bit subtract with borrow; `%a` is tied to `%d`; `src` is a register, memory, or a 32-bit immediate."
        => plain("sbb32", &[D, T0, RMI32], F, F),
        isa(M::Sbb, &[op(0, DW), op(2, DW)]);
    SBB64: "`%d = sbb64 %a, src`: 64-bit subtract with borrow; `%a` is tied to `%d`; `src` is a register, memory, or a 32-bit immediate."
        => plain("sbb64", &[D, T0, RMI32], F, F),
        isa(M::Sbb, &[op(0, QW), op(2, QW)]);
    CMP32: "`cmp32 a, b`: 32-bit compare (`a - b`), setting the flags."
        => plain("cmp32", &[RM, RMI32], NO, F),
        isa(M::Cmp, &[op(0, DW), op(1, DW)]);
    CMP64: "`cmp64 a, b`: 64-bit compare (`a - b`), setting the flags."
        => plain("cmp64", &[RM, RMI32], NO, F),
        isa(M::Cmp, &[op(0, QW), op(1, QW)]);
    TEST32: "`test32 a, b`: 32-bit `a & b`, setting the flags."
        => plain("test32", &[RM, RI32], NO, F),
        isa(M::Test, &[op(0, DW), op(1, DW)]);
    TEST64: "`test64 a, b`: 64-bit `a & b`, setting the flags."
        => plain("test64", &[RM, RI32], NO, F),
        isa(M::Test, &[op(0, QW), op(1, QW)]);
    BT32: "`bt32 a, bit`: copy bit `bit` of `a` into the carry flag (other flags partly unchanged, so the flags are modelled as read)."
        => plain("bt32", &[RM, RU8], F, F),
        isa(M::Bt, &[op(0, DW), op(1, DW)]);
    BT64: "`bt64 a, bit`: copy bit `bit` of `a` into the carry flag (other flags partly unchanged, so the flags are modelled as read)."
        => plain("bt64", &[RM, RU8], F, F),
        isa(M::Bt, &[op(0, QW), op(1, QW)]);
    INC32: "`%d = inc32 %a`: 32-bit increment (the carry flag is left unchanged, so the flags are modelled as read); `%a` is tied to `%d`."
        => plain("inc32", &[D, T0], F, F),
        isa(M::Inc, &[op(0, DW)]);
    INC64: "`%d = inc64 %a`: 64-bit increment (the carry flag is left unchanged, so the flags are modelled as read); `%a` is tied to `%d`."
        => plain("inc64", &[D, T0], F, F),
        isa(M::Inc, &[op(0, QW)]);
    DEC32: "`%d = dec32 %a`: 32-bit decrement (the carry flag is left unchanged, so the flags are modelled as read); `%a` is tied to `%d`."
        => plain("dec32", &[D, T0], F, F),
        isa(M::Dec, &[op(0, DW)]);
    DEC64: "`%d = dec64 %a`: 64-bit decrement (the carry flag is left unchanged, so the flags are modelled as read); `%a` is tied to `%d`."
        => plain("dec64", &[D, T0], F, F),
        isa(M::Dec, &[op(0, QW)]);
    NEG32: "`%d = neg32 %a`: 32-bit two's-complement negation; `%a` is tied to `%d`."
        => plain("neg32", &[D, T0], NO, F),
        isa(M::Neg, &[op(0, DW)]);
    NEG64: "`%d = neg64 %a`: 64-bit two's-complement negation; `%a` is tied to `%d`."
        => plain("neg64", &[D, T0], NO, F),
        isa(M::Neg, &[op(0, QW)]);
    NOT32: "`%d = not32 %a`: 32-bit bitwise complement (flags unchanged); `%a` is tied to `%d`."
        => plain("not32", &[D, T0], NO, NO),
        isa(M::Not, &[op(0, DW)]);
    NOT64: "`%d = not64 %a`: 64-bit bitwise complement (flags unchanged); `%a` is tied to `%d`."
        => plain("not64", &[D, T0], NO, NO),
        isa(M::Not, &[op(0, QW)]);
    IMUL32: "`%d = imul32 %a, src`: 32-bit multiply (low half); `%a` is tied to `%d`."
        => plain("imul32", &[D, T0, RM], NO, F),
        isa(M::Imul, &[op(0, DW), op(2, DW)]);
    IMUL64: "`%d = imul64 %a, src`: 64-bit multiply (low half); `%a` is tied to `%d`."
        => plain("imul64", &[D, T0, RM], NO, F),
        isa(M::Imul, &[op(0, QW), op(2, QW)]);
    IMUL32RI: "`%d = imul32ri src, imm`: 32-bit multiply by a 32-bit immediate (three operands, no tie)."
        => plain("imul32ri", &[D, RM, IMM32], NO, F),
        isa(M::Imul, &[op(0, DW), op(1, DW), op(2, DW)]);
    IMUL64RI: "`%d = imul64ri src, imm`: 64-bit multiply by a 32-bit immediate (three operands, no tie)."
        => plain("imul64ri", &[D, RM, IMM32], NO, F),
        isa(M::Imul, &[op(0, QW), op(1, QW), op(2, QW)]);
    MUL32: "`mul32 src`: 32-bit unsigned widening multiply: `edx:eax = eax * src`. The `rax`/`rdx` operands are implicit."
        => plain("mul32", &[RM], RAX_, RAX_RDX_F),
        isa(M::Mul, &[op(0, DW)]);
    MUL64: "`mul64 src`: 64-bit unsigned widening multiply: `rdx:rax = rax * src`. The `rax`/`rdx` operands are implicit."
        => plain("mul64", &[RM], RAX_, RAX_RDX_F),
        isa(M::Mul, &[op(0, QW)]);
    IMULW32: "`imulw32 src`: 32-bit signed widening multiply: `edx:eax = eax * src`. The `rax`/`rdx` operands are implicit."
        => plain("imulw32", &[RM], RAX_, RAX_RDX_F),
        isa(M::Imul, &[op(0, DW)]);
    IMULW64: "`imulw64 src`: 64-bit signed widening multiply: `rdx:rax = rax * src`. The `rax`/`rdx` operands are implicit."
        => plain("imulw64", &[RM], RAX_, RAX_RDX_F),
        isa(M::Imul, &[op(0, QW)]);
    DIV32: "`div32 src`: 32-bit unsigned divide of `edx:eax` by `src`: quotient in `eax`, remainder in `edx`. The `rax`/`rdx` operands are implicit."
        => plain("div32", &[RM], RAX_RDX, RAX_RDX_F),
        isa(M::Div, &[op(0, DW)]);
    DIV64: "`div64 src`: 64-bit unsigned divide of `rdx:rax` by `src`: quotient in `rax`, remainder in `rdx`. The `rax`/`rdx` operands are implicit."
        => plain("div64", &[RM], RAX_RDX, RAX_RDX_F),
        isa(M::Div, &[op(0, QW)]);
    IDIV32: "`idiv32 src`: 32-bit signed divide of `edx:eax` by `src`: quotient in `eax`, remainder in `edx`. The `rax`/`rdx` operands are implicit."
        => plain("idiv32", &[RM], RAX_RDX, RAX_RDX_F),
        isa(M::Idiv, &[op(0, DW)]);
    IDIV64: "`idiv64 src`: 64-bit signed divide of `rdx:rax` by `src`: quotient in `rax`, remainder in `rdx`. The `rax`/`rdx` operands are implicit."
        => plain("idiv64", &[RM], RAX_RDX, RAX_RDX_F),
        isa(M::Idiv, &[op(0, QW)]);
    CDQ: "`cdq`: sign-extend `eax` into `edx` (before a 32-bit `idiv`)."
        => plain("cdq", &[], RAX_, RDX_),
        isa(M::Cdq, &[]);
    CQO: "`cqo`: sign-extend `rax` into `rdx` (before a 64-bit `idiv`)."
        => plain("cqo", &[], RAX_, RDX_),
        isa(M::Cqo, &[]);
    CDQE: "`cdqe`: sign-extend `eax` to `rax`."
        => plain("cdqe", &[], RAX_, RAX_),
        isa(M::Cdqe, &[]);
    SHL32: "`%d = shl32 %a, count`: 32-bit shift left by an immediate (the processor masks the count); `%a` is tied to `%d`. A zero count leaves the flags unchanged, so the flags are modelled as read."
        => plain("shl32", &[D, T0, COUNT], F, F),
        isa(M::Shl, &[op(0, DW), op(2, DW)]);
    SHL32CL: "`%d = shl32cl %a`: 32-bit shift left by `cl` (an implicit use of `rcx`); `%a` is tied to `%d`."
        => plain("shl32cl", &[D, T0], RCX_F, F),
        isa(M::Shl, &[op(0, DW), Src::Fixed(IReg::CL)]);
    SHL64: "`%d = shl64 %a, count`: 64-bit shift left by an immediate (the processor masks the count); `%a` is tied to `%d`. A zero count leaves the flags unchanged, so the flags are modelled as read."
        => plain("shl64", &[D, T0, COUNT], F, F),
        isa(M::Shl, &[op(0, QW), op(2, QW)]);
    SHL64CL: "`%d = shl64cl %a`: 64-bit shift left by `cl` (an implicit use of `rcx`); `%a` is tied to `%d`."
        => plain("shl64cl", &[D, T0], RCX_F, F),
        isa(M::Shl, &[op(0, QW), Src::Fixed(IReg::CL)]);
    SHR32: "`%d = shr32 %a, count`: 32-bit logical shift right by an immediate (the processor masks the count); `%a` is tied to `%d`. A zero count leaves the flags unchanged, so the flags are modelled as read."
        => plain("shr32", &[D, T0, COUNT], F, F),
        isa(M::Shr, &[op(0, DW), op(2, DW)]);
    SHR32CL: "`%d = shr32cl %a`: 32-bit logical shift right by `cl` (an implicit use of `rcx`); `%a` is tied to `%d`."
        => plain("shr32cl", &[D, T0], RCX_F, F),
        isa(M::Shr, &[op(0, DW), Src::Fixed(IReg::CL)]);
    SHR64: "`%d = shr64 %a, count`: 64-bit logical shift right by an immediate (the processor masks the count); `%a` is tied to `%d`. A zero count leaves the flags unchanged, so the flags are modelled as read."
        => plain("shr64", &[D, T0, COUNT], F, F),
        isa(M::Shr, &[op(0, QW), op(2, QW)]);
    SHR64CL: "`%d = shr64cl %a`: 64-bit logical shift right by `cl` (an implicit use of `rcx`); `%a` is tied to `%d`."
        => plain("shr64cl", &[D, T0], RCX_F, F),
        isa(M::Shr, &[op(0, QW), Src::Fixed(IReg::CL)]);
    SAR32: "`%d = sar32 %a, count`: 32-bit arithmetic shift right by an immediate (the processor masks the count); `%a` is tied to `%d`. A zero count leaves the flags unchanged, so the flags are modelled as read."
        => plain("sar32", &[D, T0, COUNT], F, F),
        isa(M::Sar, &[op(0, DW), op(2, DW)]);
    SAR32CL: "`%d = sar32cl %a`: 32-bit arithmetic shift right by `cl` (an implicit use of `rcx`); `%a` is tied to `%d`."
        => plain("sar32cl", &[D, T0], RCX_F, F),
        isa(M::Sar, &[op(0, DW), Src::Fixed(IReg::CL)]);
    SAR64: "`%d = sar64 %a, count`: 64-bit arithmetic shift right by an immediate (the processor masks the count); `%a` is tied to `%d`. A zero count leaves the flags unchanged, so the flags are modelled as read."
        => plain("sar64", &[D, T0, COUNT], F, F),
        isa(M::Sar, &[op(0, QW), op(2, QW)]);
    SAR64CL: "`%d = sar64cl %a`: 64-bit arithmetic shift right by `cl` (an implicit use of `rcx`); `%a` is tied to `%d`."
        => plain("sar64cl", &[D, T0], RCX_F, F),
        isa(M::Sar, &[op(0, QW), Src::Fixed(IReg::CL)]);
    ROL32: "`%d = rol32 %a, count`: 32-bit rotate left by an immediate (the processor masks the count); `%a` is tied to `%d`. A zero count leaves the flags unchanged, so the flags are modelled as read."
        => plain("rol32", &[D, T0, COUNT], F, F),
        isa(M::Rol, &[op(0, DW), op(2, DW)]);
    ROL32CL: "`%d = rol32cl %a`: 32-bit rotate left by `cl` (an implicit use of `rcx`); `%a` is tied to `%d`."
        => plain("rol32cl", &[D, T0], RCX_F, F),
        isa(M::Rol, &[op(0, DW), Src::Fixed(IReg::CL)]);
    ROL64: "`%d = rol64 %a, count`: 64-bit rotate left by an immediate (the processor masks the count); `%a` is tied to `%d`. A zero count leaves the flags unchanged, so the flags are modelled as read."
        => plain("rol64", &[D, T0, COUNT], F, F),
        isa(M::Rol, &[op(0, QW), op(2, QW)]);
    ROL64CL: "`%d = rol64cl %a`: 64-bit rotate left by `cl` (an implicit use of `rcx`); `%a` is tied to `%d`."
        => plain("rol64cl", &[D, T0], RCX_F, F),
        isa(M::Rol, &[op(0, QW), Src::Fixed(IReg::CL)]);
    ROR32: "`%d = ror32 %a, count`: 32-bit rotate right by an immediate (the processor masks the count); `%a` is tied to `%d`. A zero count leaves the flags unchanged, so the flags are modelled as read."
        => plain("ror32", &[D, T0, COUNT], F, F),
        isa(M::Ror, &[op(0, DW), op(2, DW)]);
    ROR32CL: "`%d = ror32cl %a`: 32-bit rotate right by `cl` (an implicit use of `rcx`); `%a` is tied to `%d`."
        => plain("ror32cl", &[D, T0], RCX_F, F),
        isa(M::Ror, &[op(0, DW), Src::Fixed(IReg::CL)]);
    ROR64: "`%d = ror64 %a, count`: 64-bit rotate right by an immediate (the processor masks the count); `%a` is tied to `%d`. A zero count leaves the flags unchanged, so the flags are modelled as read."
        => plain("ror64", &[D, T0, COUNT], F, F),
        isa(M::Ror, &[op(0, QW), op(2, QW)]);
    ROR64CL: "`%d = ror64cl %a`: 64-bit rotate right by `cl` (an implicit use of `rcx`); `%a` is tied to `%d`."
        => plain("ror64cl", &[D, T0], RCX_F, F),
        isa(M::Ror, &[op(0, QW), Src::Fixed(IReg::CL)]);
    SETCC: "`%d = setcc cc`: set the low byte of `%d` to 1 if `cc` holds, else 0; the upper bits are unspecified (zero-extend with `movzx8`)."
        => plain("setcc", &[D, COND], F, NO),
        Enc::Isa { fam: Fam::Setcc, ops: &[op(0, BY)], cond: 1 };
    CMOV32: "`%d = cmov32 %a, src, cc`: `%d = cc ? src : %a`, 32 bits; `%a` is tied to `%d`."
        => plain("cmov32", &[D, T0, RM, COND], F, NO),
        Enc::Isa { fam: Fam::Cmovcc, ops: &[op(0, DW), op(2, DW)], cond: 3 };
    CMOV64: "`%d = cmov64 %a, src, cc`: `%d = cc ? src : %a`, 64 bits; `%a` is tied to `%d`."
        => plain("cmov64", &[D, T0, RM, COND], F, NO),
        Enc::Isa { fam: Fam::Cmovcc, ops: &[op(0, QW), op(2, QW)], cond: 3 };
    JMP: "`jmp block(args)`: jump, passing arguments to the target's parameters."
        => InstDesc::new("jmp", InstKind::Jump, &[BLOCK], NO, NO, RegSet::EMPTY),
        Enc::Jmp;
    JCC: "`jcc cc, then(args), else(args)`: branch to `then` if `cc` holds, else to `else`."
        => InstDesc::new("jcc", InstKind::Branch, &[COND, BLOCK, BLOCK], F, NO, RegSet::EMPTY),
        Enc::Jcc;
    CALL: "`call target`: call a function by symbol, register, or memory. Argument, result, and clobbered registers are attached per call with `Effects`."
        => InstDesc::new("call", InstKind::Call, &[RM_SYM], NO, NO, RegSet::EMPTY),
        Enc::Call;
    RET: "`ret`: run the epilogue and return. Result registers are attached with `Effects`."
        => InstDesc::new("ret", InstKind::Return, &[], NO, NO, RegSet::EMPTY),
        Enc::Ret;
    UD2: "`ud2`: raise an invalid-opcode trap; never falls through."
        => InstDesc::new("ud2", InstKind::Trap, &[], NO, NO, RegSet::EMPTY),
        isa(M::Ud2, &[]);
    INT3: "`int3`: a debugger breakpoint."
        => plain("int3", &[], NO, NO),
        isa(M::Int3, &[]);
    NOP: "`nop`: no operation."
        => plain("nop", &[], NO, NO),
        isa(M::Nop, &[]);
    SYSCALL: "`syscall`: enter the kernel. Reads `rax` (and the argument registers, attached with `Effects`), writes the result to `rax`, clobbers `rcx`, `r11`, and the flags. Encoded as the bytes `0f 05`: isa-lang 0.2 has no `SYSCALL` form."
        => InstDesc::new("syscall", InstKind::Plain, &[], RAX_, RAX_, RegSet::of(&[RCX, R11, FLAGS])),
        Enc::Raw(&[0x0F, 0x05]);
    TAILCALL: "`tailcall target`: run the epilogue, then jump to a function by symbol or register; the callee returns to this function's caller. A register target must survive the epilogue (not a callee-saved register the frame restores). Argument registers are attached with `Effects`."
        => InstDesc::new("tailcall", InstKind::TailCall, &[REG_SYM], NO, NO, RegSet::EMPTY),
        Enc::TailCall;
    JMPTBL: "`%t = jmptbl %i, b0, b1, ...`: jump to block `b<i>` through a table of offsets placed after the function. `%i` must already be in range (zero-extended, below the target count); `%t` is a temporary the jump writes before it last reads `%i`. Targets take no arguments."
        => InstDesc::new("jmptbl", InstKind::Switch, &[D, REG], NO, F, RegSet::EMPTY),
        Enc::JumpTable;
    LANDINGPAD: "`landingpad`: the entry of a landing pad. Emits nothing; it defines `rax`, in which the unwinder delivers the exception payload, so the register allocator sees where the payload comes from."
        => InstDesc::new("landingpad", InstKind::Plain, &[], NO, RAX_, RegSet::EMPTY),
        Enc::Raw(&[]);
    XCHG8: "`%d = xchg8 mem, %v`: atomically exchange the byte at `mem` with the low byte of `%v`; `%d` receives the old byte (upper bits unchanged from `%v`). `%v` is tied to `%d`. A full memory barrier."
        => plain("xchg8", &[D, MEM, T0], NO, NO),
        isa(M::Xchg, &[op(1, BY), op(0, BY)]);
    XCHG16: "`%d = xchg16 mem, %v`: atomically exchange 16 bits at `mem` with `%v`; `%d` receives the old value. `%v` is tied to `%d`. A full memory barrier."
        => plain("xchg16", &[D, MEM, T0], NO, NO),
        isa(M::Xchg, &[op(1, WD), op(0, WD)]);
    XCHG32: "`%d = xchg32 mem, %v`: atomically exchange 32 bits at `mem` with `%v`; `%d` receives the old value, zero-extended. `%v` is tied to `%d`. A full memory barrier."
        => plain("xchg32", &[D, MEM, T0], NO, NO),
        isa(M::Xchg, &[op(1, DW), op(0, DW)]);
    XCHG64: "`%d = xchg64 mem, %v`: atomically exchange 64 bits at `mem` with `%v`; `%d` receives the old value. `%v` is tied to `%d`. A full memory barrier."
        => plain("xchg64", &[D, MEM, T0], NO, NO),
        isa(M::Xchg, &[op(1, QW), op(0, QW)]);
}
