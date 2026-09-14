//! 再現可能なQEMU command列の構築。

use std::ffi::{OsStr, OsString};
use std::path::Path;

use crate::RuntimeError;

/// QEMUへ渡すguest resource量。
///
/// 値はCLIで検証済みのものを渡す。数値だけを書式化するため、QEMU引数への
/// 注入面はない。範囲外の値が届いてもQEMUが起動時に拒否し、host側の安全は
/// 損なわない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QemuResources {
    /// guest memory量 (MiB)。
    pub memory_mib: u32,
    /// guest vCPU数。
    pub cpus: u32,
}

impl QemuResources {
    /// option省略時の既定値。kernelが検証する配置を保つ。
    pub const DEFAULT: Self = Self {
        memory_mib: Self::DEFAULT_MEMORY_MIB,
        cpus: Self::DEFAULT_CPUS,
    };
    /// 既定のguest memory量 (MiB)。
    pub const DEFAULT_MEMORY_MIB: u32 = 128;
    /// 既定のguest vCPU数。
    pub const DEFAULT_CPUS: u32 = 1;
    /// 公開する最小のguest memory量 (MiB)。予約窓`0x8780_0000`がRAMに
    /// 載る下限であり、既定値と一致する。
    pub const MIN_MEMORY_MIB: u32 = 128;
    /// 公開する最大のguest memory量 (MiB)。
    pub const MAX_MEMORY_MIB: u32 = 8192;
    /// 公開する最小のguest vCPU数。
    pub const MIN_CPUS: u32 = 1;
    /// 公開する最大のguest vCPU数。
    pub const MAX_CPUS: u32 = 8;
}

/// payload実行のための、引数順まで決定的なQEMU起動command。
pub struct QemuCommand {
    program: OsString,
    args: Vec<OsString>,
}

impl QemuCommand {
    /// 確定したkernel path、payload path、resource量からQEMU commandを組み立てる。
    ///
    /// payloadは`-device loader,file=...`へ展開されるため、pathに`,`が含まれる
    /// 場合はQEMUのoption構文を壊すとして拒否する。kernelは`-kernel`のplainな
    /// argv要素として渡されるため、この制約の外にある。
    pub fn new(
        kernel: impl AsRef<Path>,
        payload: impl AsRef<Path>,
        resources: QemuResources,
    ) -> Result<Self, RuntimeError> {
        let payload = payload.as_ref();
        if payload.as_os_str().to_string_lossy().contains(',') {
            return Err(RuntimeError::UnsafePayloadPath(payload.to_path_buf()));
        }

        let mut loader = OsString::from("loader,file=");
        loader.push(payload.as_os_str());
        loader.push(",addr=0x87800000,force-raw=on");
        let memory = format!("{}M", resources.memory_mib);
        let cpus = format!("{}", resources.cpus);
        let args = [
            OsStr::new("-machine"),
            OsStr::new("virt"),
            OsStr::new("-m"),
            OsStr::new(&memory),
            OsStr::new("-smp"),
            OsStr::new(&cpus),
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
    use super::{QemuCommand, QemuResources, RuntimeError};
    use std::ffi::OsStr;

    // Catches drifting the default resources: omitting the CLI options must
    // keep the 128 MiB, single-vCPU layout the kernel validates.
    #[test]
    fn default_resources_keep_128m_and_one_cpu() {
        let command =
            QemuCommand::new("/kernel", "/tmp/run/payload.mcb", QemuResources::DEFAULT).unwrap();

        assert_eq!(QemuResources::DEFAULT.memory_mib, 128);
        assert_eq!(QemuResources::DEFAULT.cpus, 1);
        assert!(command.contains_pair("-m", "128M"));
        assert!(command.contains_pair("-smp", "1"));
    }

    // Catches generating the wrong QEMU arguments for configured resources:
    // numeric values render deterministically with no injection surface.
    #[test]
    fn configured_resources_render_exact_qemu_arguments() {
        let command = QemuCommand::new(
            "/kernel",
            "/tmp/run/payload.mcb",
            QemuResources {
                memory_mib: 256,
                cpus: 2,
            },
        )
        .unwrap();

        assert!(command.contains_pair("-m", "256M"));
        assert!(command.contains_pair("-smp", "2"));
    }

    // Catches a QEMU invocation that drifts away from the single-hart, 128 MiB,
    // reserved-window layout the kernel validates.
    #[test]
    fn command_uses_one_hart_128m_and_reserved_loader_address() {
        let command =
            QemuCommand::new("/kernel", "/tmp/run/payload.mcb", QemuResources::DEFAULT).unwrap();
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
        let command = QemuCommand::new(
            "/opt/kernels/m1.elf",
            "/tmp/run/payload.mcb",
            QemuResources::DEFAULT,
        )
        .unwrap();
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
            QemuCommand::new("/kernel", "/tmp/run,payload.mcb", QemuResources::DEFAULT),
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
        let command = QemuCommand::new("/kernel", payload, QemuResources::DEFAULT).unwrap();
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
        let command =
            QemuCommand::new(kernel, "/tmp/run/payload.mcb", QemuResources::DEFAULT).unwrap();
        let kernel_arg = command
            .args()
            .windows(2)
            .find(|pair| pair[0] == "-kernel")
            .map(|pair| pair[1].as_os_str())
            .unwrap();

        assert_eq!(kernel_arg.as_bytes(), b"/tmp/kernel,\xffm1.elf");
    }
}
