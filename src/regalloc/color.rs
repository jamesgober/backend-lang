//! Coloring in dominance order, with affinity-class coalescing and spill
//! decisions at definitions.

use alloc::vec;
use alloc::vec::Vec;

use super::RegAllocError;
use super::liveness::Analysis;
use super::rewrite::{SpillReport, SpillState};
use crate::mir::cfg::NONE;
use crate::mir::visit::Access;
use crate::mir::{Block, Function, Inst, Operand};
use crate::regs::{MAX_PREGS, PReg, Reg, RegClass, RegSet, VReg};
use crate::target::{InstKind, OperandRule};

/// A value's place during coloring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Loc {
    /// Not reached yet, or never live.
    None,
    /// A register.
    Reg(PReg),
    /// Chosen for spilling this round.
    Spill,
}

/// One round's result: each value's place, and the values to spill before
/// the next round (empty when the coloring is final).
pub(crate) struct Colored {
    pub(crate) locations: Vec<super::Location>,
    pub(crate) spill: Vec<VReg>,
}

/// Union-find over values joined by copies, ties, and edges, with each
/// class's register in the round being colored.
#[derive(Clone)]
struct Affinity {
    parent: Vec<u32>,
    reg: Vec<Option<PReg>>,
}

impl Affinity {
    fn find(&mut self, mut v: u32) -> u32 {
        // Path halving; the walk only climbs, so it ends.
        while let Some(&p) = self.parent.get(v as usize) {
            if p == v {
                break;
            }
            let gp = self.parent.get(p as usize).copied().unwrap_or(p);
            self.parent[v as usize] = gp;
            v = gp;
        }
        v
    }

    fn union(&mut self, a: u32, b: u32) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            // Keep the smaller root so the result does not depend on order.
            let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
            self.parent[hi as usize] = lo;
        }
    }
}

/// Whether `inst` only materializes a constant or an address (no inputs, no
/// memory read, no side effect), so its value can be recomputed anywhere.
pub(crate) fn is_remat(f: &Function, inst: Inst) -> bool {
    use crate::x86_64::op;
    let Some(opcode) = f.opcode(inst) else {
        return false;
    };
    let lea = opcode == op::LEA64 || opcode == op::LEA32;
    if opcode != op::MOV64RI && opcode != op::MOV32RI && !lea {
        return false;
    }
    if !core::ptr::eq(f.target(), &crate::x86_64::TARGET) {
        return false;
    }
    // An address with no register in it: a slot, a symbol, or an absolute.
    f.operands(inst).iter().skip(1).all(|op| match op {
        Operand::Imm(_) | Operand::Symbol(_) | Operand::Slot { .. } => true,
        Operand::Mem(m) => m.base_reg().is_none() && m.index_reg().is_none(),
        _ => false,
    })
}

/// What coloring prefers, built once per allocation: the affinity classes
/// (values joined by copies, ties, and edges), hints toward fixed registers,
/// and which values are rematerializable. A spill round only adds
/// temporaries, so [`Affinities::update`] looks at the instructions that use
/// them and nothing else.
pub(crate) struct Affinities {
    classes: Affinity,
    hint: Vec<Option<PReg>>,
    remat: Vec<bool>,
}

impl Affinities {
    pub(crate) fn new(f: &Function, an: &Analysis<'_>) -> Affinities {
        let nv = f.vreg_count();
        let mut a = Affinities {
            classes: Affinity {
                parent: (0..nv as u32).collect(),
                reg: Vec::new(),
            },
            hint: vec![None; nv],
            remat: vec![false; nv],
        };
        for &b in &an.cfg.rpo {
            for &inst in f.insts(Block::from_index(b as usize)) {
                a.add(f, inst);
            }
        }
        a
    }

    /// Takes in a spill round's new instructions and the instructions that
    /// now read a temporary.
    pub(crate) fn update(&mut self, f: &Function, report: &SpillReport) {
        let nv = f.vreg_count();
        let old = self.classes.parent.len() as u32;
        self.classes.parent.extend(old..nv as u32);
        self.hint.resize(nv, None);
        self.remat.resize(nv, false);
        for &(_, def, _) in &report.temps {
            self.add(f, def);
        }
        for &inst in &report.anchors {
            self.add(f, inst);
        }
    }

    /// Records the affinities and hints `inst` creates.
    fn add(&mut self, f: &Function, inst: Inst) {
        let Some(desc) = f.opcode(inst).and_then(|o| f.target().inst(o)) else {
            return;
        };
        let ops = f.operands(inst);
        let (aff, hint) = (&mut self.classes, &mut self.hint);
        let mut pair = |d: Operand, s: Operand| {
            let (Some(dr), Some(sr)) = (d.reg(), s.reg()) else {
                return;
            };
            match (dr.as_virt(), sr.as_virt(), dr.as_phys(), sr.as_phys()) {
                (Some(dv), Some(sv), _, _) => aff.union(dv.index() as u32, sv.index() as u32),
                (Some(dv), None, _, Some(sp)) => {
                    if let Some(h) = hint.get_mut(dv.index()) {
                        *h = Some(sp);
                    }
                }
                (None, Some(sv), Some(dp), _) => {
                    if let Some(h) = hint.get_mut(sv.index()) {
                        *h = Some(dp);
                    }
                }
                _ => {}
            }
        };
        match desc.kind() {
            InstKind::Copy => {
                if let (Some(&d), Some(&s)) = (ops.first(), ops.get(1)) {
                    pair(d, s);
                }
            }
            InstKind::ParallelCopy => {
                let n = ops.len() / 2;
                for k in 0..n {
                    pair(ops[k], ops[n + k]);
                }
            }
            _ => {
                for (k, rule) in desc.operands().iter().enumerate() {
                    if let OperandRule::Tied(d) = *rule {
                        if let (Some(&dop), Some(&sop)) = (ops.get(usize::from(d)), ops.get(k)) {
                            pair(dop, sop);
                        }
                    }
                }
            }
        }
        for op in ops {
            if let Operand::Block(call) = op {
                let params = f.params(call.block());
                for (p, a) in params.iter().zip(f.args(*call)) {
                    pair(*p, *a);
                }
            }
        }
        if is_remat(f, inst) {
            if let Some(v) = ops.first().and_then(Operand::reg).and_then(Reg::as_virt) {
                if let Some(r) = self.remat.get_mut(v.index()) {
                    *r = true;
                }
            }
        }
    }
}

/// Colors `f` given the analysis and the affinities.
pub(crate) fn color(
    f: &Function,
    an: &Analysis<'_>,
    prefs: &Affinities,
    state: &SpillState,
) -> Result<Colored, RegAllocError> {
    let nv = f.vreg_count();
    let callee = f.abi().callee_saved();
    let mut loc = vec![Loc::None; nv];
    let mut aff = Affinity {
        parent: prefs.classes.parent.clone(),
        reg: vec![None; nv],
    };
    let hint = &prefs.hint;
    let remat = &prefs.remat;
    let mut tied_of: Vec<u32> = Vec::new();

    let weight = |v: u32| -> u64 {
        let i = v as usize;
        if remat.get(i).copied().unwrap_or(false) {
            return 0;
        }
        let uses = u64::from(an.uses.get(i).copied().unwrap_or(0));
        let span = an.span.get(i).copied().unwrap_or(0);
        // Uses per instruction spanned, scaled to keep integer precision.
        (uses.saturating_add(1) << 20) / span.saturating_add(1)
    };

    let mut owner = [NONE; MAX_PREGS];
    let mut spill: Vec<VReg> = Vec::new();
    let mut use_regs: Vec<(u32, Option<PReg>)> = Vec::new();
    let mut defs: Vec<(u32, usize)> = Vec::new();
    let entry = an.cfg.rpo.first().copied();
    for &b in &an.cfg.rpo {
        let block = Block::from_index(b as usize);
        owner.iter_mut().for_each(|o| *o = NONE);
        for &v in an.live_in(b as usize) {
            if let Some(Loc::Reg(r)) = loc.get(v as usize) {
                owner[r.index()] = v;
            }
        }
        // Block parameters: written together by each incoming edge, so they
        // need registers distinct from every live-in value and each other.
        let entry_phys: RegSet = if Some(b) == entry {
            f.params(block)
                .iter()
                .filter_map(|p| p.reg().and_then(Reg::as_phys))
                .collect()
        } else {
            RegSet::EMPTY
        };
        let mut dead_params: Vec<PReg> = Vec::new();
        for p in f.params(block) {
            let Some(v) = p.reg().and_then(Reg::as_virt) else {
                continue;
            };
            let vi = v.index() as u32;
            let class = super::class_of(f, v);
            let allowed = allowed_regs(f, an, class, vi).difference(entry_phys);
            match pick(f, &owner, allowed, &mut aff, hint, vi, None, callee) {
                Some(r) => {
                    loc[vi as usize] = Loc::Reg(r);
                    owner[r.index()] = vi;
                    if an.uses.get(vi as usize).copied().unwrap_or(0) == 0 {
                        dead_params.push(r);
                    }
                }
                None => {
                    evict_or_spill(
                        &mut loc, &mut owner, &mut spill, state, &weight, vi, allowed,
                    );
                }
            }
        }
        for r in dead_params {
            owner[r.index()] = NONE;
        }
        for &inst in f.insts(block) {
            color_inst(
                f,
                an,
                state,
                inst,
                &mut loc,
                &mut owner,
                &mut aff,
                hint,
                &weight,
                &mut spill,
                &mut use_regs,
                &mut defs,
                &mut tied_of,
                callee,
            )?;
        }
    }

    let locations = loc
        .iter()
        .map(|l| match *l {
            Loc::Reg(r) => super::Location::Reg(r),
            _ => super::Location::None,
        })
        .collect();
    spill.sort_unstable();
    spill.dedup();
    Ok(Colored { locations, spill })
}

/// The registers a value of `class` may take at all: allocatable (or the
/// frame pointer, when the frame does not keep one), not reserved, and never
/// written while it is live.
fn allowed_regs(f: &Function, an: &Analysis<'_>, class: RegClass, v: u32) -> RegSet {
    let target = f.target();
    target.class(class).regs().intersect(an.tracked).difference(
        an.forbidden
            .get(v as usize)
            .copied()
            .unwrap_or(RegSet::EMPTY),
    )
}

/// Picks a free register from `cand` for `v`: its tied operand's, its class's,
/// its hint, then caller-saved registers before callee-saved ones.
#[allow(clippy::too_many_arguments, reason = "one call site, all state")]
fn pick(
    f: &Function,
    owner: &[u32; MAX_PREGS],
    cand: RegSet,
    aff: &mut Affinity,
    hint: &[Option<PReg>],
    v: u32,
    tied: Option<PReg>,
    callee: RegSet,
) -> Option<PReg> {
    let _ = f;
    let free = |r: PReg| cand.contains(r) && owner[r.index()] == NONE;
    if let Some(r) = tied.filter(|&r| free(r)) {
        return Some(r);
    }
    let root = aff.find(v);
    if let Some(r) = aff
        .reg
        .get(root as usize)
        .copied()
        .flatten()
        .filter(|&r| free(r))
    {
        return Some(r);
    }
    if let Some(r) = hint.get(v as usize).copied().flatten().filter(|&r| free(r)) {
        let _ = aff.reg[root as usize].get_or_insert(r);
        return Some(r);
    }
    let mut avail = RegSet::EMPTY;
    for r in cand.iter() {
        if owner[r.index()] == NONE {
            avail = avail.with(r);
        }
    }
    let r = avail.difference(callee).first().or_else(|| avail.first())?;
    if let Some(slot) = aff.reg.get_mut(root as usize) {
        let _ = slot.get_or_insert(r);
    }
    Some(r)
}

/// No register is free for `v`: spill whichever of `v` and the values in
/// registers it could use has the lowest weight.
#[allow(clippy::too_many_arguments, reason = "one decision, all state")]
fn evict_or_spill(
    loc: &mut [Loc],
    owner: &mut [u32; MAX_PREGS],
    spill: &mut Vec<VReg>,
    state: &SpillState,
    weight: &impl Fn(u32) -> u64,
    v: u32,
    usable: RegSet,
) {
    let mut best: Option<(u64, PReg, u32)> = None;
    for r in usable.iter() {
        let w = owner[r.index()];
        if w == NONE || !state.spillable(w) {
            continue;
        }
        let wt = weight(w);
        if best.is_none_or(|(bw, _, _)| wt < bw) {
            best = Some((wt, r, w));
        }
    }
    let own = weight(v);
    match best {
        Some((bw, r, w)) if !state.spillable(v) || bw < own => {
            loc[w as usize] = Loc::Spill;
            spill.push(VReg::from_index(w));
            owner[r.index()] = v;
            loc[v as usize] = Loc::Reg(r);
        }
        _ => {
            // Spilling `v` itself; if it cannot be spilled either, the next
            // round still makes progress as long as something else spilled.
            loc[v as usize] = Loc::Spill;
            if state.spillable(v) {
                spill.push(VReg::from_index(v));
            }
        }
    }
}

#[allow(clippy::too_many_arguments, reason = "the coloring walk's state")]
fn color_inst(
    f: &Function,
    an: &Analysis<'_>,
    state: &SpillState,
    inst: Inst,
    loc: &mut [Loc],
    owner: &mut [u32; MAX_PREGS],
    aff: &mut Affinity,
    hint: &[Option<PReg>],
    weight: &impl Fn(u32) -> u64,
    spill: &mut Vec<VReg>,
    use_regs: &mut Vec<(u32, Option<PReg>)>,
    defs: &mut Vec<(u32, usize)>,
    tied_of: &mut Vec<u32>,
    callee: RegSet,
) -> Result<(), RegAllocError> {
    let target = f.target();
    let Some(desc) = f.opcode(inst).and_then(|o| target.inst(o)) else {
        return Ok(());
    };
    // Registers the instruction reads (virtual registers by their current
    // place), and which definition each tied use belongs to.
    use_regs.clear();
    defs.clear();
    let ops = f.operands(inst);
    f.visit_regs(inst, |pos, r, access| {
        let Some(v) = r.as_virt() else { return };
        let vi = v.index() as u32;
        match access {
            Access::Def => {
                let index = match pos {
                    crate::mir::visit::RegPos::Op(i) => i as usize,
                    _ => 0,
                };
                defs.push((vi, index));
            }
            Access::Use => {
                let place = match loc.get(vi as usize) {
                    Some(Loc::Reg(p)) => Some(*p),
                    _ => None,
                };
                use_regs.push((vi, place));
            }
        }
    });
    // Tied uses: the operand index each definition is tied to.
    tied_of.clear();
    let first_op = ops.as_ptr();
    let _ = first_op;
    for (k, rule) in desc.operands().iter().enumerate() {
        if let OperandRule::Tied(d) = *rule {
            tied_of.push(((usize::from(d) as u32) << 16) | k as u32);
        }
    }
    let early_clobber = desc.kind() == InstKind::Switch;
    let read_regs: RegSet = use_regs.iter().filter_map(|&(_, p)| p).collect();
    // Restricted to the tracked registers, which is all `allowed` can hold.
    let phys_read = an.reads(inst);

    // Last uses free their registers before the definitions are placed.
    let killed = an.kills(inst);
    for &k in killed {
        if let Some(Loc::Reg(r)) = loc.get(k as usize) {
            if owner[r.index()] == k {
                owner[r.index()] = NONE;
            }
        }
    }

    let w = an.writes(inst);
    let after = an
        .phys_after
        .get(inst.index())
        .copied()
        .unwrap_or(RegSet::EMPTY);
    // Definitions are listed first, so a definition's operand index is its
    // position among the instruction's operands.
    let base = f.operand_base(inst);
    for &(d, abs) in defs.iter() {
        let index = abs.saturating_sub(base);
        let class = super::class_of(f, VReg::from_index(d));
        let mut allowed = allowed_regs(f, an, class, d)
            .difference(after)
            .difference(w);
        // The tied use, if any.
        let mut tied_reg = None;
        let mut tie_free = false;
        for &t in tied_of.iter() {
            if (t >> 16) as usize == index {
                let use_index = (t & 0xFFFF) as usize;
                if let Some(Operand::Reg(r)) = ops.get(use_index) {
                    if let Some(v) = r.as_virt() {
                        if let Some(Loc::Reg(p)) = loc.get(v.index()) {
                            tied_reg = Some(*p);
                            tie_free = killed.contains(&(v.index() as u32));
                        }
                    }
                }
            }
        }
        let has_tie = tied_of.iter().any(|&t| (t >> 16) as usize == index);
        if early_clobber {
            allowed = allowed.difference(read_regs);
        }
        let choice = if has_tie
            && tie_free
            && tied_reg.is_some_and(|r| allowed.contains(r) && owner[r.index()] == NONE)
        {
            tied_reg
        } else {
            if has_tie {
                // The tie is fixed with a copy into the definition's register
                // before the instruction: it must not hold another operand.
                allowed = allowed.difference(read_regs).difference(phys_read);
            }
            pick(f, owner, allowed, aff, hint, d, None, callee)
        };
        match choice {
            Some(r) => {
                loc[d as usize] = Loc::Reg(r);
                owner[r.index()] = d;
                let root = aff.find(d);
                if let Some(slot) = aff.reg.get_mut(root as usize) {
                    let _ = slot.get_or_insert(r);
                }
            }
            None => {
                if !state.spillable(d)
                    && !allowed.iter().any(|r| {
                        let o = owner[r.index()];
                        o != NONE && state.spillable(o)
                    })
                    && spill.is_empty()
                {
                    return Err(RegAllocError::NoRegister { class });
                }
                evict_or_spill(loc, owner, spill, state, weight, d, allowed);
            }
        }
    }
    // Definitions nobody reads die at once.
    for &(d, _) in defs.iter() {
        if an.uses.get(d as usize).copied().unwrap_or(0) == 0 {
            if let Some(Loc::Reg(r)) = loc.get(d as usize) {
                if owner[r.index()] == d {
                    owner[r.index()] = NONE;
                }
            }
        }
    }
    Ok(())
}
