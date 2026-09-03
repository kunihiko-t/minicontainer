//! `minictr run`のargument parseと既定path解決。
//!
//! parseはUTF-8のimage名とoption名だけを受け付け、storeとkernelの値は
//! OS pathとして非UTF-8 byteも透過的に扱う。

use std::{ffi::OsString, fmt, path::PathBuf, time::Duration};

/// `--timeout-ms`を省略したときの待ち時間。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// `minictr`が公開するcommand。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// MiniBundleを一つのQEMU仮想machineで実行する。
    Run(RunArgs),
}

/// `run` commandの型付き引数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunArgs {
    /// store内で解決するimage tag。
    pub image: String,
    /// `--store`の指定値。`None`なら環境とHOMEから解決する。
    pub store: Option<PathBuf>,
    /// `--kernel`の指定値。`None`なら環境とHOMEから解決する。
    pub kernel: Option<PathBuf>,
    /// UART control eventを待つ全体の期限。
    pub timeout: Duration,
}

/// `run`に必要な不変入力を解決した結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRun {
    /// 解決するimage tag。
    pub image: String,
    /// bundle storeのroot。
    pub store: PathBuf,
    /// miniOS kernelのpath。
    pub kernel: PathBuf,
    /// UART control eventを待つ全体の期限。
    pub timeout: Duration,
}

/// command parseまたは既定path解決が失敗した理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// commandが一つも与えられなかった。
    MissingCommand,
    /// 対応していないcommand。
    UnknownCommand(String),
    /// `run`にimageが与えられなかった。
    MissingImage,
    /// 余分なpositional引数。
    UnexpectedArgument(String),
    /// 対応していないoption。
    UnknownOption(String),
    /// 同じoptionが二回以上与えられた。
    DuplicateOption(&'static str),
    /// optionの値が欠けている。
    MissingValue(&'static str),
    /// `--timeout-ms`が非負整数として読めない。
    InvalidTimeout(String),
    /// `--timeout-ms`が0である。
    ZeroTimeout,
    /// command、option名、imageをUTF-8として読めない。
    NonUtf8Argument(OsString),
    /// HOMEがなく既定pathを作れない。
    MissingHome,
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCommand => formatter.write_str("missing minictr command"),
            Self::UnknownCommand(command) => {
                write!(formatter, "unknown minictr command: {command}")
            }
            Self::MissingImage => formatter.write_str("missing image for `minictr run`"),
            Self::UnexpectedArgument(argument) => {
                write!(formatter, "unexpected minictr argument: {argument}")
            }
            Self::UnknownOption(option) => {
                write!(formatter, "unknown minictr option: {option}")
            }
            Self::DuplicateOption(option) => {
                write!(formatter, "duplicate minictr option: {option}")
            }
            Self::MissingValue(option) => {
                write!(formatter, "missing value for minictr option: {option}")
            }
            Self::InvalidTimeout(value) => {
                write!(formatter, "invalid minictr timeout: {value}")
            }
            Self::ZeroTimeout => formatter.write_str("minictr timeout must not be zero"),
            Self::NonUtf8Argument(_) => formatter.write_str("minictr argument is not valid UTF-8"),
            Self::MissingHome => {
                formatter.write_str("cannot resolve the default minictr path without HOME")
            }
        }
    }
}

impl std::error::Error for CliError {}

/// 公開command syntax。
pub fn help() -> &'static str {
    "usage: minictr run [--store PATH] [--kernel PATH] [--timeout-ms N] IMAGE"
}

/// OS引数からcommandをparseする。
pub fn parse_os(arguments: impl IntoIterator<Item = OsString>) -> Result<Command, CliError> {
    let mut arguments = arguments.into_iter();
    let command = arguments.next().ok_or(CliError::MissingCommand)?;
    let command = command.into_string().map_err(CliError::NonUtf8Argument)?;
    if command != "run" {
        return Err(CliError::UnknownCommand(command));
    }

    let mut image: Option<String> = None;
    let mut store: Option<PathBuf> = None;
    let mut kernel: Option<PathBuf> = None;
    let mut timeout = DEFAULT_TIMEOUT;

    let mut pending: Vec<OsString> = arguments.collect();
    // `--opt=value`形式を`--opt value`へ正規化する。非UTF-8引数は分割せず、
    // 共通経路でNonUtf8Argumentとして型付きerrorにする。
    let mut expanded: Vec<OsString> = Vec::with_capacity(pending.len());
    for argument in pending.drain(..) {
        match argument.to_str() {
            Some(text) if text.starts_with("--") && text.contains('=') => {
                let (name, value) = text.split_once('=').expect("contains '='");
                expanded.push(OsString::from(name));
                expanded.push(OsString::from(value));
            }
            _ => expanded.push(argument),
        }
    }

    let mut rest = expanded.into_iter().peekable();
    while let Some(argument) = rest.next() {
        if is_option(&argument) {
            let name = argument.into_string().map_err(CliError::NonUtf8Argument)?;
            match name.as_str() {
                "--store" => {
                    if store.is_some() {
                        return Err(CliError::DuplicateOption("--store"));
                    }
                    let value = rest.next().ok_or(CliError::MissingValue("--store"))?;
                    store = Some(PathBuf::from(value));
                }
                "--kernel" => {
                    if kernel.is_some() {
                        return Err(CliError::DuplicateOption("--kernel"));
                    }
                    let value = rest.next().ok_or(CliError::MissingValue("--kernel"))?;
                    kernel = Some(PathBuf::from(value));
                }
                "--timeout-ms" => {
                    let value = rest.next().ok_or(CliError::MissingValue("--timeout-ms"))?;
                    let value = value.into_string().map_err(CliError::NonUtf8Argument)?;
                    timeout = parse_timeout(&value)?;
                }
                unknown => return Err(CliError::UnknownOption(unknown.to_owned())),
            }
            continue;
        }

        let text = argument.into_string().map_err(CliError::NonUtf8Argument)?;
        if image.is_some() {
            return Err(CliError::UnexpectedArgument(text));
        }
        image = Some(text);
    }

    let Some(image) = image else {
        return Err(CliError::MissingImage);
    };
    Ok(Command::Run(RunArgs {
        image,
        store,
        kernel,
        timeout,
    }))
}

fn is_option(argument: &OsString) -> bool {
    argument.as_encoded_bytes().starts_with(b"-") && argument.len() > 1
}

fn parse_timeout(value: &str) -> Result<Duration, CliError> {
    let millis: u64 = value
        .parse()
        .map_err(|_| CliError::InvalidTimeout(value.to_owned()))?;
    if millis == 0 {
        return Err(CliError::ZeroTimeout);
    }
    Ok(Duration::from_millis(millis))
}

/// 実行時環境から既定pathを読む境界。testでは偽装できる。
pub trait Environ {
    /// `MINICTR_STORE`の値。
    fn store_override(&self) -> Option<OsString>;
    /// `MINICTR_KERNEL`の値。
    fn kernel_override(&self) -> Option<OsString>;
    /// `HOME`の値。
    fn home(&self) -> Option<OsString>;
}

/// 実際のprocess環境。
pub struct RealEnv;

impl Environ for RealEnv {
    fn store_override(&self) -> Option<OsString> {
        std::env::var_os("MINICTR_STORE")
    }

    fn kernel_override(&self) -> Option<OsString> {
        std::env::var_os("MINICTR_KERNEL")
    }

    fn home(&self) -> Option<OsString> {
        std::env::var_os("HOME")
    }
}

/// parse済み引数と環境から実行時pathを解決する。
pub fn resolve(args: &RunArgs, env: &dyn Environ) -> Result<ResolvedRun, CliError> {
    let store = match &args.store {
        Some(path) => path.clone(),
        None => match env.store_override() {
            Some(path) => PathBuf::from(path),
            None => default_store_root(env)?,
        },
    };
    let kernel = match &args.kernel {
        Some(path) => path.clone(),
        None => match env.kernel_override() {
            Some(path) => PathBuf::from(path),
            None => default_kernel_path(env)?,
        },
    };
    Ok(ResolvedRun {
        image: args.image.clone(),
        store,
        kernel,
        timeout: args.timeout,
    })
}

fn default_store_root(env: &dyn Environ) -> Result<PathBuf, CliError> {
    let home = env.home().ok_or(CliError::MissingHome)?;
    let mut root = PathBuf::from(home);
    root.push(".minicontainer");
    Ok(root)
}

fn default_kernel_path(env: &dyn Environ) -> Result<PathBuf, CliError> {
    let mut root = default_store_root(env)?;
    root.push("minios-kernel");
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    /// UTF-8引数からのparse。binary本体は`parse_os`を使うが、testの可読性の
    /// ため同じ呼び出し形を提供する。
    fn parse(arguments: impl IntoIterator<Item = impl AsRef<str>>) -> Result<Command, CliError> {
        parse_os(
            arguments
                .into_iter()
                .map(|argument| OsString::from(argument.as_ref())),
        )
    }

    struct FakeEnv {
        store: Option<OsString>,
        kernel: Option<OsString>,
        home: Option<OsString>,
    }

    impl Environ for FakeEnv {
        fn store_override(&self) -> Option<OsString> {
            self.store.clone()
        }

        fn kernel_override(&self) -> Option<OsString> {
            self.kernel.clone()
        }

        fn home(&self) -> Option<OsString> {
            self.home.clone()
        }
    }

    fn home_env() -> FakeEnv {
        FakeEnv {
            store: None,
            kernel: None,
            home: Some(OsString::from("/home/test")),
        }
    }

    // Catches drifting from the M1 acceptance syntax: only `run IMAGE`
    // with documented options and a 5 second default timeout.
    #[test]
    fn parses_run_with_defaults() {
        assert_eq!(
            parse(["run", "hello"]),
            Ok(Command::Run(RunArgs {
                image: "hello".into(),
                store: None,
                kernel: None,
                timeout: Duration::from_secs(5),
            }))
        );
    }

    // Catches accepting a full option set with reordered options.
    #[test]
    fn parses_run_with_every_option() {
        assert_eq!(
            parse([
                "run",
                "--timeout-ms",
                "250",
                "--store",
                "/data/store",
                "--kernel",
                "/data/kernel",
                "hello",
            ]),
            Ok(Command::Run(RunArgs {
                image: "hello".into(),
                store: Some(PathBuf::from("/data/store")),
                kernel: Some(PathBuf::from("/data/kernel")),
                timeout: Duration::from_millis(250),
            }))
        );
    }

    // Catches accepting `--opt=value` on one path and rejecting it on another.
    #[test]
    fn parses_equals_form_options() {
        assert_eq!(
            parse(["run", "--store=/data/store", "--timeout-ms=750", "hello"]),
            Ok(Command::Run(RunArgs {
                image: "hello".into(),
                store: Some(PathBuf::from("/data/store")),
                kernel: None,
                timeout: Duration::from_millis(750),
            }))
        );
    }

    // Catches running without an image, with an unknown command or option,
    // or with a second positional argument.
    #[test]
    fn rejects_missing_image_unknown_command_option_and_extra_positional() {
        assert_eq!(parse(["run"]), Err(CliError::MissingImage));
        assert_eq!(parse([] as [&str; 0]), Err(CliError::MissingCommand));
        assert_eq!(
            parse(["status"]),
            Err(CliError::UnknownCommand("status".to_owned()))
        );
        assert_eq!(
            parse(["run", "--volume", "data", "hello"]),
            Err(CliError::UnknownOption("--volume".to_owned()))
        );
        assert_eq!(
            parse(["run", "hello", "extra"]),
            Err(CliError::UnexpectedArgument("extra".to_owned()))
        );
    }

    // Catches silently preferring one of two repeated options.
    #[test]
    fn rejects_duplicate_options() {
        assert_eq!(
            parse(["run", "--store", "/a", "--store", "/b", "hello"]),
            Err(CliError::DuplicateOption("--store"))
        );
        assert_eq!(
            parse(["run", "--kernel", "/a", "--kernel", "/b", "hello"]),
            Err(CliError::DuplicateOption("--kernel"))
        );
    }

    // Catches waiting forever on a zero timeout or misreading a
    // non-numeric timeout as a valid deadline.
    #[test]
    fn rejects_zero_and_non_numeric_timeouts() {
        assert_eq!(
            parse(["run", "--timeout-ms", "0", "hello"]),
            Err(CliError::ZeroTimeout)
        );
        assert_eq!(
            parse(["run", "--timeout-ms", "fast", "hello"]),
            Err(CliError::InvalidTimeout("fast".to_owned()))
        );
        assert_eq!(
            parse(["run", "--timeout-ms"]),
            Err(CliError::MissingValue("--timeout-ms"))
        );
    }

    // Catches dropping a value-taking option at the end of argv.
    #[test]
    fn rejects_a_missing_option_value() {
        assert_eq!(
            parse(["run", "hello", "--store"]),
            Err(CliError::MissingValue("--store"))
        );
    }

    // Catches interpreting undecodable OS bytes as a command or image name.
    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_command_option_and_image() {
        use std::os::unix::ffi::OsStringExt;

        let invalid = OsString::from_vec(vec![0xff]);
        assert_eq!(
            parse_os([invalid.clone()]),
            Err(CliError::NonUtf8Argument(invalid.clone()))
        );
        assert_eq!(
            parse_os([OsString::from("run"), invalid.clone()]),
            Err(CliError::NonUtf8Argument(invalid))
        );
    }

    // Catches keeping non-UTF-8 store paths from reaching QEMU.
    #[cfg(unix)]
    #[test]
    fn keeps_non_utf8_store_paths_as_paths() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let raw = OsString::from_vec(vec![0x2f, 0x74, 0x6d, 0x70, 0xff]);
        let parsed = parse_os([OsString::from("run"), raw.clone(), OsString::from("hello")]);
        // A non-UTF-8 positional is an image name, not a path, so it is rejected.
        assert_eq!(parsed, Err(CliError::NonUtf8Argument(raw.clone())));

        let parsed = parse_os([
            OsString::from("run"),
            OsString::from("--store"),
            raw.clone(),
            OsString::from("hello"),
        ])
        .unwrap();
        let Command::Run(args) = parsed;
        assert_eq!(args.store.unwrap().as_os_str().as_bytes(), raw.as_bytes());
    }

    // Catches resolving defaults from the wrong environment variable or
    // falling back to the current directory instead of HOME.
    #[test]
    fn resolves_defaults_from_the_documented_environment() {
        let args = RunArgs {
            image: "hello".into(),
            store: None,
            kernel: None,
            timeout: Duration::from_secs(5),
        };
        let resolved = resolve(&args, &home_env()).unwrap();
        assert_eq!(resolved.store, PathBuf::from("/home/test/.minicontainer"));
        assert_eq!(
            resolved.kernel,
            PathBuf::from("/home/test/.minicontainer/minios-kernel")
        );

        let env = FakeEnv {
            store: Some(OsString::from("/env/store")),
            kernel: Some(OsString::from("/env/kernel")),
            home: Some(OsString::from("/home/test")),
        };
        let resolved = resolve(&args, &env).unwrap();
        assert_eq!(resolved.store, PathBuf::from("/env/store"));
        assert_eq!(resolved.kernel, PathBuf::from("/env/kernel"));
    }

    // Catches inventing a current-directory fallback when HOME is absent.
    #[test]
    fn rejects_default_resolution_without_home() {
        let args = RunArgs {
            image: "hello".into(),
            store: None,
            kernel: None,
            timeout: Duration::from_secs(5),
        };
        let env = FakeEnv {
            store: None,
            kernel: None,
            home: None,
        };
        assert_eq!(resolve(&args, &env), Err(CliError::MissingHome));
    }

    // Catches losing an explicit CLI path to an environment override.
    #[test]
    fn explicit_paths_win_over_the_environment() {
        let args = RunArgs {
            image: "hello".into(),
            store: Some(PathBuf::from("/cli/store")),
            kernel: Some(PathBuf::from("/cli/kernel")),
            timeout: Duration::from_secs(5),
        };
        let env = FakeEnv {
            store: Some(OsString::from("/env/store")),
            kernel: Some(OsString::from("/env/kernel")),
            home: Some(OsString::from("/home/test")),
        };
        let resolved = resolve(&args, &env).unwrap();
        assert_eq!(resolved.store, PathBuf::from("/cli/store"));
        assert_eq!(resolved.kernel, PathBuf::from("/cli/kernel"));
    }

    // Catches drifting usage text between the parser and the binary.
    #[test]
    fn help_names_the_public_run_syntax() {
        assert_eq!(
            help(),
            "usage: minictr run [--store PATH] [--kernel PATH] [--timeout-ms N] IMAGE"
        );
    }
}
