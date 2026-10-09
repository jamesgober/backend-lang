//! Terminators (IR.md §7–§8): jumps, branches, switches, returns, traps,
//! tail calls, invokes, checks, and resume.

use alloc::vec::Vec;

use ir_lang::{BlockCall, InstData, SwitchCase, Terminator, Value};
use isa_lang::x86_64::Cond;

use super::ops::bits;
use super::{Ext, Fail, Result, Sel};
use crate::mir::{Block, MirError, Operand};
use crate::target::ImmRange;
use crate::x86_64::{self, op};

/// The largest jump table, in entries.
const MAX_TABLE: u64 = 1 << 16;

impl Sel<'_> {
    pub(crate) fn terminator(&mut self, b: ir_lang::Block, term: &Terminator) -> Result<()> {
        let _ = b;
        match term {
            Terminator::Jump(dest) => {
                let c = self.edge(dest, &[], None, None)?;
                let _ = self.emit(op::JMP, &[c.into()])?;
                Ok(())
            }
            Terminator::Branch {
                cond,
                then_dest,
                else_dest,
            } => {
                let cc = self.condition(*cond)?;
                let t = self.edge(then_dest, &[], None, None)?;
                let e = self.edge(else_dest, &[], None, None)?;
                let _ = self.emit(op::JCC, &[x86_64::cond(cc).into(), t.into(), e.into()])?;
                Ok(())
            }
            Terminator::Switch {
                value,
                cases,
                default,
            } => self.switch(*value, cases, default),
            Terminator::Return { values } => self.ret(values),
            Terminator::Unreachable => {
                let _ = self.emit(op::UD2, &[])?;
                Ok(())
            }
            Terminator::Trap { code } => self.trap_call(*code),
            Terminator::TailCall(call) => self.tail_call(call),
            Terminator::Invoke {
                call,
                normal,
                unwind,
            } => self.invoke(call, normal, unwind),
            Terminator::Check { op, normal, error } => {
                let fail = Fail::Edge(error);
                let r = match op.as_ref() {
                    InstData::Unary { op, policy, arg } => self.unary(*op, *policy, *arg, fail)?,
                    InstData::Binary { op, policy, args } => {
                        self.binary(*op, *policy, *args, fail)?
                    }
                    InstData::Convert {
                        op,
                        policy,
                        to,
                        arg,
                    } => self.convert(*op, *policy, *to, *arg, fail)?,
                    _ => return self.unsupported("a check of an operation that cannot fail"),
                };
                let c = self.edge(normal, &[r.into()], None, None)?;
                let _ = self.emit(op::JMP, &[c.into()])?;
                Ok(())
            }
            Terminator::Resume { payload } => self.resume(*payload),
            _ => self.unsupported("a terminator this release does not know"),
        }
    }

    /// A branch target without arguments for `dest`: its block when the edge
    /// passes nothing, else a stub block that passes the arguments.
    fn plain_target(&mut self, dest: &BlockCall) -> Result<Block> {
        let target = self.block(dest.block)?;
        if dest.args.is_empty() {
            return Ok(target);
        }
        let saved = self.cur;
        let stub = self.new_block()?;
        self.cur = stub;
        let c = self.edge(dest, &[], None, None)?;
        let _ = self.emit(op::JMP, &[c.into()])?;
        self.cur = saved;
        Ok(stub)
    }

    /// `switch`: a jump table when the cases are dense, else a balanced tree
    /// of compares. Case values are raw bits, so the value is compared
    /// zero-extended.
    fn switch(&mut self, value: Value, cases: &[SwitchCase], default: &BlockCall) -> Result<()> {
        let ty = self.ty(value)?;
        let _ = bits(ty);
        let v = self.val(value)?;
        let z = self.ext64(v, ty, Ext::Zero)?;
        let default_block = self.plain_target(default)?;
        if cases.is_empty() {
            let c = self.f.block_call(default_block, &[])?;
            let _ = self.emit(op::JMP, &[c.into()])?;
            return Ok(());
        }
        let mut sorted: Vec<(u64, Block)> = Vec::with_capacity(cases.len());
        for c in cases {
            let b = self.plain_target(&c.dest)?;
            sorted.push((c.value, b));
        }
        sorted.sort_unstable_by_key(|&(k, _)| k);
        let n = sorted.len() as u64;
        let (lo, hi) = (sorted[0].0, sorted[sorted.len() - 1].0);
        let range = u128::from(hi) - u128::from(lo) + 1;
        let dense = n >= u64::from(self.opts.jump_table_min_cases)
            && range <= u128::from(n) * u128::from(self.opts.jump_table_max_spread)
            && range <= u128::from(MAX_TABLE);
        if dense {
            self.jump_table(z, &sorted, lo, range as u64, default_block)
        } else {
            self.compare_tree(z, &sorted, default_block)
        }
    }

    fn jump_table(
        &mut self,
        z: crate::Reg,
        sorted: &[(u64, Block)],
        lo: u64,
        range: u64,
        default: Block,
    ) -> Result<()> {
        let idx = if lo == 0 {
            z
        } else if ImmRange::I32.fits(lo as i64) {
            self.emit_def(op::SUB64, &[z.into(), Operand::Imm(lo as i64)])?
        } else {
            let k = self.emit_def(op::MOV64RI, &[Operand::Imm(lo as i64)])?;
            self.emit_def(op::SUB64, &[z.into(), k.into()])?
        };
        let table = self.new_block()?;
        let _ = self.emit(op::CMP64, &[idx.into(), Operand::Imm(range as i64 - 1)])?;
        self.branch(Cond::A, default, table)?;
        self.cur = table;
        let t = self.new_reg()?;
        let mut ops: Vec<Operand> = Vec::with_capacity(range as usize + 2);
        ops.push(t.into());
        ops.push(idx.into());
        let mut k = 0usize;
        for i in 0..range {
            let target = match sorted.get(k) {
                Some(&(value, b)) if value - lo == i => {
                    k += 1;
                    b
                }
                _ => default,
            };
            let c = self.f.block_call(target, &[])?;
            ops.push(c.into());
        }
        let _ = self.emit(op::JMPTBL, &ops)?;
        Ok(())
    }

    fn compare_tree(
        &mut self,
        z: crate::Reg,
        sorted: &[(u64, Block)],
        default: Block,
    ) -> Result<()> {
        // An explicit stack of (block, cases[lo..hi]) keeps the walk
        // iterative however many cases there are.
        let mut stack: Vec<(Block, usize, usize)> = Vec::new();
        stack.push((self.cur, 0, sorted.len()));
        while let Some((block, lo, hi)) = stack.pop() {
            self.cur = block;
            if hi - lo <= 3 {
                for (k, &(value, target)) in sorted.iter().enumerate().take(hi).skip(lo) {
                    self.cmp_case(z, value)?;
                    let next = if k + 1 < hi {
                        self.new_block()?
                    } else {
                        default
                    };
                    self.branch(Cond::E, target, next)?;
                    if k + 1 < hi {
                        self.cur = next;
                    }
                }
                if lo == hi {
                    let c = self.f.block_call(default, &[])?;
                    let _ = self.emit(op::JMP, &[c.into()])?;
                }
                continue;
            }
            let mid = lo + (hi - lo) / 2;
            let (value, target) = sorted[mid];
            self.cmp_case(z, value)?;
            let rest = self.new_block()?;
            self.branch(Cond::E, target, rest)?;
            self.cur = rest;
            // Compare again rather than carry the flags across a block.
            self.cmp_case(z, value)?;
            let left = self.new_block()?;
            let right = self.new_block()?;
            self.branch(Cond::B, left, right)?;
            stack.push((right, mid + 1, hi));
            stack.push((left, lo, mid));
        }
        Ok(())
    }

    fn cmp_case(&mut self, z: crate::Reg, value: u64) -> Result<()> {
        let rhs = if ImmRange::I32.fits(value as i64) {
            Operand::Imm(value as i64)
        } else {
            self.emit_def(op::MOV64RI, &[Operand::Imm(value as i64)])?
                .into()
        };
        let _ = self.emit(op::CMP64, &[z.into(), rhs])?;
        Ok(())
    }
}

#[allow(dead_code, reason = "keeps the error type in this module's vocabulary")]
fn _anchor(e: MirError) -> MirError {
    e
}
