//! Checking instructions against their descriptions, and whole functions for
//! structure: terminators, branch arities, single definitions.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use super::entity::{Block, Inst, StackSlot, Symbol};
use super::function::Function;
use super::operand::{BlockCall, Operand};
use crate::regs::{PReg, Reg, RegClass, VReg};
use crate::target::{InstKind, Opcode, OperandRule};

/// Why an instruction or function is malformed.
///
/// Every variant names where the problem is: the instruction (and operand
/// index), the block, or the entity that does not exist.
///
/// # Examples
///
/// ```
/// use backend_lang::mir::{Function, MirError, Operand};
/// use backend_lang::x86_64::{op, SYSV, XMM0};
///
/// let mut f = Function::new("f", &SYSV);
/// let b = f.block()?;
/// // A vector register where add64 wants a general-purpose one.
/// let err = f.push(b, op::ADD64, &[XMM0.into(), XMM0.into(), Operand::Imm(1)]).unwrap_err();
/// assert!(matches!(err, MirError::RegClass { index: 0, .. }));
/// assert_eq!(err.to_string(), "inst0: operand 0 is a register of the wrong class");
/// # Ok::<(), MirError>(())
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MirError {
    /// The target has no such opcode.
    UnknownOpcode {
        /// The instruction.
        inst: Inst,
        /// The opcode.
        opcode: Opcode,
    },
    /// The instruction has the wrong number of explicit operands. (For
    /// `pcopy`, an odd number.)
    OperandCount {
        /// The instruction.
        inst: Inst,
        /// How many the opcode takes.
        expected: usize,
        /// How many were given.
        found: usize,
    },
    /// An operand has a shape its position does not take (a memory operand
    /// where only a register fits, a slot with an offset in a copy, an
    /// unknown condition code, ...). The opcode's
    /// [`OperandRule`](crate::target::OperandRule)s list what fits.
    OperandShape {
        /// The instruction.
        inst: Inst,
        /// The operand's position.
        index: usize,
    },
    /// A register operand is of the wrong class (or, for a tied use or a
    /// copy, not of its partner's class).
    RegClass {
        /// The instruction.
        inst: Inst,
        /// The operand's position.
        index: usize,
    },
    /// An immediate is outside the range its position takes. Load wider
    /// constants into a register first (`mov64ri`).
    ImmediateRange {
        /// The instruction.
        inst: Inst,
        /// The operand's position.
        index: usize,
        /// The immediate.
        value: i64,
    },
    /// The instruction has more memory operands than the target allows in
    /// one instruction (one, on x86-64).
    TooManyMemOperands {
        /// The instruction.
        inst: Inst,
    },
    /// A branch passes a different number of arguments than its target block
    /// has parameters. (Add a block's parameters before branches to it.)
    ArgCount {
        /// The branch.
        inst: Inst,
        /// The target block.
        block: Block,
        /// The block's parameter count.
        expected: usize,
        /// The arguments given.
        found: usize,
    },
    /// A branch argument's class differs from its parameter's.
    ArgClass {
        /// The branch.
        inst: Inst,
        /// The target block.
        block: Block,
        /// The argument's position.
        index: usize,
    },
    /// A parallel copy writes the same register or slot twice.
    DuplicateDestination {
        /// The copy.
        inst: Inst,
    },
    /// A block parameter that is not a register or a whole stack slot.
    BadParam {
        /// The block.
        block: Block,
    },
    /// A branch argument that is not a register, a whole stack slot, or an
    /// immediate.
    BadArg {
        /// The target block.
        block: Block,
    },
    /// A virtual register the function does not have.
    UnknownVReg(VReg),
    /// A physical register the target does not have.
    UnknownPReg(PReg),
    /// A stack slot the function does not have.
    UnknownSlot(StackSlot),
    /// A block the function does not have, or a branch target this function
    /// did not make.
    UnknownBlock(Block),
    /// A symbol the function does not have.
    UnknownSymbol(Symbol),
    /// A register class the target does not have.
    UnknownClass(RegClass),
    /// A virtual register is defined more than once (by instructions or as a
    /// block parameter). Before register allocation MIR is in SSA form.
    MultipleDefs {
        /// The register.
        vreg: VReg,
    },
    /// A block does not end with a terminator (jump, branch, return, trap),
    /// or is empty.
    MissingTerminator {
        /// The block.
        block: Block,
    },
    /// A terminator is followed by more instructions in its block.
    MisplacedTerminator {
        /// The terminator.
        inst: Inst,
    },
    /// The function has no blocks.
    NoBlocks,
    /// A call-site record (stack map or landing pad) names an instruction
    /// that is not a call.
    NotACall {
        /// The instruction.
        inst: Inst,
    },
    /// A virtual register is used but never defined
    /// ([`Function::verify_ssa`]).
    UndefinedVReg {
        /// The register.
        vreg: VReg,
    },
    /// A use of a virtual register in a block reachable from the entry is not
    /// dominated by its definition ([`Function::verify_ssa`]).
    NotDominated {
        /// The register.
        vreg: VReg,
        /// The instruction that uses it.
        inst: Inst,
    },
    /// A table of the function (instructions, operands, blocks, ...) would
    /// exceed its 32-bit ids, or virtual registers would exceed
    /// [`VReg::MAX`].
    Limit,
}

impl fmt::Display for MirError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MirError::UnknownOpcode { inst, opcode } => {
                write!(f, "{inst}: opcode {} is not in the target", opcode.index())
            }
            MirError::OperandCount {
                inst,
                expected,
                found,
            } => write!(f, "{inst}: takes {expected} operands, {found} given"),
            MirError::OperandShape { inst, index } => {
                write!(
                    f,
                    "{inst}: operand {index} has a shape this position does not take"
                )
            }
            MirError::RegClass { inst, index } => {
                write!(
                    f,
                    "{inst}: operand {index} is a register of the wrong class"
                )
            }
            MirError::ImmediateRange { inst, index, value } => write!(
                f,
                "{inst}: immediate {value} in operand {index} is out of range; load it into a register"
            ),
            MirError::TooManyMemOperands { inst } => {
                write!(f, "{inst}: too many memory operands for one instruction")
            }
            MirError::ArgCount {
                inst,
                block,
                expected,
                found,
            } => write!(
                f,
                "{inst}: {block} has {expected} parameters but {found} arguments are passed"
            ),
            MirError::ArgClass { inst, block, index } => write!(
                f,
                "{inst}: argument {index} to {block} is not of its parameter's class"
            ),
            MirError::DuplicateDestination { inst } => {
                write!(f, "{inst}: a parallel copy writes one destination twice")
            }
            MirError::BadParam { block } => write!(
                f,
                "{block}: a parameter must be a register or a whole stack slot"
            ),
            MirError::BadArg { block } => write!(
                f,
                "an argument to {block} must be a register, a whole stack slot, or an immediate"
            ),
            MirError::UnknownVReg(v) => write!(f, "virtual register {v:?} does not exist"),
            MirError::UnknownPReg(p) => {
                write!(f, "physical register {} is not in the target", p.index())
            }
            MirError::UnknownSlot(s) => write!(f, "stack slot {s} does not exist"),
            MirError::UnknownBlock(b) => write!(f, "{b} does not exist"),
            MirError::UnknownSymbol(s) => write!(f, "symbol {s} does not exist"),
            MirError::UnknownClass(c) => {
                write!(f, "register class {} is not in the target", c.index())
            }
            MirError::MultipleDefs { vreg } => {
                write!(f, "{vreg:?} is defined more than once")
            }
            MirError::MissingTerminator { block } => {
                write!(
                    f,
                    "{block} does not end with a jump, branch, return, or trap"
                )
            }
            MirError::MisplacedTerminator { inst } => {
                write!(
                    f,
                    "{inst}: a terminator must be the last instruction of its block"
                )
            }
            MirError::NoBlocks => f.write_str("the function has no blocks"),
            MirError::NotACall { inst } => {
                write!(
                    f,
                    "{inst}: a call-site record names an instruction that is not a call"
                )
            }
            MirError::UndefinedVReg { vreg } => {
                write!(f, "{vreg:?} is used but never defined")
            }
            MirError::NotDominated { vreg, inst } => write!(
                f,
                "{inst}: the use of {vreg:?} is not dominated by its definition"
            ),
            MirError::Limit => f.write_str("the function exceeds its 32-bit tables"),
        }
    }
}

impl core::error::Error for MirError {}

/// A location key for duplicate detection: registers and slots in disjoint
/// ranges of `u64`.
fn loc_key(op: &Operand) -> Option<u64> {
    match op {
        Operand::Reg(r) => Some(match (r.as_phys(), r.as_virt()) {
            (Some(p), _) => p.index() as u64,
            (_, Some(v)) => (1 << 32) | v.index() as u64,
            _ => return None,
        }),
        Operand::Slot { slot, .. } => Some((2 << 32) | slot.index() as u64),
        _ => None,
    }
}

impl Function {
    /// Checks one instruction's operands against its opcode's description.
    pub(crate) fn check_inst(
        &self,
        inst: Inst,
        opcode: Opcode,
        operands: &[Operand],
    ) -> Result<(), MirError> {
        let target = self.target();
        let desc = target
            .inst(opcode)
            .ok_or(MirError::UnknownOpcode { inst, opcode })?;
        match desc.kind() {
            InstKind::Copy => {
                if operands.len() != 2 {
                    return Err(MirError::OperandCount {
                        inst,
                        expected: 2,
                        found: operands.len(),
                    });
                }
                self.check_copy_pair(inst, operands, 0, 1)
            }
            InstKind::ParallelCopy => {
                if operands.len() % 2 != 0 {
                    return Err(MirError::OperandCount {
                        inst,
                        expected: operands.len() + 1,
                        found: operands.len(),
                    });
                }
                let n = operands.len() / 2;
                for i in 0..n {
                    self.check_copy_pair(inst, operands, i, n + i)?;
                }
                let mut keys: Vec<u64> = operands[..n].iter().filter_map(loc_key).collect();
                keys.sort_unstable();
                if keys.windows(2).any(|w| w[0] == w[1]) {
                    return Err(MirError::DuplicateDestination { inst });
                }
                Ok(())
            }
            InstKind::Switch => {
                // The fixed operands, then one argument-less target per index.
                let rules = desc.operands();
                if operands.len() <= rules.len() {
                    return Err(MirError::OperandCount {
                        inst,
                        expected: rules.len() + 1,
                        found: operands.len(),
                    });
                }
                let mut mems = 0usize;
                for (index, (rule, op)) in rules.iter().zip(operands).enumerate() {
                    self.check_operand(inst, index, *rule, op, rules, &mut mems)?;
                }
                for (index, op) in operands.iter().enumerate().skip(rules.len()) {
                    match op {
                        Operand::Block(call) => {
                            self.check_block_call(inst, call)?;
                            if !call.is_empty() {
                                return Err(MirError::ArgCount {
                                    inst,
                                    block: call.block(),
                                    expected: 0,
                                    found: call.len(),
                                });
                            }
                        }
                        _ => return Err(MirError::OperandShape { inst, index }),
                    }
                }
                Ok(())
            }
            _ => {
                let rules = desc.operands();
                if rules.len() != operands.len() {
                    return Err(MirError::OperandCount {
                        inst,
                        expected: rules.len(),
                        found: operands.len(),
                    });
                }
                let mut mems = 0usize;
                for (index, (rule, op)) in rules.iter().zip(operands).enumerate() {
                    self.check_operand(inst, index, *rule, op, rules, &mut mems)?;
                }
                if mems > usize::from(target.max_mem_operands()) {
                    return Err(MirError::TooManyMemOperands { inst });
                }
                Ok(())
            }
        }
    }

    /// A copy's destination (register or whole slot) and source (register,
    /// whole slot, or immediate); registers on both sides share a class.
    fn check_copy_pair(
        &self,
        inst: Inst,
        ops: &[Operand],
        dst: usize,
        src: usize,
    ) -> Result<(), MirError> {
        let shape = |index| MirError::OperandShape { inst, index };
        let (d, s) = (ops[dst], ops[src]);
        match d {
            Operand::Reg(r) => self.check_reg(r)?,
            Operand::Slot { slot, offset: 0 } => self.check_slot(slot)?,
            _ => return Err(shape(dst)),
        }
        match s {
            Operand::Reg(r) => self.check_reg(r)?,
            Operand::Slot { slot, offset: 0 } => self.check_slot(slot)?,
            Operand::Imm(_) => {}
            _ => return Err(shape(src)),
        }
        if let (Some(a), Some(b)) = (d.reg(), s.reg()) {
            if self.reg_class(a) != self.reg_class(b) {
                return Err(MirError::RegClass { inst, index: src });
            }
        }
        Ok(())
    }

    fn reg_in(&self, inst: Inst, index: usize, reg: Reg, class: RegClass) -> Result<(), MirError> {
        self.check_reg(reg)?;
        if self.reg_class(reg) == Some(class) {
            Ok(())
        } else {
            Err(MirError::RegClass { inst, index })
        }
    }

    fn check_operand(
        &self,
        inst: Inst,
        index: usize,
        rule: OperandRule,
        op: &Operand,
        rules: &[OperandRule],
        mems: &mut usize,
    ) -> Result<(), MirError> {
        let target = self.target();
        let shape = MirError::OperandShape { inst, index };
        match rule {
            OperandRule::Def(class) => match op {
                Operand::Reg(r) => self.reg_in(inst, index, *r, class),
                _ => Err(shape),
            },
            OperandRule::Tied(def) => {
                let class = match rules.get(usize::from(def)) {
                    Some(OperandRule::Def(c)) => *c,
                    _ => return Err(shape),
                };
                match op {
                    Operand::Reg(r) => self.reg_in(inst, index, *r, class),
                    _ => Err(shape),
                }
            }
            OperandRule::Use(class, accept) => match *op {
                Operand::Reg(r) if accept.reg() => self.reg_in(inst, index, r, class),
                Operand::Imm(value) if accept.imm() != crate::target::ImmRange::None => {
                    if accept.imm().fits(value) {
                        Ok(())
                    } else {
                        Err(MirError::ImmediateRange { inst, index, value })
                    }
                }
                Operand::Mem(m) if accept.mem() => {
                    *mems += 1;
                    for r in m.regs() {
                        self.reg_in(inst, index, r, target.addr_class())?;
                    }
                    Ok(())
                }
                Operand::Slot { slot, .. } if accept.mem() => {
                    *mems += 1;
                    self.check_slot(slot)
                }
                Operand::Symbol(s) if accept.symbol() => {
                    if s.index() < self.symbols.len() {
                        Ok(())
                    } else {
                        Err(MirError::UnknownSymbol(s))
                    }
                }
                _ => Err(shape),
            },
            OperandRule::Cond => match op {
                Operand::Cond(c) if c.code() < target.conds() => Ok(()),
                _ => Err(shape),
            },
            OperandRule::Block => match op {
                Operand::Block(call) => self.check_block_call(inst, call),
                _ => Err(shape),
            },
        }
    }

    fn check_block_call(&self, inst: Inst, call: &BlockCall) -> Result<(), MirError> {
        let block = call.block();
        let params = self
            .blocks
            .get(block.index())
            .ok_or(MirError::UnknownBlock(block))?
            .params
            .as_slice();
        // A call made by another function would index past this pool.
        let args = self
            .args
            .get(call.range())
            .ok_or(MirError::UnknownBlock(block))?;
        if args.len() != params.len() {
            return Err(MirError::ArgCount {
                inst,
                block,
                expected: params.len(),
                found: args.len(),
            });
        }
        for (index, (a, p)) in args.iter().zip(params).enumerate() {
            if let (Some(a), Some(p)) = (a.reg(), p.reg()) {
                if self.reg_class(a) != self.reg_class(p) {
                    return Err(MirError::ArgClass { inst, block, index });
                }
            }
        }
        Ok(())
    }

    /// Checks the whole function: every instruction against its opcode (again,
    /// since block parameters may have been added after branches to them),
    /// every block ends with exactly one terminator, and every virtual
    /// register is defined at most once (MIR before register allocation is
    /// SSA; block parameters count as definitions).
    ///
    /// Runs in time linear in the function's size. Dominance (every use
    /// defined on every path to it) is the register allocator's
    /// precondition and is checked with it (backend-lang 0.5).
    ///
    /// # Errors
    ///
    /// The first [`MirError`] found, in block order.
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Function, MirError, Operand};
    /// use backend_lang::x86_64::{op, GPR, SYSV};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let b = f.block()?;
    /// let v = f.vreg(GPR)?;
    /// f.push(b, op::MOV64RI, &[v.into(), Operand::Imm(1)])?;
    /// assert_eq!(f.verify(), Err(MirError::MissingTerminator { block: b }));
    /// f.push(b, op::MOV64RI, &[v.into(), Operand::Imm(2)])?;
    /// f.push(b, op::RET, &[])?;
    /// assert_eq!(f.verify(), Err(MirError::MultipleDefs { vreg: v.as_virt().unwrap() }));
    /// # Ok::<(), MirError>(())
    /// ```
    pub fn verify(&self) -> Result<(), MirError> {
        if self.blocks.is_empty() {
            return Err(MirError::NoBlocks);
        }
        let target = self.target();
        let mut defined = vec![false; self.vregs.len()];
        let mut define = |r: Option<Reg>| -> Result<(), MirError> {
            if let Some(v) = r.and_then(Reg::as_virt) {
                let seen = defined.get_mut(v.index()).ok_or(MirError::UnknownVReg(v))?;
                if *seen {
                    return Err(MirError::MultipleDefs { vreg: v });
                }
                *seen = true;
            }
            Ok(())
        };
        for (b, data) in self.blocks.iter().enumerate() {
            let block = Block::from_index(b);
            for p in &data.params {
                define(p.reg())?;
            }
            let Some(&last) = data.insts.last() else {
                return Err(MirError::MissingTerminator { block });
            };
            for &inst in data.insts.iter() {
                let d = self.insts.get(inst.index()).ok_or(MirError::Limit)?;
                let ops = self.operands(inst);
                self.check_inst(inst, d.opcode, ops)?;
                let desc = target.inst(d.opcode).ok_or(MirError::UnknownOpcode {
                    inst,
                    opcode: d.opcode,
                })?;
                let defs = match desc.kind() {
                    InstKind::Copy => 1,
                    InstKind::ParallelCopy => ops.len() / 2,
                    _ => desc.defs(),
                };
                for op in ops.iter().take(defs) {
                    define(op.reg())?;
                }
                let terminator = desc.kind().is_terminator();
                if terminator && inst != last {
                    return Err(MirError::MisplacedTerminator { inst });
                }
                if !terminator && inst == last {
                    return Err(MirError::MissingTerminator { block });
                }
            }
        }
        Ok(())
    }
}
