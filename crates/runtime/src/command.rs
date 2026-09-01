//! 再現可能なQEMU command列の構築。

use std::ffi::{OsStr, OsString};
use std::path::Path;

use crate::RuntimeError;

/// payload実行のための、引数順まで決定的なQEMU起動command。
pub struct QemuCommand {
    program: OsString,
    args: Vec<OsString>,
}

impl QemuCommand {
    /// 確定したkernel pathとpayload pathからQEMU commandを組み立てる。
    ///
    /// payloadは`-device loader,file=...`へ展開されるため、pathに`,`が含まれる
    /// 場合はQEMUのoption構文を壊すとして拒否する。kernelは`-kernel`のplainな
    /// argv要素として渡されるため、この制約の外にある。
    pub fn new(kernel: impl AsRef<Path>, payload: impl AsRef<Path>) -> Result<Self, RuntimeError> {
        let payload = payload.as_ref();
        if payload.as_os_str().to_string_lossy().contains(',') {
            return Err(RuntimeError::UnsafePayloadPath(payload.to_path_buf()));
        }

        let mut loader = OsString::from("loader,file=");
        loader.push(payload.as_os_str());
        loader.push(",addr=0x87800000,force-raw=on");
        let args = [
            OsStr::new("-machine"),
            OsStr::new("virt"),
            OsStr::new("-m"),
            OsStr::new("128M"),
            OsStr::new("-smp"),
            OsStr::new("1"),
            OsStr::new("-bios"),
            OsStr::new("default"),
            OsStr::new("-kernel"),
            kernel.as_ref().as_os_str(),
            OsStr::new("-device"),
            loader.as_os_str(),
            OsStr::new("-serial"),
            OsStr::new("stdio"),
            OsStr::new("-monitor"),
            OsStr::new("none"),
            OsStr::new("-display"),
            OsStr::new("none"),
        ]
        .map(OsString::from);

        Ok(Self {
            program: OsString::from("qemu-system-riscv64"),
            args: args.to_vec(),
        })
    }

    /// 起動するprogram名。
    pub fn program(&self) -> &OsStr {
        &self.program
    }

    /// `Command::args`へそのまま渡せる引数列。
    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    #[cfg(test)]
    fn contains_pair(&self, flag: &str, value: &str) -> bool {
        self.args
            .windows(2)
            .any(|pair| pair[0] == flag && pair[1] == value)
    }
}

#[cfg(test)]
mod tests {
    use super::{QemuCommand, RuntimeError};
    use std::ffi::OsStr;

    // Catches a QEMU invocation that drifts away from the single-hart, 128 MiB,
    // reserved-window layout the kernel validates.
    #[test]
    fn command_uses_one_hart_128m_and_reserved_loader_address() {
        let command = QemuCommand::new("/kernel", "/tmp/run/payload.mcb").unwrap();
        assert!(command.contains_pair("-machine", "virt"));
        assert!(command.contains_pair("-m", "128M"));
        assert!(command.contains_pair("-smp", "1"));
        assert!(command.contains_pair(
            "-device",
            "loader,file=/tmp/run/payload.mcb,addr=0x87800000,force-raw=on",
        ));
    }

    // Catches losing the program name, the kernel argument, or the headless
    // serial wiring that the harness reads back through pipes.
    #[test]
    fn command_names_qemu_and_carries_the_kernel_and_console_arguments() {
        let command = QemuCommand::new("/opt/kernels/m1.elf", "/tmp/run/payload.mcb").unwrap();
        assert_eq!(command.program(), OsStr::new("qemu-system-riscv64"));
        assert!(command.contains_pair("-kernel", "/opt/kernels/m1.elf"));
        assert!(command.contains_pair("-bios", "default"));
        assert!(command.contains_pair("-serial", "stdio"));
        assert!(command.contains_pair("-monitor", "none"));
        assert!(command.contains_pair("-display", "none"));
    }

    // Catches passing a payload path whose commas would silently split QEMU's
    // -device option into unknown keys.
    #[test]
    fn command_rejects_a_comma_in_the_payload_path() {
        assert!(matches!(
            QemuCommand::new("/kernel", "/tmp/run,payload.mcb"),
            Err(RuntimeError::UnsafePayloadPath(_))
        ));
    }

    // Catches replacing invalid Unix filename bytes while embedding the
    // payload path in QEMU's loader option.
    #[cfg(unix)]
    #[test]
    fn command_preserves_non_utf8_payload_paths() {
        use std::os::unix::ffi::OsStrExt;

        let payload = OsStr::from_bytes(b"/tmp/run/\xffpayload.mcb");
        let command = QemuCommand::new("/kernel", payload).unwrap();
        let loader = command
            .args()
            .windows(2)
            .find(|pair| pair[0] == "-device")
            .map(|pair| pair[1].as_os_str())
            .unwrap();

        assert_eq!(
            loader.as_bytes(),
            b"loader,file=/tmp/run/\xffpayload.mcb,addr=0x87800000,force-raw=on"
        );
    }

    // Catches rejecting a kernel path that only lives in a non-UTF-8 name;
    // -kernel takes a plain argv element, so commas stay legal there.
    #[cfg(unix)]
    #[test]
    fn command_preserves_non_utf8_kernel_paths_with_commas() {
        use std::os::unix::ffi::OsStrExt;

        let kernel = OsStr::from_bytes(b"/tmp/kernel,\xffm1.elf");
        let command = QemuCommand::new(kernel, "/tmp/run/payload.mcb").unwrap();
        let kernel_arg = command
            .args()
            .windows(2)
            .find(|pair| pair[0] == "-kernel")
            .map(|pair| pair[1].as_os_str())
            .unwrap();

        assert_eq!(kernel_arg.as_bytes(), b"/tmp/kernel,\xffm1.elf");
    }
}
