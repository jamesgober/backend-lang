//! The control-flow graph of a function: successors, predecessors, reverse
//! postorder, and the dominator tree, every walk iterative. Dominators are
//! computed by Lengauer and Tarjan's algorithm (O(m log n) on any graph), not
//! by the simpler Cooper–Harvey–Kennedy iteration, whose intersections walk
//! the dominator tree once per predecessor and turn quadratic when a shared
//! block (a trap block, a landing pad) has thousands of predecessors deep in
//! the tree; dominance queries are then O(1) interval tests on the tree.

use alloc::vec;
use alloc::vec::Vec;

use super::entity::Block;
use super::function::Function;
use super::operand::Operand;
use super::verify::MirError;
use super::visit::Access;
use crate::target::InstKind;

pub(crate) const NONE: u32 = u32::MAX;

/// A function's control-flow graph and dominator tree.
#[derive(Clone, Debug, Default)]
pub(crate) struct Cfg {
    succ_start: Vec<u32>,
    succs: Vec<u32>,
    pred_start: Vec<u32>,
    preds: Vec<u32>,
    /// Reachable blocks in reverse postorder (the entry first).
    pub(crate) rpo: Vec<u32>,
    /// Each block's position in `rpo`, or `NONE` if unreachable.
    pub(crate) rpo_index: Vec<u32>,
    /// Immediate dominators (the entry's is itself; `NONE` if unreachable).
    pub(crate) idom: Vec<u32>,
    pre: Vec<u32>,
    post: Vec<u32>,
}

/// The successors of `block` named by its terminator, then its call sites'
/// landing pads.
pub(crate) fn for_each_succ(f: &Function, block: Block, mut visit: impl FnMut(Block)) {
    let insts = f.insts(block);
    if let Some(&last) = insts.last() {
        let kind = f
            .opcode(last)
            .and_then(|o| f.target().inst(o))
            .map(|d| d.kind());
        if kind.is_some_and(InstKind::is_terminator) {
            for op in f.operands(last) {
                if let Operand::Block(call) = op {
                    visit(call.block());
                }
            }
        }
    }
    if !f.sites.is_empty() {
        for &inst in insts {
            if let Some(pad) = f.call_site(inst).and_then(|s| s.landing_pad()) {
                visit(pad);
            }
        }
    }
}

impl Cfg {
    /// Builds the graph, the reverse postorder, and the dominator tree.
    pub(crate) fn new(f: &Function) -> Cfg {
        let n = f.block_count();
        let mut cfg = Cfg {
            succ_start: Vec::with_capacity(n + 1),
            ..Cfg::default()
        };
        for b in f.blocks() {
            cfg.succ_start.push(cfg.succs.len() as u32);
            for_each_succ(f, b, |s| {
                if s.index() < n {
                    cfg.succs.push(s.index() as u32);
                }
            });
        }
        cfg.succ_start.push(cfg.succs.len() as u32);

        // Predecessors by counting sort over the successor lists.
        let mut count = vec![0u32; n + 1];
        for &s in &cfg.succs {
            count[s as usize + 1] += 1;
        }
        for i in 0..n {
            count[i + 1] += count[i];
        }
        cfg.pred_start = count.clone();
        let mut preds = vec![0; cfg.succs.len()];
        for b in 0..n {
            for &s in cfg.succ_slice(b) {
                let at = &mut count[s as usize];
                preds[*at as usize] = b as u32;
                *at += 1;
            }
        }
        cfg.preds = preds;

        // Reverse postorder by an explicit-stack depth-first walk.
        cfg.rpo_index = vec![NONE; n];
        if n > 0 {
            let mut post = Vec::with_capacity(n);
            let mut seen = vec![false; n];
            let mut stack: Vec<(u32, u32)> = vec![(0, 0)];
            seen[0] = true;
            while let Some(top) = stack.last_mut() {
                let (b, k) = *top;
                let succs = cfg.succ_slice(b as usize);
                if let Some(&s) = succs.get(k as usize) {
                    top.1 += 1;
                    if !seen[s as usize] {
                        seen[s as usize] = true;
                        stack.push((s, 0));
                    }
                } else {
                    post.push(b);
                    let _ = stack.pop();
                }
            }
            post.reverse();
            for (i, &b) in post.iter().enumerate() {
                cfg.rpo_index[b as usize] = i as u32;
            }
            cfg.rpo = post;
        }
        cfg.dominators();
        cfg
    }

    fn succ_slice(&self, b: usize) -> &[u32] {
        let (s, e) = (self.succ_start[b] as usize, self.succ_start[b + 1] as usize);
        self.succs.get(s..e).unwrap_or(&[])
    }

    /// The successors of block `b`.
    pub(crate) fn succs(&self, b: usize) -> &[u32] {
        if b + 1 < self.succ_start.len() {
            self.succ_slice(b)
        } else {
            &[]
        }
    }

    /// The predecessors of block `b` (with repeats for repeated edges).
    pub(crate) fn preds(&self, b: usize) -> &[u32] {
        match (self.pred_start.get(b), self.pred_start.get(b + 1)) {
            (Some(&s), Some(&e)) => self.preds.get(s as usize..e as usize).unwrap_or(&[]),
            _ => &[],
        }
    }

    /// Whether block `b` is reachable from the entry.
    pub(crate) fn reachable(&self, b: usize) -> bool {
        self.rpo_index.get(b).is_some_and(|&i| i != NONE)
    }

    fn dominators(&mut self) {
        let n = self.rpo_index.len();
        self.idom = vec![NONE; n];
        let Some(&entry) = self.rpo.first() else {
            return;
        };
        self.lengauer_tarjan(entry);
        // Pre- and postorder numbers of the dominator tree make dominance an
        // O(1) interval test.
        let mut child_start = vec![0u32; n + 1];
        for &b in &self.rpo {
            let d = self.idom[b as usize];
            if d != b && d != NONE {
                child_start[d as usize + 1] += 1;
            }
        }
        for i in 0..n {
            child_start[i + 1] += child_start[i];
        }
        let mut fill = child_start.clone();
        let mut children = vec![0u32; child_start[n] as usize];
        for &b in &self.rpo {
            let d = self.idom[b as usize];
            if d != b && d != NONE {
                children[fill[d as usize] as usize] = b;
                fill[d as usize] += 1;
            }
        }
        self.pre = vec![NONE; n];
        self.post = vec![NONE; n];
        let (mut pre, mut post) = (0u32, 0u32);
        let mut stack: Vec<(u32, u32)> = vec![(entry, child_start[entry as usize])];
        self.pre[entry as usize] = pre;
        pre += 1;
        while let Some(top) = stack.last_mut() {
            let (b, k) = *top;
            if k < child_start[b as usize + 1] {
                top.1 += 1;
                let c = children[k as usize];
                self.pre[c as usize] = pre;
                pre += 1;
                stack.push((c, child_start[c as usize]));
            } else {
                self.post[b as usize] = post;
                post += 1;
                let _ = stack.pop();
            }
        }
    }

    /// Immediate dominators by Lengauer and Tarjan's algorithm with path
    /// compression: O(m log n) on any graph, where the simpler iterative
    /// algorithm degrades to quadratic time when one block (a shared trap
    /// block, say) has many predecessors deep in the dominator tree. Every
    /// walk is iterative.
    fn lengauer_tarjan(&mut self, entry: u32) {
        let n = self.rpo_index.len();
        // Depth-first preorder numbers (1-based; 0 = not reached), parents.
        let mut dfn = vec![0u32; n];
        let mut vertex: Vec<u32> = Vec::with_capacity(n + 1);
        vertex.push(NONE);
        let mut parent = vec![0u32; n + 1];
        let mut stack: Vec<(u32, u32)> = vec![(entry, 0)];
        dfn[entry as usize] = 1;
        vertex.push(entry);
        while let Some(top) = stack.last_mut() {
            let (b, k) = *top;
            let succs = self.succ_slice(b as usize);
            if let Some(&s) = succs.get(k as usize) {
                top.1 += 1;
                if dfn[s as usize] == 0 {
                    let num = vertex.len() as u32;
                    dfn[s as usize] = num;
                    vertex.push(s);
                    parent[num as usize] = dfn[b as usize];
                    stack.push((s, 0));
                }
            } else {
                let _ = stack.pop();
            }
        }
        let count = vertex.len() - 1;
        // Working arrays indexed by preorder number.
        let mut semi: Vec<u32> = (0..=count as u32).collect();
        let mut label: Vec<u32> = (0..=count as u32).collect();
        let mut ancestor = vec![0u32; count + 1];
        let mut idom = vec![0u32; count + 1];
        let mut bucket_head = vec![0u32; count + 1];
        let mut bucket_next = vec![0u32; count + 1];
        let mut path: Vec<u32> = Vec::new();
        // eval(v): the vertex of minimum semi on the compressed path from v
        // to its forest root, compressing as it goes.
        let mut eval =
            |v: u32, ancestor: &mut Vec<u32>, label: &mut Vec<u32>, semi: &Vec<u32>| -> u32 {
                if ancestor[v as usize] == 0 {
                    return v;
                }
                path.clear();
                let mut x = v;
                while ancestor[ancestor[x as usize] as usize] != 0 {
                    path.push(x);
                    x = ancestor[x as usize];
                }
                // Compress from the top of the path down.
                while let Some(y) = path.pop() {
                    let a = ancestor[y as usize];
                    if semi[label[a as usize] as usize] < semi[label[y as usize] as usize] {
                        label[y as usize] = label[a as usize];
                    }
                    ancestor[y as usize] = ancestor[a as usize];
                }
                label[v as usize]
            };
        for w in (2..=count as u32).rev() {
            let block = vertex[w as usize];
            for &p in self.preds(block as usize) {
                let v = dfn[p as usize];
                if v == 0 {
                    continue;
                }
                let u = eval(v, &mut ancestor, &mut label, &semi);
                if semi[u as usize] < semi[w as usize] {
                    semi[w as usize] = semi[u as usize];
                }
            }
            // Bucket w under its semidominator.
            let sd = semi[w as usize] as usize;
            bucket_next[w as usize] = bucket_head[sd];
            bucket_head[sd] = w;
            let pw = parent[w as usize];
            ancestor[w as usize] = pw;
            let mut v = bucket_head[pw as usize];
            bucket_head[pw as usize] = 0;
            while v != 0 {
                let next = bucket_next[v as usize];
                let u = eval(v, &mut ancestor, &mut label, &semi);
                idom[v as usize] = if semi[u as usize] < semi[v as usize] {
                    u
                } else {
                    pw
                };
                v = next;
            }
        }
        for w in 2..=count {
            if idom[w] != semi[w] {
                idom[w] = idom[idom[w] as usize];
            }
        }
        self.idom[entry as usize] = entry;
        for w in 2..=count {
            self.idom[vertex[w] as usize] = vertex[idom[w] as usize];
        }
    }

    /// Whether block `a` dominates block `b` (both reachable).
    pub(crate) fn dominates(&self, a: usize, b: usize) -> bool {
        match (
            self.pre.get(a),
            self.pre.get(b),
            self.post.get(a),
            self.post.get(b),
        ) {
            (Some(&pa), Some(&pb), Some(&qa), Some(&qb)) => {
                pa != NONE && pb != NONE && pa <= pb && qb <= qa
            }
            _ => false,
        }
    }
}

impl Function {
    /// Checks [`verify`](Function::verify)'s rules, then SSA dominance: in
    /// every block reachable from the entry, each use of a virtual register
    /// is dominated by its definition (a use in the defining block comes
    /// after the definition; a branch argument is a use at the end of its
    /// block). This is the register allocator's precondition. Linear in the
    /// size of the function.
    ///
    /// # Errors
    ///
    /// The first [`MirError`]: [`verify`](Function::verify)'s errors, then
    /// [`MirError::UndefinedVReg`] or [`MirError::NotDominated`].
    ///
    /// # Examples
    ///
    /// ```
    /// use backend_lang::mir::{Function, MirError, Operand};
    /// use backend_lang::x86_64::{op, GPR, SYSV};
    ///
    /// let mut f = Function::new("f", &SYSV);
    /// let (a, b) = (f.block()?, f.block()?);
    /// let v = f.vreg(GPR)?;
    /// let to_b = f.block_call(b, &[])?;
    /// f.push(a, op::JMP, &[to_b.into()])?;
    /// let w = f.vreg(GPR)?;
    /// f.push(b, op::MOV64, &[w.into(), v.into()])?; // v is never defined
    /// f.push(b, op::RET, &[])?;
    /// assert!(matches!(f.verify_ssa(), Err(MirError::UndefinedVReg { .. })));
    /// # Ok::<(), MirError>(())
    /// ```
    pub fn verify_ssa(&self) -> Result<(), MirError> {
        self.verify()?;
        let cfg = Cfg::new(self);
        self.check_dominance(&cfg)
    }

    pub(crate) fn check_dominance(&self, cfg: &Cfg) -> Result<(), MirError> {
        // Each virtual register's defining block and position (0 for a block
        // parameter, k for the k-th instruction, counting from 1).
        let mut def_block = vec![NONE; self.vreg_count()];
        let mut def_pos = vec![0u32; self.vreg_count()];
        for b in self.blocks() {
            for p in self.params(b) {
                if let Some(v) = p.reg().and_then(crate::Reg::as_virt) {
                    if let Some(d) = def_block.get_mut(v.index()) {
                        *d = b.index() as u32;
                    }
                }
            }
            for (k, &inst) in self.insts(b).iter().enumerate() {
                self.visit_regs(inst, |_, r, access| {
                    if let (Access::Def, Some(v)) = (access, r.as_virt()) {
                        if let Some(d) = def_block.get_mut(v.index()) {
                            *d = b.index() as u32;
                            def_pos[v.index()] = k as u32 + 1;
                        }
                    }
                });
            }
        }
        for &b in &cfg.rpo {
            let block = Block::from_index(b as usize);
            for (k, &inst) in self.insts(block).iter().enumerate() {
                let mut err = None;
                self.visit_regs(inst, |_, r, access| {
                    let Some(v) = r.as_virt() else { return };
                    if access != Access::Use || err.is_some() {
                        return;
                    }
                    let d = def_block.get(v.index()).copied().unwrap_or(NONE);
                    if d == NONE {
                        err = Some(MirError::UndefinedVReg { vreg: v });
                    } else if d == b {
                        if def_pos[v.index()] > k as u32 {
                            err = Some(MirError::NotDominated { vreg: v, inst });
                        }
                    } else if !cfg.dominates(d as usize, b as usize) {
                        err = Some(MirError::NotDominated { vreg: v, inst });
                    }
                });
                if let Some(e) = err {
                    return Err(e);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::needless_range_loop,
    reason = "the reference compares dominator sets by block index"
)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use proptest::prelude::*;

    use super::*;
    use crate::mir::Operand;
    use crate::x86_64::{self, SYSV, op};

    /// A function whose block `i` branches to `succs[i]` (zero, one, or two
    /// targets; more become a jump table).
    fn graph(succs: &[Vec<usize>]) -> Function {
        let mut f = Function::new("g", &SYSV);
        let blocks: Vec<Block> = succs
            .iter()
            .map(|_| f.block().unwrap_or(Block::new(0)))
            .collect();
        for (i, s) in succs.iter().enumerate() {
            let b = blocks[i];
            let call = |f: &mut Function, t: usize| f.block_call(blocks[t], &[]).map(Operand::from);
            let r = match s.len() {
                0 => f.push(b, op::RET, &[]),
                1 => call(&mut f, s[0]).and_then(|c| f.push(b, op::JMP, &[c])),
                2 => {
                    let (t, e) = (call(&mut f, s[0]), call(&mut f, s[1]));
                    match (t, e) {
                        (Ok(t), Ok(e)) => f.push(
                            b,
                            op::JCC,
                            &[x86_64::cond(isa_lang::x86_64::Cond::E).into(), t, e],
                        ),
                        (Err(e), _) | (_, Err(e)) => Err(e),
                    }
                }
                _ => {
                    let t = f.vreg(x86_64::GPR).and_then(|t| {
                        let i = f.vreg(x86_64::GPR)?;
                        Ok((t, i))
                    });
                    match t {
                        Ok((t, i)) => {
                            let mut ops = vec![Operand::from(t), Operand::from(i)];
                            for &k in s {
                                if let Ok(c) = call(&mut f, k) {
                                    ops.push(c);
                                }
                            }
                            f.push(b, op::JMPTBL, &ops)
                        }
                        Err(e) => Err(e),
                    }
                }
            };
            assert!(r.is_ok(), "{r:?}");
        }
        f
    }

    /// Dominator sets by the textbook data-flow iteration.
    fn reference(cfg: &Cfg, n: usize) -> Vec<Option<Vec<bool>>> {
        let mut dom: Vec<Option<Vec<bool>>> = vec![None; n];
        dom[0] = Some({
            let mut v = vec![false; n];
            v[0] = true;
            v
        });
        let mut changed = true;
        while changed {
            changed = false;
            for b in 1..n {
                if !cfg.reachable(b) {
                    continue;
                }
                let mut acc: Option<Vec<bool>> = None;
                for &p in cfg.preds(b) {
                    if let Some(d) = &dom[p as usize] {
                        acc = Some(match acc {
                            None => d.clone(),
                            Some(a) => a.iter().zip(d).map(|(x, y)| *x && *y).collect(),
                        });
                    }
                }
                if let Some(mut a) = acc {
                    a[b] = true;
                    if dom[b].as_ref() != Some(&a) {
                        dom[b] = Some(a);
                        changed = true;
                    }
                }
            }
        }
        dom
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]

        #[test]
        fn prop_dominators_match_the_dataflow_reference(
            edges in prop::collection::vec(prop::collection::vec(0usize..24, 0..4), 1..24)
        ) {
            let n = edges.len();
            let succs: Vec<Vec<usize>> = edges
                .iter()
                .map(|s| s.iter().map(|&t| t % n).collect::<Vec<_>>())
                .map(|s: Vec<usize>| if s.len() == 2 && s[0] == s[1] { vec![s[0]] } else { s })
                .collect();
            let f = graph(&succs);
            let cfg = Cfg::new(&f);
            let dom = reference(&cfg, n);
            for b in 0..n {
                prop_assert_eq!(cfg.reachable(b), dom[b].is_some());
                let Some(d) = &dom[b] else { continue };
                for a in 0..n {
                    prop_assert_eq!(cfg.dominates(a, b), d[a], "{} dom {}", a, b);
                }
                if b != 0 {
                    // The immediate dominator: the strict dominator that every
                    // other strict dominator dominates.
                    let idom = cfg.idom[b] as usize;
                    prop_assert!(d[idom] && idom != b);
                    for a in 0..n {
                        if d[a] && a != b {
                            prop_assert!(cfg.dominates(a, idom));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn test_many_predecessors_deep_in_the_tree_stay_linear() {
        // A chain of 20,000 blocks, each also branching to one shared sink:
        // the shape that makes the simple iterative algorithm quadratic.
        let n = 20_000;
        let mut succs: Vec<Vec<usize>> = (0..n).map(|i| vec![i + 1, n]).collect();
        succs.push(vec![]); // n: the sink
        succs[n - 1] = vec![n];
        let f = graph(&succs);
        let cfg = Cfg::new(&f);
        assert_eq!(cfg.idom[n] as usize, 0);
        assert!(cfg.dominates(0, n) && cfg.dominates(n - 1, n - 1));
    }
}
