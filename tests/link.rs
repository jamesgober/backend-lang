//! Compiled modules into object files with object-lang 0.2: an ELF
//! relocatable carrying the code's and data's relocations exactly as
//! backend-lang reports them (read back and compared), and a static
//! executable linked here (the relocations applied at fixed addresses) that
//! runs as a Linux process with its result as the exit status. Everywhere,
//! the same module also runs in the test interpreter.

#![allow(clippy::unwrap_used, reason = "test failures should be loud")]

mod common;

use backend_lang::Options;
use backend_lang::x86_64::RelocKind;
use common::Stop;
use common::load::{self, Conv, Hosts};
use ir_lang::interp::{self, Outcome};
use ir_lang::{
    Call, CmpOp, Global, Linkage, MemFlags, MemType, Module, Overflow, Reloc as IrReloc, Signature,
    SwitchCase, SymbolRef, Type,
};
use isa_lang::x86_64::{Assembler, Inst, Mnemonic as M, Operand as O, Reg, Size};
use object_lang::{
    Architecture, Binding, FileKind, Object, Relocation, RelocationTarget, Section, SectionKind,
    Symbol, SymbolSection, X86_64Reloc,
};

/// `main() -> i32`: a loop over a table of function pointers in a global
/// (data relocations), indirect calls, a switch, a checked add, and a
/// mutable counter global; returns a value below 256. With `trap`, it ends
/// in a trap instead.
fn program(trap: bool) -> Module {
    let mut m = Module::new("prog");
    let sig = Signature::new(&[Type::I32], &[Type::I32]);
    let twice = m.declare_function("twice", &sig, Linkage::Local).unwrap();
    let plus3 = m.declare_function("plus3", &sig, Linkage::Local).unwrap();
    let main = m
        .declare_function("main", &Signature::new(&[], &[Type::I32]), Linkage::Export)
        .unwrap();
    let sig_id = m.intern_signature(&sig).unwrap();
    let table = m
        .declare_global(
            "table",
            Global::zeroed(16, 8)
                .with_reloc(IrReloc {
                    offset: 0,
                    target: SymbolRef::Func(twice),
                    addend: 0,
                })
                .with_reloc(IrReloc {
                    offset: 8,
                    target: SymbolRef::Func(plus3),
                    addend: 0,
                }),
        )
        .unwrap();
    let counter = m
        .declare_global("counter", Global::bytes(&[5, 0, 0, 0], 4).mutable())
        .unwrap();
    for (f, double) in [(twice, true), (plus3, false)] {
        let mut b = m.build(f).unwrap();
        let x = b.param(0).unwrap();
        let r = if double {
            b.add(x, x, Overflow::Wrap).unwrap()
        } else {
            let k = b.iconst(Type::I32, 3).unwrap();
            b.add(x, k, Overflow::Wrap).unwrap()
        };
        b.ret(&[r]).unwrap();
    }
    let mut b = m.build(main).unwrap();
    let head = b.create_block(&[Type::I64, Type::I32]).unwrap();
    let body = b.create_block(&[]).unwrap();
    let exit = b.create_block(&[Type::I32]).unwrap();
    let (z64, one32) = (
        b.iconst(Type::I64, 0).unwrap(),
        b.iconst(Type::I32, 1).unwrap(),
    );
    b.jump(head, &[z64, one32]).unwrap();
    b.switch_to(head).unwrap();
    let (i, acc) = (
        b.block_param(head, 0).unwrap(),
        b.block_param(head, 1).unwrap(),
    );
    let n = b.iconst(Type::I64, 6).unwrap();
    let more = b.compare(CmpOp::Lt, i, n).unwrap();
    b.branch(more, body, &[], exit, &[acc]).unwrap();
    b.switch_to(body).unwrap();
    let tab = b.global_addr(table).unwrap();
    let one64 = b.iconst(Type::I64, 1).unwrap();
    let which = b
        .binary(ir_lang::BinaryOp::And, ir_lang::Policy::NONE, i, one64)
        .unwrap();
    let slot = b.elem_addr(tab, MemType::Scalar(Type::Ptr), which).unwrap();
    let fp = b.load(Type::Ptr, slot, MemFlags::new(8)).unwrap();
    let c = b.call(Call::indirect(sig_id, fp, &[acc])).unwrap();
    let r = b.results(c).first().unwrap();
    let i2 = b.add(i, one64, Overflow::Wrap).unwrap();
    b.jump(head, &[i2, r]).unwrap();
    b.switch_to(exit).unwrap();
    let v = b.block_param(exit, 0).unwrap();
    let cnt = b.global_addr(counter).unwrap();
    let old = b.load(Type::I32, cnt, MemFlags::new(4)).unwrap();
    let new = b.add(old, v, Overflow::Wrap).unwrap();
    let _ = b.store(new, cnt, MemFlags::new(4)).unwrap();
    let back = b.load(Type::I32, cnt, MemFlags::new(4)).unwrap();
    let a = b.create_block(&[]).unwrap();
    let bb = b.create_block(&[]).unwrap();
    let dflt = b.create_block(&[]).unwrap();
    b.switch(
        back,
        vec![
            SwitchCase {
                value: 30,
                dest: ir_lang::BlockCall::new(a, &[]),
            },
            SwitchCase {
                value: 81,
                dest: ir_lang::BlockCall::new(bb, &[]),
            },
        ],
        ir_lang::BlockCall::new(dflt, &[]),
    )
    .unwrap();
    for (blk, k) in [(a, 1), (bb, 2), (dflt, 3)] {
        b.switch_to(blk).unwrap();
        if trap {
            b.trap(9).unwrap();
            continue;
        }
        let kk = b.iconst(Type::I32, k).unwrap();
        let s = b.add(back, kk, Overflow::Wrap).unwrap();
        b.ret(&[s]).unwrap();
    }
    m
}

fn expected(m: &Module) -> Outcome {
    let main = m
        .functions()
        .find(|&f| m.func_decl(f).unwrap().name() == "main")
        .unwrap();
    interp::run(m, main, &[]).unwrap()
}

#[test]
fn test_relocatable_object_carries_every_relocation() {
    let m = program(false);
    let c = backend_lang::compile(&m, &Options::sysv()).unwrap();
    assert!(c.relocations().iter().any(|r| r.kind == RelocKind::Pc32));
    let mut obj = Object::relocatable(Architecture::X86_64);
    let text = obj.add_section(
        Section::new(".text", SectionKind::Text)
            .with_align(16)
            .with_data(c.code().to_vec()),
    );
    let mut data_bytes = Vec::new();
    let mut offsets = Vec::new();
    for d in c.data() {
        while data_bytes.len() % d.align() as usize != 0 {
            data_bytes.push(0);
        }
        offsets.push(data_bytes.len() as u64);
        match d.bytes() {
            Some(b) => data_bytes.extend_from_slice(b),
            None => data_bytes.resize(data_bytes.len() + d.size() as usize, 0),
        }
    }
    let data = obj.add_section(
        Section::new(".data", SectionKind::Data)
            .with_align(8)
            .with_data(data_bytes),
    );
    for f in c.functions() {
        let binding = if c.exports().iter().any(|e| e == f.name()) {
            Binding::Global
        } else {
            Binding::Local
        };
        let _ = obj.add_symbol(
            Symbol::function(f.name(), text, f.offset() as u64, f.size() as u64)
                .with_binding(binding),
        );
    }
    for (d, &o) in c.data().iter().zip(&offsets) {
        let _ =
            obj.add_symbol(Symbol::data(d.name(), data, o, d.size()).with_binding(Binding::Global));
    }
    for name in c.imports() {
        let _ = obj.add_symbol(Symbol::undefined(name.clone()));
    }
    let kind = |k: RelocKind| match k {
        RelocKind::Call32 => X86_64Reloc::Plt32,
        RelocKind::Pc32 => X86_64Reloc::Pc32,
        _ => X86_64Reloc::Abs64,
    };
    for r in c.relocations() {
        let s = obj.symbol_by_name(&r.symbol).unwrap();
        obj.add_relocation(
            text,
            Relocation::new(
                r.offset as u64,
                kind(r.kind),
                RelocationTarget::Symbol(s),
                r.addend,
            ),
        )
        .unwrap();
    }
    for (d, &o) in c.data().iter().zip(&offsets) {
        for r in d.relocations() {
            let s = obj.symbol_by_name(&r.symbol).unwrap();
            obj.add_relocation(
                data,
                Relocation::new(
                    o + r.offset as u64,
                    kind(r.kind),
                    RelocationTarget::Symbol(s),
                    r.addend,
                ),
            )
            .unwrap();
        }
    }
    let bytes = object_lang::elf::write(&obj).unwrap();
    let back = object_lang::elf::read(&bytes).unwrap();
    assert_eq!(back.kind(), FileKind::Relocatable);
    let t = back.section_by_name(".text").unwrap();
    let ts = back.section(t).unwrap();
    assert_eq!(ts.data(), c.code());
    let mut got: Vec<(u64, String, i64)> = ts
        .relocations()
        .iter()
        .map(|r| match r.target {
            RelocationTarget::Symbol(s) => {
                (r.offset, back.symbol(s).unwrap().name.clone(), r.addend)
            }
            _ => panic!(),
        })
        .collect();
    let mut want: Vec<(u64, String, i64)> = c
        .relocations()
        .iter()
        .map(|r| (r.offset as u64, r.symbol.clone(), r.addend))
        .collect();
    got.sort();
    want.sort();
    assert_eq!(got, want);
    let ds = back
        .section(back.section_by_name(".data").unwrap())
        .unwrap();
    assert_eq!(
        ds.relocations().len(),
        2,
        "the table's two function pointers"
    );
    let main = back.symbol(back.symbol_by_name("main").unwrap()).unwrap();
    assert_eq!(main.binding, Binding::Global);
    assert!(matches!(main.section, SymbolSection::Section(_)));
}

/// Links the module statically: code at the ELF code address, then a
/// `_start` stub (`call main; exit(eax)`) and a trap handler
/// (`exit(100 + code)`), data on the next page; every relocation applied.
fn executable(m: &Module) -> Vec<u8> {
    let c = backend_lang::compile(m, &Options::sysv()).unwrap();
    let text_at = object_lang::elf::code_address(Architecture::X86_64);
    let code_len = c.code().len().div_ceil(16) * 16;
    // The stubs.
    let mut asm = Assembler::new();
    let r = |n| Reg::new(Size::Qword, n).unwrap();
    let r32 = |n| Reg::new(Size::Dword, n).unwrap();
    let main_at = text_at + c.function("main").unwrap().offset() as u64;
    let start_at = text_at + code_len as u64;
    // call rel32 to main, hand-patched below.
    asm.bytes(&[0xE8, 0, 0, 0, 0]).unwrap();
    asm.emit(&Inst::new(M::Mov, [O::Reg(r32(7)), O::Reg(r32(0))]))
        .unwrap();
    asm.emit(&Inst::new(M::Mov, [O::Reg(r32(0)), O::Imm(60)]))
        .unwrap();
    asm.bytes(&[0x0F, 0x05]).unwrap();
    let trap_off = asm.label();
    asm.bind(trap_off).unwrap();
    asm.emit(&Inst::new(M::Add, [O::Reg(r32(7)), O::Imm(100)]))
        .unwrap();
    asm.emit(&Inst::new(M::Mov, [O::Reg(r32(0)), O::Imm(60)]))
        .unwrap();
    asm.bytes(&[0x0F, 0x05]).unwrap();
    let _ = r;
    let stub = asm.finish().unwrap();
    let trap_at = start_at + stub.label_offset(trap_off).unwrap() as u64;
    let mut text = c.code().to_vec();
    text.resize(code_len, 0xCC);
    let call_disp = (main_at as i64 - (start_at as i64 + 5)) as i32;
    let mut stub_bytes = stub.into_bytes();
    stub_bytes[1..5].copy_from_slice(&call_disp.to_le_bytes());
    text.extend_from_slice(&stub_bytes);
    let data_at = (text_at + text.len() as u64).div_ceil(4096) * 4096 + 4096;
    let mut symbols = std::collections::BTreeMap::new();
    for f in c.functions() {
        let _ = symbols.insert(f.name().to_string(), text_at + f.offset() as u64);
    }
    let _ = symbols.insert("__lexersketch_trap".to_string(), trap_at);
    let mut data = Vec::new();
    let mut placed = Vec::new();
    for d in c.data() {
        while data.len() % d.align() as usize != 0 {
            data.push(0);
        }
        placed.push(data.len());
        let _ = symbols.insert(d.name().to_string(), data_at + data.len() as u64);
        match d.bytes() {
            Some(b) => data.extend_from_slice(b),
            None => data.resize(data.len() + d.size() as usize, 0),
        }
    }
    for rel in c.relocations() {
        let s = symbols[&rel.symbol];
        let p = text_at + rel.offset as u64;
        let v = (s as i64 + rel.addend - p as i64) as i32;
        text[rel.offset..rel.offset + 4].copy_from_slice(&v.to_le_bytes());
    }
    for (d, &o) in c.data().iter().zip(&placed) {
        for rel in d.relocations() {
            let v = (symbols[&rel.symbol] as i64 + rel.addend) as u64;
            data[o + rel.offset..o + rel.offset + 8].copy_from_slice(&v.to_le_bytes());
        }
    }
    let mut obj = Object::executable(Architecture::X86_64);
    let t = obj.add_section(
        Section::new(".text", SectionKind::Text)
            .with_align(16)
            .with_address(text_at)
            .with_data(text),
    );
    let _ = obj.add_section(
        Section::new(".data", SectionKind::Data)
            .with_align(8)
            .with_address(data_at)
            .with_data(data),
    );
    let _ = obj.add_symbol(Symbol::function(
        "_start",
        t,
        start_at,
        stub_bytes.len() as u64,
    ));
    obj.set_entry(start_at);
    let bytes = object_lang::elf::write(&obj).unwrap();
    let back = object_lang::elf::read(&bytes).unwrap();
    assert_eq!(back.kind(), FileKind::Executable);
    assert_eq!(back.entry(), start_at);
    bytes
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn run_process(bytes: &[u8], name: &str) -> i32 {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("backend-lang-link-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A sibling test thread forking while this file is open for writing
    // makes exec fail with ETXTBSY until that child execs; retry.
    const ETXTBSY: i32 = 26;
    let mut attempts = 0;
    let out = loop {
        match std::process::Command::new(&path).output() {
            Err(e) if e.raw_os_error() == Some(ETXTBSY) && attempts < 100 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            r => break r.unwrap(),
        }
    };
    let _ = std::fs::remove_file(&path);
    out.status.code().unwrap()
}

#[test]
fn test_static_executable_runs_with_its_result_as_exit_status() {
    for trap in [false, true] {
        let m = program(trap);
        let want = match expected(&m) {
            Outcome::Return(v) => v[0].bits() & 0xFF,
            Outcome::Trap(code) => 100 + u64::from(code),
            other => panic!("{other:?}"),
        };
        // The same module in the test interpreter.
        let c = backend_lang::compile(&m, &Options::sysv()).unwrap();
        let img = load::load(&c).unwrap();
        let mut mach = load::machine(&img);
        let mut hosts = Hosts::new(&img, Conv::SysV);
        let got = match load::call(&img, &mut mach, Conv::SysV, "main", &[], &mut hosts, 1).unwrap()
        {
            Stop::Returned => mach.regs[0] & 0xFF,
            Stop::Trap(code) => 100 + code,
            other => panic!("{other:?}"),
        };
        assert_eq!(got, want, "interpreter, trap {trap}");
        let bytes = executable(&m);
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        {
            let status = run_process(&bytes, if trap { "trap" } else { "main" });
            assert_eq!(status as u64, want, "process, trap {trap}");
        }
        let _ = bytes;
    }
}
