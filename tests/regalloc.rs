//! The register allocator against an independent checker: on every function
//! instruction selection makes from random IR programs, and on hand-built
//! functions with cycles, ties, and fixed registers, the final assignment is
//! checked point by point with a liveness computed here from scratch:
//!
//! - no two values live at the same point share a register;
//! - no value is live across an instruction that writes its register (a
//!   call's clobbers, an implicit definition, a copy into a fixed register);
//! - no definition takes a register a fixed-register value still needs;
//! - a block's parameters avoid each other and every value live into it.
//!
//! The allocated code itself is executed against ir-lang's interpreter in
//! `differential.rs`.

#![allow(clippy::unwrap_used, reason = "test failures should be loud")]

mod common;

use std::collections::BTreeSet;

use backend_lang::mir::{Block, Effects, Function, ImplicitKind, Inst, Operand};

use backend_lang::regalloc::{self, Location, Options};
use backend_lang::target::{InstKind, OperandRule};
use backend_lang::x86_64::{self, GPR, RAX, RCX, RDI, RDX, RSI, SYSV, op};
use backend_lang::{PReg, Reg, RegSet};
use common::irgen::{self, GenOptions};
use isa_lang::x86_64::Cond;
use proptest::prelude::*;

/// Registers read and written by one instruction, as the checker sees them.
struct Access {
    defs: Vec<Reg>,
    uses: Vec<Reg>,
    writes: RegSet,
}

fn access(f: &Function, inst: Inst) -> Access {
    let desc = f.target().inst(f.opcode(inst).unwrap()).unwrap();
    let ops = f.operands(inst);
    let mut defs = Vec::new();
    let mut uses = Vec::new();
    let mut writes = f.clobbers(inst).union(desc.clobbers());
    for &r in desc.implicit_defs() {
        writes = writes.with(r);
    }
    for i in f.implicit(inst) {
        if i.kind == ImplicitKind::Def {
            writes = writes.with(i.reg);
        }
    }
    let ndefs = match desc.kind() {
        InstKind::Copy => 1,
        InstKind::ParallelCopy => ops.len() / 2,
        _ => desc
            .operands()
            .iter()
            .take_while(|r| matches!(r, OperandRule::Def(_)))
            .count(),
    };
    for (k, op) in ops.iter().enumerate() {
        match op {
            Operand::Reg(r) if k < ndefs => {
                defs.push(*r);
                if let Some(p) = r.as_phys() {
                    writes = writes.with(p);
                }
            }
            Operand::Reg(r) => uses.push(*r),
            Operand::Mem(m) => {
                uses.extend(m.base_reg());
                uses.extend(m.index_reg().map(|(r, _)| r));
            }
            Operand::Block(call) => {
                uses.extend(f.args(*call).iter().filter_map(|a| a.reg()));
            }
            _ => {}
        }
    }
    Access { defs, uses, writes }
}

fn phys_uses(f: &Function, inst: Inst) -> RegSet {
    let desc = f.target().inst(f.opcode(inst).unwrap()).unwrap();
    let mut set: RegSet = desc.implicit_uses().iter().copied().collect();
    for i in f.implicit(inst) {
        if i.kind == ImplicitKind::Use {
            set = set.with(i.reg);
        }
    }
    for r in access(f, inst).uses {
        if let Some(p) = r.as_phys() {
            set = set.with(p);
        }
    }
    set
}

fn successors(f: &Function, b: Block) -> Vec<Block> {
    let mut out = Vec::new();
    if let Some(&last) = f.insts(b).last() {
        for op in f.operands(last) {
            if let Operand::Block(c) = op {
                out.push(c.block());
            }
        }
    }
    for &i in f.insts(b) {
        if let Some(pad) = f.call_site(i).and_then(|s| s.landing_pad()) {
            out.push(pad);
        }
    }
    out
}

/// Checks an assignment of `f`'s virtual registers; returns the first
/// violation.
fn check(f: &Function, locs: &[Location]) -> Result<(), String> {
    let n = f.block_count();
    let reachable = {
        let mut seen = vec![false; n];
        let mut stack = vec![Block::new(0)];
        seen[0] = true;
        while let Some(b) = stack.pop() {
            for s in successors(f, b) {
                if !seen[s.index()] {
                    seen[s.index()] = true;
                    stack.push(s);
                }
            }
        }
        seen
    };
    let virt = |r: Reg| r.as_virt().map(|v| v.index());
    // Live-in sets by iterating to a fixed point (fine at test sizes).
    let mut live_in: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
    let mut changed = true;
    while changed {
        changed = false;
        for b in (0..n).rev() {
            if !reachable[b] {
                continue;
            }
            let block = Block::new(b as u32);
            let mut live: BTreeSet<usize> = BTreeSet::new();
            for s in successors(f, block) {
                live.extend(live_in[s.index()].iter().copied());
            }
            for &i in f.insts(block).iter().rev() {
                let a = access(f, i);
                for d in a.defs.iter().filter_map(|&r| virt(r)) {
                    let _ = live.remove(&d);
                }
                live.extend(a.uses.iter().filter_map(|&r| virt(r)));
            }
            for p in f.params(block) {
                if let Some(v) = p.reg().and_then(virt) {
                    let _ = live.remove(&v);
                }
            }
            if live != live_in[b] {
                live_in[b] = live;
                changed = true;
            }
        }
    }
    let reg_of = |v: usize| match locs.get(v) {
        Some(Location::Reg(r)) => Some(*r),
        _ => None,
    };
    let distinct = |set: &BTreeSet<usize>, what: &str| -> Result<(), String> {
        let mut seen: Vec<(PReg, usize)> = Vec::new();
        for &v in set {
            if let Some(r) = reg_of(v) {
                if let Some((_, w)) = seen.iter().find(|(p, _)| *p == r) {
                    return Err(format!("{what}: %{v} and %{w} share {r:?}"));
                }
                seen.push((r, v));
            }
        }
        Ok(())
    };
    for b in 0..n {
        if !reachable[b] {
            continue;
        }
        let block = Block::new(b as u32);
        // Parameters are written together on each edge: distinct from each
        // other and from everything live into the block.
        let mut at_start = live_in[b].clone();
        for p in f.params(block) {
            if let Some(v) = p.reg().and_then(virt) {
                if reg_of(v).is_none() {
                    return Err(format!("parameter %{v} of {block} has no register"));
                }
                let _ = at_start.insert(v);
            }
        }
        distinct(&at_start, &format!("start of {block}"))?;
        let mut live: BTreeSet<usize> = BTreeSet::new();
        for s in successors(f, block) {
            live.extend(live_in[s.index()].iter().copied());
        }
        let mut phys = RegSet::EMPTY;
        for &i in f.insts(block).iter().rev() {
            let a = access(f, i);
            let defs: Vec<usize> = a.defs.iter().filter_map(|&r| virt(r)).collect();
            distinct(&live, &format!("after {i}"))?;
            // A value live after `i` that `i` does not define is live across
            // it: `i` must not write its register.
            for &v in &live {
                if defs.contains(&v) {
                    continue;
                }
                if let Some(r) = reg_of(v) {
                    if a.writes.contains(r) {
                        return Err(format!("{i} writes {r:?}, which holds live %{v}"));
                    }
                }
            }
            // A definition must not take a register a fixed-register value
            // live after `i` holds, nor one `i` writes otherwise.
            for &d in &defs {
                let r = reg_of(d).ok_or(format!("%{d} defined at {i} has no register"))?;
                if phys.contains(r) {
                    return Err(format!(
                        "{i} defines %{d} in {r:?}, live as a fixed register"
                    ));
                }
                let own_phys_def = a.defs.iter().any(|x| x.as_phys() == Some(r));
                if a.writes.contains(r) && !own_phys_def {
                    return Err(format!("{i} defines %{d} in {r:?}, which it also clobbers"));
                }
            }
            for d in &defs {
                let _ = live.remove(d);
            }
            live.extend(a.uses.iter().filter_map(|&r| virt(r)));
            phys = phys.difference(a.writes).union(phys_uses(f, i));
        }
    }
    Ok(())
}

fn allocate_checked(f: &mut Function) -> Result<regalloc::Allocation, String> {
    let a = regalloc::allocate_with(f, &Options::new().keep_assignment(true))
        .map_err(|e| format!("{e}"))?;
    let (virt, locs) = a.assignment().unwrap();
    check(virt, locs).map_err(|e| format!("{e}\n{virt}"))?;
    Ok(a)
}

#[test]
fn test_assignments_of_selected_random_programs_are_interference_free() {
    let opts = GenOptions {
        statements: 120,
        functions: 3,
        ..GenOptions::default()
    };
    let mut spilled = 0;
    for seed in 0..60u64 {
        let prog = irgen::generate(seed, &opts);
        for options in [
            backend_lang::Options::sysv(),
            backend_lang::Options::win64().with_frame_pointer(false),
        ] {
            for f in prog.module.functions() {
                let mut mir = backend_lang::isel::select(&prog.module, f, &options).unwrap();
                let a = allocate_checked(&mut mir).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
                spilled += a.spilled();
                // And the result emits.
                let mut e = x86_64::Emitter::new();
                let _ = e
                    .function(&mir)
                    .unwrap_or_else(|err| panic!("seed {seed}: {err}\n{mir}"));
            }
        }
    }
    assert!(
        spilled > 100,
        "the sweep should exercise spilling ({spilled})"
    );
}

/// A function that keeps `n` values live at once and sums them, so the
/// allocator must spill once `n` passes the register count.
fn many_live(n: usize, across_call: bool) -> Function {
    let mut f = Function::new("many", &SYSV);
    let b = f.block().unwrap();
    let mut vals = Vec::new();
    for k in 0..n {
        let v = f.vreg(GPR).unwrap();
        f.push(b, op::MOV64RI, &[v.into(), Operand::Imm(k as i64 * 3 + 1)])
            .unwrap();
        // Not constants: rematerialization would hide the spilling.
        let w = f.vreg(GPR).unwrap();
        f.push(b, op::ADD64, &[w.into(), v.into(), Operand::Imm(1)])
            .unwrap();
        vals.push(w);
    }
    if across_call {
        let g = f.symbol("g").unwrap();
        f.push_with(
            b,
            op::CALL,
            &[g.into()],
            Effects::new().clobbers(SYSV.call_clobbers()),
        )
        .unwrap();
    }
    let mut acc = vals[0];
    for &v in &vals[1..] {
        let s = f.vreg(GPR).unwrap();
        f.push(b, op::ADD64, &[s.into(), acc.into(), v.into()])
            .unwrap();
        acc = s;
    }
    f.push(b, op::COPY, &[RAX.into(), acc.into()]).unwrap();
    f.push_with(b, op::RET, &[], Effects::new().uses(&[RAX]))
        .unwrap();
    f
}

#[test]
fn test_pressure_beyond_the_register_file_spills_and_still_computes() {
    for (n, call) in [(10, false), (20, false), (40, false), (8, true), (30, true)] {
        let mut f = many_live(n, call);
        let a = allocate_checked(&mut f).unwrap();
        if n > 14 || (call && n > 5) {
            assert!(a.spilled() > 0, "{n} values ({call}) must spill");
        }
        // Run it: the sum of k*3+2 for k < n.
        let mut e = x86_64::Emitter::new();
        let info = e.function(&f).unwrap();
        let _ = info;
        if call {
            let mut g = Function::new("g", &SYSV);
            let gb = g.block().unwrap();
            // Scribble on every caller-saved register before returning.
            for r in SYSV
                .caller_saved()
                .iter()
                .filter(|r| x86_64::TARGET.class_of(*r) == Some(GPR))
            {
                g.push(gb, op::MOV64RI, &[r.into(), Operand::Imm(-1)])
                    .unwrap();
            }
            g.push(gb, op::RET, &[]).unwrap();
            let _ = e.function(&g).unwrap();
        }
        let p = e.finish().unwrap();
        let (m, stop) = common::run(p.bytes(), 0, &[]);
        assert_eq!(stop, Ok(common::Stop::Returned));
        let want: i64 = (0..n as i64).map(|k| k * 3 + 2).sum();
        assert_eq!(m.regs[0] as i64, want, "{n} live values ({call})");
    }
}

#[test]
fn test_rematerialized_mov32ri_keeps_its_zero_extension() {
    // k = mov32ri -1 holds 0xFFFF_FFFF. Live across twenty other values, it
    // is the cheapest to spill (a constant), so it is rematerialized at its
    // use, a 64-bit add: the immediate -1 there would subtract one instead.
    let mut f = Function::new("many", &SYSV);
    let b = f.block().unwrap();
    let k = f.vreg(GPR).unwrap();
    f.push(b, op::MOV32RI, &[k.into(), Operand::Imm(-1)])
        .unwrap();
    let mut vals = Vec::new();
    for i in 0..20 {
        let v = f.vreg(GPR).unwrap();
        f.push(b, op::MOV64RI, &[v.into(), Operand::Imm(i * 3 + 1)])
            .unwrap();
        let w = f.vreg(GPR).unwrap();
        f.push(b, op::ADD64, &[w.into(), v.into(), Operand::Imm(1)])
            .unwrap();
        vals.push(w);
    }
    let mut acc = vals[0];
    for &v in &vals[1..] {
        let s = f.vreg(GPR).unwrap();
        f.push(b, op::ADD64, &[s.into(), acc.into(), v.into()])
            .unwrap();
        acc = s;
    }
    let s = f.vreg(GPR).unwrap();
    f.push(b, op::ADD64, &[s.into(), acc.into(), k.into()])
        .unwrap();
    f.push(b, op::COPY, &[RAX.into(), s.into()]).unwrap();
    f.push_with(b, op::RET, &[], Effects::new().uses(&[RAX]))
        .unwrap();
    let a = allocate_checked(&mut f).unwrap();
    assert!(
        a.rematerialized() > 0,
        "the constant should be rematerialized\n{f}"
    );
    let code = backend_lang::emit(&f).unwrap();
    let (m, stop) = common::run(&code, 0, &[]);
    assert_eq!(stop, Ok(common::Stop::Returned));
    let want: i64 = (0..20i64).map(|k| k * 3 + 2).sum::<i64>() + 0xFFFF_FFFF;
    assert_eq!(m.regs[0] as i64, want, "{f}");
}

#[test]
fn test_jump_table_temporary_is_never_spilled() {
    // Thirteen values live through a jump table fill the register file, so
    // the table's address temporary finds no free register. Nothing can
    // follow the jump to store it, so it is never a spill candidate itself
    // (the allocator pins values terminators define): something else is
    // spilled to make room.
    let mut f = Function::new("table", &SYSV);
    let entry = f.block().unwrap();
    let table = f.block().unwrap();
    let cases: Vec<Block> = (0..4).map(|_| f.block().unwrap()).collect();
    f.add_param(entry, RDI.into()).unwrap();
    let x = f.vreg(GPR).unwrap();
    f.push(entry, op::COPY, &[x.into(), RDI.into()]).unwrap();
    let mut vals = Vec::new();
    for d in 0..13 {
        let w = f.vreg(GPR).unwrap();
        let at = backend_lang::mir::Mem::base(x).with_disp(d);
        f.push(entry, op::LEA64, &[w.into(), at.into()]).unwrap();
        vals.push(w);
    }
    let i = f.vreg(GPR).unwrap();
    f.push(entry, op::MOV32, &[i.into(), x.into()]).unwrap();
    let to_table = f.block_call(table, &[]).unwrap();
    f.push(entry, op::JMP, &[to_table.into()]).unwrap();
    let t = f.vreg(GPR).unwrap();
    let mut ops: Vec<Operand> = vec![t.into(), i.into()];
    for &c in &cases {
        ops.push(f.block_call(c, &[]).unwrap().into());
    }
    f.push(table, op::JMPTBL, &ops).unwrap();
    for (k, &c) in cases.iter().enumerate() {
        let mut acc = vals[0];
        let mut first = true;
        for _ in 0..3 {
            for &v in &vals {
                if first {
                    first = false;
                    continue;
                }
                let s = f.vreg(GPR).unwrap();
                f.push(c, op::ADD64, &[s.into(), acc.into(), v.into()])
                    .unwrap();
                acc = s;
            }
        }
        let s = f.vreg(GPR).unwrap();
        f.push(
            c,
            op::ADD64,
            &[s.into(), acc.into(), Operand::Imm(k as i64)],
        )
        .unwrap();
        f.push(c, op::COPY, &[RAX.into(), s.into()]).unwrap();
        f.push_with(c, op::RET, &[], Effects::new().uses(&[RAX]))
            .unwrap();
    }
    let _ = allocate_checked(&mut f).unwrap();
    let code = backend_lang::emit(&f).unwrap();
    for arg in 0..4u64 {
        // The table sits after the code, so the code is readable data too.
        let mut m = common::Machine::new();
        m.regions.push(common::Region {
            base: common::CODE_BASE,
            bytes: code.clone(),
        });
        m.regs[7] = arg;
        let stop = m.run(&code, 0);
        assert_eq!(stop, Ok(common::Stop::Returned));
        let x = arg as i64;
        let want: i64 = (0..13i64).map(|d| x + d).sum::<i64>() * 3 + x;
        assert_eq!(m.regs[0] as i64, want, "case {arg}\n{f}");
    }
}

#[test]
fn test_fixed_registers_and_ties() {
    // q = a / b; r = a % b; shifted = q << (r & 7); all with a and b live
    // after the division, so neither may sit in rax, rdx, or rcx.
    let mut f = Function::new("div", &SYSV);
    f.omit_frame_pointer();
    let b = f.block().unwrap();
    f.add_param(b, RDI.into()).unwrap();
    f.add_param(b, RSI.into()).unwrap();
    let (a, d) = (f.vreg(GPR).unwrap(), f.vreg(GPR).unwrap());
    f.push(b, op::PCOPY, &[a.into(), d.into(), RDI.into(), RSI.into()])
        .unwrap();
    f.push(b, op::COPY, &[RAX.into(), a.into()]).unwrap();
    f.push(b, op::CQO, &[]).unwrap();
    f.push(b, op::IDIV64, &[d.into()]).unwrap();
    let q = f.vreg(GPR).unwrap();
    let r = f.vreg(GPR).unwrap();
    f.push(b, op::PCOPY, &[q.into(), r.into(), RAX.into(), RDX.into()])
        .unwrap();
    let r7 = f.vreg(GPR).unwrap();
    f.push(b, op::AND64, &[r7.into(), r.into(), Operand::Imm(7)])
        .unwrap();
    f.push(b, op::COPY, &[RCX.into(), r7.into()]).unwrap();
    let s = f.vreg(GPR).unwrap();
    f.push(b, op::SHL64CL, &[s.into(), q.into()]).unwrap();
    let t = f.vreg(GPR).unwrap();
    f.push(b, op::ADD64, &[t.into(), s.into(), a.into()])
        .unwrap();
    let u = f.vreg(GPR).unwrap();
    f.push(b, op::ADD64, &[u.into(), t.into(), d.into()])
        .unwrap();
    f.push(b, op::COPY, &[RAX.into(), u.into()]).unwrap();
    f.push_with(b, op::RET, &[], Effects::new().uses(&[RAX]))
        .unwrap();
    let _ = allocate_checked(&mut f).unwrap();
    let code = backend_lang::emit(&f).unwrap();
    for (a0, b0) in [(100i64, 7i64), (-100, 7), (5, -3), (1 << 40, 12345)] {
        let (m, stop) = common::run(&code, 0, &[(7, a0 as u64), (6, b0 as u64)]);
        assert_eq!(stop, Ok(common::Stop::Returned));
        let want = ((a0 / b0) << ((a0 % b0) & 7)) + a0 + b0;
        assert_eq!(m.regs[0] as i64, want, "{a0} {b0}");
    }
}

#[test]
fn test_back_edge_cycles_coalesce_and_resolve() {
    // A loop rotating three values each iteration: block parameters and their
    // arguments form a 3-cycle the edge's parallel copy must resolve.
    let mut f = Function::new("rot", &SYSV);
    f.omit_frame_pointer();
    let (entry, head, exit) = (f.block().unwrap(), f.block().unwrap(), f.block().unwrap());
    let ps: Vec<Reg> = (0..4).map(|_| f.vreg(GPR).unwrap()).collect();
    for &p in &ps {
        f.add_param(head, p.into()).unwrap();
    }
    let res = f.vreg(GPR).unwrap();
    f.add_param(exit, res.into()).unwrap();
    let init = f
        .block_call(
            head,
            &[
                Operand::Imm(1),
                Operand::Imm(2),
                Operand::Imm(3),
                Operand::Imm(0),
            ],
        )
        .unwrap();
    f.push(entry, op::JMP, &[init.into()]).unwrap();
    let i2 = f.vreg(GPR).unwrap();
    f.push(head, op::ADD64, &[i2.into(), ps[3].into(), Operand::Imm(1)])
        .unwrap();
    f.push(head, op::CMP64, &[i2.into(), Operand::Imm(7)])
        .unwrap();
    let again = f
        .block_call(head, &[ps[1].into(), ps[2].into(), ps[0].into(), i2.into()])
        .unwrap();
    let w = f.vreg(GPR).unwrap();
    let done = f.block_call(exit, &[ps[0].into()]).unwrap();
    let _ = w;
    f.push(
        head,
        op::JCC,
        &[x86_64::cond(Cond::L).into(), again.into(), done.into()],
    )
    .unwrap();
    f.push(exit, op::COPY, &[RAX.into(), res.into()]).unwrap();
    f.push_with(exit, op::RET, &[], Effects::new().uses(&[RAX]))
        .unwrap();
    let _ = allocate_checked(&mut f).unwrap();
    let code = backend_lang::emit(&f).unwrap();
    let (m, stop) = common::run(&code, 0, &[]);
    assert_eq!(stop, Ok(common::Stop::Returned));
    // The back edge runs six times: (1, 2, 3) rotated six times is itself.
    assert_eq!(m.regs[0], 1);
}

#[test]
fn test_unreachable_blocks_become_traps_and_dead_code_goes() {
    let mut f = Function::new("f", &SYSV);
    let (a, dead) = (f.block().unwrap(), f.block().unwrap());
    let v = f.vreg(GPR).unwrap();
    f.push(a, op::MOV64RI, &[v.into(), Operand::Imm(5)])
        .unwrap(); // never read
    f.push(a, op::RET, &[]).unwrap();
    let w = f.vreg(GPR).unwrap();
    f.push(dead, op::MOV64RI, &[w.into(), Operand::Imm(1)])
        .unwrap();
    f.push(dead, op::RET, &[]).unwrap();
    let _ = allocate_checked(&mut f).unwrap();
    assert_eq!(f.insts(a).len(), 1, "the dead constant is removed:\n{f}");
    let t = f.insts(dead)[0];
    assert_eq!(f.opcode(t), Some(op::UD2));
}

#[test]
fn test_errors_are_reported_not_panics() {
    use backend_lang::regalloc::RegAllocError;
    // A fixed register live into another block.
    let mut f = Function::new("f", &SYSV);
    let (a, b) = (f.block().unwrap(), f.block().unwrap());
    f.push(a, op::MOV64RI, &[RAX.into(), Operand::Imm(1)])
        .unwrap();
    let to = f.block_call(b, &[]).unwrap();
    f.push(a, op::JMP, &[to.into()]).unwrap();
    f.push(b, op::CMP64, &[RAX.into(), Operand::Imm(0)])
        .unwrap();
    f.push(b, op::RET, &[]).unwrap();
    assert!(matches!(
        regalloc::allocate(&mut f),
        Err(RegAllocError::PhysLiveIn { .. })
    ));
    // A use not dominated by its definition.
    let mut f = Function::new("f", &SYSV);
    let (a, b, c) = (f.block().unwrap(), f.block().unwrap(), f.block().unwrap());
    let v = f.vreg(GPR).unwrap();
    let to_b = f.block_call(b, &[]).unwrap();
    let to_c = f.block_call(c, &[]).unwrap();
    f.push(a, op::CMP64, &[RDI.into(), Operand::Imm(0)])
        .unwrap();
    f.push(
        a,
        op::JCC,
        &[x86_64::cond(Cond::E).into(), to_b.into(), to_c.into()],
    )
    .unwrap();
    f.push(b, op::MOV64RI, &[v.into(), Operand::Imm(1)])
        .unwrap();
    let to_c2 = f.block_call(c, &[]).unwrap();
    f.push(b, op::JMP, &[to_c2.into()]).unwrap();
    f.push(c, op::CMP64, &[v.into(), Operand::Imm(0)]).unwrap();
    f.push(c, op::RET, &[]).unwrap();
    assert!(matches!(
        regalloc::allocate(&mut f),
        Err(RegAllocError::Mir(_))
    ));
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn prop_random_programs_allocate_interference_free(seed in any::<u64>(), stmts in 10u32..140) {
        let opts = GenOptions { statements: stmts, ..GenOptions::default() };
        let prog = irgen::generate(seed, &opts);
        for f in prog.module.functions() {
            let mut mir = backend_lang::isel::select(&prog.module, f, &backend_lang::Options::sysv()).unwrap();
            if let Err(e) = allocate_checked(&mut mir) {
                prop_assert!(false, "seed {seed}: {e}");
            }
        }
    }

    #[test]
    fn prop_many_live_values_sum_correctly(n in 1usize..48, call in any::<bool>()) {
        let mut f = many_live(n, call);
        prop_assert!(allocate_checked(&mut f).is_ok());
    }
}

/// Random MIR: random rows of the instruction table with operands drawn
/// from a few virtual and physical registers, immediates, and a slot;
/// random branches. Most of it is not valid SSA; none of it may panic the
/// allocator or, when it allocates, the emitter.
fn hostile(ops: &[(u16, [u8; 4])], blocks: usize, regs: usize) -> Function {
    use backend_lang::mir::{SlotKind, StackSlot};
    let mut f = Function::new("h", &SYSV);
    let bs: Vec<Block> = (0..blocks).map(|_| f.block().unwrap()).collect();
    let _ = f.stack_slot(SlotKind::Local { size: 8, align: 8 });
    let table = x86_64::TARGET.insts();
    let per = ops.len() / blocks + 1;
    for (bi, chunk) in ops.chunks(per.max(1)).enumerate().take(blocks) {
        let block = bs[bi];
        // Values defined so far in this block: uses draw from them, so the
        // function is often valid SSA and reaches the allocator.
        let mut defined: Vec<Reg> = Vec::new();
        for &(code, pick) in chunk {
            let opcode = backend_lang::target::Opcode::new(code % table.len() as u16);
            let desc = table[opcode.index()];
            if desc.kind().is_terminator() {
                continue;
            }
            let mut operands = Vec::new();
            let mut fresh = Vec::new();
            for (k, rule) in desc.operands().iter().enumerate() {
                let p = pick[k % 4] as usize;
                let o: Operand = match rule {
                    OperandRule::Def(_) => {
                        let v = f.vreg(GPR).unwrap();
                        fresh.push(v);
                        v.into()
                    }
                    OperandRule::Tied(_) | OperandRule::Use(..)
                        if !defined.is_empty() && p % 4 != 0 =>
                    {
                        match rule {
                            OperandRule::Use(_, acc) if acc.mem() && p % 5 == 1 => {
                                Operand::slot(StackSlot::new(0))
                            }
                            _ => defined[p % defined.len()].into(),
                        }
                    }
                    OperandRule::Use(_, acc)
                        if acc.imm() != backend_lang::target::ImmRange::None =>
                    {
                        Operand::Imm((p % 7) as i64)
                    }
                    OperandRule::Cond => {
                        x86_64::cond(Cond::from_code((p % 16) as u8).unwrap()).into()
                    }
                    _ => RCX.into(),
                };
                operands.push(o);
            }
            if desc.kind() == InstKind::Copy || desc.kind() == InstKind::ParallelCopy {
                if let Some(&src) = defined.last() {
                    let d = f.vreg(GPR).unwrap();
                    if f.push(block, opcode, &[d.into(), src.into()]).is_ok() {
                        defined.push(d);
                    }
                }
                continue;
            }
            if f.push(block, opcode, &operands).is_ok() {
                defined.extend(fresh);
            }
        }
        let _ = regs;
        // A terminator: return, or branch to later blocks.
        let next = bs.get(bi + 1).copied();
        let r = match (next, defined.first()) {
            (Some(t), Some(&v)) if bi % 2 == 0 => {
                let _ = f.push(block, op::CMP64, &[v.into(), Operand::Imm(3)]);
                let c1 = f.block_call(t, &[]).unwrap();
                let c2 = f.block_call(bs[blocks - 1], &[]).unwrap();
                f.push(
                    block,
                    op::JCC,
                    &[x86_64::cond(Cond::L).into(), c1.into(), c2.into()],
                )
            }
            (Some(t), _) => {
                let c = f.block_call(t, &[]).unwrap();
                f.push(block, op::JMP, &[c.into()])
            }
            _ => f.push(block, op::RET, &[]),
        };
        let _ = r;
    }
    for &b in &bs {
        if f.insts(b).is_empty() {
            let _ = f.push(b, op::RET, &[]);
        }
    }
    f
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn prop_hostile_mir_never_panics(
        ops in prop::collection::vec((any::<u16>(), any::<[u8; 4]>()), 0..40),
        blocks in 1usize..4,
        regs in 1usize..6,
    ) {
        let mut f = hostile(&ops, blocks, regs);
        if regalloc::allocate(&mut f).is_ok() {
            let mut e = x86_64::Emitter::new();
            let _ = e.function(&f);
        }
    }
}

#[test]
#[ignore = "statistics"]
fn hostile_rate() {
    let mut rng = common::irgen::Rng::new(5);
    let (mut ok, mut emitted) = (0, 0);
    for _ in 0..2000 {
        let n = rng.below(40) as usize;
        let ops: Vec<(u16, [u8; 4])> = (0..n)
            .map(|_| (rng.next() as u16, (rng.next() as u32).to_le_bytes()))
            .collect();
        let mut f = hostile(&ops, 1 + rng.below(3) as usize, 1 + rng.below(5) as usize);
        if regalloc::allocate(&mut f).is_ok() {
            ok += 1;
            let mut e = x86_64::Emitter::new();
            if e.function(&f).is_ok() {
                emitted += 1;
            }
        }
    }
    println!("allocated {ok} emitted {emitted} of 2000");
}
