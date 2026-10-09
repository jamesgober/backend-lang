//! The deterministic text form of a function; the grammar is documented on
//! the [`mir`](crate::mir) module.

use core::fmt::{self, Write};

use super::entity::Block;
use super::function::{Function, SlotKind};
use super::operand::{ImplicitKind, Operand};
use crate::regs::{Reg, RegSet};
use crate::target::{InstKind, Target};

impl Function {
    fn write_reg(&self, f: &mut fmt::Formatter<'_>, reg: Reg, with_class: bool) -> fmt::Result {
        let target = self.target();
        match (reg.as_phys(), reg.as_virt()) {
            (Some(p), _) => f.write_str(target.reg(p).name()),
            (_, Some(v)) => {
                write!(f, "%{}", v.index())?;
                if with_class {
                    let class = self
                        .vreg_class(v)
                        .map_or("<invalid>", |c| target.class(c).name());
                    write!(f, ":{class}")?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn write_operand(&self, f: &mut fmt::Formatter<'_>, op: &Operand, def: bool) -> fmt::Result {
        let target = self.target();
        match *op {
            Operand::Reg(r) => self.write_reg(f, r, def),
            Operand::Imm(v) => write!(f, "{v}"),
            Operand::Mem(m) => {
                f.write_char('[')?;
                let mut first = true;
                if let Some(b) = m.base_reg() {
                    self.write_reg(f, b, false)?;
                    first = false;
                }
                if let Some((i, s)) = m.index_reg() {
                    if !first {
                        f.write_str(" + ")?;
                    }
                    self.write_reg(f, i, false)?;
                    write!(f, "*{}", s.factor())?;
                    first = false;
                }
                let d = m.disp();
                if first {
                    write!(f, "{d}")?;
                } else if d > 0 {
                    write!(f, " + {d}")?;
                } else if d < 0 {
                    write!(f, " - {}", i64::from(d).unsigned_abs())?;
                }
                f.write_char(']')
            }
            Operand::Slot { slot, offset } => {
                write!(f, "{slot}")?;
                if offset != 0 {
                    write!(f, "{offset:+}")?;
                }
                Ok(())
            }
            Operand::Symbol(s) => write!(f, "@{}", self.symbol_name(s).unwrap_or("<invalid>")),
            Operand::Cond(c) => f.write_str(target.cond_name(c)),
            Operand::Block(call) => {
                write!(f, "{}(", call.block())?;
                for (i, a) in self.args(call).iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    self.write_operand(f, a, false)?;
                }
                f.write_char(')')
            }
        }
    }

    fn write_list(&self, f: &mut fmt::Formatter<'_>, ops: &[Operand], def: bool) -> fmt::Result {
        for (i, op) in ops.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            self.write_operand(f, op, def)?;
        }
        Ok(())
    }

    fn write_block_header(&self, f: &mut fmt::Formatter<'_>, block: Block) -> fmt::Result {
        write!(f, "{block}")?;
        let params = self.params(block);
        if !params.is_empty() {
            f.write_char('(')?;
            self.write_list(f, params, true)?;
            f.write_char(')')?;
        }
        f.write_str(":\n")
    }
}

fn write_regs(
    f: &mut fmt::Formatter<'_>,
    target: &Target,
    label: &str,
    regs: RegSet,
) -> fmt::Result {
    if regs.is_empty() {
        return Ok(());
    }
    write!(f, " {label}(")?;
    for (i, r) in regs.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        f.write_str(target.reg(r).name())?;
    }
    f.write_char(')')
}

impl fmt::Display for Function {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let target = self.target();
        writeln!(f, "function {} ({}) {{", self.name(), self.abi().name())?;
        if !self.frame_pointer() {
            f.write_str("    omit-frame-pointer\n")?;
        }
        if self.outgoing() > 0 {
            writeln!(f, "    outgoing {}", self.outgoing())?;
        }
        for (i, s) in self.slots().iter().enumerate() {
            match *s {
                SlotKind::Local { size, align } => {
                    writeln!(f, "    ss{i} = local {size}, align {align}")?;
                }
                SlotKind::Incoming { offset, size } => {
                    writeln!(f, "    ss{i} = incoming {offset}, size {size}")?;
                }
            }
        }
        for (i, name) in self.symbols.iter().enumerate() {
            writeln!(f, "    sym{i} = @{name}")?;
        }
        for block in self.blocks() {
            self.write_block_header(f, block)?;
            for &inst in self.insts(block) {
                let Some(opcode) = self.opcode(inst) else {
                    continue;
                };
                let ops = self.operands(inst);
                let (name, defs) = match target.inst(opcode) {
                    Some(d) => (
                        d.name(),
                        match d.kind() {
                            InstKind::Copy => ops.len().min(1),
                            InstKind::ParallelCopy => ops.len() / 2,
                            _ => d.defs().min(ops.len()),
                        },
                    ),
                    None => ("<invalid>", 0),
                };
                f.write_str("    ")?;
                if defs > 0 {
                    self.write_list(f, &ops[..defs], true)?;
                    f.write_str(" = ")?;
                }
                f.write_str(name)?;
                if ops.len() > defs {
                    f.write_char(' ')?;
                    self.write_list(f, &ops[defs..], false)?;
                }
                let imp = self.implicit(inst);
                let uses: RegSet = imp
                    .iter()
                    .filter(|i| i.kind == ImplicitKind::Use)
                    .map(|i| i.reg)
                    .collect();
                let defs: RegSet = imp
                    .iter()
                    .filter(|i| i.kind == ImplicitKind::Def)
                    .map(|i| i.reg)
                    .collect();
                write_regs(f, target, "implicit", uses)?;
                write_regs(f, target, "implicit-def", defs)?;
                write_regs(f, target, "clobber", self.clobbers(inst))?;
                f.write_char('\n')?;
            }
        }
        f.write_str("}\n")
    }
}
