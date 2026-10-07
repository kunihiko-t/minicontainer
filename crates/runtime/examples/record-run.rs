//! sourceから監視基盤を実QEMUで試す例。公開minictr commandではない。

use minicontainer_runtime::{
    InstanceDir, InstanceRegistration, QemuResources, RunRecords, RunRequest, Runtime,
};
use std::{ffi::OsStr, fs, path::PathBuf, time::Duration};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if !(3..=4).contains(&args.len()) {
        return Err("usage: record-run STORE BUNDLE KERNEL [TIMEOUT_MS]".into());
    }
    let store = PathBuf::from(&args[0]);
    let bundle = fs::read(&args[1])?;
    let kernel = PathBuf::from(&args[2]);
    let timeout = args
        .get(3)
        .map(|n| {
            n.to_str()
                .ok_or("non-UTF-8 timeout")?
                .parse::<u64>()
                .map_err(|_| "invalid timeout")
        })
        .transpose()?
        .unwrap_or(5000);
    let records = RunRecords::open(&store)?;
    let run = records.begin()?;
    let id = run.id().to_owned();
    println!("record={id}");
    let instances = InstanceDir::open(&store)?;
    let result = run.supervise(
        &Runtime::default(),
        RunRequest {
            bundle: &bundle,
            kernel: &kernel,
            deadline: Duration::from_millis(timeout),
            resources: QemuResources::DEFAULT,
            input: None,
            interrupts: None,
            instances: Some(InstanceRegistration {
                dir: &instances,
                image: "recorded",
                program: OsStr::new("qemu-system-riscv64"),
            }),
        },
    )?;
    println!("observation={:?}", records.observe(&id)?);
    let code = match result {
        Ok(outcome) => i32::try_from(outcome.exit_code)
            .ok()
            .filter(|n| *n <= 255)
            .unwrap_or(125),
        Err(_) => 125,
    };
    std::process::exit(code);
}
