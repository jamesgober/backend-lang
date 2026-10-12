//! SSA built through ir-lang's variables API (`declare_var`, `def_var`,
//! `use_var`, `seal_block`): the block parameters the builder places for
//! mutable locals (on loop headers left unsealed until their back edge, on
//! joins, forwarded and removed when trivial) and the zero it reads for an
//! unassigned variable. Random programs of assignments, branches, nested
//! loops, `check`ed OPS v2 operations whose error edges assign a variable,
//! and calls are compiled and run in the test interpreter under SysV and
//! Win64 and natively (x86-64 Linux and Windows), against ir-lang's
//! interpreter.

#![allow(clippy::unwrap_used, reason = "test failures should be loud")]

mod common;

use backend_lang::Options;
use common::Stop;
use common::irgen::{self, INTS, Rng};
use common::load::{self, Conv, Hosts};
use ir_lang::interp::{self, Outcome};
use ir_lang::{
    BinaryOp, BlockArg, BlockCall, Builder, Call, CmpOp, ConvOp, FuncId, InstData, Linkage, Module,
    Overflow, Policy, Shift, Signature, Type, Value, Variable,
};

struct Program {
    module: Module,
    entry: FuncId,
    params: Vec<Type>,
}

struct VGen<'m> {
    b: Builder<'m>,
    rng: Rng,
    vars: Vec<(Variable, Type)>,
    helper: FuncId,
    budget: u32,
}

/// The value of `v` (of type `from`) as type `to`, wrapping.
fn cast(b: &mut Builder<'_>, v: Value, from: Type, to: Type) -> Value {
    if from == to {
        return v;
    }
    b.convert(ConvOp::IntCast, Policy::overflow(Overflow::Wrap), v, to)
        .unwrap()
}

impl VGen<'_> {
    fn read(&mut self) -> (Value, Type) {
        let (var, ty) = self.rng.pick(&self.vars);
        (self.b.use_var(var).unwrap(), ty)
    }

    /// A value of `ty`: a variable (cast) or a constant.
    fn operand(&mut self, ty: Type) -> Value {
        if self.rng.chance(70) {
            let (v, t) = self.read();
            cast(&mut self.b, v, t, ty)
        } else {
            let bits = irgen::interesting(&mut self.rng, ty);
            self.b.iconst(ty, irgen::as_i128(ty, bits)).unwrap()
        }
    }

    fn assign(&mut self) {
        let (var, ty) = self.rng.pick(&self.vars);
        let a = self.operand(ty);
        let c = self.operand(ty);
        let op = self.rng.pick(&[
            BinaryOp::Add,
            BinaryOp::Sub,
            BinaryOp::Mul,
            BinaryOp::Xor,
            BinaryOp::And,
            BinaryOp::Or,
            BinaryOp::Shl,
            BinaryOp::Shr,
            BinaryOp::Min,
            BinaryOp::Max,
            BinaryOp::Pow,
        ]);
        let policy = match op {
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Pow => {
                Policy::overflow(Overflow::Wrap)
            }
            BinaryOp::Shl | BinaryOp::Shr => {
                Policy::shift(self.rng.pick(&[Shift::Mask, Shift::Saturate]))
            }
            _ => Policy::NONE,
        };
        if op.can_error(policy, ty) {
            // A signed `pow` or saturating shift: a `check` whose error edge
            // assigns the error code instead.
            let ok = self.b.create_block(&[ty]).unwrap();
            let err = self.b.create_block(&[Type::U32]).unwrap();
            let join = self.b.create_block(&[]).unwrap();
            self.b
                .check(
                    InstData::Binary {
                        op,
                        policy,
                        args: [a, c],
                    },
                    BlockCall::with_args(ok, &[BlockArg::Result(0)]),
                    BlockCall::with_args(err, &[BlockArg::ErrorCode]),
                )
                .unwrap();
            self.b.switch_to(ok).unwrap();
            self.b.seal_block(ok).unwrap();
            let r = self.b.block_param(ok, 0).unwrap();
            self.b.def_var(var, r).unwrap();
            self.b.jump(join, &[]).unwrap();
            self.b.switch_to(err).unwrap();
            self.b.seal_block(err).unwrap();
            let code = self.b.block_param(err, 0).unwrap();
            let code = cast(&mut self.b, code, Type::U32, ty);
            self.b.def_var(var, code).unwrap();
            self.b.jump(join, &[]).unwrap();
            self.b.switch_to(join).unwrap();
            self.b.seal_block(join).unwrap();
        } else {
            let r = self.b.binary(op, policy, a, c).unwrap();
            self.b.def_var(var, r).unwrap();
        }
    }

    fn cond(&mut self) -> Value {
        let ty = self.rng.pick(&INTS);
        let a = self.operand(ty);
        let c = self.operand(ty);
        let op = self.rng.pick(&[CmpOp::Lt, CmpOp::Ge, CmpOp::Eq, CmpOp::Ne]);
        self.b.compare(op, a, c).unwrap()
    }

    fn if_else(&mut self, depth: u32) {
        let c = self.cond();
        let (then, other, join) = (
            self.b.create_block(&[]).unwrap(),
            self.b.create_block(&[]).unwrap(),
            self.b.create_block(&[]).unwrap(),
        );
        self.b.branch(c, then, &[], other, &[]).unwrap();
        for arm in [then, other] {
            self.b.switch_to(arm).unwrap();
            self.b.seal_block(arm).unwrap();
            let n = self.rng.below(4);
            for _ in 0..n {
                self.stmt(depth + 1);
            }
            self.b.jump(join, &[]).unwrap();
        }
        self.b.switch_to(join).unwrap();
        self.b.seal_block(join).unwrap();
    }

    /// `for (k = 0; k < trips; k++) { ... }` with `k` a variable: the
    /// header stays unsealed until the back edge exists.
    fn loop_(&mut self, depth: u32) {
        let k = self.b.declare_var(Type::I32).unwrap();
        let zero = self.b.iconst(Type::I32, 0).unwrap();
        self.b.def_var(k, zero).unwrap();
        let (head, body, exit) = (
            self.b.create_block(&[]).unwrap(),
            self.b.create_block(&[]).unwrap(),
            self.b.create_block(&[]).unwrap(),
        );
        self.b.jump(head, &[]).unwrap();
        self.b.switch_to(head).unwrap();
        let kv = self.b.use_var(k).unwrap();
        let trips = self.b.iconst(Type::I32, self.rng.below(5) as i128).unwrap();
        let more = self.b.compare(CmpOp::Lt, kv, trips).unwrap();
        self.b.branch(more, body, &[], exit, &[]).unwrap();
        self.b.switch_to(body).unwrap();
        self.b.seal_block(body).unwrap();
        let n = 1 + self.rng.below(4);
        for _ in 0..n {
            self.stmt(depth + 1);
        }
        let kv = self.b.use_var(k).unwrap();
        let one = self.b.iconst(Type::I32, 1).unwrap();
        let k2 = self.b.add(kv, one, Overflow::Wrap).unwrap();
        self.b.def_var(k, k2).unwrap();
        self.b.jump(head, &[]).unwrap();
        self.b.seal_block(head).unwrap();
        self.b.switch_to(exit).unwrap();
        self.b.seal_block(exit).unwrap();
    }

    fn call(&mut self) {
        let (var, ty) = self.rng.pick(&self.vars);
        let a = self.operand(Type::I64);
        let c = self.operand(Type::I64);
        let i = self.b.call(Call::direct(self.helper, &[a, c])).unwrap();
        let r = self.b.results(i).first().unwrap();
        let r = cast(&mut self.b, r, Type::I64, ty);
        self.b.def_var(var, r).unwrap();
    }

    fn stmt(&mut self, depth: u32) {
        self.budget = self.budget.saturating_sub(1);
        match self.rng.below(10) {
            0..=4 => self.assign(),
            5 | 6 if depth < 3 => self.if_else(depth),
            7 if depth < 2 => self.loop_(depth),
            8 => self.call(),
            _ => self.assign(),
        }
    }
}

/// A program whose entry is built entirely through the variables API.
fn generate(seed: u64, statements: u32) -> Program {
    let mut rng = Rng::new(seed);
    let mut m = Module::new("vars");
    let helper = m
        .declare_function(
            "h",
            &Signature::new(&[Type::I64, Type::I64], &[Type::I64]),
            Linkage::Export,
        )
        .unwrap();
    {
        let mut b = m.build(helper).unwrap();
        let (x, y) = (b.param(0).unwrap(), b.param(1).unwrap());
        let s = b.mul(x, y, Overflow::Wrap).unwrap();
        let t = b.binary(BinaryOp::Xor, Policy::NONE, s, x).unwrap();
        b.ret(&[t]).unwrap();
    }
    let params: Vec<Type> = (0..rng.below(6)).map(|_| rng.pick(&INTS)).collect();
    let entry = m
        .declare_function(
            "main",
            &Signature::new(&params, &[Type::I64]),
            Linkage::Export,
        )
        .unwrap();
    let mut b = m.build(entry).unwrap();
    let nvars = 2 + rng.below(9) as usize;
    let mut vars = Vec::new();
    for k in 0..nvars {
        let ty = rng.pick(&INTS);
        let var = b.declare_var(ty).unwrap();
        // Some start from a parameter, some from a constant, and some are
        // never assigned before a read (the builder reads zero).
        if k < params.len() && rng.chance(70) {
            let p = b.param(k).unwrap();
            let p = cast(&mut b, p, params[k], ty);
            b.def_var(var, p).unwrap();
        } else if rng.chance(60) {
            let bits = irgen::interesting(&mut rng, ty);
            let c = b.iconst(ty, irgen::as_i128(ty, bits)).unwrap();
            b.def_var(var, c).unwrap();
        }
        vars.push((var, ty));
    }
    let mut g = VGen {
        b,
        rng,
        vars,
        helper,
        budget: statements,
    };
    while g.budget > 0 {
        g.stmt(0);
    }
    // Fold every variable into the result, so each stays live to the end.
    let mut acc = g.b.iconst(Type::I64, 0x5EED).unwrap();
    for k in 0..g.vars.len() {
        let (var, ty) = g.vars[k];
        let v = g.b.use_var(var).unwrap();
        let v = cast(&mut g.b, v, ty, Type::I64);
        let rot = g.b.iconst(Type::I64, (k as i128 * 7) & 63).unwrap();
        let v =
            g.b.binary(BinaryOp::Shl, Policy::shift(Shift::Mask), v, rot)
                .unwrap();
        acc = g.b.add(acc, v, Overflow::Wrap).unwrap();
    }
    g.b.ret(&[acc]).unwrap();
    g.b.seal_all_blocks().unwrap();
    Program {
        module: m,
        entry,
        params,
    }
}

fn want(p: &Program, vals: &[interp::Val]) -> u64 {
    match interp::run(&p.module, p.entry, vals).unwrap() {
        Outcome::Return(v) => v[0].bits(),
        other => panic!("{other:?}\n{}", p.module),
    }
}

#[test]
fn test_variables_api_programs_match_the_interpreter() {
    let mut params_seen = 0usize;
    for seed in 0..count(200) {
        let p = generate(seed, 30);
        p.module
            .validate()
            .unwrap_or_else(|e| panic!("seed {seed}: {e}\n{}", p.module));
        // Block parameters the builder placed (outside the entry).
        let f = p.module.function(p.entry).unwrap();
        params_seen += f
            .blocks()
            .filter(|&b| b != f.entry())
            .map(|b| f.block_params(b).len())
            .sum::<usize>();
        let mut rng = Rng::new(seed ^ 0x77);
        let (vals, regs) = irgen::args(&mut rng, &p.params);
        let expected = want(&p, &vals);
        for conv in [Conv::SysV, Conv::Win64] {
            let opts = match conv {
                Conv::SysV => Options::sysv(),
                Conv::Win64 => Options::win64(),
            }
            .with_frame_pointer(seed % 2 == 0);
            let c = backend_lang::compile(&p.module, &opts)
                .unwrap_or_else(|e| panic!("seed {seed}: {e}\n{}", p.module));
            let img = load::load(&c).unwrap();
            let mut m = load::machine(&img);
            let mut hosts = Hosts::new(&img, conv);
            let stop = load::call(&img, &mut m, conv, "main", &regs, &mut hosts, seed);
            assert_eq!(
                stop,
                Ok(Stop::Returned),
                "seed {seed} {conv:?}\n{}",
                p.module
            );
            assert_eq!(m.regs[0], expected, "seed {seed} {conv:?}\n{}", p.module);
        }
        native(&p, &regs, expected, seed);
    }
    assert!(params_seen > 500, "{params_seen} block parameters placed");
}

#[test]
fn test_unassigned_variables_read_zero_and_loops_carry_parameters() {
    // A fixed shape: a variable read before any assignment (zero), a loop
    // whose header gets a parameter for each variable it changes, and a
    // variable assigned in only one arm of a branch.
    let mut m = Module::new("fixed");
    let f = m
        .declare_function(
            "main",
            &Signature::new(&[Type::I64], &[Type::I64]),
            Linkage::Export,
        )
        .unwrap();
    let mut b = m.build(f).unwrap();
    let n = b.param(0).unwrap();
    let (i, acc, unset) = (
        b.declare_var(Type::I64).unwrap(),
        b.declare_var(Type::I64).unwrap(),
        b.declare_var(Type::U16).unwrap(),
    );
    let zero = b.iconst(Type::I64, 0).unwrap();
    b.def_var(i, zero).unwrap();
    b.def_var(acc, zero).unwrap();
    let (head, body, odd, even, join, exit) = (
        b.create_block(&[]).unwrap(),
        b.create_block(&[]).unwrap(),
        b.create_block(&[]).unwrap(),
        b.create_block(&[]).unwrap(),
        b.create_block(&[]).unwrap(),
        b.create_block(&[]).unwrap(),
    );
    b.jump(head, &[]).unwrap();
    b.switch_to(head).unwrap();
    let iv = b.use_var(i).unwrap();
    let more = b.compare(CmpOp::Lt, iv, n).unwrap();
    b.branch(more, body, &[], exit, &[]).unwrap();
    b.switch_to(body).unwrap();
    b.seal_block(body).unwrap();
    let iv = b.use_var(i).unwrap();
    let one = b.iconst(Type::I64, 1).unwrap();
    let bit = b.binary(BinaryOp::And, Policy::NONE, iv, one).unwrap();
    let is_odd = b.compare(CmpOp::Ne, bit, zero).unwrap();
    b.branch(is_odd, odd, &[], even, &[]).unwrap();
    b.switch_to(odd).unwrap();
    b.seal_block(odd).unwrap();
    let a = b.use_var(acc).unwrap();
    let three = b.iconst(Type::I64, 3).unwrap();
    // A signed pow can fail (a negative exponent), so it must be a check;
    // the builder refuses it as a plain instruction. Cube unsigned instead.
    let signed = b.binary(BinaryOp::Pow, Policy::overflow(Overflow::Wrap), iv, three);
    assert!(signed.is_err());
    let u = b
        .convert(ConvOp::Bitcast, Policy::NONE, iv, Type::U64)
        .unwrap();
    let three_u = b.iconst(Type::U64, 3).unwrap();
    let cube = b
        .binary(BinaryOp::Pow, Policy::overflow(Overflow::Wrap), u, three_u)
        .unwrap();
    let cube = b
        .convert(ConvOp::Bitcast, Policy::NONE, cube, Type::I64)
        .unwrap();
    let a2 = b.add(a, cube, Overflow::Wrap).unwrap();
    b.def_var(acc, a2).unwrap();
    b.jump(join, &[]).unwrap();
    b.switch_to(even).unwrap();
    b.seal_block(even).unwrap();
    b.jump(join, &[]).unwrap();
    b.switch_to(join).unwrap();
    b.seal_block(join).unwrap();
    let iv = b.use_var(i).unwrap();
    let i2 = b.add(iv, one, Overflow::Wrap).unwrap();
    b.def_var(i, i2).unwrap();
    b.jump(head, &[]).unwrap();
    b.seal_block(head).unwrap();
    b.switch_to(exit).unwrap();
    b.seal_block(exit).unwrap();
    let a = b.use_var(acc).unwrap();
    let z = b.use_var(unset).unwrap();
    let z = b.convert(ConvOp::Zext, Policy::NONE, z, Type::I64).unwrap();
    let r = b.add(a, z, Overflow::Wrap).unwrap();
    b.ret(&[r]).unwrap();
    b.seal_all_blocks().unwrap();
    // The header carries i and acc as parameters.
    assert_eq!(b.func().block_params(head).len(), 2);
    drop(b);
    m.validate().unwrap();
    let p = Program {
        module: m,
        entry: f,
        params: vec![Type::I64],
    };
    for x in [0u64, 1, 2, 5, 10] {
        let expected = want(&p, &[interp::Val::i64(x as i64)]);
        let sum: u64 = (0..x).filter(|k| k % 2 == 1).map(|k| k * k * k).sum();
        assert_eq!(expected, sum);
        for conv in [Conv::SysV, Conv::Win64] {
            let opts = match conv {
                Conv::SysV => Options::sysv(),
                Conv::Win64 => Options::win64(),
            };
            let c = backend_lang::compile(&p.module, &opts).unwrap();
            let img = load::load(&c).unwrap();
            let mut m = load::machine(&img);
            let mut hosts = Hosts::new(&img, conv);
            let stop = load::call(&img, &mut m, conv, "main", &[x], &mut hosts, x);
            assert_eq!(stop, Ok(Stop::Returned));
            assert_eq!(m.regs[0], expected, "x {x} {conv:?}");
        }
        native(&p, &[x], expected, x);
    }
}

fn count(default: u64) -> u64 {
    std::env::var("BACKEND_DIFF_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[cfg(all(target_arch = "x86_64", any(target_os = "linux", windows)))]
fn native(p: &Program, regs: &[u64], expected: u64, seed: u64) {
    use common::native::{Native, Ran};
    let conv = if cfg!(windows) {
        Conv::Win64
    } else {
        Conv::SysV
    };
    let c = backend_lang::compile(&p.module, &Options::host()).unwrap();
    let n = Native::load(&c, conv).unwrap();
    match n.call("main", regs).unwrap() {
        Ran::Returned(r, _) => assert_eq!(r, expected, "native seed {seed}\n{}", p.module),
        other => panic!("native seed {seed}: {other:?}"),
    }
}

#[cfg(not(all(target_arch = "x86_64", any(target_os = "linux", windows))))]
fn native(_: &Program, _: &[u64], _: u64, _: u64) {}
