//! 最小のMiniContainerゲスト例。
//!
//! Guest ABIの`write`と`exit`だけを使い、標準出力と標準エラー出力へ
//! 一行ずつ書いて42で終わる。system call番号とfile descriptorは
//! `minios-abi`の定義だけを参照し、番号の直書きをしない。

#![no_std]
#![no_main]

use core::arch::asm;

use minios_abi::syscall::{STDERR, STDOUT, SyscallNumber};

const STDOUT_MESSAGE: &[u8] = b"hello from guest\n";
const STDERR_MESSAGE: &[u8] = b"guest stderr\n";
const EXIT_OK: u32 = 42;
const EXIT_FAILED: u32 = 1;

/// guestのentry point。loaderが`sp`をuser stack topへ置いて呼び出す。
/// 先頭配置のsectionへ置き、entry addressを`0x0010_0000`に固定する。
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.start")]
pub extern "C" fn _start() -> ! {
    if write(STDOUT, STDOUT_MESSAGE) != STDOUT_MESSAGE.len() as isize {
        exit(EXIT_FAILED);
    }
    if write(STDERR, STDERR_MESSAGE) != STDERR_MESSAGE.len() as isize {
        exit(EXIT_FAILED);
    }
    exit(EXIT_OK)
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
