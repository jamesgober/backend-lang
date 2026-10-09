//! Walking the registers an instruction reads and writes, with a position for
//! each, so passes can read and rewrite them without knowing operand shapes.

use super::entity::Inst;
use super::function::Function;
use super::operand::{Mem, Operand};
use crate::regs::Reg;
use crate::target::{InstKind, OperandRule};

/// Where a register sits inside a function's operand pools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegPos {
    /// A register operand (`operands[i]`).
    Op(u32),
    /// The base register of a memory operand (`operands[i]`).
    MemBase(u32),
    /// The index register of a memory operand (`operands[i]`).
    MemIndex(u32),
    /// A branch argument (`args[i]`).
    Arg(u32),
}

/// Whether a register is read or written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    /// Read by the instruction (for a branch argument: on the edge).
    Use,
    /// Written by the instruction.
    Def,
}

/// How many leading operands of an instruction are definitions.
pub(crate) fn def_count(f: &Function, inst: Inst) -> usize {
    let ops = f.operands(inst);
    let Some(desc) = f.opcode(inst).and_then(|o| f.target().inst(o)) else {
        return 0;
    };
    match desc.kind() {
        InstKind::Copy => ops.len().min(1),
        InstKind::ParallelCopy => ops.len() / 2,
        _ => desc.defs(),
    }
}

impl Function {
    /// Calls `visit` for every register `inst` names explicitly, with its
    /// position and access, definitions first. Memory-operand registers and
    /// branch arguments are uses.
    pub(crate) fn visit_regs(&self, inst: Inst, mut visit: impl FnMut(RegPos, Reg, Access)) {
        let Some(data) = self.insts.get(inst.index()) else {
            return;
        };
        let defs = def_count(self, inst);
        let base = data.ops;
        let ops = self.operands(inst);
        let rules: &[OperandRule] = self
            .target()
            .inst(data.opcode)
            .map_or(&[], |d| d.operands());
        // Definitions first, so a caller sees them before the uses of the
        // same instruction.
        for (i, op) in ops.iter().enumerate().take(defs) {
            if let Operand::Reg(r) = op {
                visit(RegPos::Op(base + i as u32), *r, Access::Def);
            }
        }
        for (i, op) in ops.iter().enumerate().skip(defs) {
            let at = base + i as u32;
            match op {
                Operand::Reg(r) => {
                    // A `Def` rule past the leading definitions does not
                    // occur in any table; treat every other register as read.
                    let access = match rules.get(i) {
                        Some(OperandRule::Def(_)) => Access::Def,
                        _ => Access::Use,
                    };
                    visit(RegPos::Op(at), *r, access);
                }
                Operand::Mem(m) => {
                    if let Some(b) = m.base_reg() {
                        visit(RegPos::MemBase(at), b, Access::Use);
                    }
                    if let Some((x, _)) = m.index_reg() {
                        visit(RegPos::MemIndex(at), x, Access::Use);
                    }
                }
                Operand::Block(call) => {
                    let range = call.range();
                    for (k, a) in self
                        .args
                        .get(range.clone())
                        .unwrap_or(&[])
                        .iter()
                        .enumerate()
                    {
                        if let Operand::Reg(r) = a {
                            visit(RegPos::Arg((range.start + k) as u32), *r, Access::Use);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Replaces the register at `pos`.
    pub(crate) fn set_reg_at(&mut self, pos: RegPos, reg: Reg) {
        match pos {
            RegPos::Op(i) => {
                if let Some(op) = self.operands.get_mut(i as usize) {
                    *op = Operand::Reg(reg);
                }
            }
            RegPos::MemBase(i) => {
                if let Some(Operand::Mem(m)) = self.operands.get_mut(i as usize) {
                    let mut out = Mem::base(reg).with_disp(m.disp());
                    if let Some((x, s)) = m.index_reg() {
                        out = out.with_index(x, s);
                    }
                    *m = out;
                }
            }
            RegPos::MemIndex(i) => {
                if let Some(Operand::Mem(m)) = self.operands.get_mut(i as usize) {
                    if let Some((_, s)) = m.index_reg() {
                        *m = m.with_index(reg, s);
                    }
                }
            }
            RegPos::Arg(i) => {
                if let Some(a) = self.args.get_mut(i as usize) {
                    *a = Operand::Reg(reg);
                }
            }
        }
    }

    /// Replaces the whole operand at `pos` (a register operand or argument)
    /// with `op`, such as a stack slot or an immediate.
    pub(crate) fn set_operand_at(&mut self, pos: RegPos, op: Operand) {
        match pos {
            RegPos::Op(i) => {
                if let Some(o) = self.operands.get_mut(i as usize) {
                    *o = op;
                }
            }
            RegPos::Arg(i) => {
                if let Some(a) = self.args.get_mut(i as usize) {
                    *a = op;
                }
            }
            RegPos::MemBase(_) | RegPos::MemIndex(_) => {}
        }
    }
}
