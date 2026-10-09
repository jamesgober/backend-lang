//! Rewriting the function: dead-code removal before allocation, spill code
//! between rounds, and the final substitution of physical registers.

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

/// Code to insert around existing instructions, applied in one pass.
#[derive(Default)]
struct Edits {
    /// (anchor, 0 = before / 1 = after, sequence, new instruction).
    insert: Vec<(u32, u8, u32, Inst)>,
    deleted: Vec<bool>,
}

impl Edits {
    fn new(f: &Function) -> Edits {
        Edits {
            insert: Vec::new(),
            deleted: vec![false; f.inst_count()],
        }
    }

    fn before(&mut self, anchor: Inst, new: Inst) {
        let seq = self.insert.len() as u32;
        self.insert.push((anchor.index() as u32, 0, seq, new));
    }

    fn after(&mut self, anchor: Inst, new: Inst) {
        let seq = self.insert.len() as u32;
        self.insert.push((anchor.index() as u32, 1, seq, new));
    }

    fn delete(&mut self, inst: Inst) {
        if let Some(d) = self.deleted.get_mut(inst.index()) {
            *d = true;
        }
    }

    /// Rebuilds every block's list with the insertions and deletions.
    fn apply(mut self, f: &mut Function) {
        if self.insert.is_empty() && !self.deleted.iter().any(|&d| d) {
            return;
        }
        self.insert.sort_unstable();
        let mut k = 0usize;
        let n = f.block_count();
        // Insertions are keyed by anchor id; blocks list anchors in layout
        // order, so look each anchor's insertions up by binary search.
        for b in 0..n {
            let block = Block::from_index(b);
            let old = f.take_block_insts(block);
            let mut out = Vec::with_capacity(old.len());
            for inst in old {
                let id = inst.index() as u32;
                k = self.insert.partition_point(|e| e.0 < id);
                let mut j = k;
                while let Some(&(a, place, _, new)) = self.insert.get(j) {
                    if a != id || place != 0 {
                        break;
                    }
                    out.push(new);
                    j += 1;
                }
                if !self.deleted.get(inst.index()).copied().unwrap_or(false) {
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
        let _ = k;
    }
}

/// The definition site of each virtual register.
#[derive(Clone, Copy, Debug)]
enum Def {
    None,
    Param { block: Block, index: usize },
    Inst(Inst),
}

fn def_sites(f: &Function, cfg: &Cfg) -> Vec<Def> {
    let mut defs = vec![Def::None; f.vreg_count()];
    for &b in &cfg.rpo {
        let block = Block::from_index(b as usize);
        for (index, p) in f.params(block).iter().enumerate() {
            if let Some(v) = p.reg().and_then(Reg::as_virt) {
                defs[v.index()] = Def::Param { block, index };
            }
        }
        for &inst in f.insts(block) {
            f.visit_regs(inst, |_, r, access| {
                if let (Access::Def, Some(v)) = (access, r.as_virt()) {
                    defs[v.index()] = Def::Inst(inst);
                }
            });
        }
    }
    defs
}

/// Removes instructions that only compute a value nobody reads (constants,
/// addresses, copies), repeatedly, before allocation.
pub(crate) fn remove_dead_pure(
    f: &mut Function,
    cfg: &Cfg,
    state: &mut SpillState,
) -> Result<(), RegAllocError> {
    let _ = state;
    let mut uses = vec![0u32; f.vreg_count()];
    for &b in &cfg.rpo {
        for &inst in f.insts(Block::from_index(b as usize)) {
            f.visit_regs(inst, |_, r, access| {
                if let (Access::Use, Some(v)) = (access, r.as_virt()) {
                    uses[v.index()] += 1;
                }
            });
        }
    }
    let defs = def_sites(f, cfg);
    let removable = |f: &Function, inst: Inst| -> bool {
        let kind = f
            .opcode(inst)
            .and_then(|o| f.target().inst(o))
            .map(|d| d.kind());
        let copy = kind == Some(InstKind::Copy)
            && f.operands(inst)
                .first()
                .and_then(Operand::reg)
                .is_some_and(Reg::is_virtual);
        copy || is_remat(f, inst)
    };
    let mut work: Vec<u32> = (0..f.vreg_count() as u32)
        .filter(|&v| uses[v as usize] == 0)
        .collect();
    let mut edits = Edits::new(f);
    while let Some(v) = work.pop() {
        let Some(Def::Inst(inst)) = defs.get(v as usize).copied() else {
            continue;
        };
        if edits.deleted.get(inst.index()).copied().unwrap_or(true) || !removable(f, inst) {
            continue;
        }
        edits.delete(inst);
        f.visit_regs(inst, |_, r, access| {
            if let (Access::Use, Some(u)) = (access, r.as_virt()) {
                let n = &mut uses[u.index()];
                *n = n.saturating_sub(1);
                if *n == 0 {
                    work.push(u.index() as u32);
                }
            }
        });
    }
    edits.apply(f);
    Ok(())
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

fn kind_of(f: &Function, inst: Inst) -> InstKind {
    f.opcode(inst)
        .and_then(|o| f.target().inst(o))
        .map_or(InstKind::Plain, |d| d.kind())
}

/// Spills `values`: rematerializes constants and addresses at their uses,
/// and stores everything else to a slot after its definition, reading it
/// back (directly as a memory operand where the instruction allows, else
/// through a fresh temporary) at each use.
pub(crate) fn spill(
    f: &mut Function,
    cfg: &Cfg,
    state: &mut SpillState,
    values: &[VReg],
) -> Result<(), RegAllocError> {
    let defs = def_sites(f, cfg);
    let nv = f.vreg_count();
    // What each spilled value becomes: a slot, or a rematerialization.
    #[derive(Clone, Copy)]
    enum How {
        Keep,
        Slot(StackSlot),
        Remat(Inst),
    }
    let mut how = vec![How::Keep; nv];
    let mut edits = Edits::new(f);
    for &v in values {
        let i = v.index();
        if i >= nv || !state.spillable(i as u32) {
            continue;
        }
        let class = super::class_of(f, v);
        match defs[i] {
            Def::None => continue,
            Def::Inst(inst) if is_remat(f, inst) => {
                how[i] = How::Remat(inst);
                edits.delete(inst);
                state.rematerialized += 1;
            }
            Def::Param { block, index } => {
                let s = f.spill_slot(class)?;
                let mut params = f.params(block).to_vec();
                if let Some(p) = params.get_mut(index) {
                    *p = Operand::slot(s);
                }
                f.set_params(block, params);
                how[i] = How::Slot(s);
            }
            Def::Inst(inst) => {
                let s = f.spill_slot(class)?;
                how[i] = How::Slot(s);
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
                            &[Operand::slot(s), v.into_reg().into()],
                            Effects::new(),
                        )?;
                        edits.after(inst, store);
                        state.pin(i as u32);
                    }
                }
            }
        }
        state.spilled += 1;
        if !matches!(how[i], How::Remat(_)) {
            state.pin(i as u32);
        }
    }

    // Rewrite the uses, instruction by instruction.
    let mut positions: Vec<(RegPos, VReg)> = Vec::new();
    let mut temps: Vec<(VReg, Reg)> = Vec::new();
    for &b in &cfg.rpo {
        let block = Block::from_index(b as usize);
        let insts = f.insts(block).to_vec();
        for inst in insts {
            if edits.deleted.get(inst.index()).copied().unwrap_or(false) {
                continue;
            }
            positions.clear();
            f.visit_regs(inst, |pos, r, access| {
                if let (Access::Use, Some(v)) = (access, r.as_virt()) {
                    if !matches!(how.get(v.index()), Some(How::Keep) | None) {
                        positions.push((pos, v));
                    }
                }
            });
            if positions.is_empty() {
                continue;
            }
            temps.clear();
            let kind = kind_of(f, inst);
            let base = f.operand_base(inst);
            for &(pos, v) in &positions {
                match how[v.index()] {
                    How::Keep => {}
                    How::Remat(def) => {
                        let opcode = f.opcode(def).ok_or(MirError::Limit)?;
                        let src = f.operands(def).get(1).copied().ok_or(MirError::Limit)?;
                        if let Operand::Imm(imm) = src {
                            // The value the register held: `mov32ri`
                            // zero-extends its 32-bit immediate, so `-1` there
                            // is 0xFFFF_FFFF, not an all-ones register.
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
                        let t = temp_for(
                            f,
                            state,
                            &mut temps,
                            v,
                            |f, t| f.create_inst(block, opcode, &[t.into(), src], Effects::new()),
                            &mut edits,
                            inst,
                        )?;
                        f.set_reg_at(pos, t);
                    }
                    How::Slot(s) => {
                        if slot_ok(f, inst, kind, pos, base) {
                            f.set_operand_at(pos, Operand::slot(s));
                            continue;
                        }
                        let t = temp_for(
                            f,
                            state,
                            &mut temps,
                            v,
                            |f, t| {
                                f.create_inst(
                                    block,
                                    Opcode::COPY,
                                    &[t.into(), Operand::slot(s)],
                                    Effects::new(),
                                )
                            },
                            &mut edits,
                            inst,
                        )?;
                        f.set_reg_at(pos, t);
                    }
                }
            }
        }
    }
    edits.apply(f);
    Ok(())
}

/// The reload temporary of `v` at `inst`, made once per instruction.
fn temp_for(
    f: &mut Function,
    state: &mut SpillState,
    temps: &mut Vec<(VReg, Reg)>,
    v: VReg,
    make: impl FnOnce(&mut Function, Reg) -> Result<Inst, MirError>,
    edits: &mut Edits,
    inst: Inst,
) -> Result<Reg, RegAllocError> {
    if let Some(&(_, t)) = temps.iter().find(|(w, _)| *w == v) {
        return Ok(t);
    }
    let class = super::class_of(f, v);
    let t = f.vreg(class)?;
    if let Some(tv) = t.as_virt() {
        state.pin(tv.index() as u32);
    }
    let load = make(f, t)?;
    edits.before(inst, load);
    temps.push((v, t));
    Ok(t)
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
    cfg: &Cfg,
    state: &mut SpillState,
) -> Result<(), RegAllocError> {
    let defs = def_sites(f, cfg);
    let all: Vec<VReg> = (0..f.vreg_count())
        .filter(|&i| state.spillable(i as u32) && !matches!(defs[i], Def::None))
        .map(|i| VReg::from_index(i as u32))
        .collect();
    spill(f, cfg, state, &all)
}

/// Substitutes physical registers for virtual ones, fixes two-address ties
/// with copies, drops copies that became moves of a register to itself, and
/// turns blocks unreachable from the entry into a trap.
pub(crate) fn apply(
    f: &mut Function,
    an: &Analysis<'_>,
    locations: &[Location],
) -> Result<(), RegAllocError> {
    let mut edits = Edits::new(f);
    let place = |v: VReg| -> Result<Reg, RegAllocError> {
        match locations.get(v.index()) {
            Some(Location::Reg(r)) => Ok(Reg::from(*r)),
            _ => Err(RegAllocError::Mir(MirError::UnknownVReg(v))),
        }
    };
    let mut positions: Vec<(RegPos, VReg)> = Vec::new();
    for b in 0..f.block_count() {
        let block = Block::from_index(b);
        if !an.cfg.reachable(b) {
            // Dead code: nothing can reach it, so it only needs to be valid.
            let trap = f.create_inst(block, crate::x86_64::op::UD2, &[], Effects::new())?;
            f.set_block_insts(block, vec![trap]);
            f.set_params(block, Vec::new());
            continue;
        }
        let mut params = f.params(block).to_vec();
        for p in &mut params {
            if let Some(v) = p.reg().and_then(Reg::as_virt) {
                *p = Operand::Reg(place(v)?);
            }
        }
        f.set_params(block, params);
        let insts = f.insts(block).to_vec();
        for inst in insts {
            positions.clear();
            f.visit_regs(inst, |pos, r, _| {
                if let Some(v) = r.as_virt() {
                    positions.push((pos, v));
                }
            });
            for &(pos, v) in &positions {
                let r = place(v)?;
                f.set_reg_at(pos, r);
            }
            let Some(desc) = f.opcode(inst).and_then(|o| f.target().inst(o)) else {
                continue;
            };
            match desc.kind() {
                InstKind::Copy => {
                    let ops = f.operands(inst);
                    if ops.first() == ops.get(1) {
                        edits.delete(inst);
                    }
                }
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
    edits.apply(f);
    Ok(())
}

trait IntoReg {
    fn into_reg(self) -> Reg;
}

impl IntoReg for VReg {
    fn into_reg(self) -> Reg {
        Reg::from(self)
    }
}
