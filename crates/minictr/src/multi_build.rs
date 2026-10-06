//! 複数imageの入力検証、bundle構築、store登録。

use std::{
    fs::File,
    io,
    io::Write,
    path::{Component, Path},
};

use crate::{
    BoundedReadError, BuildError, ImageSpec, ImageStore, MAX_BUNDLE_LEN, RUNTIME_EXIT, build,
    cli::ResolvedBuildMulti, format_digest, read_bounded_with_limit,
};

/// 全入力の構築に成功してからstoreを更新し、既存buildと同じ成功行を出す。
pub fn build_resolved(
    resolved: &ResolvedBuildMulti,
    store: &dyn ImageStore,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let result = build_bundle(resolved).and_then(|bundle| {
        let digest = store.import(&resolved.store, &bundle)?;
        store.tag(&resolved.store, &resolved.image, digest)?;
        Ok(digest)
    });
    let digest = match result {
        Ok(digest) => digest,
        Err(error) => {
            let _ = writeln!(stderr, "minictr: {error}");
            return RUNTIME_EXIT;
        }
    };
    if let Err(error) = writeln!(
        stdout,
        "{} sha256:{}",
        resolved.image,
        format_digest(digest)
    )
    .and_then(|()| stdout.flush())
    {
        let _ = writeln!(stderr, "minictr: failed to write build result: {error}");
        return RUNTIME_EXIT;
    }
    0
}

fn build_bundle(resolved: &ResolvedBuildMulti) -> Result<Vec<u8>, BuildError> {
    // store tagのmanifest規則とpath component規則を、登録前に確認する。
    build(ImageSpec {
        name: &resolved.image,
        args: &[],
        elf: b"_",
    })?;
    let mut components = Path::new(&resolved.image).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(BuildError::Store(
            minicontainer_bundle::StoreError::UnsafeTagName,
        ));
    }
    let args: Vec<Vec<&str>> = resolved
        .images
        .iter()
        .map(|image| image.args.iter().map(String::as_str).collect())
        .collect();
    // 安価なdummy ELFで名前・引数・個数を先に検証する。最終offsetと
    // manifest合計長は実ELFを渡すbuilderでも検証する。
    minicontainer_bundle::build_multi(
        &resolved
            .images
            .iter()
            .zip(&args)
            .map(|(image, args)| ImageSpec {
                name: &image.name,
                args,
                elf: b"_",
            })
            .collect::<Vec<_>>(),
    )?;
    let mut remaining = MAX_BUNDLE_LEN;
    let mut elfs = Vec::with_capacity(resolved.images.len());
    for image in &resolved.images {
        let elf =
            read_bounded_with_limit(&image.elf, &open_regular, remaining).map_err(|error| {
                match error {
                    BoundedReadError::Io(error) => BuildError::ElfIo(io::Error::new(
                        error.kind(),
                        format!("{}: {error}", image.elf.display()),
                    )),
                    BoundedReadError::TooLarge => {
                        BuildError::Bundle(minicontainer_bundle::BundleError::TooLarge)
                    }
                }
            })?;
        remaining -= elf.len() as u64;
        elfs.push(elf);
    }
    let specs: Vec<_> = resolved
        .images
        .iter()
        .zip(&args)
        .zip(&elfs)
        .map(|((image, args), elf)| ImageSpec {
            name: &image.name,
            args,
            elf,
        })
        .collect();
    minicontainer_bundle::build_multi(&specs).map_err(BuildError::Bundle)
}

/// FIFOなどの特殊fileを待たずに拒否する。symlink先の通常fileは許可する。
fn open_regular(path: &Path) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ELF input must be a regular file",
        ));
    }
    Ok(file)
}
