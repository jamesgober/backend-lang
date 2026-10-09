//! Test support: a small x86-64 interpreter over isa-lang's decoder, so code
//! the emitter produces can be run on every platform, not only on Linux.
//!
//! It executes the integer subset the backend emits (moves, arithmetic,
//! shifts, compares and branches, calls and returns, `xchg`, `push`/`pop`,
//! `syscall` for `exit`) with the flags the conditions read. It is a test
//! oracle written independently of the emitter: it reads only bytes.
//!
//! Memory is one flat stack region. Code lives at `CODE_BASE`, so a
//! RIP-relative address is `CODE_BASE + offset`. The interpreter checks the
//! ABI's stack alignment at every `call`.

#![allow(dead_code, reason = "each test binary uses a different part")]
#![allow(clippy::unwrap_used, reason = "test support")]

use isa_lang::x86_64::{Cond, Inst, Mem, Mnemonic as M, Operand, Reg, Size, decode};

pub const CODE_BASE: u64 = 0x40_0000;
pub const STACK_BASE: u64 = 0x7000_0000;
pub const STACK_SIZE: usize = 1 << 20;
/// The return address the harness pushes; returning to it ends the run.
pub const SENTINEL: u64 = 0xDEAD_BEEF;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// `ud2` executed at this code offset.
    Ud2(usize),
    /// `syscall` with `rax = 60` (exit): the status.
    Exit(u64),
    /// Returned to the sentinel.
    Returned,
    /// `int3`.
    Breakpoint,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Flags {
    cf: bool,
    zf: bool,
    sf: bool,
    of: bool,
    pf: bool,
}

pub struct Machine {
    pub regs: [u64; 16],
    flags: Flags,
    pub mem: Vec<u8>,
    pub pc: usize,
    pub steps: u64,
    pub max_steps: u64,
}

fn mask(bits: u32) -> u64 {
    if bits == 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    }
}

fn sign(v: u64, bits: u32) -> bool {
    (v >> (bits - 1)) & 1 == 1
}

fn sext(v: u64, bits: u32) -> i64 {
    let shift = 64 - bits;
    ((v << shift) as i64) >> shift
}

impl Machine {
    /// A machine with an empty stack: `rsp` points at the sentinel return
    /// address, so the stack is 8 off 16-alignment, as at a function entry.
    pub fn new() -> Machine {
        let mut m = Machine {
            regs: [0; 16],
            flags: Flags::default(),
            mem: vec![0; STACK_SIZE],
            pc: 0,
            steps: 0,
            max_steps: 50_000_000,
        };
        let top = STACK_BASE + STACK_SIZE as u64 - 64;
        m.regs[4] = top - 8;
        m.write(top - 8, 8, SENTINEL).unwrap();
        m
    }

    pub fn rsp(&self) -> u64 {
        self.regs[4]
    }

    pub fn read(&self, addr: u64, bytes: u32) -> Result<u64, String> {
        let off =
            addr.checked_sub(STACK_BASE)
                .ok_or_else(|| format!("read below the stack at {addr:#x}"))? as usize;
        let slice = self
            .mem
            .get(off..off + bytes as usize)
            .ok_or_else(|| format!("read outside the stack at {addr:#x}"))?;
        let mut v = 0u64;
        for (i, b) in slice.iter().enumerate() {
            v |= u64::from(*b) << (8 * i);
        }
        Ok(v)
    }

    pub fn write(&mut self, addr: u64, bytes: u32, v: u64) -> Result<(), String> {
        let off =
            addr.checked_sub(STACK_BASE)
                .ok_or_else(|| format!("write below the stack at {addr:#x}"))? as usize;
        let slice = self
            .mem
            .get_mut(off..off + bytes as usize)
            .ok_or_else(|| format!("write outside the stack at {addr:#x}"))?;
        for (i, b) in slice.iter_mut().enumerate() {
            *b = (v >> (8 * i)) as u8;
        }
        Ok(())
    }

    fn reg(&self, r: Reg) -> u64 {
        let n = r.num() as usize;
        if r.is_high_byte() {
            return (self.regs[n - 4] >> 8) & 0xFF;
        }
        self.regs[n] & mask(r.size().bits())
    }

    fn set_reg(&mut self, r: Reg, v: u64) {
        let n = r.num() as usize;
        if r.is_high_byte() {
            let old = self.regs[n - 4];
            self.regs[n - 4] = (old & !0xFF00) | ((v & 0xFF) << 8);
            return;
        }
        match r.size() {
            Size::Qword => self.regs[n] = v,
            Size::Dword => self.regs[n] = v & 0xFFFF_FFFF,
            s => {
                let m = mask(s.bits());
                self.regs[n] = (self.regs[n] & !m) | (v & m);
            }
        }
    }

    fn addr(&self, m: &Mem, next_pc: usize) -> u64 {
        let mut a = i64::from(m.disp()) as u64;
        if m.is_rip_relative() {
            a = a.wrapping_add(CODE_BASE + next_pc as u64);
        }
        if let Some(b) = m.base() {
            a = a.wrapping_add(self.regs[b.num() as usize]);
        }
        if let Some(i) = m.index() {
            a = a.wrapping_add(
                self.regs[i.num() as usize].wrapping_mul(u64::from(m.scale().factor())),
            );
        }
        a
    }

    fn get(&self, op: &Operand, bits: u32, next_pc: usize) -> Result<u64, String> {
        Ok(match op {
            Operand::Reg(r) => self.reg(*r),
            Operand::Mem(m) => self.read(self.addr(m, next_pc), m.size().bytes())?,
            Operand::Imm(v) => (*v as u64) & mask(bits),
            other => return Err(format!("cannot read {other:?}")),
        })
    }

    fn put(&mut self, op: &Operand, v: u64, next_pc: usize) -> Result<(), String> {
        match op {
            Operand::Reg(r) => {
                self.set_reg(*r, v);
                Ok(())
            }
            Operand::Mem(m) => {
                let a = self.addr(m, next_pc);
                self.write(a, m.size().bytes(), v & mask(m.size().bits()))
            }
            other => Err(format!("cannot write {other:?}")),
        }
    }

    fn size_of(op: &Operand) -> u32 {
        match op {
            Operand::Reg(r) => r.size().bits(),
            Operand::Mem(m) => m.size().bits(),
            _ => 64,
        }
    }

    fn logic_flags(&mut self, r: u64, bits: u32) {
        self.flags = Flags {
            cf: false,
            of: false,
            zf: r & mask(bits) == 0,
            sf: sign(r, bits),
            pf: (r as u8).count_ones() % 2 == 0,
        };
    }

    fn arith(&mut self, a: u64, b: u64, carry: bool, sub: bool, bits: u32) -> u64 {
        let m = mask(bits);
        let (a, b) = (a & m, b & m);
        let c = u64::from(carry);
        let r = if sub {
            a.wrapping_sub(b).wrapping_sub(c)
        } else {
            a.wrapping_add(b).wrapping_add(c)
        } & m;
        let wide_a = u128::from(a);
        let wide_b = u128::from(b) + u128::from(c);
        let cf = if sub {
            wide_a < wide_b
        } else {
            wide_a + wide_b > u128::from(m)
        };
        let (sa, sb, sr) = (sign(a, bits), sign(b, bits), sign(r, bits));
        let of = if sub {
            sa != sb && sr != sa
        } else {
            sa == sb && sr != sa
        };
        self.flags = Flags {
            cf,
            of,
            zf: r == 0,
            sf: sr,
            pf: (r as u8).count_ones() % 2 == 0,
        };
        r
    }

    fn cond(&self, c: Cond) -> bool {
        let f = self.flags;
        let base = match c.code() >> 1 {
            0 => f.of,
            1 => f.cf,
            2 => f.zf,
            3 => f.cf || f.zf,
            4 => f.sf,
            5 => f.pf,
            6 => f.sf != f.of,
            _ => f.zf || f.sf != f.of,
        };
        base ^ (c.code() & 1 == 1)
    }

    fn push(&mut self, v: u64) -> Result<(), String> {
        self.regs[4] -= 8;
        self.write(self.regs[4], 8, v)
    }

    fn pop(&mut self) -> Result<u64, String> {
        let v = self.read(self.regs[4], 8)?;
        self.regs[4] += 8;
        Ok(v)
    }

    /// Runs `code` from `entry` until it stops.
    pub fn run(&mut self, code: &[u8], entry: usize) -> Result<Stop, String> {
        self.pc = entry;
        loop {
            self.steps += 1;
            if self.steps > self.max_steps {
                return Err("step limit".into());
            }
            let at = code.get(self.pc..).ok_or("pc out of code")?;
            if at.starts_with(&[0x0F, 0x05]) {
                if self.regs[0] == 60 {
                    return Ok(Stop::Exit(self.regs[7]));
                }
                return Err(format!("unsupported syscall {}", self.regs[0]));
            }
            let d = decode(at).map_err(|e| format!("decode at {:#x}: {e}", self.pc))?;
            let next = self.pc + d.len();
            if let Some(stop) = self.exec(&d.inst(), next)? {
                return Ok(stop);
            }
        }
    }

    fn exec(&mut self, i: &Inst, next: usize) -> Result<Option<Stop>, String> {
        let ops = i.operands();
        let bits = ops.first().map_or(64, Self::size_of);
        let at = self.pc;
        self.pc = next;
        match i.mnemonic() {
            M::Mov => {
                let v = self.get(&ops[1], bits, next)?;
                self.put(&ops[0], v, next)?;
            }
            M::Movzx => {
                let v = self.get(&ops[1], Self::size_of(&ops[1]), next)?;
                self.put(&ops[0], v, next)?;
            }
            M::Movsx | M::Movsxd => {
                let sb = Self::size_of(&ops[1]);
                let v = sext(self.get(&ops[1], sb, next)?, sb) as u64;
                self.put(&ops[0], v & mask(bits), next)?;
            }
            M::Lea => match &ops[1] {
                Operand::Mem(m) => {
                    let a = self.addr(m, next);
                    self.put(&ops[0], a & mask(bits), next)?;
                }
                _ => return Err("lea without memory".into()),
            },
            m @ (M::Add | M::Sub | M::Adc | M::Sbb | M::Cmp) => {
                let a = self.get(&ops[0], bits, next)?;
                let b = self.get(&ops[1], bits, next)?;
                let carry = matches!(m, M::Adc | M::Sbb) && self.flags.cf;
                let sub = matches!(m, M::Sub | M::Sbb | M::Cmp);
                let r = self.arith(a, b, carry, sub, bits);
                if m != M::Cmp {
                    self.put(&ops[0], r, next)?;
                }
            }
            m @ (M::And | M::Or | M::Xor | M::Test) => {
                let a = self.get(&ops[0], bits, next)?;
                let b = self.get(&ops[1], bits, next)?;
                let r = match m {
                    M::Or => a | b,
                    M::Xor => a ^ b,
                    _ => a & b,
                } & mask(bits);
                self.logic_flags(r, bits);
                if m != M::Test {
                    self.put(&ops[0], r, next)?;
                }
            }
            m @ (M::Inc | M::Dec) => {
                let a = self.get(&ops[0], bits, next)?;
                let cf = self.flags.cf;
                let r = self.arith(a, 1, false, m == M::Dec, bits);
                self.flags.cf = cf;
                self.put(&ops[0], r, next)?;
            }
            M::Neg => {
                let a = self.get(&ops[0], bits, next)?;
                let r = self.arith(0, a, false, true, bits);
                self.flags.cf = a & mask(bits) != 0;
                self.put(&ops[0], r, next)?;
            }
            M::Not => {
                let a = self.get(&ops[0], bits, next)?;
                self.put(&ops[0], !a & mask(bits), next)?;
            }
            M::Imul if ops.len() >= 2 => {
                let (a, b) = if ops.len() == 3 {
                    (
                        self.get(&ops[1], bits, next)?,
                        self.get(&ops[2], bits, next)?,
                    )
                } else {
                    (
                        self.get(&ops[0], bits, next)?,
                        self.get(&ops[1], bits, next)?,
                    )
                };
                let wide = i128::from(sext(a, bits)) * i128::from(sext(b, bits));
                let r = (wide as u64) & mask(bits);
                let over = i128::from(sext(r, bits)) != wide;
                self.flags.cf = over;
                self.flags.of = over;
                self.put(&ops[0], r, next)?;
            }
            m @ (M::Mul | M::Imul | M::Div | M::Idiv) => {
                let src = self.get(&ops[0], bits, next)?;
                let mk = mask(bits);
                let (ax, dx) = (self.regs[0] & mk, self.regs[2] & mk);
                let (lo, hi) = match m {
                    M::Mul => {
                        let w = u128::from(ax) * u128::from(src);
                        (w as u64 & mk, (w >> bits) as u64 & mk)
                    }
                    M::Imul => {
                        let w = i128::from(sext(ax, bits)) * i128::from(sext(src, bits));
                        (w as u64 & mk, (w >> bits) as u64 & mk)
                    }
                    M::Div => {
                        if src == 0 {
                            return Err("#DE".into());
                        }
                        let n = (u128::from(dx) << bits) | u128::from(ax);
                        let q = n / u128::from(src);
                        if q > u128::from(mk) {
                            return Err("#DE".into());
                        }
                        (q as u64, (n % u128::from(src)) as u64)
                    }
                    _ => {
                        let s = i128::from(sext(src, bits));
                        if s == 0 {
                            return Err("#DE".into());
                        }
                        let n = (i128::from(sext(dx, bits)) << bits) | i128::from(ax);
                        let q = n / s;
                        if q > i128::from(sext(mk >> 1, bits)) || q < -(1i128 << (bits - 1)) {
                            return Err("#DE".into());
                        }
                        (q as u64 & mk, (n % s) as u64 & mk)
                    }
                };
                let (rax, rdx) = if bits == 64 {
                    (Reg::RAX, Reg::RDX)
                } else {
                    (Reg::EAX, Reg::EDX)
                };
                self.set_reg(rax, lo);
                self.set_reg(rdx, hi);
            }
            M::Cqo => self.regs[2] = if sign(self.regs[0], 64) { u64::MAX } else { 0 },
            M::Cdq => self.set_reg(Reg::EDX, if sign(self.regs[0], 32) { u64::MAX } else { 0 }),
            M::Cdqe => self.regs[0] = sext(self.regs[0], 32) as u64,
            m @ (M::Shl | M::Shr | M::Sar | M::Rol | M::Ror) => {
                let a = self.get(&ops[0], bits, next)?;
                let count = self.get(&ops[1], 8, next)? & if bits == 64 { 63 } else { 31 };
                if count != 0 {
                    let c = count as u32;
                    let mk = mask(bits);
                    let r = match m {
                        M::Shl => (a << c) & mk,
                        M::Shr => (a & mk) >> c,
                        M::Sar => (sext(a, bits) >> c) as u64 & mk,
                        M::Rol => {
                            ((a << (c % bits)) | ((a & mk) >> ((bits - c % bits) % bits))) & mk
                        }
                        _ => (((a & mk) >> (c % bits)) | (a << ((bits - c % bits) % bits))) & mk,
                    };
                    self.flags.zf = r == 0;
                    self.flags.sf = sign(r, bits);
                    self.put(&ops[0], r, next)?;
                }
            }
            M::Bt => {
                let a = self.get(&ops[0], bits, next)?;
                let b = self.get(&ops[1], bits, next)? % u64::from(bits);
                self.flags.cf = (a >> b) & 1 == 1;
            }
            M::Setcc(c) => {
                let v = u64::from(self.cond(c));
                self.put(&ops[0], v, next)?;
            }
            M::Cmovcc(c) => {
                // A 32-bit cmov zero-extends its destination even when the
                // condition is false.
                let v = if self.cond(c) {
                    self.get(&ops[1], bits, next)?
                } else {
                    self.get(&ops[0], bits, next)?
                };
                self.put(&ops[0], v, next)?;
            }
            M::Jcc(c) => {
                if let Operand::Rel(d) = ops[0] {
                    if self.cond(c) {
                        self.pc = (at as i64 + d) as usize;
                    }
                }
            }
            M::Jmp => match ops[0] {
                Operand::Rel(d) => self.pc = (at as i64 + d) as usize,
                ref other => self.pc = (self.get(other, 64, next)? - CODE_BASE) as usize,
            },
            M::Call => {
                if self.regs[4] % 16 != 0 {
                    return Err(format!(
                        "misaligned stack at call {at:#x}: rsp = {:#x}",
                        self.regs[4]
                    ));
                }
                let target = match ops[0] {
                    Operand::Rel(d) => (at as i64 + d) as usize,
                    ref other => (self.get(other, 64, next)? - CODE_BASE) as usize,
                };
                self.push(CODE_BASE + next as u64)?;
                self.pc = target;
            }
            M::Ret => {
                let r = self.pop()?;
                if r == SENTINEL {
                    return Ok(Some(Stop::Returned));
                }
                self.pc = (r - CODE_BASE) as usize;
            }
            M::Push => {
                let v = self.get(&ops[0], 64, next)?;
                self.push(v)?;
            }
            M::Pop => {
                let v = self.pop()?;
                self.put(&ops[0], v, next)?;
            }
            M::Xchg => {
                let a = self.get(&ops[0], bits, next)?;
                let b = self.get(&ops[1], bits, next)?;
                self.put(&ops[0], b, next)?;
                self.put(&ops[1], a, next)?;
            }
            M::Nop => {}
            M::Ud2 => return Ok(Some(Stop::Ud2(at))),
            M::Int3 => return Ok(Some(Stop::Breakpoint)),
            other => return Err(format!("unsupported instruction {other}")),
        }
        Ok(None)
    }
}

/// Runs `code` from offset 0 the way the kernel starts a static program:
/// the stack pointer 16-aligned, no return address.
pub fn run_process(code: &[u8]) -> Result<Stop, String> {
    let mut m = Machine::new();
    m.regs[4] = STACK_BASE + STACK_SIZE as u64 - 64;
    m.run(code, 0)
}

/// Runs `code` from `entry` on a fresh machine with the given registers.
pub fn run(code: &[u8], entry: usize, init: &[(usize, u64)]) -> (Machine, Result<Stop, String>) {
    let mut m = Machine::new();
    for &(r, v) in init {
        m.regs[r] = v;
    }
    let stop = m.run(code, entry);
    (m, stop)
}
