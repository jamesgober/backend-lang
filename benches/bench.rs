//! Benchmarks at realistic scale: building and emitting a 100,000-instruction
//! MIR function; resolving parallel copies from small edges up to 100,000
//! moves; compiling 100,000-instruction ir-lang functions (the whole
//! pipeline and each phase); and the code it produces against ir-lang's
//! interpreter on a recursive Fibonacci, a counting loop, and a memory
//! kernel (natively, on x86-64 Linux and Windows).

#[path = "../tests/common/mod.rs"]
#[allow(unused_imports, reason = "the shared test support")]
mod common;

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

/// The whole pipeline and each phase on a 100,000-instruction function of
/// realistic shape, and on a high-pressure generated one.
fn compile(c: &mut Criterion) {
    use backend_lang::Options;
    let (m, f) = common::irgen::big_function(INSTS);
    let n = m.function(f).map_or(0, |b| b.inst_count()) as u64;
    let opts = Options::sysv();
    let mut g = c.benchmark_group("compile_100k");
    g.throughput(Throughput::Elements(n));
    g.sample_size(20);
    g.bench_function("compile", |b| {
        b.iter(|| black_box(backend_lang::compile(&m, &opts)))
    });
    g.bench_function("isel", |b| {
        b.iter(|| black_box(backend_lang::isel::select(&m, f, &opts)))
    });
    let Ok(selected) = backend_lang::isel::select(&m, f, &opts) else {
        panic!("selection failed");
    };
    g.bench_function("regalloc", |b| {
        b.iter_batched(
            || selected.clone(),
            |mut mir| black_box(backend_lang::regalloc::allocate(&mut mir).map(|a| a.rounds())),
            BatchSize::LargeInput,
        )
    });
    let Ok(allocated) = backend_lang::compile_function(&m, f, &opts) else {
        panic!("compilation failed");
    };
    g.bench_function("emit", |b| {
        b.iter(|| {
            let mut e = x86_64::Emitter::new();
            let _ = e.declare_external("helper");
            let _ = e.function(&allocated);
            black_box(e.finish())
        })
    });
    g.finish();

    // A generated function under heavy register pressure (dozens of values
    // live across loops and branches; the allocator spills and reruns).
    let opts_gen = common::irgen::GenOptions {
        functions: 1,
        statements: 20_000,
        traps: false,
        calls: false,
        ..common::irgen::GenOptions::default()
    };
    let p = common::irgen::generate(1, &opts_gen);
    let n = p.module.function(p.entry).map_or(0, |b| b.inst_count()) as u64;
    let mut g = c.benchmark_group("compile_pressure");
    g.throughput(Throughput::Elements(n));
    g.sample_size(10);
    g.bench_function("compile", |b| {
        b.iter(|| black_box(backend_lang::compile(&p.module, &opts)))
    });
    g.finish();
}

#[cfg(all(target_arch = "x86_64", any(target_os = "linux", windows)))]
mod kernels {
    use ir_lang::{
        BinaryOp, BlockCall, Builder, Call, CmpOp, Linkage, MemFlags, MemType, Module, Overflow,
        Policy, Signature, Type, Value,
    };

    fn ok<T, E: std::fmt::Display>(r: Result<T, E>) -> T {
        r.unwrap_or_else(|e| panic!("{e}"))
    }

    fn loop_head(b: &mut Builder<'_>, types: &[Type]) -> ir_lang::Block {
        ok(b.create_block(types))
    }

    /// `fib(n)`, recursively.
    pub fn fib() -> Module {
        let mut m = Module::new("fib");
        let f = ok(m.declare_function(
            "fib",
            &Signature::new(&[Type::I64], &[Type::I64]),
            Linkage::Export,
        ));
        let mut b = ok(m.build(f));
        let n = ok(b.param(0));
        let two = ok(b.iconst(Type::I64, 2));
        let small = ok(b.compare(CmpOp::Lt, n, two));
        let base = loop_head(&mut b, &[]);
        let rec = loop_head(&mut b, &[]);
        ok(b.branch(small, base, &[], rec, &[]));
        ok(b.switch_to(base));
        ok(b.ret(&[n]));
        ok(b.switch_to(rec));
        let one = ok(b.iconst(Type::I64, 1));
        let n1 = ok(b.sub(n, one, Overflow::Wrap));
        let n2 = ok(b.sub(n, two, Overflow::Wrap));
        let c1 = ok(b.call(Call::direct(f, &[n1])));
        let r1 = b.results(c1).first().unwrap_or_else(|| panic!("no result"));
        let c2 = ok(b.call(Call::direct(f, &[n2])));
        let r2 = b.results(c2).first().unwrap_or_else(|| panic!("no result"));
        let s = ok(b.add(r1, r2, Overflow::Wrap));
        ok(b.ret(&[s]));
        m
    }

    /// The sum over `i < n` of `(i * i) ^ i`.
    pub fn counting_loop() -> Module {
        let mut m = Module::new("loop");
        let f = ok(m.declare_function(
            "run",
            &Signature::new(&[Type::I64], &[Type::I64]),
            Linkage::Export,
        ));
        let mut b = ok(m.build(f));
        let n = ok(b.param(0));
        let head = loop_head(&mut b, &[Type::I64, Type::I64]);
        let body = loop_head(&mut b, &[]);
        let exit = loop_head(&mut b, &[Type::I64]);
        let z = ok(b.iconst(Type::I64, 0));
        ok(b.jump(head, &[z, z]));
        ok(b.switch_to(head));
        let i = ok(b.block_param(head, 0));
        let acc = ok(b.block_param(head, 1));
        let more = ok(b.compare(CmpOp::Lt, i, n));
        ok(b.branch(more, body, &[], exit, &[acc]));
        ok(b.switch_to(body));
        let sq = ok(b.mul(i, i, Overflow::Wrap));
        let x = ok(b.binary(BinaryOp::Xor, Policy::NONE, sq, i));
        let acc2 = ok(b.add(acc, x, Overflow::Wrap));
        let one = ok(b.iconst(Type::I64, 1));
        let i2 = ok(b.add(i, one, Overflow::Wrap));
        ok(b.jump(head, &[i2, acc2]));
        ok(b.switch_to(exit));
        let r = ok(b.block_param(exit, 0));
        ok(b.ret(&[r]));
        m
    }

    /// A 1024-word array on the stack: filled, `passes` prefix-sum passes,
    /// copied with `memcpy`; returns the copy's last word.
    pub fn memory_kernel() -> Module {
        let mut m = Module::new("mem");
        let f = ok(m.declare_function(
            "run",
            &Signature::new(&[Type::I64], &[Type::I64]),
            Linkage::Export,
        ));
        let mut b = ok(m.build(f));
        let passes = ok(b.param(0));
        let a = ok(b.stack_slot(8192, 8));
        let c2 = ok(b.stack_slot(8192, 8));
        let pa = ok(b.stack_addr(a));
        let pc = ok(b.stack_addr(c2));
        let words = ok(b.iconst(Type::I64, 1024));
        let one = ok(b.iconst(Type::I64, 1));
        let z = ok(b.iconst(Type::I64, 0));
        let fill_head = loop_head(&mut b, &[Type::I64]);
        let fill_body = loop_head(&mut b, &[]);
        let pass_head = loop_head(&mut b, &[Type::I64]);
        ok(b.jump(fill_head, &[z]));
        ok(b.switch_to(fill_head));
        let i = ok(b.block_param(fill_head, 0));
        let more = ok(b.compare(CmpOp::Lt, i, words));
        ok(b.branch(more, fill_body, &[], pass_head, &[z]));
        ok(b.switch_to(fill_body));
        let three = ok(b.iconst(Type::I64, 3));
        let v = ok(b.mul(i, three, Overflow::Wrap));
        let at = ok(b.elem_addr(pa, MemType::Scalar(Type::I64), i));
        let _ = ok(b.store(v, at, MemFlags::new(8)));
        let i2 = ok(b.add(i, one, Overflow::Wrap));
        ok(b.jump(fill_head, &[i2]));
        ok(b.switch_to(pass_head));
        let p = ok(b.block_param(pass_head, 0));
        let pass_more = ok(b.compare(CmpOp::Lt, p, passes));
        let inner = loop_head(&mut b, &[Type::I64]);
        let inner_body = loop_head(&mut b, &[]);
        let done = loop_head(&mut b, &[]);
        ok(b.branch(pass_more, inner, &[one], done, &[]));
        ok(b.switch_to(inner));
        let j = ok(b.block_param(inner, 0));
        let jm = ok(b.compare(CmpOp::Lt, j, words));
        let p2 = ok(b.add(p, one, Overflow::Wrap));
        ok(b.branch(jm, inner_body, &[], pass_head, &[p2]));
        ok(b.switch_to(inner_body));
        let jp = ok(b.sub(j, one, Overflow::Wrap));
        let at_p = ok(b.elem_addr(pa, MemType::Scalar(Type::I64), jp));
        let at_j = ok(b.elem_addr(pa, MemType::Scalar(Type::I64), j));
        let x = ok(b.load(Type::I64, at_p, MemFlags::new(8)));
        let y = ok(b.load(Type::I64, at_j, MemFlags::new(8)));
        let s = ok(b.add(x, y, Overflow::Wrap));
        let _ = ok(b.store(s, at_j, MemFlags::new(8)));
        let j2 = ok(b.add(j, one, Overflow::Wrap));
        ok(b.jump(inner, &[j2]));
        ok(b.switch_to(done));
        let bytes = ok(b.iconst(Type::U64, 8192));
        let _ = ok(b.memcpy(pc, pa, bytes, MemFlags::new(8)));
        let last = ok(b.iconst(Type::I64, 1023));
        let at = ok(b.elem_addr(pc, MemType::Scalar(Type::I64), last));
        let r = ok(b.load(Type::I64, at, MemFlags::new(8)));
        ok(b.ret(&[r]));
        m
    }

    #[allow(dead_code, reason = "keeps the imported names in use")]
    fn _anchor(v: Value, c: BlockCall) -> (Value, BlockCall) {
        (v, c)
    }
}

/// The compiled code against ir-lang's interpreter, natively.
#[cfg(all(target_arch = "x86_64", any(target_os = "linux", windows)))]
fn quality(c: &mut Criterion) {
    use backend_lang::Options;
    use common::load::Conv;
    use common::native::{Native, Ran};
    use ir_lang::interp::{Interpreter, Limits, NoHost, Outcome, Val};

    let conv = if cfg!(windows) {
        Conv::Win64
    } else {
        Conv::SysV
    };
    let limits = Limits {
        fuel: u64::MAX,
        max_depth: 100_000,
        memory: 16 << 20,
    };
    let mut g = c.benchmark_group("quality");
    g.sample_size(10);
    for (name, m, entry, arg) in [
        ("fib_25", kernels::fib(), "fib", 25u64),
        ("loop_1m", kernels::counting_loop(), "run", 1_000_000),
        ("memory_1024x20", kernels::memory_kernel(), "run", 20),
    ] {
        let compiled =
            backend_lang::compile(&m, &Options::host()).unwrap_or_else(|e| panic!("{e}"));
        let native = Native::load(&compiled, conv).unwrap_or_else(|e| panic!("{e}"));
        let f = m
            .functions()
            .next()
            .unwrap_or_else(|| panic!("no function"));
        // Both sides compute the same answer.
        let want = Interpreter::with_limits(&m, NoHost, limits)
            .and_then(|mut vm| vm.call(f, &[Val::i64(arg as i64)]))
            .unwrap_or_else(|e| panic!("{e}"));
        match (&want, native.call(entry, &[arg])) {
            (Outcome::Return(v), Ok(Ran::Returned(a, _))) if v[0].bits() == a => {}
            other => panic!("{name}: {other:?}"),
        }
        g.bench_function(BenchmarkId::new("native", name), |b| {
            b.iter(|| black_box(native.call(entry, &[black_box(arg)])))
        });
        g.bench_function(BenchmarkId::new("interp", name), |b| {
            b.iter(|| {
                Interpreter::with_limits(&m, NoHost, limits)
                    .and_then(|mut vm| vm.call(f, &[Val::i64(black_box(arg) as i64)]))
            })
        });
    }
    g.finish();
}

#[cfg(not(all(target_arch = "x86_64", any(target_os = "linux", windows))))]
fn quality(_: &mut Criterion) {}

criterion_group!(benches, mir, moves, compile, quality);
criterion_main!(benches);
