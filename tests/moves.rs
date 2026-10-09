//! The parallel-move resolver against a parallel-assignment reference.
//!
//! Every resolved sequence is executed on a model machine and compared with
//! the parallel copy's meaning: every destination ends with its source's
//! value from before the copy, every location that is neither a destination
//! nor declared free ends unchanged, and every step is one the target can
//! execute (no memory-to-memory move, no immediate store out of range, swaps
//! of registers only). Move counts are checked against the minimal count
//! for register-only copies.

#![allow(clippy::unwrap_used, reason = "test failures should be loud")]

use std::collections::{BTreeMap, BTreeSet};

use backend_lang::mir::StackSlot;
use backend_lang::moves::{Loc, Move, MoveEnv, MoveError, MoveResolver, Source, Step};
use backend_lang::target::ImmRange;
use backend_lang::x86_64::{self, GPR, RBP, RSP, VEC, XMM0, XMM1};
use backend_lang::{PReg, RegSet};
use proptest::prelude::*;

/// The fourteen allocatable general-purpose registers.
fn gprs() -> Vec<PReg> {
    x86_64::TARGET.class(GPR).allocatable().iter().collect()
}

const SCRATCH: StackSlot = StackSlot::new(10_000_000);
const VICTIM: StackSlot = StackSlot::new(10_000_001);

fn initial(l: Loc) -> i64 {
    match l {
        Loc::Reg(r) => 1_000 + r.index() as i64,
        Loc::Slot(s) => 1_000_000 + s.index() as i64,
    }
}

/// Runs the steps on a model where every location starts with `initial`.
fn execute(steps: &[Step]) -> BTreeMap<Loc, i64> {
    let mut state: BTreeMap<Loc, i64> = BTreeMap::new();
    let get = |st: &BTreeMap<Loc, i64>, l: Loc| st.get(&l).copied().unwrap_or_else(|| initial(l));
    for s in steps {
        match *s {
            Step::Move { dst, src, .. } => {
                let v = match src {
                    Source::Loc(l) => get(&state, l),
                    Source::Imm(v) => v,
                };
                let _ = state.insert(dst, v);
            }
            Step::Swap { a, b, .. } => {
                let (va, vb) = (get(&state, Loc::Reg(a)), get(&state, Loc::Reg(b)));
                let _ = state.insert(Loc::Reg(a), vb);
                let _ = state.insert(Loc::Reg(b), va);
            }
        }
    }
    state
}

/// Checks `steps` perform the parallel copy `moves` legally on x86-64.
fn check(moves: &[Move], env: &MoveEnv, steps: &[Step]) {
    for s in steps {
        match *s {
            Step::Move { dst, src, class } => {
                assert_eq!(class, GPR);
                if let Loc::Slot(_) = dst {
                    match src {
                        Source::Loc(l) => assert!(!l.is_mem(), "memory-to-memory step {s:?}"),
                        Source::Imm(v) => {
                            assert!(ImmRange::I32.fits(v), "unstorable immediate {s:?}")
                        }
                    }
                }
                for l in [
                    Some(dst),
                    if let Source::Loc(l) = src {
                        Some(l)
                    } else {
                        None
                    },
                ]
                .into_iter()
                .flatten()
                {
                    if let Loc::Reg(r) = l {
                        assert!(r != RSP && r != RBP, "frame register used: {s:?}");
                    }
                }
            }
            Step::Swap { a, b, .. } => {
                assert!(env.swap);
                assert_ne!(a, b);
            }
        }
    }
    let state = execute(steps);
    let value = |l: Loc| state.get(&l).copied().unwrap_or_else(|| initial(l));
    let mut dsts = BTreeSet::new();
    for m in moves {
        let want = match m.src {
            Source::Loc(l) => initial(l),
            Source::Imm(v) => v,
        };
        assert_eq!(
            value(m.dst),
            want,
            "destination {:?} of {moves:?}\nsteps {steps:?}",
            m.dst
        );
        let _ = dsts.insert(m.dst);
    }
    for (&l, &v) in &state {
        let free = matches!(l, Loc::Reg(r) if env.free.contains(r))
            || l == Loc::Slot(SCRATCH)
            || l == Loc::Slot(VICTIM);
        if !dsts.contains(&l) && !free {
            assert_eq!(
                v,
                initial(l),
                "{l:?} changed but is not a destination\nmoves {moves:?}\nsteps {steps:?}"
            );
        }
    }
}

fn resolve(moves: &[Move], env: &MoveEnv) -> Vec<Step> {
    let mut out = Vec::new();
    MoveResolver::new()
        .resolve(&x86_64::TARGET, moves, env, &mut out)
        .unwrap_or_else(|e| panic!("{e}: {moves:?}"));
    out
}

fn full_env(free: RegSet, swap: bool) -> MoveEnv {
    MoveEnv::new()
        .with_free(free)
        .with_scratch_slot(SCRATCH)
        .with_victim_slot(VICTIM)
        .with_swap(swap)
}

/// The minimal step count for a register-only copy with a free register:
/// one move per non-trivial copy; per cycle, nothing extra if a cycle value
/// is also copied out of the cycle (fan-out), else one swap fewer than its
/// length with swaps, else one extra move.
fn minimal(moves: &[Move], swap: bool) -> usize {
    let nontrivial: Vec<&Move> = moves
        .iter()
        .filter(|m| m.src != Source::Loc(m.dst))
        .collect();
    let writer: BTreeMap<Loc, Loc> = nontrivial
        .iter()
        .filter_map(|m| match m.src {
            Source::Loc(s) => Some((m.dst, s)),
            Source::Imm(_) => None,
        })
        .collect();
    let mut seen = BTreeSet::new();
    let mut total = nontrivial.len();
    for &start in writer.keys() {
        if seen.contains(&start) {
            continue;
        }
        // Walk backwards along sources; a cycle is found if we return to a
        // node visited in this walk.
        let mut path = Vec::new();
        let mut cur = start;
        let cycle = loop {
            if seen.contains(&cur) {
                break None;
            }
            if let Some(pos) = path.iter().position(|&p| p == cur) {
                break Some(path[pos..].to_vec());
            }
            path.push(cur);
            match writer.get(&cur) {
                Some(&next) => cur = next,
                None => break None,
            }
        };
        seen.extend(path.iter().copied());
        if let Some(cycle) = cycle {
            let set: BTreeSet<Loc> = cycle.iter().copied().collect();
            let fanout = nontrivial.iter().any(|m| {
                matches!(m.src, Source::Loc(s) if set.contains(&s)) && !set.contains(&m.dst)
            });
            if fanout {
                continue;
            }
            if swap {
                total -= 1;
            } else {
                total += 1;
            }
        }
    }
    total
}

/// A random parallel copy over `regs` registers and `slots` stack slots,
/// built from explicit cycles (of every length), fan-out copies, chains, and
/// immediates.
fn copy_strategy(max_regs: usize, max_slots: usize) -> impl Strategy<Value = Vec<Move>> {
    let regs = gprs();
    (0..=max_regs, 0..=max_slots, any::<u64>()).prop_map(move |(nr, ns, seed)| {
        let mut rng = Rng(seed | 1);
        let mut locs: Vec<Loc> = regs.iter().take(nr).map(|&r| Loc::Reg(r)).collect();
        locs.extend((0..ns).map(|s| Loc::Slot(StackSlot::new(s as u32))));
        rng.shuffle(&mut locs);
        let mut moves = Vec::new();
        let mut i = 0;
        let mut dsts = BTreeSet::new();
        while i < locs.len() {
            let len = 1 + rng.below(locs.len() - i);
            let group = &locs[i..i + len];
            match rng.below(4) {
                // A cycle: group[k] := group[k+1], the last := the first.
                0 | 1 => {
                    for k in 0..len {
                        moves.push(Move::new(group[k], group[(k + 1) % len], GPR));
                        let _ = dsts.insert(group[k]);
                    }
                }
                // A chain: group[k] := group[k+1]; the last is only read.
                2 => {
                    for k in 0..len.saturating_sub(1) {
                        moves.push(Move::new(group[k], group[k + 1], GPR));
                        let _ = dsts.insert(group[k]);
                    }
                }
                // Immediates, some too wide to store directly.
                _ => {
                    for &l in group {
                        let v = if rng.below(3) == 0 {
                            1 << 40
                        } else {
                            rng.below(1000) as i64 - 500
                        };
                        moves.push(Move {
                            dst: l,
                            src: Source::Imm(v),
                            class: GPR,
                        });
                        let _ = dsts.insert(l);
                    }
                }
            }
            i += len;
        }
        // Fan-out: copy random sources into locations nothing writes yet.
        let spare: Vec<Loc> = locs.iter().copied().filter(|l| !dsts.contains(l)).collect();
        for &d in &spare {
            if rng.below(2) == 0 && !locs.is_empty() {
                let s = locs[rng.below(locs.len())];
                moves.push(Move::new(d, s, GPR));
            }
        }
        rng.shuffle(&mut moves);
        moves
    })
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i + 1);
            v.swap(i, j);
        }
    }
}

fn free_strategy() -> impl Strategy<Value = RegSet> {
    proptest::collection::vec(any::<bool>(), 14).prop_map(|bits| {
        gprs()
            .into_iter()
            .zip(bits)
            .filter(|&(_, b)| b)
            .map(|(r, _)| r)
            .collect()
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3000))]

    #[test]
    fn resolved_steps_equal_the_parallel_copy(
        moves in copy_strategy(14, 8),
        free in free_strategy(),
        swap in any::<bool>(),
    ) {
        let env = full_env(free, swap);
        let steps = resolve(&moves, &env);
        check(&moves, &env, &steps);
    }

    #[test]
    fn register_only_copies_use_the_minimal_number_of_steps(
        moves in copy_strategy(14, 0),
        swap in any::<bool>(),
    ) {
        // Keep one register out of the copy so a free register exists.
        let spare = gprs()[13];
        let moves: Vec<Move> = moves
            .into_iter()
            .filter(|m| m.dst != Loc::Reg(spare) && m.src != Source::Loc(Loc::Reg(spare)))
            .collect();
        let env = full_env(RegSet::of(&[spare]), swap);
        let steps = resolve(&moves, &env);
        check(&moves, &env, &steps);
        prop_assert_eq!(steps.len(), minimal(&moves, swap));
    }

    #[test]
    fn without_free_registers_cycles_through_memory_borrow_safely(
        moves in copy_strategy(4, 12),
        swap in any::<bool>(),
    ) {
        let env = full_env(RegSet::EMPTY, swap);
        let steps = resolve(&moves, &env);
        check(&moves, &env, &steps);
    }

    #[test]
    fn a_resolver_reused_across_copies_gives_the_same_steps(
        a in copy_strategy(14, 6),
        b in copy_strategy(14, 6),
    ) {
        let env = full_env(RegSet::EMPTY, true);
        let mut r = MoveResolver::new();
        let mut first = Vec::new();
        let _ = r.resolve(&x86_64::TARGET, &a, &env, &mut first).unwrap();
        let mut second = Vec::new();
        let _ = r.resolve(&x86_64::TARGET, &b, &env, &mut second).unwrap();
        prop_assert_eq!(second, resolve(&b, &env));
        prop_assert_eq!(first, resolve(&a, &env));
    }
}

/// Every cycle length from 2 to 14 over registers, swaps on and off; and
/// over slots up to 64, with and without a free register.
#[test]
fn test_cycles_of_every_length() {
    let regs = gprs();
    for k in 2..=14 {
        let cycle: Vec<Move> = (0..k)
            .map(|i| Move::new(regs[i], regs[(i + 1) % k], GPR))
            .collect();
        for swap in [false, true] {
            let env = full_env(RegSet::EMPTY, swap);
            let steps = resolve(&cycle, &env);
            check(&cycle, &env, &steps);
            if swap {
                assert_eq!(steps.len(), k - 1, "a {k}-cycle is {} swaps", k - 1);
                assert!(steps.iter().all(|s| matches!(s, Step::Swap { .. })));
            }
        }
        if k < 14 {
            let env = full_env(RegSet::of(&[regs[13]]), false);
            let steps = resolve(&cycle, &env);
            check(&cycle, &env, &steps);
            assert_eq!(
                steps.len(),
                k + 1,
                "a {k}-cycle through a free register is k + 1 moves"
            );
        }
    }
    for k in 2..=64u32 {
        let cycle: Vec<Move> = (0..k)
            .map(|i| Move::new(StackSlot::new(i), StackSlot::new((i + 1) % k), GPR))
            .collect();
        for free in [RegSet::EMPTY, RegSet::of(&[regs[0], regs[1]])] {
            let env = full_env(free, true);
            let steps = resolve(&cycle, &env);
            check(&cycle, &env, &steps);
        }
    }
}

#[test]
fn test_trivial_and_empty_copies() {
    let env = MoveEnv::new();
    assert!(resolve(&[], &env).is_empty());
    let r = gprs()[0];
    assert!(resolve(&[Move::new(r, r, GPR)], &env).is_empty());
    let s = StackSlot::new(3);
    assert!(resolve(&[Move::new(s, s, GPR)], &env).is_empty());
}

#[test]
fn test_fan_out_breaks_a_cycle_for_free() {
    let r = gprs();
    // r0 <-> r1 swap, and r2 := r0: r2's copy of r0 breaks the cycle.
    let moves = [
        Move::new(r[0], r[1], GPR),
        Move::new(r[1], r[0], GPR),
        Move::new(r[2], r[0], GPR),
    ];
    let env = full_env(RegSet::EMPTY, false);
    let steps = resolve(&moves, &env);
    check(&moves, &env, &steps);
    assert_eq!(steps.len(), 3);
}

#[test]
fn test_errors() {
    let r = gprs();
    let mut out = Vec::new();
    let mut res = MoveResolver::new();
    let t = &x86_64::TARGET;
    // Duplicate destinations, including one hidden behind a self-move.
    assert_eq!(
        res.resolve(
            t,
            &[Move::new(r[0], r[1], GPR), Move::new(r[0], r[0], GPR)],
            &MoveEnv::new(),
            &mut out
        ),
        Err(MoveError::DuplicateDestination { index: 1 })
    );
    // A register outside the move's class.
    assert_eq!(
        res.resolve(t, &[Move::new(XMM0, r[0], GPR)], &MoveEnv::new(), &mut out),
        Err(MoveError::ClassMismatch { index: 0 })
    );
    assert_eq!(
        res.resolve(
            t,
            &[Move::new(r[0], r[1], backend_lang::RegClass::new(9))],
            &MoveEnv::new(),
            &mut out
        ),
        Err(MoveError::ClassMismatch { index: 0 })
    );
    // A register cycle without swaps, free registers, or a scratch slot.
    let swap = [Move::new(r[0], r[1], GPR), Move::new(r[1], r[0], GPR)];
    assert_eq!(
        res.resolve(t, &swap, &MoveEnv::new().with_swap(false), &mut out),
        Err(MoveError::NoScratch { class: GPR })
    );
    // Slot to slot without a free register or a victim slot.
    let copy = [Move::new(StackSlot::new(0), StackSlot::new(1), GPR)];
    assert_eq!(
        res.resolve(t, &copy, &MoveEnv::new(), &mut out),
        Err(MoveError::NoTemp { class: GPR })
    );
    // A wide immediate into a slot needs a temporary too.
    let wide = [Move {
        dst: Loc::Slot(StackSlot::new(0)),
        src: Source::Imm(1 << 40),
        class: GPR,
    }];
    assert_eq!(
        res.resolve(t, &wide, &MoveEnv::new(), &mut out),
        Err(MoveError::NoTemp { class: GPR })
    );
    assert!(
        MoveError::NoTemp { class: GPR }
            .to_string()
            .contains("victim slot")
    );
    // The flags class has no register to borrow.
    let flags = [Move::new(
        StackSlot::new(0),
        StackSlot::new(1),
        x86_64::FLAGS_CLASS,
    )];
    assert_eq!(
        res.resolve(
            t,
            &flags,
            &MoveEnv::new().with_victim_slot(VICTIM),
            &mut out
        ),
        Err(MoveError::NoTemp {
            class: x86_64::FLAGS_CLASS
        })
    );
}

#[test]
fn test_vector_class_swaps_stay_in_class() {
    let moves = [Move::new(XMM0, XMM1, VEC), Move::new(XMM1, XMM0, VEC)];
    let steps = resolve(&moves, &MoveEnv::new());
    assert_eq!(
        steps,
        [Step::Swap {
            a: XMM0,
            b: XMM1,
            class: VEC
        }]
    );
}

/// Large copies: a 100,000-slot random permutation (every value moves through
/// memory) and 100,000 copies fanning out of 14 registers, resolved and
/// checked.
#[test]
fn test_scale_100k() {
    let n = 100_000u32;
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut perm: Vec<u32> = (0..n).collect();
    rng.shuffle(&mut perm);
    let moves: Vec<Move> = (0..n)
        .map(|i| Move::new(StackSlot::new(i), StackSlot::new(perm[i as usize]), GPR))
        .collect();
    let env = full_env(RegSet::of(&[gprs()[0]]), true);
    let start = std::time::Instant::now();
    let steps = resolve(&moves, &env);
    let took = start.elapsed();
    check(&moves, &env, &steps);
    assert!(took.as_secs() < 5, "100k moves took {took:?}");

    let regs = gprs();
    let fan: Vec<Move> = (0..n)
        .map(|i| Move::new(StackSlot::new(i), regs[i as usize % regs.len()], GPR))
        .collect();
    let env = full_env(RegSet::EMPTY, true);
    let steps = resolve(&fan, &env);
    check(&fan, &env, &steps);
    assert_eq!(steps.len(), n as usize);
}
