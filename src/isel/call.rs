//! Calls, safepoints, tail calls, invokes, and returns (IR.md §6.4, §7–§9).
//!
//! A call places its arguments where the callee's convention wants them
//! (stack arguments stored into the outgoing area, register arguments with
//! one parallel copy), calls, and copies the results out of the return
//! registers. At a safepoint, each GC reference in the call's list is stored
//! into a root slot before the call and reloaded after it as the relocated
//! value; the call records those slots as its stack map, so no reference is
//! in a register while a collector runs (IR.md §9 rule 1 guarantees the list
//! is every live reference).

use alloc::vec::Vec;

use ir_lang::{Attrs, Call, CallTarget, Signature, Type, Value, ValueRange};

use super::ops::bits;
use super::{Ext, Result, Sel, abi_for, first_int_arg};
use crate::mir::{Block, Effects, Inst, Mem, MirError, Operand, StackSlot};
use crate::regs::{PReg, Reg};
use crate::target::ImmRange;
use crate::target::abi::{Abi, ArgClass, ArgLoc};
use crate::x86_64::{R11, RAX, RSP, op};

/// What a lowered call leaves behind.
pub(crate) struct CallOut {
    /// The callee's results, in registers.
    pub(crate) results: Vec<Reg>,
    /// The relocated GC references, in list order.
    pub(crate) relocated: Vec<Reg>,
}

impl Sel<'_> {
    /// The argument classes of a parameter list (all integer-like).
    pub(crate) fn arg_classes(&self, params: &[Type]) -> Result<Vec<ArgClass>> {
        let mut out = Vec::with_capacity(params.len());
        for &t in params {
            if t.is_float() {
                return self.unsupported(
                    "floating-point arguments and results wait on isa-lang's SSE forms (ISSUES P11)",
                );
            }
            out.push(ArgClass::Int);
        }
        Ok(out)
    }

    /// The register locations of a signature's results under `abi`.
    fn ret_regs(&self, abi: &Abi, returns: &[Type]) -> Result<Vec<PReg>> {
        let classes = self.arg_classes(returns)?;
        let mut locs = Vec::new();
        if abi.assign_rets(&classes, &mut locs).is_err() {
            return self.unsupported(
                "more results than the convention's return registers (results in memory come with aggregate lowering)",
            );
        }
        Ok(locs
            .iter()
            .filter_map(|l| match l {
                ArgLoc::Reg(r) => Some(*r),
                ArgLoc::Stack(_) => None,
            })
            .collect())
    }

    /// An argument or result as it crosses a call boundary: 8- and 16-bit
    /// values extended to 32 bits (what C compilers expect), constants as
    /// immediates.
    fn boundary_operand(&mut self, v: Value) -> Result<Operand> {
        let ty = self.ty(v)?;
        if let Some(k) = self.konst(v) {
            let w = bits(ty);
            let value = if ty.is_signed_int() {
                super::ops::sext_bits(k, w)
            } else {
                super::ops::zext_bits(k, w) as i64
            };
            return Ok(Operand::Imm(value));
        }
        let r = self.val(v)?;
        if bits(ty) < 32 && ty != Type::Bool {
            return Ok(self.ext32(r, ty, Ext::Type)?.into());
        }
        Ok(r.into())
    }

    fn callee_sig(&self, call: &Call) -> Result<(&'_ Signature, Attrs)> {
        match call.target {
            CallTarget::Direct(f) => {
                let decl = self.m.func_decl(f).ok_or(MirError::Limit)?;
                let sig = self.m.func_signature(f).ok_or(MirError::Limit)?;
                Ok((sig, decl.attrs()))
            }
            CallTarget::Indirect { sig, .. } => {
                Ok((self.m.signature(sig).ok_or(MirError::Limit)?, Attrs::NONE))
            }
            _ => self.unsupported("a call target kind this release does not know"),
        }
    }

    /// Lowers a call; `landing` is the landing pad of an `invoke`.
    pub(crate) fn emit_call(&mut self, call: &Call, landing: Option<Block>) -> Result<CallOut> {
        let (sig, decl_attrs) = self.callee_sig(call)?;
        let sig = sig.clone();
        let abi = abi_for(self.func, sig.conv, self.opts)?;
        let attrs = call.attrs.union(decl_attrs);
        let safepoint = !attrs.contains(Attrs::NOGC);
        let classes = self.arg_classes(&sig.params)?;
        let mut locs = Vec::new();
        let outgoing = abi.assign_args(&classes, &mut locs);
        self.f.reserve_outgoing(outgoing);
        let rets = self.ret_regs(abi, &sig.returns)?;

        // GC roots into their slots.
        let mut roots: Vec<StackSlot> = Vec::with_capacity(call.gc.len());
        if safepoint {
            for (k, &r) in call.gc.iter().enumerate() {
                let s = self.gc_slot(k)?;
                let src = self.operand(r)?;
                let _ = self.emit(op::COPY, &[Operand::slot(s), src])?;
                roots.push(s);
            }
        }
        // Stack arguments, then register arguments in one parallel copy.
        let mut dsts: Vec<Operand> = Vec::new();
        let mut srcs: Vec<Operand> = Vec::new();
        let mut arg_regs: Vec<PReg> = Vec::new();
        for (&a, loc) in call.args.iter().zip(&locs) {
            let src = self.boundary_operand(a)?;
            match *loc {
                ArgLoc::Reg(r) => {
                    dsts.push(r.into());
                    srcs.push(src);
                    arg_regs.push(r);
                }
                ArgLoc::Stack(off) => {
                    let src = match src {
                        Operand::Imm(k) if !ImmRange::I32.fits(k) => self.copy_to_new(src)?.into(),
                        other => other,
                    };
                    let at = Mem::base(RSP.into()).with_disp(off as i32);
                    let _ = self.emit(op::STORE64, &[at.into(), src])?;
                }
            }
        }
        let target: Operand = match call.target {
            CallTarget::Direct(f) => {
                let name = self.m.func_decl(f).ok_or(MirError::Limit)?.name();
                self.symbol(name)?.into()
            }
            CallTarget::Indirect { callee, .. } => self.val(callee)?.into(),
            _ => return self.unsupported("a call target kind this release does not know"),
        };
        if !dsts.is_empty() {
            dsts.extend(srcs);
            let _ = self.emit(op::PCOPY, &dsts)?;
        }
        let inst = self.emit_with(
            op::CALL,
            &[target],
            Effects::new()
                .uses(&arg_regs)
                .defs(&rets)
                .clobbers(abi.call_clobbers()),
        )?;
        if safepoint || landing.is_some() {
            self.f.set_call_site(inst, &roots, landing)?;
        }
        let results = self.take_results(&rets, &sig.returns)?;
        let mut relocated = Vec::with_capacity(roots.len());
        for &s in &roots {
            relocated.push(self.copy_to_new(Operand::slot(s))?);
        }
        Ok(CallOut { results, relocated })
    }

    /// Copies results out of their return registers; booleans from outside
    /// are normalized.
    fn take_results(&mut self, rets: &[PReg], types: &[Type]) -> Result<Vec<Reg>> {
        let mut out = Vec::with_capacity(rets.len());
        if rets.is_empty() {
            return Ok(out);
        }
        let mut ops: Vec<Operand> = Vec::with_capacity(rets.len() * 2);
        for _ in rets {
            let v = self.new_reg()?;
            out.push(v);
            ops.push(v.into());
        }
        ops.extend(rets.iter().map(|&r| Operand::from(r)));
        let _ = self.emit(op::PCOPY, &ops)?;
        for (r, &t) in out.iter_mut().zip(types) {
            if t == Type::Bool {
                *r = self.emit_def(op::MOVZX8, &[(*r).into()])?;
            }
        }
        Ok(out)
    }

    /// A `call` instruction: results, then relocated references.
    pub(crate) fn call(&mut self, call: &Call, results: ValueRange) -> Result<()> {
        let out = self.emit_call(call, None)?;
        for (v, r) in results.iter().zip(out.results.iter().chain(&out.relocated)) {
            self.set_val(v, *r);
        }
        Ok(())
    }

    /// A `safepoint`: a call to the runtime's GC poll with the references.
    pub(crate) fn safepoint(&mut self, gc: &[Value], results: ValueRange) -> Result<()> {
        let abi = self.opts.abi;
        let mut roots = Vec::with_capacity(gc.len());
        for (k, &r) in gc.iter().enumerate() {
            let s = self.gc_slot(k)?;
            let src = self.operand(r)?;
            let _ = self.emit(op::COPY, &[Operand::slot(s), src])?;
            roots.push(s);
        }
        let sym = self.symbol(self.opts.gc_poll_symbol)?;
        let inst = self.emit_with(
            op::CALL,
            &[sym.into()],
            Effects::new().clobbers(abi.call_clobbers()),
        )?;
        self.f.set_call_site(inst, &roots, None)?;
        for (v, &s) in results.iter().zip(&roots) {
            let r = self.copy_to_new(Operand::slot(s))?;
            self.set_val(v, r);
        }
        Ok(())
    }

    /// Float `pow` (OPS v2): a call to the runtime's `ls_pow` routine for the
    /// type ([`Options::pow_f64_symbol`](crate::Options::pow_f64_symbol) or
    /// [`Options::pow_f32_symbol`](crate::Options::pow_f32_symbol)) under the
    /// platform's C convention, with the operands' bit patterns in the first
    /// two integer argument registers and the result's bit pattern in the
    /// first integer return register. The routine is ir-lang's `ls_pow` (or a
    /// bit-identical port), so every tier computes the same bits; it is not a
    /// safepoint and never collects.
    pub(crate) fn float_pow(&mut self, args: [Value; 2]) -> Result<Reg> {
        let ty = self.ty_any(args[0])?;
        let name = if ty == Type::F32 {
            self.opts.pow_f32_symbol
        } else {
            self.opts.pow_f64_symbol
        };
        let abi = self.opts.abi;
        let ints = abi.args().int();
        let (Some(&a0), Some(&a1)) = (ints.first(), ints.get(1)) else {
            return Err(MirError::Limit.into());
        };
        let ret = abi.rets().int().first().copied().ok_or(MirError::Limit)?;
        let x = self.operand(args[0])?;
        let y = self.operand(args[1])?;
        let _ = self.emit(op::PCOPY, &[a0.into(), a1.into(), x, y])?;
        let sym = self.symbol(name)?;
        let _ = self.emit_with(
            op::CALL,
            &[sym.into()],
            Effects::new()
                .uses(&[a0, a1])
                .defs(&[ret])
                .clobbers(abi.call_clobbers()),
        )?;
        self.copy_to_new(ret.into())
    }

    /// `ret v...`: the values into the return registers, then the epilogue.
    pub(crate) fn ret(&mut self, values: &[Value]) -> Result<()> {
        let sig = self
            .m
            .func_signature(self.func)
            .ok_or(MirError::Limit)?
            .clone();
        let rets = self.ret_regs(self.abi, &sig.returns)?;
        if !rets.is_empty() {
            let mut ops: Vec<Operand> = rets.iter().map(|&r| Operand::from(r)).collect();
            for &v in values {
                let o = self.boundary_operand(v)?;
                ops.push(o);
            }
            let _ = self.emit(op::PCOPY, &ops)?;
        }
        let _ = self.emit_with(op::RET, &[], Effects::new().uses(&rets))?;
        Ok(())
    }

    /// `tail_call`: arguments into the callee's locations (stack arguments
    /// into this function's own incoming area, which the callee then owns),
    /// the epilogue, and a jump.
    pub(crate) fn tail_call(&mut self, call: &Call) -> Result<()> {
        let (sig, _) = self.callee_sig(call)?;
        let sig = sig.clone();
        let own = self
            .m
            .func_signature(self.func)
            .ok_or(MirError::Limit)?
            .clone();
        let abi = abi_for(self.func, sig.conv, self.opts)?;
        let classes = self.arg_classes(&sig.params)?;
        let mut locs = Vec::new();
        let needed = abi.assign_args(&classes, &mut locs);
        let own_classes = self.arg_classes(&own.params)?;
        let mut own_locs = Vec::new();
        let have = self.abi.assign_args(&own_classes, &mut own_locs);
        if needed > have {
            return self.unsupported(
                "a tail call needs more stack-argument space than the caller received",
            );
        }
        let mut dsts: Vec<Operand> = Vec::new();
        let mut srcs: Vec<Operand> = Vec::new();
        let mut uses: Vec<PReg> = Vec::new();
        for (&a, loc) in call.args.iter().zip(&locs) {
            let src = self.boundary_operand(a)?;
            match *loc {
                ArgLoc::Reg(r) => {
                    dsts.push(r.into());
                    uses.push(r);
                }
                ArgLoc::Stack(off) => {
                    let s = self.incoming_slot(off)?;
                    dsts.push(Operand::slot(s));
                }
            }
            srcs.push(src);
        }
        let target: Operand = match call.target {
            CallTarget::Direct(f) => {
                let name = self.m.func_decl(f).ok_or(MirError::Limit)?.name();
                self.symbol(name)?.into()
            }
            CallTarget::Indirect { callee, .. } => {
                // r11 carries no argument in either convention and the
                // epilogue does not restore it.
                dsts.push(R11.into());
                srcs.push(self.val(callee)?.into());
                uses.push(R11);
                R11.into()
            }
            _ => return self.unsupported("a call target kind this release does not know"),
        };
        if !dsts.is_empty() {
            dsts.extend(srcs);
            let _ = self.emit(op::PCOPY, &dsts)?;
        }
        let _ = self.emit_with(op::TAILCALL, &[target], Effects::new().uses(&uses))?;
        Ok(())
    }

    /// `invoke`: the call with a landing pad, the normal edge, and the
    /// landing pad's entry stub. The unwinder enters the stub with the
    /// payload in `rax` and the frame as it was at the call; the stub
    /// reloads the relocated references and jumps to the IR landing pad.
    pub(crate) fn invoke(
        &mut self,
        call: &Call,
        normal: &ir_lang::BlockCall,
        unwind: &ir_lang::BlockCall,
    ) -> Result<()> {
        let stub = self.new_block()?;
        let out = self.emit_call(call, Some(stub))?;
        let mut results: Vec<Operand> = out.results.iter().map(|&r| r.into()).collect();
        results.extend(out.relocated.iter().map(|&r| Operand::from(r)));
        let to_normal = self.edge(normal, &results, None, None)?;
        let _ = self.emit(op::JMP, &[to_normal.into()])?;

        self.cur = stub;
        let _ = self.emit(op::LANDINGPAD, &[])?;
        let exn = self.copy_to_new(RAX.into())?;
        let mut on_unwind: Vec<Operand> = out.results.iter().map(|_| Operand::Imm(0)).collect();
        let nroots = out.relocated.len();
        for k in 0..nroots {
            let s = self.gc_slot(k)?;
            let r = self.copy_to_new(Operand::slot(s))?;
            on_unwind.push(r.into());
        }
        let to_pad = self.edge(unwind, &on_unwind, Some(exn.into()), None)?;
        let _ = self.emit(op::JMP, &[to_pad.into()])?;
        Ok(())
    }

    /// `resume p`: hand the payload to the runtime's unwinder, which does not
    /// return.
    pub(crate) fn resume(&mut self, payload: Value) -> Result<()> {
        let abi = self.opts.abi;
        let arg = first_int_arg(abi)?;
        let src = self.operand(payload)?;
        let _ = self.emit(op::COPY, &[arg.into(), src])?;
        let sym = self.symbol(self.opts.resume_symbol)?;
        let uses = [arg];
        let _ = self.emit_with(
            op::CALL,
            &[sym.into()],
            Effects::new().uses(&uses).clobbers(abi.call_clobbers()),
        )?;
        let _ = self.emit(op::UD2, &[])?;
        Ok(())
    }
}

#[allow(
    dead_code,
    reason = "keeps the instruction type in this module's vocabulary"
)]
fn _inst_anchor(i: Inst) -> Inst {
    i
}
