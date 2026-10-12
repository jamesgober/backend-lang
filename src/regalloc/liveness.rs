//! Liveness and constraints: live-in sets by SSA path exploration, then one
//! backward walk per block for last uses, physical-register liveness, and
//! the registers written while each value is live.
//!
//! The analysis runs once per allocation. A spill round changes liveness in
//! a way that can be stated exactly (every spilled value leaves every live-in
//! set; each new temporary lives from just before its user to its user; a
//! stored value from its definition to the store after it; spill code reads
//! and writes no physical register), so [`Analysis::update`] applies those
//! changes in time proportional to the live sets and the spill code instead
//! of exploring paths and walking every block again.

use alloc::vec;
use alloc::vec::Vec;

use super::RegAllocError;
use super::rewrite::{Def, SpillReport, UseIndex};
use crate::mir::cfg::{Cfg, NONE};
use crate::mir::visit::Access;
use crate::mir::{Block, Function, ImplicitKind, Inst};
use crate::regs::{MAX_PREGS, Reg, RegSet};

/// Everything the coloring needs to know about the function.
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
    /// The tracked physical registers each instruction writes and reads,
    /// for the instructions that touch any: an index into `fx_sets` by
    /// instruction id, `NONE` for none.
    fx: Vec<u32>,
    fx_sets: Vec<[RegSet; 2]>,
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
    /// The most values of one class live at once, before any spilling.
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

    /// The tracked physical registers `inst` writes (definitions, implicit
    /// definitions, clobbers).
    pub(crate) fn writes(&self, inst: Inst) -> RegSet {
        self.fx_of(inst)[0]
    }

    /// The tracked physical registers `inst` reads.
    pub(crate) fn reads(&self, inst: Inst) -> RegSet {
        self.fx_of(inst)[1]
    }

    fn fx_of(&self, inst: Inst) -> [RegSet; 2] {
        match self.fx.get(inst.index()) {
            Some(&k) if k != NONE => self
                .fx_sets
                .get(k as usize)
                .copied()
                .unwrap_or([RegSet::EMPTY; 2]),
            _ => [RegSet::EMPTY; 2],
        }
    }

    /// Applies a spill round's changes (see the module documentation).
    pub(crate) fn update(&mut self, f: &Function, report: &SpillReport) {
        let nv = f.vreg_count();
        let ni = f.inst_count();
        let mut gone = vec![false; nv];
        for &v in &report.spilled {
            if let Some(g) = gone.get_mut(v as usize) {
                *g = true;
            }
        }
        let keep = |v: u32| !gone.get(v as usize).copied().unwrap_or(false);

        // Live-in sets: the spilled values leave them.
        let nb = self.live_start.len().saturating_sub(1);
        let mut live = Vec::with_capacity(self.live.len());
        let mut live_start = Vec::with_capacity(nb + 1);
        for b in 0..nb {
            live_start.push(live.len() as u32);
            live.extend(self.live_in(b).iter().copied().filter(|&v| keep(v)));
        }
        live_start.push(live.len() as u32);
        self.live = live;
        self.live_start = live_start;

        // Last uses: the spilled values' go; each temporary dies at its user,
        // each stored value at its store.
        let mut extra: Vec<(u32, u32)> =
            Vec::with_capacity(report.temps.len() + report.stored.len());
        extra.extend(
            report
                .temps
                .iter()
                .map(|&(t, _, user)| (user.index() as u32, t)),
        );
        extra.extend(
            report
                .stored
                .iter()
                .map(|&(v, _, store)| (store.index() as u32, v)),
        );
        let mut count = vec![0u32; ni + 1];
        let old_ni = self.kill_start.len().saturating_sub(1);
        for i in 0..old_ni {
            let n = self
                .kills(Inst::from_index(i))
                .iter()
                .filter(|&&v| keep(v))
                .count();
            count[i + 1] = n as u32;
        }
        for &(i, _) in &extra {
            if let Some(c) = count.get_mut(i as usize + 1) {
                *c += 1;
            }
        }
        for k in 0..ni {
            count[k + 1] += count[k];
        }
        let mut kills = vec![0u32; count[ni] as usize];
        let mut fill = count.clone();
        for i in 0..old_ni {
            for &v in self.kills(Inst::from_index(i)) {
                if keep(v) {
                    kills[fill[i] as usize] = v;
                    fill[i] += 1;
                }
            }
        }
        for &(i, v) in &extra {
            if let Some(at) = fill.get_mut(i as usize) {
                if let Some(slot) = kills.get_mut(*at as usize) {
                    *slot = v;
                }
                *at += 1;
            }
        }
        self.kills = kills;
        self.kill_start = count;

        // Physical registers after the new instructions: a temporary's
        // definition sits just before its user, so what is live after it is
        // what is live into the user; a store sits just after a definition.
        self.phys_after.resize(ni, RegSet::EMPTY);
        self.fx.resize(ni, NONE);
        for &(_, def, user) in &report.temps {
            let before = self
                .phys_after
                .get(user.index())
                .copied()
                .unwrap_or(RegSet::EMPTY)
                .difference(self.writes(user))
                .union(self.reads(user));
            if let Some(p) = self.phys_after.get_mut(def.index()) {
                *p = before;
            }
        }
        for &(_, def, store) in &report.stored {
            let after = self
                .phys_after
                .get(def.index())
                .copied()
                .unwrap_or(RegSet::EMPTY);
            if let Some(p) = self.phys_after.get_mut(store.index()) {
                *p = after;
            }
        }

        // Per-value facts for the new temporaries and the stored values.
        self.forbidden.resize(nv, RegSet::EMPTY);
        self.uses.resize(nv, 0);
        self.span.resize(nv, 0);
        for &(t, _, user) in &report.temps {
            let mut n = 0u32;
            f.visit_regs(user, |_, r, access| {
                if access == Access::Use && r.as_virt().is_some_and(|x| x.index() as u32 == t) {
                    n += 1;
                }
            });
            let i = t as usize;
            if let (Some(fb), Some(u), Some(s)) = (
                self.forbidden.get_mut(i),
                self.uses.get_mut(i),
                self.span.get_mut(i),
            ) {
                *fb = RegSet::EMPTY;
                *u = n;
                *s = 1;
            }
        }
        for &(v, _, _) in &report.stored {
            let i = v as usize;
            if let (Some(fb), Some(u), Some(s)) = (
                self.forbidden.get_mut(i),
                self.uses.get_mut(i),
                self.span.get_mut(i),
            ) {
                *fb = RegSet::EMPTY;
                *u = 1;
                *s = 1;
            }
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

/// Analyzes `f`, whose definitions and uses `index` lists.
pub(crate) fn analyze<'c>(
    f: &Function,
    cfg: &'c Cfg,
    index: &UseIndex,
    budget: usize,
) -> Result<Analysis<'c>, RegAllocError> {
    let nb = f.block_count();
    let nv = f.vreg_count();
    let ni = f.inst_count();

    // Definitions: block of each value.
    let def_block: Vec<u32> = index
        .def
        .iter()
        .map(|d| match *d {
            Def::None => NONE,
            Def::Param { block, .. } => block.index() as u32,
            Def::Inst(inst) => f.inst_block(inst).map_or(NONE, |b| b.index() as u32),
        })
        .collect();

    // Path exploration: for each value, walk up from its uses outside its
    // definition's block to the definition, marking it live-in on the way.
    let mut mark = vec![NONE; nb];
    let mut pairs: Vec<(u32, u32)> = Vec::new(); // (block, value)
    let mut stack: Vec<u32> = Vec::new();
    for (v, &d) in def_block.iter().enumerate().take(nv) {
        for &(inst, _) in index.uses(v) {
            if index.is_dead(inst) {
                continue;
            }
            let b = f.inst_block(inst).map_or(NONE, |b| b.index() as u32);
            if b != d && b != NONE {
                stack.push(b);
            }
        }
        let v = v as u32;
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
        fx: vec![NONE; ni],
        fx_sets: Vec::new(),
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
    // `last_set` only grows with `cp`, so a memo keyed by (open, now) holds.
    let mut memo = (u64::MAX, u64::MAX, RegSet::EMPTY);
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
                        &mut memo,
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
            if !w.is_empty() || !reads.is_empty() {
                if let Some(slot) = an.fx.get_mut(inst.index()) {
                    *slot = an.fx_sets.len() as u32;
                    an.fx_sets.push([w, reads]);
                }
            }
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
                &mut memo,
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
#[allow(
    clippy::too_many_arguments,
    reason = "the walk's state, one call shape"
)]
fn close(
    an: &mut Analysis,
    v: u32,
    open: u64,
    from: u64,
    to: u64,
    last_set: &[u64; MAX_PREGS],
    now: u64,
    memo: &mut (u64, u64, RegSet),
) {
    // No constraint point since the segment opened: nothing to add.
    if now > open {
        // Segments closing together mostly opened together (every value
        // live through a block opens at its end and closes at its start),
        // so the mask of one (open, now) pair serves them all.
        let add = if memo.0 == open && memo.1 == now {
            memo.2
        } else {
            let mut add = RegSet::EMPTY;
            for r in an.tracked.iter() {
                if last_set[r.index()] > open {
                    add = add.with(r);
                }
            }
            *memo = (open, now, add);
            add
        };
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
