//! Liveness and constraints: live-in sets by SSA path exploration, then one
//! backward walk per block for last uses, physical-register liveness, and
//! the registers written while each value is live.

use alloc::vec;
use alloc::vec::Vec;

use super::RegAllocError;
use super::rewrite::SpillState;
use crate::mir::cfg::{Cfg, NONE};
use crate::mir::visit::{Access, def_count};
use crate::mir::{Block, Function, ImplicitKind, Inst};
use crate::regs::{MAX_PREGS, Reg, RegSet};

/// Everything the coloring needs to know about one round's function.
pub(crate) struct Analysis<'c> {
    pub(crate) cfg: &'c Cfg,
    /// Values live into each block (CSR by block).
    live_start: Vec<u32>,
    live: Vec<u32>,
    /// Values whose last use is each instruction (CSR by instruction id).
    kill_start: Vec<u32>,
    kills: Vec<u32>,
    /// Physical registers live after each instruction, by instruction id.
    pub(crate) phys_after: Vec<RegSet>,
    /// Registers written while each value is live (not counting its own
    /// definition), by virtual register.
    pub(crate) forbidden: Vec<RegSet>,
    /// Uses of each value.
    pub(crate) uses: Vec<u32>,
    /// Instructions each value is live across, summed over its segments.
    pub(crate) span: Vec<u64>,
    /// The physical registers any instruction may hand out: allocatable and
    /// not reserved for the frame.
    pub(crate) tracked: RegSet,
    pub(crate) max_pressure: u32,
}

impl Analysis<'_> {
    /// The values live into `block`.
    pub(crate) fn live_in(&self, block: usize) -> &[u32] {
        match (self.live_start.get(block), self.live_start.get(block + 1)) {
            (Some(&s), Some(&e)) => self.live.get(s as usize..e as usize).unwrap_or(&[]),
            _ => &[],
        }
    }

    /// The values whose last use is `inst`.
    pub(crate) fn kills(&self, inst: Inst) -> &[u32] {
        let i = inst.index();
        match (self.kill_start.get(i), self.kill_start.get(i + 1)) {
            (Some(&s), Some(&e)) => self.kills.get(s as usize..e as usize).unwrap_or(&[]),
            _ => &[],
        }
    }
}

/// Every physical register `inst` writes: explicit physical definitions,
/// implicit definitions, and clobbers.
pub(crate) fn writes(f: &Function, inst: Inst) -> RegSet {
    let mut set = f.clobbers(inst);
    if let Some(desc) = f.opcode(inst).and_then(|o| f.target().inst(o)) {
        set = set.union(desc.clobbers());
        for &r in desc.implicit_defs() {
            set = set.with(r);
        }
    }
    for imp in f.implicit(inst) {
        if imp.kind == ImplicitKind::Def {
            set = set.with(imp.reg);
        }
    }
    f.visit_regs(inst, |_, r, access| {
        if let (Access::Def, Some(p)) = (access, r.as_phys()) {
            set = set.with(p);
        }
    });
    set
}

/// Every physical register `inst` reads.
pub(crate) fn phys_uses(f: &Function, inst: Inst) -> RegSet {
    let mut set = RegSet::EMPTY;
    if let Some(desc) = f.opcode(inst).and_then(|o| f.target().inst(o)) {
        for &r in desc.implicit_uses() {
            set = set.with(r);
        }
    }
    for imp in f.implicit(inst) {
        if imp.kind == ImplicitKind::Use {
            set = set.with(imp.reg);
        }
    }
    f.visit_regs(inst, |_, r, access| {
        if let (Access::Use, Some(p)) = (access, r.as_phys()) {
            set = set.with(p);
        }
    });
    set
}

/// A set of virtual registers with O(1) insert, remove, and test, cleared in
/// O(1) by bumping its generation.
pub(crate) struct SparseSet {
    stamp: Vec<u32>,
    pos: Vec<u32>,
    members: Vec<u32>,
    generation: u32,
}

impl SparseSet {
    pub(crate) fn new(n: usize) -> SparseSet {
        SparseSet {
            stamp: vec![0; n],
            pos: vec![0; n],
            members: Vec::new(),
            generation: 1,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.members.clear();
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.stamp.iter_mut().for_each(|s| *s = 0);
            self.generation = 1;
        }
    }

    pub(crate) fn contains(&self, v: u32) -> bool {
        self.stamp.get(v as usize) == Some(&self.generation)
    }

    pub(crate) fn insert(&mut self, v: u32) -> bool {
        if self.contains(v) {
            return false;
        }
        if let (Some(s), Some(p)) = (self.stamp.get_mut(v as usize), self.pos.get_mut(v as usize)) {
            *s = self.generation;
            *p = self.members.len() as u32;
            self.members.push(v);
            true
        } else {
            false
        }
    }

    pub(crate) fn remove(&mut self, v: u32) -> bool {
        if !self.contains(v) {
            return false;
        }
        let at = self.pos[v as usize] as usize;
        if let Some(last) = self.members.pop() {
            if last != v {
                self.members[at] = last;
                self.pos[last as usize] = at as u32;
            }
        }
        self.stamp[v as usize] = self.generation.wrapping_sub(1);
        true
    }

    pub(crate) fn members(&self) -> &[u32] {
        &self.members
    }
}

/// Analyzes `f` for one coloring round.
pub(crate) fn analyze<'c>(
    f: &Function,
    cfg: &'c Cfg,
    state: &SpillState,
    budget: usize,
) -> Result<Analysis<'c>, RegAllocError> {
    let nb = f.block_count();
    let nv = f.vreg_count();
    let ni = f.inst_count();
    let _ = state;

    // Definitions: block of each value.
    let mut def_block = vec![NONE; nv];
    for &b in &cfg.rpo {
        let block = Block::from_index(b as usize);
        for p in f.params(block) {
            if let Some(v) = p.reg().and_then(Reg::as_virt) {
                def_block[v.index()] = b;
            }
        }
        for &inst in f.insts(block) {
            let n = def_count(f, inst);
            for op in f.operands(inst).iter().take(n) {
                if let Some(v) = op.reg().and_then(Reg::as_virt) {
                    def_block[v.index()] = b;
                }
            }
        }
    }

    // Uses outside the defining block, bucketed by value.
    let mut outside: Vec<(u32, u32)> = Vec::new();
    for &b in &cfg.rpo {
        let block = Block::from_index(b as usize);
        for &inst in f.insts(block) {
            f.visit_regs(inst, |_, r, access| {
                if let (Access::Use, Some(v)) = (access, r.as_virt()) {
                    if def_block.get(v.index()).is_some_and(|&d| d != b) {
                        outside.push((v.index() as u32, b));
                    }
                }
            });
        }
    }
    // Bucket by value in linear time (a counting sort); duplicates are
    // harmless, the walk below skips blocks already marked.
    let mut start = vec![0u32; nv + 1];
    for &(v, _) in &outside {
        start[v as usize + 1] += 1;
    }
    for k in 0..nv {
        start[k + 1] += start[k];
    }
    let mut sorted = vec![(0u32, 0u32); outside.len()];
    {
        let mut fill = start.clone();
        for &(v, b) in &outside {
            sorted[fill[v as usize] as usize] = (v, b);
            fill[v as usize] += 1;
        }
    }
    let outside = sorted;

    // Path exploration: for each value, walk up from its outside uses to its
    // definition, marking it live-in on the way.
    let mut mark = vec![NONE; nb];
    let mut pairs: Vec<(u32, u32)> = Vec::new(); // (block, value)
    let mut stack: Vec<u32> = Vec::new();
    let mut i = 0;
    while i < outside.len() {
        let v = outside[i].0;
        let d = def_block[v as usize];
        while i < outside.len() && outside[i].0 == v {
            stack.push(outside[i].1);
            i += 1;
        }
        while let Some(b) = stack.pop() {
            if b == d || mark[b as usize] == v {
                continue;
            }
            mark[b as usize] = v;
            if pairs.len() >= budget {
                return Err(RegAllocError::TooLarge);
            }
            pairs.push((b, v));
            for &p in cfg.preds(b as usize) {
                if cfg.reachable(p as usize) {
                    stack.push(p);
                }
            }
        }
    }
    // CSR by block.
    let mut live_start = vec![0u32; nb + 1];
    for &(b, _) in &pairs {
        live_start[b as usize + 1] += 1;
    }
    for k in 0..nb {
        live_start[k + 1] += live_start[k];
    }
    let mut fill = live_start.clone();
    let mut live = vec![0u32; pairs.len()];
    for &(b, v) in &pairs {
        live[fill[b as usize] as usize] = v;
        fill[b as usize] += 1;
    }
    drop(pairs);

    let target = f.target();
    let reserved = f.reserved();
    let mut tracked = RegSet::EMPTY;
    for c in target.classes() {
        tracked = tracked.union(c.allocatable());
    }
    // Without a frame pointer, its register is one more for values (the
    // frame saves it like any callee-saved register it writes).
    if !f.frame_pointer() {
        tracked = tracked.with(target.frame_pointer());
    }
    let tracked = tracked.difference(reserved);

    let mut an = Analysis {
        cfg,
        live_start,
        live,
        kill_start: Vec::new(),
        kills: Vec::new(),
        phys_after: vec![RegSet::EMPTY; ni],
        forbidden: vec![RegSet::EMPTY; nv],
        uses: vec![0; nv],
        span: vec![0; nv],
        tracked,
        max_pressure: 0,
    };

    // Backward walk per block.
    let mut set = SparseSet::new(nv);
    let mut open_cp = vec![0u64; nv];
    let mut open_pos = vec![0u64; nv];
    let mut last_set = [0u64; MAX_PREGS];
    let mut cp = 0u64;
    let mut pos = 0u64;
    let mut kill_pairs: Vec<(u32, u32)> = Vec::new(); // (inst, value)
    let mut pressure = vec![0u32; target.classes().len()];
    let mut defs_here: Vec<u32> = Vec::new();
    let mut uses_here: Vec<u32> = Vec::new();
    let rpo = an.cfg.rpo.clone();
    for &b in &rpo {
        let block = Block::from_index(b as usize);
        set.clear();
        // Live out: everything live into a successor.
        for &s in an.cfg.succs(b as usize) {
            let (st, en) = (
                an.live_start[s as usize] as usize,
                an.live_start[s as usize + 1] as usize,
            );
            for k in st..en {
                let v = an.live[k];
                if set.insert(v) {
                    open_cp[v as usize] = cp;
                    open_pos[v as usize] = pos;
                }
            }
        }
        pressure.iter_mut().for_each(|p| *p = 0);
        for &v in set.members() {
            let c = super::class_of(f, crate::regs::VReg::from_index(v)).index();
            if let Some(p) = pressure.get_mut(c) {
                *p += 1;
            }
        }
        let mut phys = RegSet::EMPTY;
        for &inst in f.insts(block).iter().rev() {
            pos += 1;
            if let Some(slot) = an.phys_after.get_mut(inst.index()) {
                *slot = phys;
            }
            defs_here.clear();
            uses_here.clear();
            let (mut pdefs, mut puses) = (RegSet::EMPTY, RegSet::EMPTY);
            f.visit_regs(inst, |_, r, access| {
                match (r.as_virt(), r.as_phys(), access) {
                    (Some(v), _, Access::Def) => defs_here.push(v.index() as u32),
                    (Some(v), _, Access::Use) => uses_here.push(v.index() as u32),
                    (_, Some(p), Access::Def) => pdefs = pdefs.with(p),
                    (_, Some(p), Access::Use) => puses = puses.with(p),
                    _ => {}
                }
            });
            // Definitions end their values' segments (walking backward).
            for &d in &defs_here {
                if set.remove(d) {
                    close(
                        &mut an,
                        d,
                        open_cp[d as usize],
                        open_pos[d as usize],
                        pos,
                        &last_set,
                        cp,
                    );
                    let c = super::class_of(f, crate::regs::VReg::from_index(d)).index();
                    if let Some(p) = pressure.get_mut(c) {
                        *p = p.saturating_sub(1);
                    }
                }
            }
            // What the instruction writes constrains every value live across
            // it; what it only clobbers must not be read later.
            let (w, clobbered, reads) = if has_implicit(f, inst) {
                let only = clobbers_only(f, inst);
                (
                    writes(f, inst).intersect(tracked),
                    only.intersect(tracked),
                    phys_uses(f, inst).intersect(tracked),
                )
            } else {
                (
                    pdefs.intersect(tracked),
                    RegSet::EMPTY,
                    puses.intersect(tracked),
                )
            };
            if !w.is_empty() {
                cp += 1;
                for r in w.iter() {
                    last_set[r.index()] = cp;
                }
            }
            if let Some(r) = clobbered.intersect(phys).first() {
                return Err(RegAllocError::PhysClobbered { block, reg: r });
            }
            phys = phys.difference(w).union(reads);
            // Uses start segments; the first use seen backward is the last.
            for &u in &uses_here {
                if let Some(n) = an.uses.get_mut(u as usize) {
                    *n = n.saturating_add(1);
                }
                if set.insert(u) {
                    open_cp[u as usize] = cp;
                    open_pos[u as usize] = pos - 1;
                    kill_pairs.push((inst.index() as u32, u));
                    let c = super::class_of(f, crate::regs::VReg::from_index(u)).index();
                    if let Some(p) = pressure.get_mut(c) {
                        *p += 1;
                    }
                }
            }
            for &p in &pressure {
                an.max_pressure = an.max_pressure.max(p);
            }
        }
        // Block start: what remains live is live in (or a parameter).
        for k in 0..set.members().len() {
            let v = set.members()[k];
            close(
                &mut an,
                v,
                open_cp[v as usize],
                open_pos[v as usize],
                pos,
                &last_set,
                cp,
            );
        }
        let entry_params: RegSet = if an.cfg.rpo.first() == Some(&b) {
            f.params(block)
                .iter()
                .filter_map(|p| p.reg().and_then(Reg::as_phys))
                .collect()
        } else {
            RegSet::EMPTY
        };
        if let Some(r) = phys.difference(entry_params).first() {
            return Err(RegAllocError::PhysLiveIn { block, reg: r });
        }
    }

    // Kills: CSR by instruction id.
    let mut kill_start = vec![0u32; ni + 1];
    for &(inst, _) in &kill_pairs {
        if let Some(c) = kill_start.get_mut(inst as usize + 1) {
            *c += 1;
        }
    }
    for k in 0..ni {
        kill_start[k + 1] += kill_start[k];
    }
    let mut fill = kill_start.clone();
    let mut kills = vec![0u32; kill_pairs.len()];
    for &(inst, v) in &kill_pairs {
        let at = &mut fill[inst as usize];
        kills[*at as usize] = v;
        *at += 1;
    }
    an.kill_start = kill_start;
    an.kills = kills;
    Ok(an)
}

/// Ends value `v`'s segment that opened at constraint count `open` and
/// position `from`: every register written at a constraint point since then
/// is forbidden for it.
fn close(
    an: &mut Analysis,
    v: u32,
    open: u64,
    from: u64,
    to: u64,
    last_set: &[u64; MAX_PREGS],
    now: u64,
) {
    // No constraint point since the segment opened: nothing to add.
    if now > open {
        let mut add = RegSet::EMPTY;
        for r in an.tracked.iter() {
            if last_set[r.index()] > open {
                add = add.with(r);
            }
        }
        if let Some(slot) = an.forbidden.get_mut(v as usize) {
            *slot = slot.union(add);
        }
    }
    if let Some(s) = an.span.get_mut(v as usize) {
        *s = s.saturating_add(to.saturating_sub(from));
    }
}

/// Whether `inst` has implicit operands or clobbers, from its description
/// or its own effects (the slow path of the constraint walk).
fn has_implicit(f: &Function, inst: Inst) -> bool {
    if !f.implicit(inst).is_empty() || !f.clobbers(inst).is_empty() {
        return true;
    }
    f.opcode(inst)
        .and_then(|o| f.target().inst(o))
        .is_some_and(|d| {
            !d.implicit_uses().is_empty()
                || !d.implicit_defs().is_empty()
                || !d.clobbers().is_empty()
        })
}

/// The registers `inst` leaves unspecified without defining them.
fn clobbers_only(f: &Function, inst: Inst) -> RegSet {
    let mut set = f.clobbers(inst);
    let mut defined = RegSet::EMPTY;
    if let Some(desc) = f.opcode(inst).and_then(|o| f.target().inst(o)) {
        set = set.union(desc.clobbers());
        for &r in desc.implicit_defs() {
            defined = defined.with(r);
        }
    }
    for imp in f.implicit(inst) {
        if imp.kind == ImplicitKind::Def {
            defined = defined.with(imp.reg);
        }
    }
    set.difference(defined)
}
