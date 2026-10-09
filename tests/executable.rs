//! The whole chain: MIR built by hand → backend-lang emission → isa-lang
//! bytes → object-lang static ELF executable → a running Linux process whose
//! exit status is the program's result.
//!
//! On every platform the executable is read back by object-lang and the
//! code is run in the test interpreter. On Linux x86-64 the file is also
//! written out and executed, and its exit status checked. Nothing here uses
//! a system assembler, linker, or C library: the programs leave through the
//! `exit` system call.

#![allow(clippy::unwrap_used, reason = "test failures should be loud")]

mod common;
mod programs;

use backend_lang::x86_64::Emitter;
use object_lang::{Architecture, FileKind};

/// The kernel enters a static program with the stack 16-aligned and no
/// return address; a SysV function expects to be entered by a call. This
/// five-byte `call` (to the next instruction) gives the program's first
/// function the state it expects. The real program entry belongs to the
/// runtime library (rt-lang).
const ENTRY_STUB: [u8; 5] = [0xE8, 0, 0, 0, 0];

/// Emits functions, the first being the entry, behind the entry stub.
fn link(functions: &[backend_lang::mir::Function]) -> Vec<u8> {
    let mut e = Emitter::new();
    for f in functions {
        let _ = e.function(f).unwrap_or_else(|err| panic!("{err}\n{f}"));
    }
    let p = e.finish().unwrap();
    assert_eq!(p.symbol(functions[0].name()), Some(0));
    let mut code = ENTRY_STUB.to_vec();
    code.extend_from_slice(p.bytes());
    code
}

/// Wraps code in a static executable and checks it reads back with the
/// code at the entry point.
fn executable(code: &[u8]) -> Vec<u8> {
    let bytes = object_lang::elf::executable(Architecture::X86_64, code).unwrap();
    let obj = object_lang::elf::read(&bytes).unwrap();
    assert_eq!(obj.kind(), FileKind::Executable);
    assert_eq!(obj.architecture(), Architecture::X86_64);
    assert_eq!(
        obj.entry(),
        object_lang::elf::code_address(Architecture::X86_64)
    );
    assert_eq!(obj.sections()[0].data(), code);
    bytes
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn run_native(bytes: &[u8], name: &str) -> std::process::Output {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("backend-lang-exec-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Tests run on parallel threads. A sibling thread that forks while this
    // file's write descriptor is still open hands the child a copy of it, and
    // exec then fails with ETXTBSY (errno 26) until that child execs and
    // drops it. The window is short, so retry rather than flake.
    const ETXTBSY: i32 = 26;
    let mut attempts = 0;
    let output = loop {
        match std::process::Command::new(&path).output() {
            Err(e) if e.raw_os_error() == Some(ETXTBSY) && attempts < 100 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            result => break result.unwrap(),
        }
    };
    let _ = std::fs::remove_file(&path);
    output
}

fn check(name: &str, code: &[u8], status: u64) {
    assert_eq!(
        common::run_process(code),
        Ok(common::Stop::Exit(status)),
        "{name} in the interpreter"
    );
    let bytes = executable(code);
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        let output = run_native(&bytes, name);
        assert_eq!(
            output.status.code(),
            Some(status as i32),
            "{name}: {output:?}"
        );
    }
    let _ = bytes;
}

#[test]
fn fib_loop_with_parallel_copies_exits_55() {
    // A leaf that never returns: it can be the entry point directly.
    let code = backend_lang::emit(&programs::fib().unwrap()).unwrap();
    check("fib", &code, 55);
}

#[test]
fn recursive_factorial_exits_120() {
    let code = link(&[programs::fact_main().unwrap(), programs::fact().unwrap()]);
    check("fact", &code, 120);
}

#[test]
fn kitchen_sink_exits_200() {
    let code = link(&[programs::kitchen().unwrap(), programs::helper().unwrap()]);
    check("kitchen", &code, 200);
}
