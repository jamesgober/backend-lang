//! Addresses, loads and stores, atomics, fences, and block memory operations
//! (IR.md §6.2–§6.3).

use ir_lang::{InstData, MemOrder, RmwOp, Type, Value, ValueRange};
use isa_lang::x86_64::Cond;

use super::ops::{bits, is64, sext_bits, zext_bits};
use super::{Addr, Ext, Result, Sel};
use crate::mir::{Block, Mem, MirError, Operand, Scale};
use crate::regs::Reg;
use crate::target::{ImmRange, Opcode};
use crate::x86_64::op;

impl Sel<'_> {
    pub(crate) fn memory(&mut self, data: &InstData, results: ValueRange) -> Result<()> {
        let r = results.first();
        match *data {
            InstData::StackAddr { slot } => {
                let r = r.ok_or(MirError::Limit)?;
                let (s, round) = self.mir_slot(slot)?;
                let a = self.emit_def(op::LEA64, &[Operand::slot(s)])?;
                if round == 0 {
                    self.set_val(r, a);
                    self.set_addr(r, Addr::Slot(s, 0));
                } else {
                    // Round up inside the over-sized slot.
                    let up = self.emit_def(op::ADD64, &[a.into(), Operand::Imm(i64::from(round - 1))])?;
                    let d = self.emit_def(op::AND64, &[up.into(), Operand::Imm(-i64::from(round))])?;
                    self.set_val(r, d);
                    self.set_addr(r, Addr::Reg(d, 0));
                }
                Ok(())
            }
            InstData::GlobalAddr { global } => {
                let r = r.ok_or(MirError::Limit)?;
                let name = self.m.global_name(global).ok_or(MirError::Limit)?;
                let sym = self.symbol(name)?;
                let d = self.emit_def(op::LEA64, &[sym.into()])?;
                self.set_val(r, d);
                self.set_addr(r, Addr::Reg(d, 0));
                Ok(())
            }
            InstData::FuncAddr { func } => {
                let r = r.ok_or(MirError::Limit)?;
                let name = self.m.func_decl(func).ok_or(MirError::Limit)?.name();
                let sym = self.symbol(name)?;
                let d = self.emit_def(op::LEA64, &[sym.into()])?;
                self.set_val(r, d);
                Ok(())
            }
            InstData::FieldAddr { base, agg, field } => {
                let r = r.ok_or(MirError::Limit)?;
                let off = self.m.field_offset(agg, field).ok_or(MirError::Limit)?;
                self.offset_addr(r, base, off as i64)
            }
            InstData::PtrOffset { base, offset } => {
                let r = r.ok_or(MirError::Limit)?;
                let oty = self.ty(offset)?;
                if let Some(k) = self.konst(offset) {
                    let off = if oty.is_signed_int() {
                        sext_bits(k, bits(oty))
                    } else {
                        zext_bits(k, bits(oty)) as i64
                    };
                    return self.offset_addr(r, base, off);
                }
                let b = self.val(base)?;
                let o = self.val(offset)?;
                let o = self.ext64(o, oty, Ext::Type)?;
                let d = self.emit_def(op::LEA64, &[Mem::base(b).with_index(o, Scale::X1).into()])?;
                self.set_val(r, d);
                Ok(())
            }
            InstData::ElemAddr { base, elem, index } => {
                let r = r.ok_or(MirError::Limit)?;
                let size = self.m.layout(elem).ok_or(MirError::Limit)?.size();
                let ity = self.ty(index)?;
                if let Some(k) = self.konst(index) {
                    let i = if ity.is_signed_int() {
                        sext_bits(k, bits(ity))
                    } else {
                        zext_bits(k, bits(ity)) as i64
                    };
                    return self.offset_addr(r, base, i.wrapping_mul(size as i64));
                }
                let b = self.val(base)?;
                let i = self.val(index)?;
                let i = self.ext64(i, ity, Ext::Type)?;
                let scale = match size {
                    1 => Some(Scale::X1),
                    2 => Some(Scale::X2),
                    4 => Some(Scale::X4),
                    8 => Some(Scale::X8),
                    _ => None,
                };
                let d = match scale {
                    Some(s) => self.emit_def(op::LEA64, &[Mem::base(b).with_index(i, s).into()])?,
                    None => {
                        let scaled = if ImmRange::I32.fits(size as i64) {
                            self.emit_def(op::IMUL64RI, &[i.into(), Operand::Imm(size as i64)])?
                        } else {
                            let k = self.emit_def(op::MOV64RI, &[Operand::Imm(size as i64)])?;
                            self.emit_def(op::IMUL64, &[i.into(), k.into()])?
                        };
                        self.emit_def(op::LEA64, &[Mem::base(b).with_index(scaled, Scale::X1).into()])?
                    }
                };
                self.set_val(r, d);
                Ok(())
            }
            InstData::Load { ty, addr, .. } | InstData::AtomicLoad { ty, addr, .. } => {
                // x86-64 loads are atomic when aligned (IR.md requires natural
                // alignment for atomics) and acquire under TSO.
                let r = r.ok_or(MirError::Limit)?;
                if ty.is_float() {
                    return self.unsupported("floating-point loads wait on isa-lang's SSE forms (ISSUES P11)");
                }
                let m = self.mem_operand(addr)?;
                let d = self.emit_def(load_op(ty), &[m])?;
                self.set_val(r, d);
                Ok(())
            }
            InstData::Store { value, addr, .. } => self.store(value, addr),
            InstData::AtomicStore { value, addr, order } => {
                if order == MemOrder::SeqCst {
                    // A sequentially consistent store needs a full barrier:
                    // xchg with memory is implicitly locked.
                    let ty = self.ty(value)?;
                    let m = self.mem_operand(addr)?;
                    let v = self.val(value)?;
                    let _ = self.emit_def(xchg_op(ty), &[m, v.into()])?;
                    Ok(())
                } else {
                    self.store(value, addr)
                }
            }
            InstData::AtomicRmw { op: RmwOp::Xchg, addr, value, .. } => {
                let r = r.ok_or(MirError::Limit)?;
                let ty = self.ty(value)?;
                let m = self.mem_operand(addr)?;
                let v = self.val(value)?;
                let d = self.emit_def(xchg_op(ty), &[m, v.into()])?;
                self.set_val(r, d);
                Ok(())
            }
            InstData::AtomicRmw { .. } => self.unsupported(
                "atomic read-modify-write other than xchg waits on isa-lang's lock-prefixed forms (ISSUES P11)",
            ),
            InstData::CmpXchg { .. } => {
                self.unsupported("cmpxchg waits on isa-lang's lock cmpxchg form (ISSUES P11)")
            }
            InstData::Fence { order } => {
                // x86-64 is TSO: only a sequentially consistent fence needs an
                // instruction, and a locked xchg on a private slot is one.
                if order == MemOrder::SeqCst {
                    let s = self.fence_slot()?;
                    let z = self.emit_def(op::MOV64RI, &[Operand::Imm(0)])?;
                    let _ = self.emit_def(op::XCHG64, &[Operand::slot(s), z.into()])?;
                }
                Ok(())
            }
            InstData::MemSet { dst, byte, len, .. } => self.memset(dst, byte, len),
            InstData::MemCopy { dst, src, len, .. } => {
                let (d, s, n) = self.block_operands(dst, src, len)?;
                let done = self.new_block()?;
                self.copy_forward(d, s, n, done)?;
                self.cur = done;
                Ok(())
            }
            InstData::MemMove { dst, src, len, .. } => {
                let (d, s, n) = self.block_operands(dst, src, len)?;
                let done = self.new_block()?;
                let forward = self.new_block()?;
                let backward = self.new_block()?;
                // Copying toward lower addresses is safe front to back, toward
                // higher addresses back to front.
                let _ = self.emit(op::CMP64, &[d.into(), s.into()])?;
                self.branch(Cond::Be, forward, backward)?;
                self.cur = forward;
                self.copy_forward(d, s, n, done)?;
                self.cur = backward;
                self.copy_backward(d, s, n, done)?;
                self.cur = done;
                Ok(())
            }
            _ => self.unsupported("an instruction this release does not know"),
        }
    }

    /// `r = base + off`, folding the offset into later memory operands.
    fn offset_addr(&mut self, r: Value, base: Value, off: i64) -> Result<()> {
        let folded = match self.addr_of(base) {
            Some(Addr::Slot(s, o)) => Some(Addr::Slot(s, o.wrapping_add(off))),
            Some(Addr::Reg(b, o)) => Some(Addr::Reg(b, o.wrapping_add(off))),
            None => Some(Addr::Reg(self.val(base)?, off)),
        };
        let b = self.val(base)?;
        let d = if ImmRange::I32.fits(off) {
            self.emit_def(op::LEA64, &[Mem::base(b).with_disp(off as i32).into()])?
        } else {
            let k = self.emit_def(op::MOV64RI, &[Operand::Imm(off)])?;
            self.emit_def(op::ADD64, &[b.into(), k.into()])?
        };
        self.set_val(r, d);
        if let Some(a) = folded {
            self.set_addr(r, a);
        }
        Ok(())
    }

    /// The memory operand for address `addr`: a slot or register with its
    /// folded offset when it fits, else `[addr]`.
    pub(crate) fn mem_operand(&mut self, addr: Value) -> Result<Operand> {
        match self.addr_of(addr) {
            Some(Addr::Slot(s, off)) if ImmRange::I32.fits(off) => Ok(Operand::Slot {
                slot: s,
                offset: off as i32,
            }),
            Some(Addr::Reg(b, off)) if ImmRange::I32.fits(off) => {
                Ok(Mem::base(b).with_disp(off as i32).into())
            }
            _ => Ok(Mem::base(self.val(addr)?).into()),
        }
    }

    fn store(&mut self, value: Value, addr: Value) -> Result<()> {
        let ty = self.ty(value)?;
        let m = self.mem_operand(addr)?;
        let (opcode, range) = match bits(ty) {
            8 => (op::STORE8, ImmRange::I8),
            16 => (op::STORE16, ImmRange::I16),
            32 => (op::STORE32, ImmRange::I32),
            _ => (op::STORE64, ImmRange::I32),
        };
        let src = match self.konst(value) {
            Some(k) if range.fits(sext_bits(k, bits(ty))) => Operand::Imm(sext_bits(k, bits(ty))),
            _ => self.val(value)?.into(),
        };
        let _ = self.emit(opcode, &[m, src])?;
        Ok(())
    }

    fn block_operands(&mut self, dst: Value, src: Value, len: Value) -> Result<(Reg, Reg, Reg)> {
        let lty = self.ty(len)?;
        let d = self.val(dst)?;
        let s = self.val(src)?;
        let n = self.val(len)?;
        let n = self.ext64(n, lty, Ext::Zero)?;
        Ok((d, s, n))
    }

    /// A loop block with parameters `(regs...)`.
    fn loop_block(&mut self, n: usize) -> Result<(Block, [Reg; 3])> {
        let b = self.new_block()?;
        let mut params = [Reg::from(crate::x86_64::RAX); 3];
        for p in params.iter_mut().take(n) {
            *p = self.new_reg()?;
            self.f.add_param(b, (*p).into())?;
        }
        Ok((b, params))
    }

    /// Eight bytes at a time while at least eight remain, then bytes, front
    /// to back; continues at `done`.
    fn copy_forward(&mut self, d0: Reg, s0: Reg, n0: Reg, done: Block) -> Result<()> {
        let (head, [d, s, n]) = self.loop_block(3)?;
        let (tail, [td, ts, tn]) = self.loop_block(3)?;
        let body = self.new_block()?;
        let tbody = self.new_block()?;
        self.jump(head, &[d0.into(), s0.into(), n0.into()])?;
        self.cur = head;
        let _ = self.emit(op::CMP64, &[n.into(), Operand::Imm(8)])?;
        let to_tail = self.f.block_call(tail, &[d.into(), s.into(), n.into()])?;
        let to_body = self.f.block_call(body, &[])?;
        let _ = self.emit(
            op::JCC,
            &[
                crate::x86_64::cond(Cond::B).into(),
                to_tail.into(),
                to_body.into(),
            ],
        )?;
        self.cur = body;
        let t = self.emit_def(op::MOV64, &[Mem::base(s).into()])?;
        let _ = self.emit(op::STORE64, &[Mem::base(d).into(), t.into()])?;
        let d2 = self.emit_def(op::LEA64, &[Mem::base(d).with_disp(8).into()])?;
        let s2 = self.emit_def(op::LEA64, &[Mem::base(s).with_disp(8).into()])?;
        let n2 = self.emit_def(op::LEA64, &[Mem::base(n).with_disp(-8).into()])?;
        self.jump(head, &[d2.into(), s2.into(), n2.into()])?;
        self.cur = tail;
        let _ = self.emit(op::TEST64, &[tn.into(), tn.into()])?;
        self.branch(Cond::E, done, tbody)?;
        self.cur = tbody;
        let t = self.emit_def(op::MOVZX8, &[Mem::base(ts).into()])?;
        let _ = self.emit(op::STORE8, &[Mem::base(td).into(), t.into()])?;
        let d2 = self.emit_def(op::LEA64, &[Mem::base(td).with_disp(1).into()])?;
        let s2 = self.emit_def(op::LEA64, &[Mem::base(ts).with_disp(1).into()])?;
        let n2 = self.emit_def(op::LEA64, &[Mem::base(tn).with_disp(-1).into()])?;
        self.jump(tail, &[d2.into(), s2.into(), n2.into()])?;
        Ok(())
    }

    /// The same, back to front: the last eight bytes first.
    fn copy_backward(&mut self, d: Reg, s: Reg, n0: Reg, done: Block) -> Result<()> {
        let (head, [n, _, _]) = self.loop_block(1)?;
        let (tail, [tn, _, _]) = self.loop_block(1)?;
        let body = self.new_block()?;
        let tbody = self.new_block()?;
        self.jump(head, &[n0.into()])?;
        self.cur = head;
        let _ = self.emit(op::CMP64, &[n.into(), Operand::Imm(8)])?;
        let to_tail = self.f.block_call(tail, &[n.into()])?;
        let to_body = self.f.block_call(body, &[])?;
        let _ = self.emit(
            op::JCC,
            &[
                crate::x86_64::cond(Cond::B).into(),
                to_tail.into(),
                to_body.into(),
            ],
        )?;
        self.cur = body;
        let n2 = self.emit_def(op::LEA64, &[Mem::base(n).with_disp(-8).into()])?;
        let t = self.emit_def(op::MOV64, &[Mem::base(s).with_index(n2, Scale::X1).into()])?;
        let _ = self.emit(
            op::STORE64,
            &[Mem::base(d).with_index(n2, Scale::X1).into(), t.into()],
        )?;
        self.jump(head, &[n2.into()])?;
        self.cur = tail;
        let _ = self.emit(op::TEST64, &[tn.into(), tn.into()])?;
        self.branch(Cond::E, done, tbody)?;
        self.cur = tbody;
        let n2 = self.emit_def(op::LEA64, &[Mem::base(tn).with_disp(-1).into()])?;
        let t = self.emit_def(op::MOVZX8, &[Mem::base(s).with_index(n2, Scale::X1).into()])?;
        let _ = self.emit(
            op::STORE8,
            &[Mem::base(d).with_index(n2, Scale::X1).into(), t.into()],
        )?;
        self.jump(tail, &[n2.into()])?;
        Ok(())
    }

    fn memset(&mut self, dst: Value, byte: Value, len: Value) -> Result<()> {
        let lty = self.ty(len)?;
        let d0 = self.val(dst)?;
        let n0 = self.val(len)?;
        let n0 = self.ext64(n0, lty, Ext::Zero)?;
        let b = self.val(byte)?;
        let b = self.ext64(b, Type::U8, Ext::Zero)?;
        // The byte in all eight lanes.
        let k = self.emit_def(op::MOV64RI, &[Operand::Imm(0x0101_0101_0101_0101)])?;
        let pat = self.emit_def(op::IMUL64, &[b.into(), k.into()])?;
        let done = self.new_block()?;
        let (head, [d, n, _]) = self.loop_block(2)?;
        let (tail, [td, tn, _]) = self.loop_block(2)?;
        let body = self.new_block()?;
        let tbody = self.new_block()?;
        self.jump(head, &[d0.into(), n0.into()])?;
        self.cur = head;
        let _ = self.emit(op::CMP64, &[n.into(), Operand::Imm(8)])?;
        let to_tail = self.f.block_call(tail, &[d.into(), n.into()])?;
        let to_body = self.f.block_call(body, &[])?;
        let _ = self.emit(
            op::JCC,
            &[
                crate::x86_64::cond(Cond::B).into(),
                to_tail.into(),
                to_body.into(),
            ],
        )?;
        self.cur = body;
        let _ = self.emit(op::STORE64, &[Mem::base(d).into(), pat.into()])?;
        let d2 = self.emit_def(op::LEA64, &[Mem::base(d).with_disp(8).into()])?;
        let n2 = self.emit_def(op::LEA64, &[Mem::base(n).with_disp(-8).into()])?;
        self.jump(head, &[d2.into(), n2.into()])?;
        self.cur = tail;
        let _ = self.emit(op::TEST64, &[tn.into(), tn.into()])?;
        self.branch(Cond::E, done, tbody)?;
        self.cur = tbody;
        let _ = self.emit(op::STORE8, &[Mem::base(td).into(), b.into()])?;
        let d2 = self.emit_def(op::LEA64, &[Mem::base(td).with_disp(1).into()])?;
        let n2 = self.emit_def(op::LEA64, &[Mem::base(tn).with_disp(-1).into()])?;
        self.jump(tail, &[d2.into(), n2.into()])?;
        self.cur = done;
        Ok(())
    }
}

/// The load of a value of `ty`, leaving the representation the selector
/// assumes (`bool` exactly 0 or 1).
fn load_op(ty: Type) -> Opcode {
    match ty {
        Type::Bool | Type::U8 => op::MOVZX8,
        Type::I8 => op::MOVSX8TO32,
        Type::U16 => op::MOVZX16,
        Type::I16 => op::MOVSX16TO32,
        Type::I32 | Type::U32 => op::MOV32,
        _ => op::MOV64,
    }
}

fn xchg_op(ty: Type) -> Opcode {
    match bits(ty) {
        8 => op::XCHG8,
        16 => op::XCHG16,
        32 => op::XCHG32,
        _ if is64(ty) => op::XCHG64,
        _ => op::XCHG64,
    }
}
