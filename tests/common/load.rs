//! Loading a compiled module into the test machine and calling its functions
//! under SysV or Win64, as a linker and a caller would: code at `CODE_BASE`,
//! data at `DATA_BASE`, every relocation patched, imports bound to host
//! functions.

use std::collections::BTreeMap;

use backend_lang::Compiled;
use backend_lang::x86_64::RelocKind;

use super::{
    CODE_BASE, DATA_BASE, HEAP_BASE, HOST_BASE, Host, Machine, Region, SENTINEL, STACK_BASE,
    STACK_SIZE, Stop,
};

/// A linked image.
pub struct Image {
    pub code: Vec<u8>,
    pub data: Vec<u8>,
    pub symbols: BTreeMap<String, u64>,
    /// Imports by host-function index.
    pub hosts: Vec<String>,
}

/// Lays out code and data and applies every relocation.
pub fn load(c: &Compiled) -> Result<Image, String> {
    let mut symbols = BTreeMap::new();
    for f in c.functions() {
        let _ = symbols.insert(f.name().to_string(), CODE_BASE + f.offset() as u64);
    }
    let mut data: Vec<u8> = Vec::new();
    let mut placed = Vec::new();
    for d in c.data() {
        let align = u64::from(d.align()).max(1) as usize;
        while data.len() % align != 0 {
            data.push(0);
        }
        let at = data.len();
        match d.bytes() {
            Some(b) => data.extend_from_slice(b),
            None => data.resize(at + d.size() as usize, 0),
        }
        let _ = symbols.insert(d.name().to_string(), DATA_BASE + at as u64);
        placed.push(at);
    }
    let mut hosts = Vec::new();
    for (k, name) in c.imports().iter().enumerate() {
        let _ = symbols.insert(name.clone(), HOST_BASE + 16 * k as u64);
        hosts.push(name.clone());
    }
    let mut code = c.code().to_vec();
    for r in c.relocations() {
        let s = *symbols
            .get(&r.symbol)
            .ok_or_else(|| format!("unresolved `{}`", r.symbol))?;
        let p = CODE_BASE + r.offset as u64;
        match r.kind {
            RelocKind::Call32 | RelocKind::Pc32 => {
                let v = (s as i64).wrapping_add(r.addend).wrapping_sub(p as i64);
                let v = i32::try_from(v).map_err(|_| format!("`{}` out of reach", r.symbol))?;
                code[r.offset..r.offset + 4].copy_from_slice(&v.to_le_bytes());
            }
            RelocKind::Abs64 => {
                let v = (s as i64).wrapping_add(r.addend) as u64;
                code[r.offset..r.offset + 8].copy_from_slice(&v.to_le_bytes());
            }
            other => return Err(format!("relocation kind {other:?}")),
        }
    }
    for (d, &at) in c.data().iter().zip(&placed) {
        for r in d.relocations() {
            let s = *symbols
                .get(&r.symbol)
                .ok_or_else(|| format!("unresolved `{}`", r.symbol))?;
            let v = (s as i64).wrapping_add(r.addend) as u64;
            let o = at + r.offset;
            data[o..o + 8].copy_from_slice(&v.to_le_bytes());
        }
    }
    Ok(Image {
        code,
        data,
        symbols,
        hosts,
    })
}

/// A calling convention, for the harness side of a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conv {
    SysV,
    Win64,
}

impl Conv {
    pub fn args(self) -> &'static [usize] {
        match self {
            Conv::SysV => &[7, 6, 2, 1, 8, 9],
            Conv::Win64 => &[1, 2, 8, 9],
        }
    }

    pub fn callee_saved(self) -> &'static [usize] {
        match self {
            Conv::SysV => &[3, 5, 12, 13, 14, 15],
            Conv::Win64 => &[3, 5, 6, 7, 12, 13, 14, 15],
        }
    }

    pub fn caller_saved(self) -> &'static [usize] {
        match self {
            Conv::SysV => &[0, 1, 2, 6, 7, 8, 9, 10, 11],
            Conv::Win64 => &[0, 1, 2, 8, 9, 10, 11],
        }
    }

    fn shadow(self) -> u64 {
        match self {
            Conv::SysV => 0,
            Conv::Win64 => 32,
        }
    }
}

/// The result of a call.
pub struct Called {
    pub machine: Machine,
    pub stop: Result<Stop, String>,
}

/// A machine with the image mapped: code (readable, for jump tables), data,
/// and a heap.
pub fn machine(img: &Image) -> Machine {
    let mut m = Machine::new();
    m.regions.push(Region {
        base: CODE_BASE,
        bytes: img.code.clone(),
    });
    m.regions.push(Region {
        base: DATA_BASE,
        bytes: img.data.clone(),
    });
    m.regions.push(Region {
        base: HEAP_BASE,
        bytes: vec![0; 1 << 20],
    });
    m
}

/// A pseudo-random value for registers that carry nothing in.
fn junk(seed: u64, i: usize) -> u64 {
    let mut x = seed ^ (0x9E37_79B9_7F4A_7C15u64.wrapping_mul(i as u64 + 1));
    x ^= x >> 31;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^ (x >> 29)
}

/// Calls `func` with `args` (already in the bit patterns the callee sees,
/// upper bits included), every other register holding junk. After the call
/// returns, the convention's callee-saved registers must hold what they held
/// before; a difference is reported as an error.
pub fn call(
    img: &Image,
    m: &mut Machine,
    conv: Conv,
    func: &str,
    args: &[u64],
    host: &mut dyn Host,
    seed: u64,
) -> Result<Stop, String> {
    let entry = *img
        .symbols
        .get(func)
        .ok_or_else(|| format!("no function `{func}`"))?;
    for i in 0..16 {
        m.regs[i] = junk(seed, i);
    }
    let regs = conv.args();
    let nstack = args.len().saturating_sub(regs.len());
    // The stack at the call: 16-aligned, shadow space and stack arguments
    // above the return address.
    let top = (STACK_BASE + STACK_SIZE as u64 - 4096) & !15;
    let base = top - ((conv.shadow() + 8 * nstack as u64 + 15) & !15);
    for (k, &a) in args.iter().enumerate() {
        if let Some(&r) = regs.get(k) {
            m.regs[r] = a;
        } else {
            let at = base + conv.shadow() + 8 * (k - regs.len()) as u64;
            m.write(at, 8, a)?;
        }
    }
    m.regs[4] = base - 8;
    m.write(base - 8, 8, SENTINEL)?;
    let saved: Vec<(usize, u64)> = conv
        .callee_saved()
        .iter()
        .map(|&r| (r, m.regs[r]))
        .collect();
    let stop = m.run_with(&img.code, (entry - CODE_BASE) as usize, host)?;
    if stop == Stop::Returned {
        for (r, v) in saved {
            if m.regs[r] != v {
                return Err(format!("callee-saved register {r} not preserved"));
            }
        }
        if m.regs[4] != base {
            return Err(format!(
                "rsp {:#x} after return, expected {base:#x}",
                m.regs[4]
            ));
        }
    }
    Ok(stop)
}

/// A host function bound to an import name.
pub type Handler<'a> = Box<dyn FnMut(&mut Machine, Conv) -> Result<Option<Stop>, String> + 'a>;

/// Host functions by import name. The trap handler stops the run with its
/// code; other names call the given closures.
pub struct Hosts<'a> {
    pub conv: Conv,
    pub names: Vec<String>,
    pub handlers: BTreeMap<String, Handler<'a>>,
}

impl<'a> Hosts<'a> {
    pub fn new(img: &Image, conv: Conv) -> Hosts<'a> {
        Hosts {
            conv,
            names: img.hosts.clone(),
            handlers: BTreeMap::new(),
        }
    }

    pub fn on(
        &mut self,
        name: &str,
        f: impl FnMut(&mut Machine, Conv) -> Result<Option<Stop>, String> + 'a,
    ) {
        let _ = self.handlers.insert(name.to_string(), Box::new(f));
    }
}

impl Host for Hosts<'_> {
    fn call(&mut self, index: usize, m: &mut Machine) -> Result<Option<Stop>, String> {
        let name = self
            .names
            .get(index)
            .ok_or_else(|| format!("no host function {index}"))?
            .clone();
        if name == "__lexersketch_trap" {
            let code = m.regs[self.conv.args()[0]] & 0xFFFF_FFFF;
            return Ok(Some(Stop::Trap(code)));
        }
        if name == "__lexersketch_pow_f64" || name == "__lexersketch_pow_f32" {
            // The runtime's float pow: ir-lang's `ls_pow` on the bit
            // patterns. Every caller-saved register is left holding junk, as
            // a real callee may leave it.
            let (x, y) = (m.regs[self.conv.args()[0]], m.regs[self.conv.args()[1]]);
            let r = if name.ends_with("f64") {
                pow_f64_bits(x, y)
            } else {
                u64::from(pow_f32_bits(x as u32, y as u32)) | 0xDEAD_0000_0000_0000
            };
            for &k in self.conv.caller_saved() {
                m.regs[k] = 0x5A5A_0000_0000_0000 | (k as u64);
            }
            m.regs[0] = r;
            return Ok(None);
        }
        match self.handlers.get_mut(&name) {
            Some(f) => f(m, self.conv),
            None => Err(format!("call to unbound import `{name}`")),
        }
    }
}

/// The runtime's `f64` pow on bit patterns: ir-lang's `ls_pow`.
pub fn pow_f64_bits(x: u64, y: u64) -> u64 {
    ir_lang::ls_pow(f64::from_bits(x), f64::from_bits(y)).to_bits()
}

/// The runtime's `f32` pow on bit patterns: `ls_pow` of the widened operands,
/// rounded once.
pub fn pow_f32_bits(x: u32, y: u32) -> u32 {
    (ir_lang::ls_pow(f64::from(f32::from_bits(x)), f64::from(f32::from_bits(y))) as f32).to_bits()
}
