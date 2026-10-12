//! Integer operations (OPS §3, §5) under every policy: arithmetic, division,
//! shifts, comparisons, and conversions.

use ir_lang::{
    BinaryOp, CmpOp, ConvOp, DivZero, OpError, Overflow, Policy, Shift, Type, UnaryOp, Value,
};
use isa_lang::x86_64::Cond;

use super::{Ext, Fail, IselError, Result, Sel};
use crate::mir::{Mem, MirError, Operand};
use crate::regs::Reg;
use crate::target::{ImmRange, Opcode};
use crate::x86_64::{RAX, RCX, RDX, op};

/// Whether a type is computed (or, for `f64`, moved) with 64-bit
/// instructions.
pub(crate) fn is64(ty: Type) -> bool {
    matches!(
        ty,
        Type::I64 | Type::U64 | Type::Ptr | Type::Ref | Type::F64
    )
}

/// The width in bits a type's value occupies (`bool` counts as 8). A float
/// is its bit pattern in a general-purpose register (see the module
/// documentation of [`isel`](crate::isel)).
pub(crate) fn bits(ty: Type) -> u32 {
    match ty {
        Type::Bool | Type::I8 | Type::U8 => 8,
        Type::I16 | Type::U16 => 16,
        Type::I32 | Type::U32 | Type::F32 => 32,
        _ => 64,
    }
}

fn pick(ty: Type, op32: Opcode, op64: Opcode) -> Opcode {
    if is64(ty) { op64 } else { op32 }
}

fn signed(ty: Type) -> bool {
    ty.is_signed_int()
}

/// Sign-extends the low `w` bits of `x`.
pub(crate) fn sext_bits(x: u64, w: u32) -> i64 {
    if w >= 64 {
        x as i64
    } else {
        let s = 64 - w;
        ((x << s) as i64) >> s
    }
}

/// Zero-extends the low `w` bits of `x`.
pub(crate) fn zext_bits(x: u64, w: u32) -> u64 {
    if w >= 64 { x } else { x & ((1u64 << w) - 1) }
}

impl Sel<'_> {
    /// `r` extended from `ty`'s width to (at least) 32 bits.
    pub(crate) fn ext32(&mut self, r: Reg, ty: Type, ext: Ext) -> Result<Reg> {
        let sign = match ext {
            Ext::Type => signed(ty),
            Ext::Zero => false,
            Ext::Sign => true,
        };
        let opcode = match (bits(ty), sign) {
            (8, false) if ty == Type::Bool => return Ok(r),
            (8, false) => op::MOVZX8,
            (8, true) => op::MOVSX8TO32,
            (16, false) => op::MOVZX16,
            (16, true) => op::MOVSX16TO32,
            _ => return Ok(r),
        };
        self.emit_def(opcode, &[r.into()])
    }

    /// `r` extended from `ty`'s width to 64 bits.
    pub(crate) fn ext64(&mut self, r: Reg, ty: Type, ext: Ext) -> Result<Reg> {
        let sign = match ext {
            Ext::Type => signed(ty),
            Ext::Zero => false,
            Ext::Sign => true,
        };
        let opcode = match (bits(ty), sign) {
            (8, false) if ty == Type::Bool => return Ok(r),
            (8, false) => op::MOVZX8,
            (8, true) => op::MOVSX8TO64,
            (16, false) => op::MOVZX16,
            (16, true) => op::MOVSX16TO64,
            (32, false) => op::MOV32,
            (32, true) => op::MOVSX32TO64,
            _ => return Ok(r),
        };
        self.emit_def(opcode, &[r.into()])
    }

    /// The constant `v` as an immediate for an operation of `ty`'s width,
    /// extended as `ext` says, if it fits `range`.
    fn imm(&self, v: Value, ty: Type, ext: Ext, range: ImmRange) -> Option<i64> {
        let bits_v = self.konst(v)?;
        let w = bits(ty);
        let sign = match ext {
            Ext::Type => signed(ty),
            Ext::Zero => false,
            Ext::Sign => true,
        };
        let value = if sign {
            sext_bits(bits_v, w)
        } else {
            zext_bits(bits_v, w) as i64
        };
        // A 32-bit operation reads its immediate as 32 bits: an unsigned
        // 32-bit value above i32::MAX is the same bit pattern as a negative.
        let value = if !is64(ty) && w == 32 {
            value as u32 as i32 as i64
        } else {
            value
        };
        range.fits(value).then_some(value)
    }

    /// `v` as a register-or-immediate source for a two-address operation in
    /// which only the low bits matter.
    fn src_low(&self, v: Value, ty: Type) -> Result<Operand> {
        if let Some(b) = self.konst(v) {
            // Only the low bits matter: the smallest immediate with them.
            let value = sext_bits(b, bits(ty));
            if ImmRange::I32.fits(value) {
                return Ok(Operand::Imm(value));
            }
        }
        Ok(self.val(v)?.into())
    }

    /// Range check of a 32-bit result of an 8- or 16-bit operation: fails
    /// unless the value fits `ty`.
    fn check_narrow(&mut self, d: Reg, ty: Type, fail: Fail<'_>, code: u32) -> Result<()> {
        if signed(ty) {
            let t = self.ext32(d, ty, Ext::Sign)?;
            let _ = self.emit(op::CMP32, &[t.into(), d.into()])?;
            self.fail_if(Cond::Ne, fail, code)
        } else {
            let max = if bits(ty) == 8 { 0xFF } else { 0xFFFF };
            let _ = self.emit(op::CMP32, &[d.into(), Operand::Imm(max)])?;
            self.fail_if(Cond::A, fail, code)
        }
    }

    pub(crate) fn unary(
        &mut self,
        uop: UnaryOp,
        policy: Policy,
        arg: Value,
        fail: Fail<'_>,
    ) -> Result<Reg> {
        let ty = self.ty(arg)?;
        let a = self.val(arg)?;
        let overflow = policy.overflow.unwrap_or(Overflow::Wrap);
        let checked = overflow != Overflow::Wrap;
        let fail = self.overflow_sink(overflow, fail);
        let code = OpError::ArithOverflow.code();
        match uop {
            UnaryOp::Not if ty == Type::Bool => {
                self.emit_def(op::XOR32, &[a.into(), Operand::Imm(1)])
            }
            UnaryOp::Not => self.emit_def(pick(ty, op::NOT32, op::NOT64), &[a.into()]),
            UnaryOp::Neg => {
                if is64(ty) || bits(ty) == 32 {
                    let d = self.emit_def(pick(ty, op::NEG32, op::NEG64), &[a.into()])?;
                    if checked {
                        // Signed: overflow only for MIN. Unsigned: any
                        // nonzero operand (NEG sets the carry then).
                        let cc = if signed(ty) { Cond::O } else { Cond::B };
                        self.fail_if(cc, fail, code)?;
                    }
                    Ok(d)
                } else {
                    let x = self.ext32(a, ty, Ext::Type)?;
                    let d = self.emit_def(op::NEG32, &[x.into()])?;
                    if checked {
                        if signed(ty) {
                            self.check_narrow(d, ty, fail, code)?;
                        } else {
                            self.fail_if(Cond::B, fail, code)?;
                        }
                    }
                    Ok(d)
                }
            }
            UnaryOp::Abs => {
                if !signed(ty) {
                    return Ok(a);
                }
                let wide = is64(ty) || bits(ty) == 32;
                let x = if wide {
                    a
                } else {
                    self.ext32(a, ty, Ext::Sign)?
                };
                let n = self.emit_def(pick(ty, op::NEG32, op::NEG64), &[x.into()])?;
                // Negative after negation: the operand was positive.
                let d = self.emit_def(
                    pick(ty, op::CMOV32, op::CMOV64),
                    &[n.into(), x.into(), crate::x86_64::cond(Cond::S).into()],
                )?;
                if checked {
                    if wide {
                        // The flags are still NEG's: OF is set only for MIN.
                        self.fail_if(Cond::O, fail, code)?;
                    } else {
                        self.check_narrow(d, ty, fail, code)?;
                    }
                }
                Ok(d)
            }
            _ => self.unsupported(
                "floating-point unary operations wait on isa-lang's SSE forms (ISSUES P11)",
            ),
        }
    }

    /// Where an overflow goes: the given sink for `error`, a trap for
    /// `trap`.
    fn overflow_sink<'a>(&self, policy: Overflow, fail: Fail<'a>) -> Fail<'a> {
        match policy {
            Overflow::Error => fail,
            _ => Fail::Trap,
        }
    }

    pub(crate) fn binary(
        &mut self,
        bop: BinaryOp,
        policy: Policy,
        args: [Value; 2],
        fail: Fail<'_>,
    ) -> Result<Reg> {
        if bop == BinaryOp::Pow && self.ty_any(args[0])?.is_float() {
            return self.float_pow(args);
        }
        let ty = self.ty(args[0])?;
        match bop {
            BinaryOp::And | BinaryOp::Or | BinaryOp::Xor => {
                let opcode = match bop {
                    BinaryOp::And => pick(ty, op::AND32, op::AND64),
                    BinaryOp::Or => pick(ty, op::OR32, op::OR64),
                    _ => pick(ty, op::XOR32, op::XOR64),
                };
                let args = self.commute(args);
                let a = self.val(args[0])?;
                let b = self.src_low(args[1], ty)?;
                self.emit_def(opcode, &[a.into(), b])
            }
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul => {
                self.arith(bop, policy, ty, args, fail)
            }
            BinaryOp::Div | BinaryOp::Rem | BinaryOp::FloorDiv | BinaryOp::FloorMod => {
                self.divide(bop, policy, ty, args, fail)
            }
            BinaryOp::Shl | BinaryOp::Shr => self.shift(bop, policy, ty, args, fail),
            BinaryOp::Pow => self.pow(policy, ty, args, fail),
            BinaryOp::Min | BinaryOp::Max => {
                let wide = is64(ty);
                let a = self.val(args[0])?;
                let b = self.val(args[1])?;
                let (a, b) = if wide || bits(ty) == 32 {
                    (a, b)
                } else {
                    (self.ext32(a, ty, Ext::Type)?, self.ext32(b, ty, Ext::Type)?)
                };
                let _ = self.emit(pick(ty, op::CMP32, op::CMP64), &[a.into(), b.into()])?;
                // min keeps a unless a > b; max keeps a unless a < b.
                let cc = match (bop == BinaryOp::Min, signed(ty)) {
                    (true, true) => Cond::G,
                    (true, false) => Cond::A,
                    (false, true) => Cond::L,
                    (false, false) => Cond::B,
                };
                self.emit_def(
                    pick(ty, op::CMOV32, op::CMOV64),
                    &[a.into(), b.into(), crate::x86_64::cond(cc).into()],
                )
            }
            _ => self.unsupported(
                "floating-point binary operations wait on isa-lang's SSE forms (ISSUES P11)",
            ),
        }
    }

    fn arith(
        &mut self,
        bop: BinaryOp,
        policy: Policy,
        ty: Type,
        args: [Value; 2],
        fail: Fail<'_>,
    ) -> Result<Reg> {
        // Addition and multiplication commute, and so do the flags that
        // report their overflow.
        let args = if bop == BinaryOp::Sub {
            args
        } else {
            self.commute(args)
        };
        let overflow = policy.overflow.unwrap_or(Overflow::Wrap);
        let checked = overflow != Overflow::Wrap;
        let fail = self.overflow_sink(overflow, fail);
        let code = OpError::ArithOverflow.code();
        let w = bits(ty);
        let a = self.val(args[0])?;
        // 32- and 64-bit: the processor's flags say whether it overflowed.
        if w >= 32 {
            match bop {
                BinaryOp::Add | BinaryOp::Sub => {
                    let opcode = match bop {
                        BinaryOp::Add => pick(ty, op::ADD32, op::ADD64),
                        _ => pick(ty, op::SUB32, op::SUB64),
                    };
                    let b = self.src_low(args[1], ty)?;
                    let d = self.emit_def(opcode, &[a.into(), b])?;
                    if checked {
                        let cc = if signed(ty) { Cond::O } else { Cond::B };
                        self.fail_if(cc, fail, code)?;
                    }
                    Ok(d)
                }
                _ => {
                    if !checked || signed(ty) {
                        // imul's OF is exactly signed overflow.
                        let d = match self.imm(args[1], ty, Ext::Sign, ImmRange::I32) {
                            Some(k) => self.emit_def(
                                pick(ty, op::IMUL32RI, op::IMUL64RI),
                                &[a.into(), Operand::Imm(k)],
                            )?,
                            None => {
                                let b = self.val(args[1])?;
                                self.emit_def(
                                    pick(ty, op::IMUL32, op::IMUL64),
                                    &[a.into(), b.into()],
                                )?
                            }
                        };
                        if checked {
                            self.fail_if(Cond::O, fail, code)?;
                        }
                        Ok(d)
                    } else if w == 32 {
                        // u32: the full product fits 64 bits; it overflowed
                        // if its upper half is not zero.
                        let b = self.val(args[1])?;
                        let x = self.emit_def(op::MOV32, &[a.into()])?;
                        let y = self.emit_def(op::MOV32, &[b.into()])?;
                        let p = self.emit_def(op::IMUL64, &[x.into(), y.into()])?;
                        let low = self.emit_def(op::MOV32, &[p.into()])?;
                        let _ = self.emit(op::CMP64, &[low.into(), p.into()])?;
                        self.fail_if(Cond::Ne, fail, code)?;
                        Ok(p)
                    } else {
                        // u64: mul sets OF and CF when rdx is not zero.
                        let b = self.val(args[1])?;
                        let _ = self.emit(op::COPY, &[RAX.into(), a.into()])?;
                        let _ = self.emit(op::MUL64, &[b.into()])?;
                        let d = self.copy_to_new(RAX.into())?;
                        self.fail_if(Cond::O, fail, code)?;
                        Ok(d)
                    }
                }
            }
        } else if !checked {
            // 8 and 16 bits, wrapping: the low bits of the 32-bit result.
            let d = match bop {
                BinaryOp::Add | BinaryOp::Sub => {
                    let b = self.src_low(args[1], ty)?;
                    let opcode = if bop == BinaryOp::Add {
                        op::ADD32
                    } else {
                        op::SUB32
                    };
                    self.emit_def(opcode, &[a.into(), b])?
                }
                _ => {
                    let b = self.val(args[1])?;
                    self.emit_def(op::IMUL32, &[a.into(), b.into()])?
                }
            };
            Ok(d)
        } else {
            // 8 and 16 bits: exact in 32 bits, then a range check.
            let x = self.ext32(a, ty, Ext::Type)?;
            let b = self.val(args[1])?;
            let y = self.ext32(b, ty, Ext::Type)?;
            let opcode = match bop {
                BinaryOp::Add => op::ADD32,
                BinaryOp::Sub => op::SUB32,
                _ => op::IMUL32,
            };
            let d = self.emit_def(opcode, &[x.into(), y.into()])?;
            if checked {
                self.check_narrow(d, ty, fail, code)?;
            }
            Ok(d)
        }
    }

    fn divide(
        &mut self,
        bop: BinaryOp,
        policy: Policy,
        ty: Type,
        args: [Value; 2],
        fail: Fail<'_>,
    ) -> Result<Reg> {
        let w = bits(ty);
        let sign = signed(ty);
        let quotient = matches!(bop, BinaryOp::Div | BinaryOp::FloorDiv);
        let floor = matches!(bop, BinaryOp::FloorDiv | BinaryOp::FloorMod) && sign;
        let overflow = policy.overflow.unwrap_or(Overflow::Wrap);
        let zero_fail = match policy.div_zero {
            Some(DivZero::Trap) => Fail::Trap,
            _ => fail,
        };
        let over_fail = self.overflow_sink(overflow, fail);
        let a0 = self.val(args[0])?;
        let b0 = self.val(args[1])?;
        // Operands extended to the width the division runs at: 32 bits for
        // 8-, 16-, and 32-bit types, 64 for the rest.
        let (a, b) = if w < 32 {
            (
                self.ext32(a0, ty, Ext::Type)?,
                self.ext32(b0, ty, Ext::Type)?,
            )
        } else {
            (a0, b0)
        };
        let run64 = is64(ty);
        let k = self.konst(args[1]).map(|bits_k| {
            if sign {
                sext_bits(bits_k, w)
            } else {
                zext_bits(bits_k, w) as i64
            }
        });
        // Division by zero.
        if k.is_none_or(|k| k == 0) {
            let _ = self.emit(
                if run64 { op::TEST64 } else { op::TEST32 },
                &[b.into(), b.into()],
            )?;
            self.fail_if(Cond::E, zero_fail, OpError::DivByZero.code())?;
        }
        // MIN / -1 faults in hardware at 32 and 64 bits; route it around.
        let minus_one_path = sign && w >= 32 && k.is_none_or(|k| k == -1);
        let join = if minus_one_path {
            let join = self.new_block()?;
            let result = self.new_reg()?;
            self.f.add_param(join, result.into())?;
            let special = self.new_block()?;
            let normal = self.new_block()?;
            let _ = self.emit(
                pick(ty, op::CMP32, op::CMP64),
                &[b.into(), Operand::Imm(-1)],
            )?;
            self.branch(Cond::E, special, normal)?;
            // a / -1 = -a (overflowing only for MIN); a rem -1 = 0.
            self.cur = special;
            let r = if quotient {
                let n = self.emit_def(pick(ty, op::NEG32, op::NEG64), &[a.into()])?;
                if overflow != Overflow::Wrap {
                    self.fail_if(Cond::O, over_fail, OpError::ArithOverflow.code())?;
                }
                Operand::from(n)
            } else {
                Operand::Imm(0)
            };
            self.jump(join, &[r])?;
            self.cur = normal;
            Some((join, result))
        } else {
            None
        };
        // The division itself: rdx:rax by b.
        let _ = self.emit(op::COPY, &[RAX.into(), a.into()])?;
        if sign {
            let _ = self.emit(if run64 { op::CQO } else { op::CDQ }, &[])?;
        } else {
            let _ = self.emit(op::COPY, &[RDX.into(), Operand::Imm(0)])?;
        }
        let div = match (run64, sign) {
            (true, true) => op::IDIV64,
            (true, false) => op::DIV64,
            (false, true) => op::IDIV32,
            (false, false) => op::DIV32,
        };
        let _ = self.emit(div, &[b.into()])?;
        let q = self.copy_to_new(RAX.into())?;
        let r = self.copy_to_new(RDX.into())?;
        let wide_ty = if run64 { Type::I64 } else { Type::I32 };
        let mut result = if quotient { q } else { r };
        if floor {
            // floor_div: q - 1 when the remainder is nonzero and the signs
            // differ. floor_mod: r + b under the same condition.
            let cmov = pick(wide_ty, op::CMOV32, op::CMOV64);
            let xor = pick(wide_ty, op::XOR32, op::XOR64);
            let test = pick(wide_ty, op::TEST32, op::TEST64);
            let lea = pick(wide_ty, op::LEA32, op::LEA64);
            if quotient {
                let q1 = self.emit_def(lea, &[Mem::base(q).with_disp(-1).into()])?;
                let _ = self.emit_def(xor, &[a.into(), b.into()])?;
                let qa = self.emit_def(
                    cmov,
                    &[q.into(), q1.into(), crate::x86_64::cond(Cond::S).into()],
                )?;
                let _ = self.emit(test, &[r.into(), r.into()])?;
                result = self.emit_def(
                    cmov,
                    &[qa.into(), q.into(), crate::x86_64::cond(Cond::E).into()],
                )?;
            } else {
                let rb = self.emit_def(
                    lea,
                    &[Mem::base(r).with_index(b, crate::mir::Scale::X1).into()],
                )?;
                let _ = self.emit_def(xor, &[r.into(), b.into()])?;
                let ma = self.emit_def(
                    cmov,
                    &[r.into(), rb.into(), crate::x86_64::cond(Cond::S).into()],
                )?;
                let _ = self.emit(test, &[r.into(), r.into()])?;
                result = self.emit_def(
                    cmov,
                    &[ma.into(), r.into(), crate::x86_64::cond(Cond::E).into()],
                )?;
            }
        }
        // 8- and 16-bit quotients can leave the type's range (-128 / -1).
        if quotient && w < 32 && sign && overflow != Overflow::Wrap {
            self.check_narrow(result, ty, over_fail, OpError::ArithOverflow.code())?;
        }
        match join {
            Some((join, param)) => {
                self.jump(join, &[result.into()])?;
                self.cur = join;
                Ok(param)
            }
            None => Ok(result),
        }
    }

    fn shift(
        &mut self,
        bop: BinaryOp,
        policy: Policy,
        ty: Type,
        args: [Value; 2],
        fail: Fail<'_>,
    ) -> Result<Reg> {
        let w = bits(ty);
        let sign = signed(ty);
        let left = bop == BinaryOp::Shl;
        let mode = policy.shift.unwrap_or(Shift::Mask);
        let a0 = self.val(args[0])?;
        // The value: right shifts need the bits above the width defined.
        let a = if left || w >= 32 {
            a0
        } else {
            self.ext32(a0, ty, Ext::Type)?
        };
        let (op_imm, op_cl) = match (left, sign, is64(ty)) {
            (true, _, false) => (op::SHL32, op::SHL32CL),
            (true, _, true) => (op::SHL64, op::SHL64CL),
            (false, false, false) => (op::SHR32, op::SHR32CL),
            (false, false, true) => (op::SHR64, op::SHR64CL),
            (false, true, false) => (op::SAR32, op::SAR32CL),
            (false, true, true) => (op::SAR64, op::SAR64CL),
        };
        // An arithmetic right shift saturates to the sign: a shift by `w - 1`
        // of the value extended to the operation's width.
        let arith = !left && sign;
        // A constant amount in range shifts by an immediate.
        if let Some(kb) = self.konst(args[1]) {
            let n = if sign {
                sext_bits(kb, w)
            } else {
                zext_bits(kb, w) as i64
            };
            if (0..i64::from(w)).contains(&n) {
                return self.emit_def(op_imm, &[a.into(), Operand::Imm(n)]);
            }
            match mode {
                Shift::Mask => {
                    let n = (kb & u64::from(w - 1)) as i64;
                    return self.emit_def(op_imm, &[a.into(), Operand::Imm(n)]);
                }
                // Every bit shifted out. A negative amount still fails: the
                // general path below tests it.
                Shift::Saturate if n >= 0 => {
                    return if arith {
                        self.emit_def(op_imm, &[a.into(), Operand::Imm(i64::from(w - 1))])
                    } else {
                        self.emit_def(op::MOV64RI, &[Operand::Imm(0)])
                    };
                }
                _ => {}
            }
        }
        let b0 = self.val(args[1])?;
        match mode {
            Shift::Mask => {
                let count = if w >= 32 {
                    b0 // the processor masks by 31 or 63
                } else {
                    self.emit_def(op::AND32, &[b0.into(), Operand::Imm(i64::from(w - 1))])?
                };
                let _ = self.emit(op::COPY, &[RCX.into(), count.into()])?;
                self.emit_def(op_cl, &[a.into()])
            }
            Shift::Saturate => {
                let n = self.ext64(b0, ty, Ext::Type)?;
                if sign {
                    // A negative amount is still out of range (OPS v2).
                    let _ = self.emit(op::TEST64, &[n.into(), n.into()])?;
                    self.fail_if(Cond::S, fail, OpError::ShiftOutOfRange.code())?;
                }
                if arith {
                    // count = min(n, w - 1), unsigned (n is not negative).
                    let top = self.emit_def(op::MOV64RI, &[Operand::Imm(i64::from(w - 1))])?;
                    let _ = self.emit(op::CMP64, &[n.into(), Operand::Imm(i64::from(w - 1))])?;
                    let count = self.emit_def(
                        op::CMOV64,
                        &[n.into(), top.into(), crate::x86_64::cond(Cond::A).into()],
                    )?;
                    let _ = self.emit(op::COPY, &[RCX.into(), count.into()])?;
                    self.emit_def(op_cl, &[a.into()])
                } else {
                    // Shifted by the masked amount, then zero when n >= w.
                    let _ = self.emit(op::COPY, &[RCX.into(), n.into()])?;
                    let shifted = self.emit_def(op_cl, &[a.into()])?;
                    let zero = self.emit_def(op::MOV64RI, &[Operand::Imm(0)])?;
                    let _ = self.emit(op::CMP64, &[n.into(), Operand::Imm(i64::from(w))])?;
                    self.emit_def(
                        op::CMOV64,
                        &[
                            shifted.into(),
                            zero.into(),
                            crate::x86_64::cond(Cond::Ae).into(),
                        ],
                    )
                }
            }
            _ => {
                // In range: 0 <= n < w, as one unsigned compare of the
                // extended amount (a negative one is huge).
                let n = self.ext64(b0, ty, Ext::Type)?;
                let _ = self.emit(op::CMP64, &[n.into(), Operand::Imm(i64::from(w))])?;
                self.fail_if(Cond::Ae, fail, OpError::ShiftOutOfRange.code())?;
                let _ = self.emit(op::COPY, &[RCX.into(), n.into()])?;
                self.emit_def(op_cl, &[a.into()])
            }
        }
    }

    /// Integer `pow` (OPS v2): the exact power by repeated squaring, `x ** 0
    /// = 1` (`0 ** 0` included), under the `overflow` policy. A negative
    /// exponent (signed types only) fails with `NegativeExponent` under every
    /// policy: it goes to `fail` (a `check`'s error edge; ir-lang refuses a
    /// signed `pow` outside a `check`), never to the overflow trap.
    ///
    /// A square is taken only while a higher exponent bit remains, so every
    /// square computed is a factor of the result, and `|r| >= 1` whenever the
    /// base is nonzero: if any product overflows, the result does. The checks
    /// therefore follow every multiplication. 8-, 16-, and 32-bit types
    /// compute exactly in 64 bits (an overflow there is far outside their
    /// range) and check the range at the end; `u64` multiplies with `mul`,
    /// whose overflow flag is unsigned overflow. Wrapping needs only the low
    /// bits, which a 64-bit multiply gives for every width.
    fn pow(&mut self, policy: Policy, ty: Type, args: [Value; 2], fail: Fail<'_>) -> Result<Reg> {
        let w = bits(ty);
        let sign = signed(ty);
        let overflow = policy.overflow.unwrap_or(Overflow::Wrap);
        let checked = overflow != Overflow::Wrap;
        let over = self.overflow_sink(overflow, fail);
        let x0 = self.val(args[0])?;
        // Wrapping needs only the low bits, which do not depend on the bits
        // above the width; an exact power needs the true operand.
        let x = if checked && w < 64 {
            self.ext64(x0, ty, Ext::Type)?
        } else {
            x0
        };
        let mul = PowMul {
            checked,
            wide_unsigned: checked && !sign && w == 64,
            over,
        };
        let e_const = self.konst(args[1]).map(|kb| {
            if sign {
                sext_bits(kb, w)
            } else {
                zext_bits(kb, w) as i64
            }
        });
        let r = match e_const {
            // A known exponent unrolls (at most 64 squarings and 64
            // multiplications). A negative one takes the general path, which
            // fails on it.
            Some(e) if e >= 0 || !sign => self.pow_unrolled(x, e as u64, mul)?,
            _ => {
                let e0 = self.val(args[1])?;
                let e = self.ext64(e0, ty, Ext::Type)?;
                if sign {
                    let _ = self.emit(op::TEST64, &[e.into(), e.into()])?;
                    self.fail_if(Cond::S, fail, OpError::NegativeExponent.code())?;
                }
                self.pow_loop(x, e, mul)?
            }
        };
        if checked && w < 64 {
            // The exact power must fit the type: extending its low bits by the
            // type's signedness gives it back.
            let t = self.ext64(r, ty, Ext::Type)?;
            let _ = self.emit(op::CMP64, &[t.into(), r.into()])?;
            self.fail_if(Cond::Ne, over, OpError::ArithOverflow.code())?;
        }
        Ok(r)
    }

    /// `a * b` for `pow`, checked as `m` says.
    fn pow_mul(&mut self, a: Reg, b: Reg, m: PowMul<'_>) -> Result<Reg> {
        if m.wide_unsigned {
            let _ = self.emit(op::COPY, &[RAX.into(), a.into()])?;
            let _ = self.emit(op::MUL64, &[b.into()])?;
            let d = self.copy_to_new(RAX.into())?;
            self.fail_if(Cond::O, m.over, OpError::ArithOverflow.code())?;
            return Ok(d);
        }
        let d = self.emit_def(op::IMUL64, &[a.into(), b.into()])?;
        if m.checked {
            self.fail_if(Cond::O, m.over, OpError::ArithOverflow.code())?;
        }
        Ok(d)
    }

    /// `x ** e` for a known `e`, as straight-line repeated squaring.
    fn pow_unrolled(&mut self, x: Reg, e: u64, m: PowMul<'_>) -> Result<Reg> {
        let mut r: Option<Reg> = None;
        let (mut base, mut n) = (x, e);
        while n != 0 {
            if n & 1 == 1 {
                r = Some(match r {
                    None => base,
                    Some(r) => self.pow_mul(r, base, m)?,
                });
            }
            n >>= 1;
            if n != 0 {
                base = self.pow_mul(base, base, m)?;
            }
        }
        match r {
            Some(r) => Ok(r),
            None => self.emit_def(op::MOV64RI, &[Operand::Imm(1)]),
        }
    }

    /// `x ** e` for an exponent known only at run time (`e` not negative,
    /// read as unsigned): a loop over its bits.
    ///
    /// ```text
    ///                 jmp head(1, x, e)
    /// head(r, b, n):  test n, 1; je step(r) else mul
    /// mul:            r2 = r * b [check]; jmp step(r2)
    /// step(rr):       n2 = n >> 1; test n2, n2; je done(rr) else sq
    /// sq:             b2 = b * b [check]; jmp head(rr, b2, n2)
    /// done(res):
    /// ```
    fn pow_loop(&mut self, x: Reg, e: Reg, m: PowMul<'_>) -> Result<Reg> {
        let head = self.new_block()?;
        let (r, b, n) = (self.new_reg()?, self.new_reg()?, self.new_reg()?);
        for p in [r, b, n] {
            self.f.add_param(head, p.into())?;
        }
        let mul = self.new_block()?;
        let step = self.new_block()?;
        let rr = self.new_reg()?;
        self.f.add_param(step, rr.into())?;
        let sq = self.new_block()?;
        let done = self.new_block()?;
        let res = self.new_reg()?;
        self.f.add_param(done, res.into())?;

        let one = self.emit_def(op::MOV64RI, &[Operand::Imm(1)])?;
        self.jump(head, &[one.into(), x.into(), e.into()])?;

        self.cur = head;
        let _ = self.emit(op::TEST64, &[n.into(), Operand::Imm(1)])?;
        let skip = self.f.block_call(step, &[r.into()])?;
        let take = self.f.block_call(mul, &[])?;
        let _ = self.emit(
            op::JCC,
            &[
                crate::x86_64::cond(Cond::E).into(),
                skip.into(),
                take.into(),
            ],
        )?;

        self.cur = mul;
        let r2 = self.pow_mul(r, b, m)?;
        self.jump(step, &[r2.into()])?;

        self.cur = step;
        let n2 = self.emit_def(op::SHR64, &[n.into(), Operand::Imm(1)])?;
        let _ = self.emit(op::TEST64, &[n2.into(), n2.into()])?;
        let out = self.f.block_call(done, &[rr.into()])?;
        let more = self.f.block_call(sq, &[])?;
        let _ = self.emit(
            op::JCC,
            &[crate::x86_64::cond(Cond::E).into(), out.into(), more.into()],
        )?;

        self.cur = sq;
        let b2 = self.pow_mul(b, b, m)?;
        self.jump(head, &[rr.into(), b2.into(), n2.into()])?;

        self.cur = done;
        Ok(res)
    }

    /// Sets the flags for an integer comparison and returns the condition
    /// that holds when it is true.
    pub(crate) fn compare(&mut self, cop: CmpOp, args: [Value; 2]) -> Result<Cond> {
        let ty = self.ty(args[0])?;
        let sign = signed(ty);
        let a0 = self.val(args[0])?;
        let wide = is64(ty);
        let a = if wide || bits(ty) == 32 || ty == Type::Bool {
            a0
        } else {
            self.ext32(a0, ty, Ext::Type)?
        };
        let b = match self.imm(args[1], ty, Ext::Type, ImmRange::I32) {
            Some(k) => Operand::Imm(k),
            None => {
                let b0 = self.val(args[1])?;
                if wide || bits(ty) == 32 || ty == Type::Bool {
                    b0.into()
                } else {
                    self.ext32(b0, ty, Ext::Type)?.into()
                }
            }
        };
        let _ = self.emit(if wide { op::CMP64 } else { op::CMP32 }, &[a.into(), b])?;
        Ok(match (cop, sign) {
            (CmpOp::Eq, _) => Cond::E,
            (CmpOp::Ne, _) => Cond::Ne,
            (CmpOp::Lt, true) => Cond::L,
            (CmpOp::Le, true) => Cond::Le,
            (CmpOp::Gt, true) => Cond::G,
            (CmpOp::Ge, true) => Cond::Ge,
            (CmpOp::Lt, false) => Cond::B,
            (CmpOp::Le, false) => Cond::Be,
            (CmpOp::Gt, false) => Cond::A,
            (CmpOp::Ge, false) => Cond::Ae,
            _ => return self.unsupported("a comparison this release does not know"),
        })
    }

    pub(crate) fn convert(
        &mut self,
        cop: ConvOp,
        policy: Policy,
        to: Type,
        arg: Value,
        fail: Fail<'_>,
    ) -> Result<Reg> {
        // A float lives as its bit pattern, so a `bitcast` to or from one
        // moves nothing.
        if cop == ConvOp::Bitcast && (to.is_float() || self.ty_any(arg)?.is_float()) {
            return self.val(arg);
        }
        if to.is_float() {
            return self.unsupported(
                "conversions to floating point wait on isa-lang's SSE forms (ISSUES P11)",
            );
        }
        let from = self.ty(arg)?;
        let a = self.val(arg)?;
        match cop {
            // Bits already in place; only the type changes.
            ConvOp::Narrow
            | ConvOp::Bitcast
            | ConvOp::BoolToInt
            | ConvOp::PtrToInt
            | ConvOp::IntToPtr
            | ConvOp::RefToPtr => Ok(a),
            ConvOp::Zext => self.ext64(a, from, Ext::Zero),
            ConvOp::Sext => {
                if is64(to) {
                    self.ext64(a, from, Ext::Sign)
                } else {
                    self.ext32(a, from, Ext::Sign)
                }
            }
            ConvOp::IntCast => {
                let overflow = policy.overflow.unwrap_or(Overflow::Wrap);
                let x = self.ext64(a, from, Ext::Type)?;
                if overflow == Overflow::Wrap {
                    return Ok(if bits(to) > bits(from) { x } else { a });
                }
                let fail = self.overflow_sink(overflow, fail);
                let code = OpError::ArithOverflow.code();
                // Fits iff extending its low bits by the target type gives
                // the value back, and (across signedness) it is not negative.
                if bits(to) < 64 {
                    let t = self.ext64(x, to, Ext::Type)?;
                    let _ = self.emit(op::CMP64, &[t.into(), x.into()])?;
                    self.fail_if(Cond::Ne, fail, code)?;
                }
                if signed(from) != signed(to) {
                    let _ = self.emit(op::TEST64, &[x.into(), x.into()])?;
                    self.fail_if(Cond::S, fail, code)?;
                }
                Ok(x)
            }
            ConvOp::CharFromU32 => {
                // A Unicode scalar: at most 0x10FFFF and not a surrogate.
                let code = OpError::InvalidChar.code();
                let _ = self.emit(op::CMP32, &[a.into(), Operand::Imm(0x10_FFFF)])?;
                self.fail_if(Cond::A, fail, code)?;
                let t = self.emit_def(op::LEA32, &[Mem::base(a).with_disp(-0xD800).into()])?;
                let _ = self.emit(op::CMP32, &[t.into(), Operand::Imm(0x7FF)])?;
                self.fail_if(Cond::Be, fail, code)?;
                Ok(a)
            }
            _ => self.unsupported(
                "floating-point conversions wait on isa-lang's SSE forms (ISSUES P11)",
            ),
        }
    }
}

/// How `pow` multiplies: whether it checks for overflow, whether it needs
/// the unsigned 64-bit `mul`, and where an overflow goes.
#[derive(Clone, Copy)]
struct PowMul<'a> {
    checked: bool,
    wide_unsigned: bool,
    over: Fail<'a>,
}

/// Keeps `IselError` and `MirError` in this module's signatures readable.
#[allow(dead_code, reason = "type anchors for the module's error paths")]
fn _anchors(e: MirError) -> IselError {
    IselError::Mir(e)
}
