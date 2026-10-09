//! Benchmarks at realistic scale: building and emitting a 100,000-instruction
//! MIR function, and resolving parallel copies from small edges up to
//! 100,000 moves.

use backend_lang::PReg;
use backend_lang::mir::{Function, Operand, SlotKind, StackSlot};
use backend_lang::moves::{Loc, Move, MoveEnv, MoveResolver};
use backend_lang::x86_64::{self, GPR, RAX, RCX, RDX, SYSV, TARGET, op};
use criterion::{
    BatchSize, BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main,
};
use isa_lang::x86_64::Cond;

const INSTS: usize = 100_000;
/// Instructions per block; each block ends in a branch whose edges carry a
/// parallel copy (a rotation through three registers and a slot).
const PER_BLOCK: usize = 50;

fn regs() -> Vec<PReg> {
    TARGET.class(GPR).allocatable().iter().collect()
}

/// A 100,000-instruction function on physical registers: blocks of ALU
/// work, loads and stores, each ending in a conditional branch whose edges
/// rotate values through registers and a stack slot.
fn build() -> Function {
    let r = regs();
    let mut f = Function::with_capacity("big", &SYSV, INSTS + INSTS / PER_BLOCK, 3 * INSTS);
    let slot = f
        .stack_slot(SlotKind::Local { size: 8, align: 8 })
        .unwrap_or(StackSlot::new(0));
    let blocks = INSTS / PER_BLOCK;
    let ids: Vec<_> = (0..=blocks).filter_map(|_| f.block().ok()).collect();
    for &b in &ids[1..] {
        for p in [
            Operand::from(RAX),
            RCX.into(),
            RDX.into(),
            Operand::slot(slot),
        ] {
            let _ = f.add_param(b, p);
        }
    }
    for (k, &b) in ids[..blocks].iter().enumerate() {
        for i in 0..PER_BLOCK - 2 {
            let d = r[(i + k) % r.len()];
            let s = r[(i * 7 + 3) % r.len()];
            let _ = match i % 6 {
                0 => f.push(b, op::MOV64RI, &[d.into(), Operand::Imm(i as i64)]),
                1 => f.push(b, op::ADD64, &[d.into(), d.into(), s.into()]),
                2 => f.push(b, op::IMUL64RI, &[d.into(), s.into(), Operand::Imm(3)]),
                3 => f.push(b, op::STORE64, &[Operand::slot(slot), s.into()]),
                4 => f.push(b, op::MOV64, &[d.into(), Operand::slot(slot)]),
                _ => f.push(b, op::SHL64, &[d.into(), d.into(), Operand::Imm(1)]),
            };
        }
        let _ = f.push(b, op::CMP64, &[RAX.into(), Operand::Imm(0)]);
        let next = ids[k + 1];
        let rotate = [
            Operand::from(RCX),
            RDX.into(),
            Operand::slot(slot),
            RAX.into(),
        ];
        let other = [Operand::from(RAX), RCX.into(), RDX.into(), Operand::Imm(7)];
        let (Ok(a), Ok(c)) = (f.block_call(next, &rotate), f.block_call(next, &other)) else {
            continue;
        };
        let _ = f.push(
            b,
            op::JCC,
            &[x86_64::cond(Cond::E).into(), a.into(), c.into()],
        );
    }
    let _ = f.push(ids[blocks], op::RET, &[]);
    f
}

fn mir(c: &mut Criterion) {
    let mut g = c.benchmark_group("mir_100k");
    g.throughput(Throughput::Elements(INSTS as u64));
    g.sample_size(20);
    g.bench_function("build", |b| b.iter(|| black_box(build())));
    let f = build();
    if let Err(e) = f.verify() {
        panic!("benchmark function does not verify: {e}");
    }
    g.bench_function("verify", |b| b.iter(|| black_box(f.verify())));
    g.bench_function("emit", |b| b.iter(|| black_box(backend_lang::emit(&f))));
    g.bench_function("print", |b| b.iter(|| black_box(f.to_string())));
    g.finish();
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// A random permutation of `n` locations (the registers first, then
/// slots): every value moves, mostly through memory, in long cycles.
fn permutation(n: usize, seed: u64) -> Vec<Move> {
    let mut rng = Rng(seed | 1);
    let mut locs: Vec<Loc> = regs().into_iter().map(Loc::Reg).collect();
    locs.extend((0..n as u32).map(|s| Loc::Slot(StackSlot::new(s))));
    locs.truncate(n);
    let mut perm: Vec<usize> = (0..locs.len()).collect();
    for i in (1..perm.len()).rev() {
        perm.swap(i, (rng.next() % (i as u64 + 1)) as usize);
    }
    (0..locs.len())
        .map(|i| Move::new(locs[i], locs[perm[i]], GPR))
        .collect()
}

fn moves(c: &mut Criterion) {
    let env = MoveEnv::new()
        .with_scratch_slot(StackSlot::new(1 << 30))
        .with_victim_slot(StackSlot::new((1 << 30) + 1));
    let mut g = c.benchmark_group("moves");
    for n in [4usize, 14, 1_000, 10_000, 100_000] {
        let set = permutation(n, 0x9E37_79B9_7F4A_7C15 ^ n as u64);
        g.throughput(Throughput::Elements(n as u64));
        let mut resolver = MoveResolver::new();
        let mut out = Vec::with_capacity(4 * n);
        g.bench_with_input(BenchmarkId::new("permutation", n), &set, |b, set| {
            b.iter(|| {
                out.clear();
                black_box(resolver.resolve(&TARGET, set, &env, &mut out))
            })
        });
    }
    // 100,000 small edges of four moves each with one resolver reused: the
    // pattern of the emitter and of the register allocator.
    let edges: Vec<Vec<Move>> = (0..100_000u64).map(|i| permutation(4, i + 1)).collect();
    g.throughput(Throughput::Elements(100_000));
    g.bench_function("edges_100k_x4", |b| {
        b.iter_batched_ref(
            || (MoveResolver::new(), Vec::with_capacity(16)),
            |(resolver, out)| {
                for e in &edges {
                    out.clear();
                    let _ = black_box(resolver.resolve(&TARGET, e, &env, out));
                }
            },
            BatchSize::LargeInput,
        )
    });
    g.finish();
}

criterion_group!(benches, mir, moves);
criterion_main!(benches);
