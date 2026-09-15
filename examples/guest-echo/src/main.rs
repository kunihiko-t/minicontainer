//! stdinをそのままstdoutへechoするMiniContainerゲスト例。
//!
//! Guest ABIの`read`と`write`、`exit`だけを使う。`read`はkernelのstdin
//! staging (最大`MAX_READ_LEN`byte) から届き、hostが送るEOF frameで0を
//! 返す。byte列は手を加えずそのまま書き戻し、EOFで42で終わる。

#![no_std]
#![no_main]

use core::arch::asm;

use minios_abi::syscall::{MAX_READ_LEN, STDIN, STDOUT, SyscallNumber};

const EXIT_OK: u32 = 42;
const EXIT_FAILED: u32 = 1;

/// guestのentry point。loaderが`sp`をuser stack topへ置いて呼び出す。
/// 先頭配置のsectionへ置き、entry addressを`0x0010_0000`に固定する。
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.start")]
pub extern "C" fn _start() -> ! {
    let mut buffer = [0_u8; MAX_READ_LEN];
    loop {
        let length = read(STDIN, &mut buffer);
        if length < 0 {
            exit(EXIT_FAILED);
        }
        if length == 0 {
            exit(EXIT_OK);
        }
        let echoed = &buffer[..length as usize];
        if write(STDOUT, echoed) != length {
            exit(EXIT_FAILED);
        }
    }
}

/// 一回の`read` system callを行い、kernelが返したbyte数を返す。
/// EOFでは0、失敗では負値が返る。
fn read(descriptor: usize, buffer: &mut [u8]) -> isize {
    syscall(
        SyscallNumber::Read,
        descriptor,
        buffer.as_mut_ptr() as usize,
        buffer.len(),
    )
}

/// 一回の`write` system callを行い、kernelが返したbyte数を返す。
fn write(descriptor: usize, bytes: &[u8]) -> isize {
    syscall(
        SyscallNumber::Write,
        descriptor,
        bytes.as_ptr() as usize,
        bytes.len(),
    )
}

fn exit(code: u32) -> ! {
    syscall(SyscallNumber::Exit, code as usize, 0, 0);
    unreachable!("exit never returns");
}

/// 生のsystem call。番号は`a7`、引数は`a0`から`a2`、戻り値は`a0`である。
fn syscall(number: SyscallNumber, arg0: usize, arg1: usize, arg2: usize) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "ecall",
            in("a7") number as usize,
            inlateout("a0") arg0 as isize => result,
            in("a1") arg1,
            in("a2") arg2,
        );
    }
    result
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(EXIT_FAILED)
}
