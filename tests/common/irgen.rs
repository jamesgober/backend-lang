//! Random ir-lang 2.0 programs for differential testing: valid, terminating,
//! free of undefined behaviour, and dense in the things a backend gets
//! wrong: every integer operation under every policy, checked operations
//! with error edges, branches, loops whose back edges permute their
//! parameters, switches (dense and sparse), stack slots and globals through
//! field and element addresses, block memory operations, direct and
//! indirect calls, tail calls, and many values live at once.
//!
//! ir-lang's own generators are internal to its test suite, so this is the
//! backend's own.

use ir_lang::{
    AggId, Aggregate, BinaryOp, BlockArg, BlockCall, Builder, Call, CallConv, CmpOp, ConvOp,
    DivZero, FuncId, Global, GlobalId, InstData, Linkage, MemFlags, MemType, Module, Overflow,
    Policy, Shift, SigId, Signature, SwitchCase, Type, UnaryOp, Value,
};

/// xorshift64*.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }

    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    pub fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize]
    }
}

pub const INTS: [Type; 8] = [
    Type::I8,
    Type::I16,
    Type::I32,
    Type::I64,
    Type::U8,
    Type::U16,
    Type::U32,
    Type::U64,
];

/// What a generated module may contain.
#[derive(Clone, Copy, Debug)]
pub struct GenOptions {
    pub functions: usize,
    /// Statements per function body (approximate).
    pub statements: u32,
    /// Plain operations may use `trap` policies.
    pub traps: bool,
    pub calls: bool,
    pub memory: bool,
    pub tail_calls: bool,
    /// Results per function (1 or 2).
    pub returns: usize,
    pub conv: CallConv,
}

impl Default for GenOptions {
    fn default() -> GenOptions {
        GenOptions {
            functions: 3,
            statements: 40,
            traps: true,
            calls: true,
            memory: true,
            tail_calls: true,
            returns: 1,
            conv: CallConv::Fast,
        }
    }
}

/// A value of `ty` that is often an edge case.
pub fn interesting(rng: &mut Rng, ty: Type) -> u64 {
    let w = ty.int_bits().unwrap_or(8);
    let mask = if w == 64 { u64::MAX } else { (1u64 << w) - 1 };
    if ty == Type::Bool {
        return rng.below(2);
    }
    let min = if ty.is_signed_int() {
        1u64 << (w - 1)
    } else {
        0
    };
    let max = if ty.is_signed_int() {
        (1u64 << (w - 1)) - 1
    } else {
        mask
    };
    let v = match rng.below(12) {
        0 => 0,
        1 => 1,
        2 => mask, // -1 or MAX
        3 => min,
        4 => max,
        5 => min.wrapping_add(1),
        6 => max.wrapping_sub(1),
        7 => rng.below(8),
        8 => u64::from(w) + rng.below(3), // shift amounts just past the width
        9 => rng.below(40),
        _ => rng.next(),
    };
    v & mask
}

/// The value with `bits` as an `i128` in `ty`'s range (for `iconst`).
pub fn as_i128(ty: Type, bits: u64) -> i128 {
    let w = ty.int_bits().unwrap_or(64);
    if ty.is_signed_int() {
        let s = 64 - w;
        i128::from(((bits << s) as i64) >> s)
    } else {
        i128::from(bits)
    }
}

/// A generated module and the function to run.
pub struct Program {
    pub module: Module,
    pub entry: FuncId,
    pub params: Vec<Type>,
    pub returns: Vec<Type>,
}

struct FnInfo {
    id: FuncId,
    sig: Signature,
    sig_id: SigId,
}

/// Generates a program from `seed`.
pub fn generate(seed: u64, opts: &GenOptions) -> Program {
    let mut rng = Rng::new(seed);
    let mut m = Module::new("gen");
    let rec = m
        .declare_aggregate(Aggregate::structure(&[
            MemType::Scalar(Type::I8),
            MemType::Scalar(Type::I16),
            MemType::Scalar(Type::U32),
            MemType::Scalar(Type::I64),
            MemType::Scalar(Type::U8),
        ]))
        .unwrap();
    let global = m
        .declare_global("g", Global::zeroed(64, 8).mutable())
        .unwrap();
    let mut returns = vec![Type::I64];
    if opts.returns > 1 {
        returns.push(rng.pick(&INTS));
    }
    let mut fns: Vec<FnInfo> = Vec::new();
    for i in 0..opts.functions.max(1) {
        let nparams = rng.below(8) as usize;
        let params: Vec<Type> = (0..nparams)
            .map(|_| {
                if rng.chance(10) {
                    Type::Bool
                } else {
                    rng.pick(&INTS)
                }
            })
            .collect();
        let sig = Signature::new(&params, &returns).with_conv(opts.conv);
        let id = m
            .declare_function(&format!("f{i}"), &sig, Linkage::Export)
            .unwrap();
        let sig_id = m.intern_signature(&sig).unwrap();
        fns.push(FnInfo { id, sig, sig_id });
    }
    for i in 0..fns.len() {
        let callees: Vec<(FuncId, Signature, SigId)> = fns[..i]
            .iter()
            .map(|f| (f.id, f.sig.clone(), f.sig_id))
            .collect();
        let sig = fns[i].sig.clone();
        let b = m.build(fns[i].id).unwrap();
        let mut g = G {
            b,
            rng: rng.clone(),
            pool: Vec::new(),
            callees,
            sig,
            opts: *opts,
            rec,
            global,
            slots: None,
            budget: opts.statements,
        };
        g.body();
        rng = g.rng;
    }
    let last = fns.last().unwrap();
    Program {
        entry: last.id,
        params: last.sig.params.clone(),
        returns,
        module: m,
    }
}

struct G<'m> {
    b: Builder<'m>,
    rng: Rng,
    pool: Vec<(Value, Type)>,
    callees: Vec<(FuncId, Signature, SigId)>,
    sig: Signature,
    opts: GenOptions,
    rec: AggId,
    global: GlobalId,
    /// Two 64-byte array slots and one record slot, zeroed at entry.
    slots: Option<[Value; 3]>,
    budget: u32,
}

const ALIGN8: MemFlags = MemFlags::new(8);

impl G<'_> {
    fn body(&mut self) {
        for i in 0..self.sig.params.len() {
            let v = self.b.param(i).unwrap();
            self.pool.push((v, self.sig.params[i]));
        }
        if self.opts.memory {
            let a = self.b.stack_slot(64, 8).unwrap();
            let c = self.b.stack_slot(64, 16).unwrap();
            let r = self.b.stack_slot_for(MemType::Agg(self.rec)).unwrap();
            let pa = self.b.stack_addr(a).unwrap();
            let pc = self.b.stack_addr(c).unwrap();
            let pr = self.b.stack_addr(r).unwrap();
            // Slots start unspecified: zero them so every load is defined.
            let zero = self.b.iconst(Type::U8, 0).unwrap();
            let n64 = self.b.iconst(Type::U64, 64).unwrap();
            let n24 = self.b.iconst(Type::U64, 24).unwrap();
            let _ = self.b.memset(pa, zero, n64, ALIGN8).unwrap();
            let _ = self.b.memset(pc, zero, n64, ALIGN8).unwrap();
            let _ = self.b.memset(pr, zero, n24, ALIGN8).unwrap();
            self.slots = Some([pa, pc, pr]);
        }
        while self.budget > 0 {
            self.stmt(0);
        }
        self.finish();
    }

    fn konst(&mut self, ty: Type) -> Value {
        if ty == Type::Bool {
            let v = self.rng.chance(50);
            return self.b.bconst(v).unwrap();
        }
        let bits = interesting(&mut self.rng, ty);
        self.b.iconst(ty, as_i128(ty, bits)).unwrap()
    }

    /// A value of `ty`, from the pool when one exists (converting another
    /// value sometimes), else a constant.
    fn value(&mut self, ty: Type) -> Value {
        let same: Vec<Value> = self
            .pool
            .iter()
            .filter(|(_, t)| *t == ty)
            .map(|(v, _)| *v)
            .collect();
        if !same.is_empty() && self.rng.chance(80) {
            return self.rng.pick(&same);
        }
        if !self.pool.is_empty() && self.rng.chance(60) {
            let (v, t) = self.rng.pick(&self.pool);
            return self.coerce(v, t, ty);
        }
        self.konst(ty)
    }

    fn coerce(&mut self, v: Value, from: Type, to: Type) -> Value {
        if from == to {
            return v;
        }
        match (from, to) {
            (Type::Bool, _) => self
                .b
                .convert(ConvOp::BoolToInt, Policy::NONE, v, to)
                .unwrap(),
            (_, Type::Bool) => {
                let z = self.b.iconst(from, 0).unwrap();
                self.b.compare(CmpOp::Ne, v, z).unwrap()
            }
            _ => {
                let (fw, tw) = (from.int_bits().unwrap(), to.int_bits().unwrap());
                let op = if fw == tw {
                    ConvOp::Bitcast
                } else if tw < fw {
                    ConvOp::Narrow
                } else if self.rng.chance(50) {
                    if from.is_signed_int() {
                        ConvOp::Sext
                    } else {
                        ConvOp::Zext
                    }
                } else {
                    return self
                        .b
                        .convert(ConvOp::IntCast, Policy::overflow(Overflow::Wrap), v, to)
                        .unwrap();
                };
                self.b.convert(op, Policy::NONE, v, to).unwrap()
            }
        }
    }

    fn int_value(&mut self) -> (Value, Type) {
        let ints: Vec<(Value, Type)> = self
            .pool
            .iter()
            .copied()
            .filter(|(_, t)| t.is_int())
            .collect();
        if !ints.is_empty() && self.rng.chance(85) {
            return self.rng.pick(&ints);
        }
        let ty = self.rng.pick(&INTS);
        (self.konst(ty), ty)
    }

    fn push(&mut self, v: Value, ty: Type) {
        self.pool.push((v, ty));
        // Keep the pool bounded so later picks still see recent values.
        if self.pool.len() > 48 {
            let k = self.rng.below(self.pool.len() as u64 - 8) as usize;
            let _ = self.pool.remove(k);
        }
    }

    fn overflow(&mut self) -> Overflow {
        if self.opts.traps && self.rng.chance(12) {
            Overflow::Trap
        } else {
            Overflow::Wrap
        }
    }

    fn stmt(&mut self, depth: u32) {
        self.budget = self.budget.saturating_sub(1);
        let roll = self.rng.below(100);
        match roll {
            0..=7 => {
                let ty = if self.rng.chance(10) {
                    Type::Bool
                } else {
                    self.rng.pick(&INTS)
                };
                let v = self.konst(ty);
                self.push(v, ty);
            }
            8..=14 => self.unary(),
            15..=36 => self.binary(),
            37..=43 => self.compare_select(),
            44..=49 => self.convert(),
            50..=59 => self.check(),
            60..=64 if depth < 2 => self.if_else(depth),
            65..=68 if depth < 2 => self.loop_(depth),
            69..=72 if depth < 2 => self.switch(depth),
            73..=82 if self.opts.memory => self.memory(),
            83..=89 if self.opts.calls => self.call(),
            90..=92 if self.opts.memory => self.block_memory(),
            _ => self.binary(),
        }
    }

    fn unary(&mut self) {
        let (v, ty) = self.int_value();
        let op = self.rng.pick(&[UnaryOp::Neg, UnaryOp::Abs, UnaryOp::Not]);
        let policy = if op == UnaryOp::Not {
            Policy::NONE
        } else {
            let o = self.overflow();
            Policy::overflow(o)
        };
        let r = self.b.unary(op, policy, v).unwrap();
        self.push(r, ty);
    }

    fn binary(&mut self) {
        let (a, ty) = self.int_value();
        let b = self.value(ty);
        let op = self.rng.pick(&[
            BinaryOp::Add,
            BinaryOp::Sub,
            BinaryOp::Mul,
            BinaryOp::Div,
            BinaryOp::Rem,
            BinaryOp::FloorDiv,
            BinaryOp::FloorMod,
            BinaryOp::And,
            BinaryOp::Or,
            BinaryOp::Xor,
            BinaryOp::Shl,
            BinaryOp::Shr,
            BinaryOp::Min,
            BinaryOp::Max,
        ]);
        let policy = match op {
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul => Policy::overflow(self.overflow()),
            BinaryOp::Div | BinaryOp::FloorDiv => {
                Policy::overflow(self.overflow()).with_div_zero(DivZero::Trap)
            }
            BinaryOp::Rem | BinaryOp::FloorMod => Policy::div_zero(DivZero::Trap),
            BinaryOp::Shl | BinaryOp::Shr => Policy::shift(Shift::Mask),
            _ => Policy::NONE,
        };
        // Usually keep divisors odd so plain divisions rarely trap.
        let b = if matches!(
            op,
            BinaryOp::Div | BinaryOp::Rem | BinaryOp::FloorDiv | BinaryOp::FloorMod
        ) && (!self.opts.traps || self.rng.chance(85))
        {
            let one = self.b.iconst(ty, 1).unwrap();
            self.b.binary(BinaryOp::Or, Policy::NONE, b, one).unwrap()
        } else {
            b
        };
        let r = self.b.binary(op, policy, a, b).unwrap();
        self.push(r, ty);
    }

    fn compare_select(&mut self) {
        let (a, ty) = self.int_value();
        let b = self.value(ty);
        let op = self.rng.pick(&[
            CmpOp::Eq,
            CmpOp::Ne,
            CmpOp::Lt,
            CmpOp::Le,
            CmpOp::Gt,
            CmpOp::Ge,
        ]);
        let c = self.b.compare(op, a, b).unwrap();
        if self.rng.chance(50) {
            let (x, xt) = if self.rng.chance(20) {
                let v = self.konst(Type::Bool);
                (v, Type::Bool)
            } else {
                self.int_value()
            };
            let y = self.value(xt);
            let r = self.b.select(c, x, y).unwrap();
            self.push(r, xt);
        } else {
            self.push(c, Type::Bool);
        }
    }

    fn convert(&mut self) {
        let (v, from) = self.int_value();
        let mut to = self.rng.pick(&INTS);
        if to == from {
            to = if from == Type::I64 {
                Type::U64
            } else {
                Type::I64
            };
        }
        let r = if self.rng.chance(40) {
            let o = self.overflow();
            self.b
                .convert(ConvOp::IntCast, Policy::overflow(o), v, to)
                .unwrap()
        } else {
            self.coerce(v, from, to)
        };
        self.push(r, to);
    }

    /// A checked operation: the error edge returns (or rejoins with a
    /// fallback), the normal edge continues.
    fn check(&mut self) {
        let (a, ty) = self.int_value();
        let (op, res_ty) = match self.rng.below(7) {
            0 => {
                let u = self.rng.pick(&[UnaryOp::Neg, UnaryOp::Abs]);
                (
                    InstData::Unary {
                        op: u,
                        policy: Policy::overflow(Overflow::Error),
                        arg: a,
                    },
                    ty,
                )
            }
            1 | 2 => {
                let b = self.value(ty);
                let bop = self
                    .rng
                    .pick(&[BinaryOp::Add, BinaryOp::Sub, BinaryOp::Mul]);
                (
                    InstData::Binary {
                        op: bop,
                        policy: Policy::overflow(Overflow::Error),
                        args: [a, b],
                    },
                    ty,
                )
            }
            3 => {
                let b = self.value(ty);
                let bop = self.rng.pick(&[
                    BinaryOp::Div,
                    BinaryOp::Rem,
                    BinaryOp::FloorDiv,
                    BinaryOp::FloorMod,
                ]);
                let policy = match bop {
                    BinaryOp::Div | BinaryOp::FloorDiv => {
                        let o = self
                            .rng
                            .pick(&[Overflow::Error, Overflow::Wrap, Overflow::Error]);
                        Policy::overflow(o).with_div_zero(DivZero::Error)
                    }
                    _ => Policy::div_zero(DivZero::Error),
                };
                (
                    InstData::Binary {
                        op: bop,
                        policy,
                        args: [a, b],
                    },
                    ty,
                )
            }
            4 => {
                let b = self.value(ty);
                let bop = self.rng.pick(&[BinaryOp::Shl, BinaryOp::Shr]);
                (
                    InstData::Binary {
                        op: bop,
                        policy: Policy::shift(Shift::Error),
                        args: [a, b],
                    },
                    ty,
                )
            }
            5 => {
                let mut to = self.rng.pick(&INTS);
                if to == ty {
                    to = if ty == Type::I8 { Type::U8 } else { Type::I8 };
                }
                (
                    InstData::Convert {
                        op: ConvOp::IntCast,
                        policy: Policy::overflow(Overflow::Error),
                        to,
                        arg: a,
                    },
                    to,
                )
            }
            _ => {
                let c = self.coerce(a, ty, Type::U32);
                (
                    InstData::Convert {
                        op: ConvOp::CharFromU32,
                        policy: Policy::NONE,
                        to: Type::U32,
                        arg: c,
                    },
                    Type::U32,
                )
            }
        };
        let ok = self.b.create_block(&[res_ty]).unwrap();
        let err = self.b.create_block(&[Type::U32]).unwrap();
        self.b
            .check(
                op,
                BlockCall::with_args(ok, &[BlockArg::Result(0)]),
                BlockCall::with_args(err, &[BlockArg::ErrorCode]),
            )
            .unwrap();
        self.b.switch_to(err).unwrap();
        let code = self.b.block_param(err, 0).unwrap();
        if self.rng.chance(50) {
            // Return something that depends on the code.
            let saved = std::mem::take(&mut self.pool);
            self.pool.push((code, Type::U32));
            self.ret_values();
            self.pool = saved;
        } else {
            let fallback = self.coerce(code, Type::U32, res_ty);
            self.b.jump(ok, &[fallback]).unwrap();
        }
        self.b.switch_to(ok).unwrap();
        let r = self.b.block_param(ok, 0).unwrap();
        self.push(r, res_ty);
    }

    fn cond(&mut self) -> Value {
        let bools: Vec<Value> = self
            .pool
            .iter()
            .filter(|(_, t)| *t == Type::Bool)
            .map(|(v, _)| *v)
            .collect();
        if !bools.is_empty() && self.rng.chance(40) {
            return self.rng.pick(&bools);
        }
        let (a, ty) = self.int_value();
        let b = self.value(ty);
        let op = self.rng.pick(&[
            CmpOp::Eq,
            CmpOp::Ne,
            CmpOp::Lt,
            CmpOp::Le,
            CmpOp::Gt,
            CmpOp::Ge,
        ]);
        self.b.compare(op, a, b).unwrap()
    }

    /// Types merged at a join, picked from the pool.
    fn merge_types(&mut self) -> Vec<Type> {
        let n = self.rng.below(4) as usize;
        (0..n)
            .map(|_| {
                if self.pool.is_empty() || self.rng.chance(20) {
                    self.rng.pick(&INTS)
                } else {
                    self.rng.pick(&self.pool).1
                }
            })
            .collect()
    }

    fn arm(&mut self, depth: u32, types: &[Type]) -> Vec<Value> {
        let n = 1 + self.rng.below(4);
        for _ in 0..n {
            if self.budget == 0 {
                break;
            }
            self.stmt(depth + 1);
        }
        types.iter().map(|&t| self.value(t)).collect()
    }

    fn if_else(&mut self, depth: u32) {
        let c = self.cond();
        let types = self.merge_types();
        let then = self.b.create_block(&[]).unwrap();
        let other = self.b.create_block(&[]).unwrap();
        let join = self.b.create_block(&types).unwrap();
        self.b.branch(c, then, &[], other, &[]).unwrap();
        let saved = self.pool.clone();
        for arm in [then, other] {
            self.b.switch_to(arm).unwrap();
            if self.opts.traps && self.rng.chance(4) {
                self.b.trap(100 + self.rng.below(50) as u32).unwrap();
            } else {
                let vals = self.arm(depth, &types);
                self.b.jump(join, &vals).unwrap();
            }
            self.pool = saved.clone();
        }
        self.b.switch_to(join).unwrap();
        for (i, &t) in types.iter().enumerate() {
            let p = self.b.block_param(join, i).unwrap();
            self.push(p, t);
        }
    }

    fn loop_(&mut self, depth: u32) {
        let types = {
            let mut t = self.merge_types();
            if t.is_empty() {
                t.push(Type::I64);
            }
            t
        };
        let init: Vec<Value> = types.iter().map(|&t| self.value(t)).collect();
        let mut header_types = vec![Type::I32];
        header_types.extend(&types);
        let header = self.b.create_block(&header_types).unwrap();
        let body = self.b.create_block(&[]).unwrap();
        let exit = self.b.create_block(&types).unwrap();
        let zero = self.b.iconst(Type::I32, 0).unwrap();
        let mut args = vec![zero];
        args.extend(&init);
        self.b.jump(header, &args).unwrap();
        self.b.switch_to(header).unwrap();
        let i = self.b.block_param(header, 0).unwrap();
        let carried: Vec<Value> = (0..types.len())
            .map(|k| self.b.block_param(header, k + 1).unwrap())
            .collect();
        let trips = self.b.iconst(Type::I32, self.rng.below(6) as i128).unwrap();
        let more = self.b.compare(CmpOp::Lt, i, trips).unwrap();
        self.b.branch(more, body, &[], exit, &carried).unwrap();
        self.b.switch_to(body).unwrap();
        let saved = self.pool.clone();
        self.pool.push((i, Type::I32));
        for (k, &t) in types.iter().enumerate() {
            self.pool.push((carried[k], t));
        }
        let mut next = self.arm(depth, &types);
        // Often pass the carried values back permuted: the back edge is a
        // parallel copy with cycles.
        if self.rng.chance(60) && types.len() > 1 {
            let rot = 1 + self.rng.below(types.len() as u64 - 1) as usize;
            let mut perm: Vec<Value> = carried.clone();
            perm.rotate_left(rot);
            for (k, &t) in types.iter().enumerate() {
                let src_t = types[(k + rot) % types.len()];
                next[k] = if src_t == t { perm[k] } else { next[k] };
            }
        }
        let one = self.b.iconst(Type::I32, 1).unwrap();
        let i2 = self.b.add(i, one, Overflow::Wrap).unwrap();
        let mut back = vec![i2];
        back.extend(&next);
        self.b.jump(header, &back).unwrap();
        self.pool = saved;
        self.b.switch_to(exit).unwrap();
        for (k, &t) in types.iter().enumerate() {
            let p = self.b.block_param(exit, k).unwrap();
            self.push(p, t);
        }
    }

    fn switch(&mut self, depth: u32) {
        let (v, ty) = self.int_value();
        let w = ty.int_bits().unwrap();
        let mask = if w == 64 { u64::MAX } else { (1u64 << w) - 1 };
        let n = 1 + self.rng.below(7) as usize;
        let dense = self.rng.chance(50);
        let base = if self.rng.chance(50) {
            0
        } else {
            self.rng.next() & mask
        };
        let mut values: Vec<u64> = Vec::new();
        while values.len() < n {
            let k = if dense {
                base.wrapping_add(values.len() as u64 + self.rng.below(2)) & mask
            } else {
                interesting(&mut self.rng, ty)
            };
            if !values.contains(&k) {
                values.push(k);
            }
            if values.len() as u64 > mask {
                break;
            }
        }
        let types = self.merge_types();
        let join = self.b.create_block(&types).unwrap();
        let default = self.b.create_block(&[]).unwrap();
        let mut cases = Vec::new();
        let mut blocks = Vec::new();
        for &k in &values {
            // Some cases pass arguments straight to the join.
            if self.rng.chance(30) {
                let vals: Vec<Value> = types.iter().map(|&t| self.value(t)).collect();
                cases.push(SwitchCase {
                    value: k,
                    dest: BlockCall::new(join, &vals),
                });
            } else {
                let blk = self.b.create_block(&[]).unwrap();
                cases.push(SwitchCase {
                    value: k,
                    dest: BlockCall::new(blk, &[]),
                });
                blocks.push(blk);
            }
        }
        self.b
            .switch(v, cases, BlockCall::new(default, &[]))
            .unwrap();
        let saved = self.pool.clone();
        blocks.push(default);
        for blk in blocks {
            self.b.switch_to(blk).unwrap();
            let vals = self.arm(depth, &types);
            self.b.jump(join, &vals).unwrap();
            self.pool = saved.clone();
        }
        self.b.switch_to(join).unwrap();
        for (i, &t) in types.iter().enumerate() {
            let p = self.b.block_param(join, i).unwrap();
            self.push(p, t);
        }
    }

    /// An in-bounds index into eight elements: a constant, or a value masked
    /// to 0..=7.
    fn index(&mut self) -> Value {
        if self.rng.chance(40) {
            let ty = self.rng.pick(&INTS);
            return self.b.iconst(ty, self.rng.below(8) as i128).unwrap();
        }
        let (v, ty) = self.int_value();
        let seven = self.b.iconst(ty, 7).unwrap();
        self.b
            .binary(BinaryOp::And, Policy::NONE, v, seven)
            .unwrap()
    }

    fn memory(&mut self) {
        let Some([pa, pc, pr]) = self.slots else {
            return;
        };
        match self.rng.below(5) {
            0 | 1 => {
                // Store into, then maybe load from, an array of `ty`.
                let ty = self.rng.pick(&INTS);
                let base = self.rng.pick(&[pa, pc]);
                let idx = self.index();
                let addr = self.b.elem_addr(base, MemType::Scalar(ty), idx).unwrap();
                let v = self.value(ty);
                let size = ty.int_bits().unwrap() / 8;
                let _ = self.b.store(v, addr, MemFlags::new(size)).unwrap();
                let idx2 = self.index();
                let addr2 = self.b.elem_addr(base, MemType::Scalar(ty), idx2).unwrap();
                let r = self.b.load(ty, addr2, MemFlags::new(size)).unwrap();
                self.push(r, ty);
            }
            2 => {
                // A record field.
                let fields = [Type::I8, Type::I16, Type::U32, Type::I64, Type::U8];
                let f = self.rng.below(5) as u32;
                let ty = fields[f as usize];
                let addr = self.b.field_addr(pr, self.rec, f).unwrap();
                let size = ty.int_bits().unwrap() / 8;
                if self.rng.chance(60) {
                    let v = self.value(ty);
                    let _ = self.b.store(v, addr, MemFlags::new(size)).unwrap();
                }
                let r = self.b.load(ty, addr, MemFlags::new(size)).unwrap();
                self.push(r, ty);
            }
            3 => {
                // The global, through a byte offset.
                let ty = self.rng.pick(&INTS);
                let size = ty.int_bits().unwrap() / 8;
                let g = self.b.global_addr(self.global).unwrap();
                let k = self.rng.below(u64::from(64 / size)) * u64::from(size);
                let off = self.b.iconst(Type::U64, k as i128).unwrap();
                let addr = self.b.ptr_offset(g, off).unwrap();
                if self.rng.chance(50) {
                    let v = self.value(ty);
                    let _ = self.b.store(v, addr, MemFlags::new(size)).unwrap();
                }
                let r = self.b.load(ty, addr, MemFlags::new(size)).unwrap();
                self.push(r, ty);
            }
            _ => {
                // A volatile byte round trip.
                let idx = self.index();
                let addr = self
                    .b
                    .elem_addr(pa, MemType::Scalar(Type::U8), idx)
                    .unwrap();
                let v = self.value(Type::U8);
                let _ = self.b.store(v, addr, MemFlags::new(1).volatile()).unwrap();
                let r = self
                    .b
                    .load(Type::U8, addr, MemFlags::new(1).volatile())
                    .unwrap();
                self.push(r, Type::U8);
            }
        }
    }

    fn block_memory(&mut self) {
        let Some([pa, pc, _]) = self.slots else {
            return;
        };
        let len = |g: &mut G<'_>, max: u64| -> Value {
            if g.rng.chance(50) {
                g.b.iconst(Type::U64, g.rng.below(max + 1) as i128).unwrap()
            } else {
                let (v, ty) = g.int_value();
                let v = g.coerce(v, ty, Type::U64);
                let m =
                    g.b.iconst(Type::U64, (max.next_power_of_two() / 2).max(1) as i128 - 1)
                        .unwrap();
                g.b.binary(BinaryOp::And, Policy::NONE, v, m).unwrap()
            }
        };
        match self.rng.below(3) {
            0 => {
                let n = len(self, 64);
                let (d, s) = if self.rng.chance(50) {
                    (pa, pc)
                } else {
                    (pc, pa)
                };
                let _ = self.b.memcpy(d, s, n, MemFlags::new(8)).unwrap();
            }
            1 => {
                // Overlapping, either direction.
                let k = self.rng.below(16);
                let off = self.b.iconst(Type::U64, k as i128).unwrap();
                let shifted = self.b.ptr_offset(pa, off).unwrap();
                let n = len(self, 48);
                let (d, s) = if self.rng.chance(50) {
                    (pa, shifted)
                } else {
                    (shifted, pa)
                };
                let _ = self.b.memmove(d, s, n, MemFlags::new(1)).unwrap();
            }
            _ => {
                let byte = self.value(Type::U8);
                let n = len(self, 64);
                let _ = self.b.memset(pc, byte, n, MemFlags::new(16)).unwrap();
            }
        }
    }

    fn call(&mut self) {
        if self.callees.is_empty() {
            return;
        }
        let (f, sig, sig_id) = {
            let k = self.rng.below(self.callees.len() as u64) as usize;
            self.callees[k].clone()
        };
        let args: Vec<Value> = sig.params.iter().map(|&t| self.value(t)).collect();
        let call = if self.rng.chance(30) {
            let fp = self.b.func_addr(f).unwrap();
            Call::indirect(sig_id, fp, &args)
        } else {
            Call::direct(f, &args)
        };
        let inst = self.b.call(call).unwrap();
        let results: Vec<Value> = self.b.results(inst).iter().collect();
        for (v, &t) in results.iter().zip(&sig.returns) {
            self.push(*v, t);
        }
    }

    /// Folds many pool values into the results, so they stay live to the end.
    fn ret_values(&mut self) {
        let returns = self.sig.returns.clone();
        let mut outs = Vec::new();
        for &t in &returns {
            let mut acc = self.konst(t);
            let picks = self.pool.len().min(24);
            for _ in 0..picks {
                let (v, vt) = self.rng.pick(&self.pool);
                let v = self.coerce(v, vt, t);
                let op = self.rng.pick(&[BinaryOp::Xor, BinaryOp::Add]);
                let policy = if op == BinaryOp::Add {
                    Policy::overflow(Overflow::Wrap)
                } else {
                    Policy::NONE
                };
                acc = self.b.binary(op, policy, acc, v).unwrap();
            }
            outs.push(acc);
        }
        self.b.ret(&outs).unwrap();
    }

    fn finish(&mut self) {
        if self.opts.tail_calls && self.rng.chance(25) {
            let own = self.sig.params.len();
            let ok: Vec<(FuncId, Signature, SigId)> = self
                .callees
                .iter()
                .filter(|(_, s, _)| s.params.len() <= own.max(4))
                .cloned()
                .collect();
            if !ok.is_empty() {
                let (f, sig, sig_id) = self.rng.pick_owned(&ok);
                let args: Vec<Value> = sig.params.iter().map(|&t| self.value(t)).collect();
                let call = if self.rng.chance(30) {
                    let fp = self.b.func_addr(f).unwrap();
                    Call::indirect(sig_id, fp, &args)
                } else {
                    Call::direct(f, &args)
                };
                self.b.tail_call(call).unwrap();
                return;
            }
        }
        self.ret_values();
    }
}

impl Rng {
    fn pick_owned<T: Clone>(&mut self, items: &[T]) -> T {
        items[self.below(items.len() as u64) as usize].clone()
    }
}

/// Random arguments for `params`, as the interpreter's values and as the
/// register images the native side passes: the low bits the value, the
/// bits above its width junk (the callee must not rely on them), except
/// `bool`, whose bits 1-7 are zero as the C conventions require.
pub fn args(rng: &mut Rng, params: &[Type]) -> (Vec<ir_lang::interp::Val>, Vec<u64>) {
    let mut vals = Vec::new();
    let mut regs = Vec::new();
    for &t in params {
        let bits = interesting(rng, t);
        vals.push(ir_lang::interp::Val::new(t, bits));
        let w = if t == Type::Bool {
            8
        } else {
            t.int_bits().unwrap_or(64)
        };
        let junk = if w == 64 { 0 } else { rng.next() << w };
        regs.push(bits | junk);
    }
    (vals, regs)
}

/// A large function of realistic shape for throughput measurements:
/// segments of straight-line integer arithmetic over a sliding window of
/// eight live values, each followed by a diamond that merges two values,
/// a loop every eighth segment, a store and a load through a stack array,
/// and a call to a small helper every tenth. About `insts` instructions.
pub fn big_function(insts: usize) -> (Module, FuncId) {
    let mut m = Module::new("big");
    let helper = m
        .declare_function(
            "helper",
            &Signature::new(&[Type::I64, Type::I64], &[Type::I64]),
            Linkage::Export,
        )
        .unwrap();
    {
        let mut b = m.build(helper).unwrap();
        let (x, y) = (b.param(0).unwrap(), b.param(1).unwrap());
        let s = b.binary(BinaryOp::Xor, Policy::NONE, x, y).unwrap();
        b.ret(&[s]).unwrap();
    }
    let f = m
        .declare_function(
            "big",
            &Signature::new(&[Type::I64, Type::I64], &[Type::I64]),
            Linkage::Export,
        )
        .unwrap();
    let mut b = m.build(f).unwrap();
    let mut rng = Rng::new(42);
    let slot = b.stack_slot(64, 8).unwrap();
    let base = b.stack_addr(slot).unwrap();
    let mut window: Vec<Value> = vec![b.param(0).unwrap(), b.param(1).unwrap()];
    while window.len() < 8 {
        let k = b.iconst(Type::I64, window.len() as i128 * 7 + 1).unwrap();
        window.push(k);
    }
    let ops = [
        BinaryOp::Add,
        BinaryOp::Sub,
        BinaryOp::Xor,
        BinaryOp::And,
        BinaryOp::Or,
        BinaryOp::Mul,
    ];
    let mut seg = 0usize;
    while b.func().inst_count() < insts {
        seg += 1;
        for _ in 0..40 {
            let a = window[rng.below(8) as usize];
            let c = window[rng.below(8) as usize];
            let op = rng.pick(&ops);
            let policy = match op {
                BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul => Policy::overflow(Overflow::Wrap),
                _ => Policy::NONE,
            };
            let r = b.binary(op, policy, a, c).unwrap();
            let k = rng.below(8) as usize;
            window[k] = r;
        }
        // A store and a load through the stack array.
        let idx = b.iconst(Type::I64, rng.below(8) as i128).unwrap();
        let at = b.elem_addr(base, MemType::Scalar(Type::I64), idx).unwrap();
        let _ = b.store(window[0], at, MemFlags::new(8)).unwrap();
        let ld = b.load(Type::I64, at, MemFlags::new(8)).unwrap();
        window[1] = ld;
        // A diamond merging two values.
        let c = b.compare(CmpOp::Lt, window[2], window[3]).unwrap();
        let (t, e) = (b.create_block(&[]).unwrap(), b.create_block(&[]).unwrap());
        let join = b.create_block(&[Type::I64, Type::I64]).unwrap();
        b.branch(c, t, &[], e, &[]).unwrap();
        b.switch_to(t).unwrap();
        let x = b.add(window[4], window[5], Overflow::Wrap).unwrap();
        b.jump(join, &[x, window[6]]).unwrap();
        b.switch_to(e).unwrap();
        let y = b.sub(window[4], window[5], Overflow::Wrap).unwrap();
        b.jump(join, &[window[7], y]).unwrap();
        b.switch_to(join).unwrap();
        window[4] = b.block_param(join, 0).unwrap();
        window[5] = b.block_param(join, 1).unwrap();
        if seg % 8 == 0 {
            // A short counted loop rotating three values.
            let head = b
                .create_block(&[Type::I64, Type::I64, Type::I64, Type::I64])
                .unwrap();
            let body = b.create_block(&[]).unwrap();
            let exit = b.create_block(&[Type::I64, Type::I64, Type::I64]).unwrap();
            let z = b.iconst(Type::I64, 0).unwrap();
            b.jump(head, &[z, window[0], window[1], window[2]]).unwrap();
            b.switch_to(head).unwrap();
            let p: Vec<Value> = (0..4).map(|k| b.block_param(head, k).unwrap()).collect();
            let n = b.iconst(Type::I64, 4).unwrap();
            let more = b.compare(CmpOp::Lt, p[0], n).unwrap();
            b.branch(more, body, &[], exit, &[p[1], p[2], p[3]])
                .unwrap();
            b.switch_to(body).unwrap();
            let one = b.iconst(Type::I64, 1).unwrap();
            let i2 = b.add(p[0], one, Overflow::Wrap).unwrap();
            let s = b.add(p[1], p[3], Overflow::Wrap).unwrap();
            b.jump(head, &[i2, p[2], p[3], s]).unwrap();
            b.switch_to(exit).unwrap();
            for (k, w) in window.iter_mut().enumerate().take(3) {
                *w = b.block_param(exit, k).unwrap();
            }
        }
        if seg % 10 == 0 {
            let c = b
                .call(Call::direct(helper, &[window[6], window[7]]))
                .unwrap();
            window[6] = b.results(c).first().unwrap();
        }
    }
    let mut acc = window[0];
    for &w in &window[1..] {
        acc = b.binary(BinaryOp::Xor, Policy::NONE, acc, w).unwrap();
    }
    b.ret(&[acc]).unwrap();
    (m, f)
}
