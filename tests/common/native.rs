//! Running compiled code natively, in this process: executable memory from
//! the operating system (`mmap` on Linux, `VirtualAlloc` on Windows, declared
//! here directly: no crate), the module's code and data copied in and every
//! relocation patched, and a small entry thunk (assembled with isa-lang)
//! that calls a compiled function under SysV or Win64 with arguments from an
//! array. The runtime trap handler is a stub that records the code and
//! unwinds straight back to the thunk.
//!
//! x86-64 Linux and Windows only.

#![cfg(all(target_arch = "x86_64", any(target_os = "linux", windows)))]

use std::collections::BTreeMap;

use backend_lang::Compiled;
use backend_lang::x86_64::RelocKind;
use isa_lang::x86_64::{Assembler, Inst, Mem, Mnemonic as M, Operand as O, Reg, Size};

use super::load::Conv;

#[cfg(target_os = "linux")]
mod os {
    unsafe extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
        fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
        fn munmap(addr: *mut u8, len: usize) -> i32;
    }

    /// # Safety
    ///
    /// `len` must be nonzero.
    pub unsafe fn alloc(len: usize) -> *mut u8 {
        // SAFETY: an anonymous private read-write mapping; no file, no
        // aliasing. MAP_FAILED is -1.
        let p = unsafe { mmap(std::ptr::null_mut(), len, 3, 0x22, -1, 0) };
        if p as isize == -1 {
            std::ptr::null_mut()
        } else {
            p
        }
    }

    /// # Safety
    ///
    /// `p..p+len` must be a page-aligned range of a mapping from `alloc`.
    pub unsafe fn protect_exec(p: *mut u8, len: usize) -> bool {
        // SAFETY: per the caller's contract.
        unsafe { mprotect(p, len, 5) == 0 }
    }

    /// # Safety
    ///
    /// `p` and `len` must be a mapping from `alloc`, not used afterwards.
    pub unsafe fn free(p: *mut u8, len: usize) {
        // SAFETY: per the caller's contract.
        let _ = unsafe { munmap(p, len) };
    }
}

#[cfg(windows)]
mod os {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn VirtualAlloc(addr: *mut u8, size: usize, kind: u32, protect: u32) -> *mut u8;
        fn VirtualProtect(addr: *mut u8, size: usize, protect: u32, old: *mut u32) -> i32;
        fn VirtualFree(addr: *mut u8, size: usize, kind: u32) -> i32;
    }

    /// # Safety
    ///
    /// `len` must be nonzero.
    pub unsafe fn alloc(len: usize) -> *mut u8 {
        // SAFETY: commits fresh read-write pages (MEM_COMMIT | MEM_RESERVE,
        // PAGE_READWRITE).
        unsafe { VirtualAlloc(std::ptr::null_mut(), len, 0x3000, 0x04) }
    }

    /// # Safety
    ///
    /// `p..p+len` must be within an allocation from `alloc`.
    pub unsafe fn protect_exec(p: *mut u8, len: usize) -> bool {
        let mut old = 0u32;
        // SAFETY: per the caller's contract; PAGE_EXECUTE_READ.
        unsafe { VirtualProtect(p, len, 0x20, &mut old) != 0 }
    }

    /// # Safety
    ///
    /// `p` must come from `alloc` and not be used afterwards.
    pub unsafe fn free(p: *mut u8, _len: usize) {
        // SAFETY: per the caller's contract; MEM_RELEASE with size 0.
        let _ = unsafe { VirtualFree(p, 0, 0x8000) };
    }
}

const PAGE: usize = 4096;

fn r(n: u8) -> Reg {
    Reg::new(Size::Qword, n).unwrap()
}

fn r32(n: u8) -> Reg {
    Reg::new(Size::Dword, n).unwrap()
}

/// A compiled module loaded into executable memory.
pub struct Native {
    base: *mut u8,
    len: usize,
    symbols: BTreeMap<String, u64>,
    thunk: u64,
    cells: u64,
}

/// What a native call did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ran {
    /// Returned `rax`, `rdx`.
    Returned(u64, u64),
    /// Called the trap handler with this code.
    Trap(u64),
}

/// The most arguments a native call passes.
pub const MAX_ARGS: usize = 12;

impl Native {
    /// Loads `c`: [code | stubs and thunk] (made executable) then [data |
    /// cells] (read-write). Calls through the thunk use `conv`.
    pub fn load(c: &Compiled, conv: Conv) -> Result<Native, String> {
        let code_len = c.code().len().div_ceil(16) * 16;
        let stub_len = 1024;
        let text = (code_len + stub_len).div_ceil(PAGE) * PAGE;
        let data_len: usize = c
            .data()
            .iter()
            .map(|d| d.size() as usize + d.align() as usize)
            .sum::<usize>()
            + 64;
        let len = text + data_len.div_ceil(PAGE) * PAGE;
        // SAFETY: len is nonzero.
        let base = unsafe { os::alloc(len) };
        if base.is_null() {
            return Err("allocation failed".into());
        }
        let addr = base as u64;
        let mut image = vec![0u8; len];
        image[..c.code().len()].copy_from_slice(c.code());
        let mut symbols = BTreeMap::new();
        for f in c.functions() {
            let _ = symbols.insert(f.name().to_string(), addr + f.offset() as u64);
        }
        let mut at = text;
        let mut placed = Vec::new();
        for d in c.data() {
            let a = d.align().max(1) as usize;
            at = at.div_ceil(a) * a;
            if let Some(b) = d.bytes() {
                image[at..at + b.len()].copy_from_slice(b);
            }
            let _ = symbols.insert(d.name().to_string(), addr + at as u64);
            placed.push(at);
            at += d.size() as usize;
        }
        // Three cells: saved stack pointer, output pointer, trap code.
        at = at.div_ceil(8) * 8;
        let cells = addr + at as u64;

        // The thunk and the trap stub.
        let (stub_code, thunk_off, trap_off) = thunk(conv, cells);
        let stubs = code_len;
        image[stubs..stubs + stub_code.len()].copy_from_slice(&stub_code);
        let thunk = addr + (stubs + thunk_off) as u64;
        let trap = addr + (stubs + trap_off) as u64;
        for name in c.imports() {
            if name == "__lexersketch_trap" {
                let _ = symbols.insert(name.clone(), trap);
            } else {
                return Err(format!("no native binding for import `{name}`"));
            }
        }
        for rel in c.relocations() {
            let s = symbols[&rel.symbol];
            let p = addr + rel.offset as u64;
            match rel.kind {
                RelocKind::Call32 | RelocKind::Pc32 => {
                    let v =
                        i32::try_from((s as i64).wrapping_add(rel.addend).wrapping_sub(p as i64))
                            .map_err(|_| "out of reach".to_string())?;
                    image[rel.offset..rel.offset + 4].copy_from_slice(&v.to_le_bytes());
                }
                _ => return Err("relocation kind".into()),
            }
        }
        for (d, &o) in c.data().iter().zip(&placed) {
            for rel in d.relocations() {
                let v = (symbols[&rel.symbol] as i64).wrapping_add(rel.addend) as u64;
                image[o + rel.offset..o + rel.offset + 8].copy_from_slice(&v.to_le_bytes());
            }
        }
        // SAFETY: `base` is a fresh read-write allocation of `len` bytes and
        // `image` is exactly `len` bytes; then the first `text` bytes (whole
        // pages) become read-execute and the rest stays read-write.
        unsafe {
            std::ptr::copy_nonoverlapping(image.as_ptr(), base, len);
            if !os::protect_exec(base, text) {
                os::free(base, len);
                return Err("mprotect failed".into());
            }
        }
        Ok(Native {
            base,
            len,
            symbols,
            thunk,
            cells,
        })
    }

    /// Calls `func` with `args` (register images, upper bits included).
    pub fn call(&self, func: &str, args: &[u64]) -> Result<Ran, String> {
        let target = *self.symbols.get(func).ok_or("no such function")?;
        if args.len() > MAX_ARGS {
            return Err("too many arguments".into());
        }
        let mut a = [0u64; MAX_ARGS + 2];
        a[0] = target;
        a[1] = args.len() as u64;
        a[2..2 + args.len()].copy_from_slice(args);
        let mut out = [0u64; 2];
        // SAFETY: `thunk` is the entry of code assembled by `thunk()` in
        // this allocation, which stays executable for `self`'s life; it
        // follows the SysV convention, reads `a`, writes `out`, and restores
        // every callee-saved register and the stack pointer (also when the
        // compiled code traps: the stub reloads the saved stack pointer).
        let status = unsafe {
            let f: extern "sysv64" fn(*const u64, *mut u64) -> u64 =
                std::mem::transmute(self.thunk as usize);
            f(a.as_ptr(), out.as_mut_ptr())
        };
        if status == 0 {
            Ok(Ran::Returned(out[0], out[1]))
        } else {
            // SAFETY: the third cell is inside the read-write data pages.
            let code = unsafe { std::ptr::read_volatile((self.cells + 16) as *const u64) };
            Ok(Ran::Trap(code & 0xFFFF_FFFF))
        }
    }

    /// The address of a symbol (for benchmarks that call through a pointer).
    pub fn symbol(&self, name: &str) -> Option<u64> {
        self.symbols.get(name).copied()
    }
}

impl Drop for Native {
    fn drop(&mut self) {
        // SAFETY: `base` and `len` are this allocation, no longer used.
        unsafe { os::free(self.base, self.len) };
    }
}

/// The SysV thunk `(a: *const u64, out: *mut u64) -> status` where
/// `a = [target, n, args...]`, calling the target under `conv`, and the trap
/// stub. Returns the bytes and the two entry offsets.
fn thunk(conv: Conv, cells: u64) -> (Vec<u8>, usize, usize) {
    let mut asm = Assembler::new();
    let e = |asm: &mut Assembler, m: M, ops: &[O]| {
        asm.emit(&Inst::from_slice(m, ops).unwrap()).unwrap()
    };
    let entry = asm.label();
    let exit = asm.label();
    let trap = asm.label();
    asm.bind(entry).unwrap();
    // Save SysV callee-saved registers (and rdi/rsi, which Win64 code
    // preserves but SysV callers do not need back; saving them keeps the
    // stack arithmetic uniform).
    for n in [3u8, 5, 12, 13, 14, 15] {
        e(&mut asm, M::Push, &[O::Reg(r(n))]);
    }
    // cells[0] = rsp, cells[1] = out.
    e(&mut asm, M::Mov, &[O::Reg(r(11)), O::Imm(cells as i64)]);
    e(
        &mut asm,
        M::Mov,
        &[Mem::new(Size::Qword, r(11)).into(), O::Reg(r(4))],
    );
    e(
        &mut asm,
        M::Mov,
        &[
            Mem::new(Size::Qword, r(11)).with_disp(8).into(),
            O::Reg(r(6)),
        ],
    );
    // Stack space: shadow (Win64) plus up to MAX_ARGS stack arguments,
    // keeping rsp 16-aligned at the call (entry is 8 off, six pushes keep
    // it 8 off, so subtract 8 mod 16).
    let (regs, shadow): (&[u8], i64) = match conv {
        Conv::SysV => (&[7, 6, 2, 1, 8, 9], 0),
        Conv::Win64 => (&[1, 2, 8, 9], 32),
    };
    let space = shadow + 8 * MAX_ARGS as i64 + 8;
    e(&mut asm, M::Sub, &[O::Reg(r(4)), O::Imm(space)]);
    // r10 = a. Stack arguments are copied unconditionally (extra words are
    // harmless); register arguments loaded last.
    e(&mut asm, M::Mov, &[O::Reg(r(10)), O::Reg(r(7))]);
    for k in 0..MAX_ARGS - regs.len() {
        let src = Mem::new(Size::Qword, r(10)).with_disp((8 * (2 + regs.len() + k)) as i32);
        e(&mut asm, M::Mov, &[O::Reg(r(11)), src.into()]);
        let dst = Mem::new(Size::Qword, r(4)).with_disp((shadow + 8 * k as i64) as i32);
        e(&mut asm, M::Mov, &[dst.into(), O::Reg(r(11))]);
    }
    for (k, &reg) in regs.iter().enumerate() {
        let src = Mem::new(Size::Qword, r(10)).with_disp((8 * (2 + k)) as i32);
        e(&mut asm, M::Mov, &[O::Reg(r(reg)), src.into()]);
    }
    e(
        &mut asm,
        M::Mov,
        &[O::Reg(r(11)), Mem::new(Size::Qword, r(10)).into()],
    );
    e(&mut asm, M::Call, &[O::Reg(r(11))]);
    // out[0..2] = rax, rdx; status 0.
    e(&mut asm, M::Mov, &[O::Reg(r(11)), O::Imm(cells as i64)]);
    e(
        &mut asm,
        M::Mov,
        &[
            O::Reg(r(11)),
            Mem::new(Size::Qword, r(11)).with_disp(8).into(),
        ],
    );
    e(
        &mut asm,
        M::Mov,
        &[Mem::new(Size::Qword, r(11)).into(), O::Reg(r(0))],
    );
    e(
        &mut asm,
        M::Mov,
        &[
            Mem::new(Size::Qword, r(11)).with_disp(8).into(),
            O::Reg(r(2)),
        ],
    );
    e(&mut asm, M::Xor, &[O::Reg(r32(0)), O::Reg(r32(0))]);
    asm.bind(exit).unwrap();
    e(&mut asm, M::Mov, &[O::Reg(r(11)), O::Imm(cells as i64)]);
    e(
        &mut asm,
        M::Mov,
        &[O::Reg(r(4)), Mem::new(Size::Qword, r(11)).into()],
    );
    for n in [15u8, 14, 13, 12, 5, 3] {
        e(&mut asm, M::Pop, &[O::Reg(r(n))]);
    }
    e(&mut asm, M::Ret, &[]);
    // The trap handler: cells[2] = code (the first argument), status 1,
    // back through the thunk's exit with its stack pointer.
    asm.bind(trap).unwrap();
    e(&mut asm, M::Mov, &[O::Reg(r(11)), O::Imm(cells as i64)]);
    let arg = regs[0];
    e(
        &mut asm,
        M::Mov,
        &[
            Mem::new(Size::Qword, r(11)).with_disp(16).into(),
            O::Reg(r(arg)),
        ],
    );
    e(&mut asm, M::Mov, &[O::Reg(r32(0)), O::Imm(1)]);
    asm.jmp(exit).unwrap();
    let code = asm.finish().unwrap();
    let t = code.label_offset(entry).unwrap();
    let p = code.label_offset(trap).unwrap();
    (code.into_bytes(), t, p)
}
