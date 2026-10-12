//! Rewriting the function: dead-code removal before allocation, spill code
//! between rounds, and the final substitution of physical registers.
//!
//! Every rewrite works from a [`UseIndex`] built once per allocation (each
//! value's definition site and the positions of its uses), so a spill round
//! touches only the spilled values' definitions and uses and the blocks that
//! contain them, not the whole function; what it changed is reported
//! ([`SpillReport`]) so liveness can be updated rather than recomputed.

use alloc::vec;
use alloc::vec::Vec;

use super::color::is_remat;
use super::liveness::Analysis;
use super::{Location, RegAllocError};
use crate::mir::cfg::Cfg;
use crate::mir::visit::{Access, RegPos};
use crate::mir::{Block, Effects, Function, Inst, MirError, Operand, StackSlot};
use crate::regs::{Reg, VReg};
use crate::target::{InstKind, Opcode, OperandRule};

/// What survives between rounds: which values may still be spilled, and the
/// running counts.
pub(crate) struct SpillState {
    unspillable: Vec<bool>,
    pub(crate) spilled: u32,
    pub(crate) rematerialized: u32,
}

impl SpillState {
    pub(crate) fn new(f: &Function) -> SpillState {
        let mut state = SpillState {
            unspillable: vec![false; f.vreg_count()],
            spilled: 0,
            rematerialized: 0,
        };
        // A value a terminator defines (a jump table's address temporary)
        // has nowhere to put a store after its definition: it always gets a
        // register, and something else is spilled to make room.
        for b in f.blocks() {
            if let Some(&last) = f.insts(b).last() {
                if kind_of(f, last).is_terminator() {
                    f.visit_regs(last, |_, r, access| {
                        if let (Access::Def, Some(v)) = (access, r.as_virt()) {
                            state.pin(v.index() as u32);
                        }
                    });
                }
            }
        }
        state
    }

    /// Whether `v` may still be spilled (not a reload temporary, not a value
    /// already spilled).
    pub(crate) fn spillable(&self, v: u32) -> bool {
        !self.unspillable.get(v as usize).copied().unwrap_or(true)
    }

    fn pin(&mut self, v: u32) {
        let i = v as usize;
        if i >= self.unspillable.len() {
            self.unspillable.resize(i + 1, false);
        }
        self.unspillable[i] = true;
    }
}

/// The definition site of a virtual register.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Def {
    /// Never defined in reachable code.
    None,
    /// A block parameter.
    Param { block: Block, index: usize },
    /// An instruction.
    Inst(Inst),
}

/// Each original value's definition site and the positions of its uses in
/// reachable code (bucketed by value, in layout order), built in one walk.
/// Positions in the operand pools never move, and a spill round only adds
/// instructions that use the value being spilled or a fresh temporary, so
/// the index stays exact for every value that can still be spilled.
pub(crate) struct UseIndex {
    pub(crate) def: Vec<Def>,
    start: Vec<u32>,
    uses: Vec<(Inst, RegPos)>,
    /// Instructions removed from their blocks (dead code, rematerialized
    /// definitions), by instruction id.
    dead: Vec<bool>,
}

impl UseIndex {
    /// Builds the index in one walk of the reachable blocks in reverse
    /// postorder, checking SSA dominance on the way (the allocator's
    /// precondition, exactly [`Function::verify_ssa`]'s rule): a definition
    /// that dominates a use is always met before it in this order (an
    /// earlier block, or earlier in the same block), so a use whose
    /// definition has not been met is the error, found where it occurs.
    pub(crate) fn new(f: &Function, cfg: &Cfg) -> Result<UseIndex, MirError> {
        let nv = f.vreg_count();
        let mut def = vec![Def::None; nv];
        let mut raw: Vec<(u32, Inst, RegPos)> = Vec::with_capacity(f.inst_count() * 2);
        let mut count = vec![0u32; nv + 1];
        let mut defs_here: Vec<u32> = Vec::new();
        for &b in &cfg.rpo {
            let block = Block::from_index(b as usize);
            for (index, p) in f.params(block).iter().enumerate() {
                if let Some(v) = p.reg().and_then(Reg::as_virt) {
                    if let Some(d) = def.get_mut(v.index()) {
                        *d = Def::Param { block, index };
                    }
                }
            }
            for &inst in f.insts(block) {
                let mut err = None;
                defs_here.clear();
                f.visit_regs(inst, |pos, r, access| {
                    let Some(v) = r.as_virt() else { return };
                    match access {
                        // Applied after the uses: an instruction does not
                        // dominate its own operands.
                        Access::Def => defs_here.push(v.index() as u32),
                        Access::Use => {
                            let dominated = match def.get(v.index()) {
                                Some(Def::Param { block: d, .. }) => {
                                    cfg.dominates(d.index(), b as usize)
                                }
                                Some(Def::Inst(d)) => f
                                    .inst_block(*d)
                                    .is_some_and(|db| cfg.dominates(db.index(), b as usize)),
                                _ => false,
                            };
                            if !dominated && err.is_none() {
                                err = Some(v);
                            }
                            if let Some(c) = count.get_mut(v.index() + 1) {
                                *c += 1;
                                raw.push((v.index() as u32, inst, pos));
                            }
                        }
                    }
                });
                if let Some(v) = err {
                    return Err(undominated(f, v, inst));
                }
                for &v in &defs_here {
                    if let Some(d) = def.get_mut(v as usize) {
                        *d = Def::Inst(inst);
                    }
                }
            }
        }
        // Bucket by value in linear time (a counting sort, stable, so each
        // value's uses stay in layout order).
        for k in 0..nv {
            count[k + 1] += count[k];
        }
        let mut fill = count.clone();
        let mut uses = vec![(Inst::new(0), RegPos::Op(0)); raw.len()];
        for (v, inst, pos) in raw {
            let at = &mut fill[v as usize];
            uses[*at as usize] = (inst, pos);
            *at += 1;
        }
        Ok(UseIndex {
            def,
            start: count,
            uses,
            dead: vec![false; f.inst_count()],
        })
    }

    /// The uses of `v` (original values only).
    pub(crate) fn uses(&self, v: usize) -> &[(Inst, RegPos)] {
        match (self.start.get(v), self.start.get(v + 1)) {
            (Some(&s), Some(&e)) => self.uses.get(s as usize..e as usize).unwrap_or(&[]),
            _ => &[],
        }
    }

    /// Whether `inst` was removed.
    pub(crate) fn is_dead(&self, inst: Inst) -> bool {
        self.dead.get(inst.index()).copied().unwrap_or(false)
    }

    fn kill(&mut self, inst: Inst) {
        if let Some(d) = self.dead.get_mut(inst.index()) {
            *d = true;
        }
    }
}

/// The error for a use of `v` at `inst` that its definition does not
/// dominate: `NotDominated` if `v` is defined anywhere (a later block, later
/// in the block, an unreachable block, or `inst` itself), else
/// `UndefinedVReg`. Only the error path scans for the definition.
fn undominated(f: &Function, v: VReg, inst: Inst) -> MirError {
    let mut defined = false;
    for b in f.blocks() {
        if f.params(b)
            .iter()
            .any(|p| p.reg().and_then(Reg::as_virt) == Some(v))
        {
            defined = true;
            break;
        }
        for &i in f.insts(b) {
            f.visit_regs(i, |_, r, access| {
                if access == Access::Def && r.as_virt() == Some(v) {
                    defined = true;
                }
            });
        }
        if defined {
            break;
        }
    }
    if defined {
        MirError::NotDominated { vreg: v, inst }
    } else {
        MirError::UndefinedVReg { vreg: v }
    }
}

/// Code to insert around existing instructions, and instructions to drop,
/// applied in one pass over the blocks that contain them.
#[derive(Default)]
struct Edits {
    /// (anchor, 0 = before / 1 = after, sequence, new instruction).
    insert: Vec<(u32, u8, u32, Inst)>,
    deleted: Vec<Inst>,
}

impl Edits {
    fn before(&mut self, anchor: Inst, new: Inst) {
        let seq = self.insert.len() as u32;
        self.insert.push((anchor.index() as u32, 0, seq, new));
    }

    fn after(&mut self, anchor: Inst, new: Inst) {
        let seq = self.insert.len() as u32;
        self.insert.push((anchor.index() as u32, 1, seq, new));
    }

    fn delete(&mut self, inst: Inst) {
        self.deleted.push(inst);
    }

    /// Rebuilds the lists of the blocks that changed; `gone` says whether an
    /// instruction is deleted.
    fn apply(mut self, f: &mut Function, gone: impl Fn(Inst) -> bool) {
        if self.insert.is_empty() && self.deleted.is_empty() {
            return;
        }
        self.insert.sort_unstable();
        // Each anchor's first insertion, by instruction id: O(1) lookups
        // while the blocks are rebuilt (anchors in a block are not in id
        // order, and a binary search per instruction is not free).
        let mut first = vec![u32::MAX; f.inst_count()];
        for (k, e) in self.insert.iter().enumerate().rev() {
            if let Some(x) = first.get_mut(e.0 as usize) {
                *x = k as u32;
            }
        }
        let mut blocks: Vec<u32> = self
            .insert
            .iter()
            .map(|e| Inst::new(e.0))
            .chain(self.deleted.iter().copied())
            .filter_map(|i| f.inst_block(i))
            .map(|b| b.index() as u32)
            .collect();
        blocks.sort_unstable();
        blocks.dedup();
        for b in blocks {
            let block = Block::from_index(b as usize);
            let mut list = f.take_block_insts(block);
            let has_inserts = list
                .iter()
                .any(|i| first.get(i.index()).is_some_and(|&k| k != u32::MAX));
            if !has_inserts {
                list.retain(|&i| !gone(i));
                f.set_block_insts(block, list);
                continue;
            }
            let mut out = Vec::with_capacity(list.len() + 4);
            for inst in list {
                let id = inst.index() as u32;
                let mut j = first.get(inst.index()).map_or(usize::MAX, |&k| k as usize);
                while let Some(&(a, place, _, new)) = self.insert.get(j) {
                    if a != id || place != 0 {
                        break;
                    }
                    out.push(new);
                    j += 1;
                }
                if !gone(inst) {
                    out.push(inst);
                }
                while let Some(&(a, _, _, new)) = self.insert.get(j) {
                    if a != id {
                        break;
                    }
                    out.push(new);
                    j += 1;
                }
            }
            f.set_block_insts(block, out);
        }
    }
}

/// Removes instructions that only compute a value nobody reads (constants,
/// addresses, copies), repeatedly, before allocation.
pub(crate) fn remove_dead_pure(f: &mut Function, index: &mut UseIndex) {
    let nv = f.vreg_count();
    let mut uses: Vec<u32> = (0..nv).map(|v| index.uses(v).len() as u32).collect();
    let removable = |f: &Function, inst: Inst| -> bool {
        let copy = kind_of(f, inst) == InstKind::Copy
            && f.operands(inst)
                .first()
                .and_then(Operand::reg)
                .is_some_and(Reg::is_virtual);
        copy || is_remat(f, inst)
    };
    let mut work: Vec<u32> = (0..nv as u32).filter(|&v| uses[v as usize] == 0).collect();
    let mut edits = Edits::default();
    while let Some(v) = work.pop() {
        let Some(Def::Inst(inst)) = index.def.get(v as usize).copied() else {
            continue;
        };
        if index.is_dead(inst) || !removable(f, inst) {
            continue;
        }
        index.kill(inst);
        edits.delete(inst);
        f.visit_regs(inst, |_, r, access| {
            if let (Access::Use, Some(u)) = (access, r.as_virt()) {
                if let Some(n) = uses.get_mut(u.index()) {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        work.push(u.index() as u32);
                    }
                }
            }
        });
    }
    edits.apply(f, |i| index.is_dead(i));
}

/// The operand rule at operand index `k` of `inst`, if the instruction has
/// fixed rules there.
fn rule_at(f: &Function, inst: Inst, k: usize) -> Option<OperandRule> {
    let desc = f.opcode(inst).and_then(|o| f.target().inst(o))?;
    match desc.kind() {
        InstKind::Copy | InstKind::ParallelCopy => None,
        _ => desc.operands().get(k).copied(),
    }
}

pub(crate) fn kind_of(f: &Function, inst: Inst) -> InstKind {
    f.opcode(inst)
        .and_then(|o| f.target().inst(o))
        .map_or(InstKind::Plain, |d| d.kind())
}

/// What a spill round changed, for the incremental liveness update.
#[derive(Default)]
pub(crate) struct SpillReport {
    /// Every value spilled this round. None is live into any block any
    /// more: each is gone (rematerialized, a slot parameter, a copy writing
    /// its slot) or lives only from its definition to the store after it.
    pub(crate) spilled: Vec<u32>,
    /// The spilled values still defined in a register: (value, its
    /// definition, the store right after it that is now its only use).
    pub(crate) stored: Vec<(u32, Inst, Inst)>,
    /// New temporaries: (value, the instruction defining it, the
    /// instruction using it). Each is defined right before its user.
    pub(crate) temps: Vec<(u32, Inst, Inst)>,
    /// The instructions that now read a temporary, each once.
    pub(crate) anchors: Vec<Inst>,
}

/// Spills `values` (sorted, without duplicates): rematerializes constants
/// and addresses at their uses, and stores everything else to a slot after
/// its definition, reading it back (directly as a memory operand where the
/// instruction allows, else through a fresh temporary) at each use.
pub(crate) fn spill(
    f: &mut Function,
    index: &mut UseIndex,
    state: &mut SpillState,
    values: &[VReg],
) -> Result<SpillReport, RegAllocError> {
    // What each spilled value becomes: a slot, or a rematerialization.
    #[derive(Clone, Copy)]
    enum How {
        Slot(StackSlot),
        Remat(Inst),
    }
    let mut report = SpillReport::default();
    let mut plan: Vec<(VReg, How)> = Vec::with_capacity(values.len());
    let mut edits = Edits::default();
    for &v in values {
        let i = v.index();
        if !state.spillable(i as u32) {
            continue;
        }
        let class = super::class_of(f, v);
        let how = match index.def.get(i).copied().unwrap_or(Def::None) {
            Def::None => continue,
            Def::Inst(inst) if is_remat(f, inst) => {
                index.kill(inst);
                edits.delete(inst);
                state.rematerialized += 1;
                How::Remat(inst)
            }
            Def::Param { block, index: k } => {
                let s = f.spill_slot(class)?;
                let mut params = f.params(block).to_vec();
                if let Some(p) = params.get_mut(k) {
                    *p = Operand::slot(s);
                }
                f.set_params(block, params);
                How::Slot(s)
            }
            Def::Inst(inst) => {
                let s = f.spill_slot(class)?;
                match kind_of(f, inst) {
                    // A copy writes the slot directly.
                    InstKind::Copy | InstKind::ParallelCopy => {
                        let mut at = None;
                        f.visit_regs(inst, |pos, r, access| {
                            if access == Access::Def && r.as_virt() == Some(v) {
                                at = Some(pos);
                            }
                        });
                        if let Some(pos) = at {
                            f.set_operand_at(pos, Operand::slot(s));
                        }
                    }
                    _ => {
                        let block = f.inst_block(inst).ok_or(MirError::Limit)?;
                        let store = f.create_inst(
                            block,
                            Opcode::COPY,
                            &[Operand::slot(s), Reg::from(v).into()],
                            Effects::new(),
                        )?;
                        edits.after(inst, store);
                        report.stored.push((i as u32, inst, store));
                    }
                }
                How::Slot(s)
            }
        };
        state.spilled += 1;
        if !matches!(how, How::Remat(_)) {
            state.pin(i as u32);
        }
        report.spilled.push(i as u32);
        plan.push((v, how));
    }

    // Rewrite each spilled value's uses, one temporary per using
    // instruction at most.
    for &(v, how) in &plan {
        let mut last: Option<(Inst, Reg)> = None;
        let index: &UseIndex = index;
        for &(inst, pos) in index.uses(v.index()) {
            if index.is_dead(inst) {
                continue;
            }
            let kind = kind_of(f, inst);
            let base = f.operand_base(inst);
            if let Some((li, t)) = last {
                if li == inst {
                    f.set_reg_at(pos, t);
                    continue;
                }
            }
            let make: (Opcode, Operand) = match how {
                How::Remat(def) => {
                    let opcode = f.opcode(def).ok_or(MirError::Limit)?;
                    let src = f.operands(def).get(1).copied().ok_or(MirError::Limit)?;
                    if let Operand::Imm(imm) = src {
                        // The value the register held: `mov32ri` zero-extends
                        // its 32-bit immediate, so `-1` there is 0xFFFF_FFFF,
                        // not an all-ones register.
                        let value = if opcode == crate::x86_64::op::MOV32RI {
                            i64::from(imm as u32)
                        } else {
                            imm
                        };
                        if imm_ok(f, inst, kind, pos, base, value) {
                            f.set_operand_at(pos, Operand::Imm(value));
                            continue;
                        }
                    }
                    (opcode, src)
                }
                How::Slot(s) => {
                    if slot_ok(f, inst, kind, pos, base) {
                        f.set_operand_at(pos, Operand::slot(s));
                        continue;
                    }
                    (Opcode::COPY, Operand::slot(s))
                }
            };
            let class = super::class_of(f, v);
            let t = f.vreg(class)?;
            if let Some(tv) = t.as_virt() {
                state.pin(tv.index() as u32);
                let block = f.inst_block(inst).ok_or(MirError::Limit)?;
                let load = f.create_inst(block, make.0, &[t.into(), make.1], Effects::new())?;
                edits.before(inst, load);
                report.temps.push((tv.index() as u32, load, inst));
                report.anchors.push(inst);
            }
            f.set_reg_at(pos, t);
            last = Some((inst, t));
        }
    }
    report.anchors.sort_unstable();
    report.anchors.dedup();
    edits.apply(f, |i| index.is_dead(i));
    Ok(report)
}

/// Whether the operand at `pos` may become the immediate `value`.
fn imm_ok(f: &Function, inst: Inst, kind: InstKind, pos: RegPos, base: usize, value: i64) -> bool {
    match pos {
        RegPos::Arg(_) => true,
        RegPos::Op(i) => {
            let k = (i as usize).saturating_sub(base);
            match kind {
                // A copy's source may be any immediate; the resolver
                // legalizes stores of wide ones.
                InstKind::Copy | InstKind::ParallelCopy => true,
                _ => {
                    matches!(rule_at(f, inst, k), Some(OperandRule::Use(_, acc)) if acc.imm().fits(value))
                }
            }
        }
        RegPos::MemBase(_) | RegPos::MemIndex(_) => false,
    }
}

/// Whether the operand at `pos` may read the spill slot directly.
fn slot_ok(f: &Function, inst: Inst, kind: InstKind, pos: RegPos, base: usize) -> bool {
    match pos {
        RegPos::Arg(_) => true,
        RegPos::Op(i) => {
            let k = (i as usize).saturating_sub(base);
            match kind {
                InstKind::Copy | InstKind::ParallelCopy => true,
                _ => {
                    let mems = f.operands(inst).iter().filter(|o| o.is_mem()).count();
                    mems < usize::from(f.target().max_mem_operands())
                        && matches!(rule_at(f, inst, k), Some(OperandRule::Use(_, acc)) if acc.mem())
                }
            }
        }
        RegPos::MemBase(_) | RegPos::MemIndex(_) => false,
    }
}

/// Spills every value that still has a register definition and may be
/// spilled: the last resort that always allocates.
pub(crate) fn spill_all(
    f: &mut Function,
    index: &mut UseIndex,
    state: &mut SpillState,
) -> Result<SpillReport, RegAllocError> {
    let all: Vec<VReg> = (0..index.def.len())
        .filter(|&i| state.spillable(i as u32) && !matches!(index.def[i], Def::None))
        .map(|i| VReg::from_index(i as u32))
        .collect();
    spill(f, index, state, &all)
}

/// Substitutes physical registers for virtual ones, fixes two-address ties
/// with copies, drops copies that became moves of a register to itself, and
/// turns blocks unreachable from the entry into a trap.
pub(crate) fn apply(
    f: &mut Function,
    an: &Analysis<'_>,
    locations: &[Location],
) -> Result<(), RegAllocError> {
    let mut edits = Edits::default();
    let mut gone = vec![false; f.inst_count()];
    let place = |v: VReg| -> Result<Reg, RegAllocError> {
        match locations.get(v.index()) {
            Some(Location::Reg(r)) => Ok(Reg::from(*r)),
            _ => Err(RegAllocError::Mir(MirError::UnknownVReg(v))),
        }
    };
    for b in 0..f.block_count() {
        let block = Block::from_index(b);
        if !an.cfg.reachable(b) {
            // Dead code: nothing can reach it, so it only needs to be valid.
            let trap = f.create_inst(block, crate::x86_64::op::UD2, &[], Effects::new())?;
            f.set_block_insts(block, vec![trap]);
            f.set_params(block, Vec::new());
            continue;
        }
        if f.params(block)
            .iter()
            .any(|p| p.reg().is_some_and(Reg::is_virtual))
        {
            let mut params = f.params(block).to_vec();
            for p in &mut params {
                if let Some(v) = p.reg().and_then(Reg::as_virt) {
                    *p = Operand::Reg(place(v)?);
                }
            }
            f.set_params(block, params);
        }
        let count = f.insts(block).len();
        for k in 0..count {
            let Some(&inst) = f.insts(block).get(k) else {
                break;
            };
            f.map_vregs(inst, place)?;
            let Some(desc) = f.opcode(inst).and_then(|o| f.target().inst(o)) else {
                continue;
            };
            match desc.kind() {
                InstKind::Copy => {
                    let ops = f.operands(inst);
                    if ops.first() == ops.get(1) {
                        if let Some(g) = gone.get_mut(inst.index()) {
                            *g = true;
                        }
                        edits.delete(inst);
                    }
                }
                InstKind::ParallelCopy => {}
                _ => {
                    // A tie whose use is in another register: copy it into
                    // the definition's register first. The coloring kept that
                    // register clear of every other operand.
                    let base = f.operand_base(inst);
                    for (k, rule) in desc.operands().iter().enumerate() {
                        if let OperandRule::Tied(d) = *rule {
                            let ops = f.operands(inst);
                            let (Some(def), Some(used)) =
                                (ops.get(usize::from(d)).copied(), ops.get(k).copied())
                            else {
                                continue;
                            };
                            if def != used {
                                let copy = f.create_inst(
                                    block,
                                    Opcode::COPY,
                                    &[def, used],
                                    Effects::new(),
                                )?;
                                edits.before(inst, copy);
                                if let Some(r) = def.reg() {
                                    f.set_reg_at(RegPos::Op((base + k) as u32), r);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    edits.apply(f, |i| gone.get(i.index()).copied().unwrap_or(false));
    Ok(())
}
