//! `minictr`のargument parseと既定path解決。
//!
//! parseはUTF-8のimage名とoption名だけを受け付け、storeとkernelの値は
//! OS pathとして非UTF-8 byteも透過的に扱う。

use std::{ffi::OsString, fmt, path::PathBuf, time::Duration};

/// `--timeout-ms`を省略したときの待ち時間。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// `minictr` binaryのversion。`--version`がそのまま表示する。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `minictr`が公開するcommand。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// help全文を表示する。
    Help,
    /// binary名とversionを表示する。
    Version,
    /// MiniBundleを一つのQEMU仮想machineで実行する。
    Run(RunArgs),
    /// imageのbuildなどstore内容を操作する。
    Image(ImageCommand),
}

/// `image` commandの型付きsubcommand。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageCommand {
    /// 静的ELFからMiniBundleを構築してstoreへ登録する。
    Build(ImageBuildArgs),
    /// 登録済みtagを一覧する。
    List(ImageListArgs),
    /// 一つのtagのmanifestを確認する。
    Inspect(ImageInspectArgs),
}

/// `image build`の型付き引数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageBuildArgs {
    /// store内で付けるimage tag。
    pub image: String,
    /// 読み取る静的ELFのpath。
    pub elf: PathBuf,
    /// manifestへ格納するguest引数。
    pub args: Vec<String>,
    /// `--store`の指定値。`None`なら環境とHOMEから解決する。
    pub store: Option<PathBuf>,
}

/// `image list`の型付き引数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageListArgs {
    /// `--store`の指定値。`None`なら環境とHOMEから解決する。
    pub store: Option<PathBuf>,
}

/// `image inspect`の型付き引数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageInspectArgs {
    /// store内で解決するimage tag。
    pub image: String,
    /// `--store`の指定値。`None`なら環境とHOMEから解決する。
    pub store: Option<PathBuf>,
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

/// `image build`に必要な不変入力を解決した結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBuild {
    /// store内で付けるimage tag。
    pub image: String,
    /// 読み取る静的ELFのpath。
    pub elf: PathBuf,
    /// manifestへ格納するguest引数。
    pub args: Vec<String>,
    /// bundle storeのroot。
    pub store: PathBuf,
}

/// `image list`に必要な不変入力を解決した結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedList {
    /// bundle storeのroot。
    pub store: PathBuf,
}

/// `image inspect`に必要な不変入力を解決した結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInspect {
    /// store内で解決するimage tag。
    pub image: String,
    /// bundle storeのroot。
    pub store: PathBuf,
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
    /// `image build`にimageが与えられなかった。
    MissingBuildImage,
    /// `image build`にELF pathが与えられなかった。
    MissingElf,
    /// `image inspect`にimageが与えられなかった。
    MissingInspectImage,
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
            Self::MissingBuildImage => {
                formatter.write_str("missing image for `minictr image build`")
            }
            Self::MissingElf => formatter.write_str("missing ELF for `minictr image build`"),
            Self::MissingInspectImage => {
                formatter.write_str("missing image for `minictr image inspect`")
            }
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
    "usage: minictr run [--store PATH] [--kernel PATH] [--timeout-ms N] IMAGE\nusage: minictr image build [--store PATH] [--arg VALUE]... IMAGE ELF\nusage: minictr image list [--store PATH]\nusage: minictr image inspect [--store PATH] IMAGE"
}

/// OS引数からcommandをparseする。
pub fn parse_os(arguments: impl IntoIterator<Item = OsString>) -> Result<Command, CliError> {
    let mut arguments = arguments.into_iter();
    let command = arguments.next().ok_or(CliError::MissingCommand)?;
    let command = command.into_string().map_err(CliError::NonUtf8Argument)?;
    match command.as_str() {
        "help" | "--help" => return parse_bare_command(arguments, Command::Help),
        "--version" => return parse_bare_command(arguments, Command::Version),
        "run" => {}
        "image" => {
            let subcommand = arguments.next().ok_or(CliError::MissingCommand)?;
            let subcommand = subcommand
                .into_string()
                .map_err(CliError::NonUtf8Argument)?;
            match subcommand.as_str() {
                "build" => return parse_image_build(arguments),
                "list" => return parse_image_list(arguments),
                "inspect" => return parse_image_inspect(arguments),
                unknown => return Err(CliError::UnknownCommand(unknown.to_owned())),
            }
        }
        unknown => return Err(CliError::UnknownCommand(unknown.to_owned())),
    }

    let mut image: Option<String> = None;
    let mut store: Option<PathBuf> = None;
    let mut kernel: Option<PathBuf> = None;
    let mut timeout: Option<Duration> = None;

    // `--opt=value`はoption位置のtokenだけを分割する。`--store`や`--kernel`が
    // 消費した値はOS pathとして不透明に扱い、`--`で始まり`=`を含むpathを
    // 壊さない。
    let pending: Vec<OsString> = arguments.collect();
    let mut rest = pending.into_iter().peekable();
    while let Some(argument) = rest.next() {
        if is_option(&argument) {
            let text = argument.into_string().map_err(CliError::NonUtf8Argument)?;
            let (name, inline_value) = match text.split_once('=') {
                Some((name, value)) => (name.to_owned(), Some(OsString::from(value))),
                None => (text, None),
            };
            match name.as_str() {
                "--store" => {
                    if store.is_some() {
                        return Err(CliError::DuplicateOption("--store"));
                    }
                    let value = match inline_value {
                        Some(value) => value,
                        None => rest.next().ok_or(CliError::MissingValue("--store"))?,
                    };
                    store = Some(PathBuf::from(value));
                }
                "--kernel" => {
                    if kernel.is_some() {
                        return Err(CliError::DuplicateOption("--kernel"));
                    }
                    let value = match inline_value {
                        Some(value) => value,
                        None => rest.next().ok_or(CliError::MissingValue("--kernel"))?,
                    };
                    kernel = Some(PathBuf::from(value));
                }
                "--timeout-ms" => {
                    if timeout.is_some() {
                        return Err(CliError::DuplicateOption("--timeout-ms"));
                    }
                    let value = match inline_value {
                        Some(value) => value,
                        None => rest.next().ok_or(CliError::MissingValue("--timeout-ms"))?,
                    };
                    let value = value.into_string().map_err(CliError::NonUtf8Argument)?;
                    timeout = Some(parse_timeout(&value)?);
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
        timeout: timeout.unwrap_or(DEFAULT_TIMEOUT),
    }))
}

/// `image build`の引数をparseする。optionはIMAGEとELFの前後どこに
/// 置いてもよく、`--arg`だけが複数回の指定を蓄積する。
fn parse_image_build(arguments: impl IntoIterator<Item = OsString>) -> Result<Command, CliError> {
    let mut image: Option<String> = None;
    let mut elf: Option<PathBuf> = None;
    let mut args: Vec<String> = Vec::new();
    let mut store: Option<PathBuf> = None;

    // `run`と同じく`--opt=value`はoption位置のtokenだけを分割し、
    // 消費した値はOS pathまたはUTF-8引数として不透明に扱う。
    let pending: Vec<OsString> = arguments.into_iter().collect();
    let mut rest = pending.into_iter().peekable();
    while let Some(argument) = rest.next() {
        if is_option(&argument) {
            let text = argument.into_string().map_err(CliError::NonUtf8Argument)?;
            let (name, inline_value) = match text.split_once('=') {
                Some((name, value)) => (name.to_owned(), Some(OsString::from(value))),
                None => (text, None),
            };
            match name.as_str() {
                "--store" => {
                    if store.is_some() {
                        return Err(CliError::DuplicateOption("--store"));
                    }
                    let value = match inline_value {
                        Some(value) => value,
                        None => rest.next().ok_or(CliError::MissingValue("--store"))?,
                    };
                    store = Some(PathBuf::from(value));
                }
                "--arg" => {
                    let value = match inline_value {
                        Some(value) => value,
                        None => rest.next().ok_or(CliError::MissingValue("--arg"))?,
                    };
                    let value = value.into_string().map_err(CliError::NonUtf8Argument)?;
                    args.push(value);
                }
                unknown => return Err(CliError::UnknownOption(unknown.to_owned())),
            }
            continue;
        }

        if image.is_none() {
            let text = argument.into_string().map_err(CliError::NonUtf8Argument)?;
            image = Some(text);
            continue;
        }
        if elf.is_none() {
            elf = Some(PathBuf::from(argument));
            continue;
        }
        let text = argument.into_string().map_err(CliError::NonUtf8Argument)?;
        return Err(CliError::UnexpectedArgument(text));
    }

    let Some(image) = image else {
        return Err(CliError::MissingBuildImage);
    };
    let Some(elf) = elf else {
        return Err(CliError::MissingElf);
    };
    Ok(Command::Image(ImageCommand::Build(ImageBuildArgs {
        image,
        elf,
        args,
        store,
    })))
}

/// `image list`の引数をparseする。positionalは取らない。
fn parse_image_list(arguments: impl IntoIterator<Item = OsString>) -> Result<Command, CliError> {
    let mut store: Option<PathBuf> = None;

    let pending: Vec<OsString> = arguments.into_iter().collect();
    let mut rest = pending.into_iter().peekable();
    while let Some(argument) = rest.next() {
        if is_option(&argument) {
            let text = argument.into_string().map_err(CliError::NonUtf8Argument)?;
            let (name, inline_value) = match text.split_once('=') {
                Some((name, value)) => (name.to_owned(), Some(OsString::from(value))),
                None => (text, None),
            };
            match name.as_str() {
                "--store" => {
                    if store.is_some() {
                        return Err(CliError::DuplicateOption("--store"));
                    }
                    let value = match inline_value {
                        Some(value) => value,
                        None => rest.next().ok_or(CliError::MissingValue("--store"))?,
                    };
                    store = Some(PathBuf::from(value));
                }
                unknown => return Err(CliError::UnknownOption(unknown.to_owned())),
            }
            continue;
        }

        let text = argument.into_string().map_err(CliError::NonUtf8Argument)?;
        return Err(CliError::UnexpectedArgument(text));
    }

    Ok(Command::Image(ImageCommand::List(ImageListArgs { store })))
}

/// `image inspect`の引数をparseする。optionはIMAGEの前後どこに置いてもよい。
fn parse_image_inspect(arguments: impl IntoIterator<Item = OsString>) -> Result<Command, CliError> {
    let mut image: Option<String> = None;
    let mut store: Option<PathBuf> = None;

    let pending: Vec<OsString> = arguments.into_iter().collect();
    let mut rest = pending.into_iter().peekable();
    while let Some(argument) = rest.next() {
        if is_option(&argument) {
            let text = argument.into_string().map_err(CliError::NonUtf8Argument)?;
            let (name, inline_value) = match text.split_once('=') {
                Some((name, value)) => (name.to_owned(), Some(OsString::from(value))),
                None => (text, None),
            };
            match name.as_str() {
                "--store" => {
                    if store.is_some() {
                        return Err(CliError::DuplicateOption("--store"));
                    }
                    let value = match inline_value {
                        Some(value) => value,
                        None => rest.next().ok_or(CliError::MissingValue("--store"))?,
                    };
                    store = Some(PathBuf::from(value));
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
        return Err(CliError::MissingInspectImage);
    };
    Ok(Command::Image(ImageCommand::Inspect(ImageInspectArgs {
        image,
        store,
    })))
}

/// 引数を取らないcommandを確定する。後続のtokenは、optionの形でも
/// 受け付けず、打ち間違いとして型付きerrorにする。
fn parse_bare_command(
    arguments: impl Iterator<Item = OsString>,
    command: Command,
) -> Result<Command, CliError> {
    let mut arguments = arguments;
    if let Some(extra) = arguments.next() {
        let extra = extra.into_string().map_err(CliError::NonUtf8Argument)?;
        return Err(CliError::UnexpectedArgument(extra));
    }
    Ok(command)
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

/// parse済み`image build`引数と環境からstore pathを解決する。
pub fn resolve_build(args: &ImageBuildArgs, env: &dyn Environ) -> Result<ResolvedBuild, CliError> {
    let store = match &args.store {
        Some(path) => path.clone(),
        None => match env.store_override() {
            Some(path) => PathBuf::from(path),
            None => default_store_root(env)?,
        },
    };
    Ok(ResolvedBuild {
        image: args.image.clone(),
        elf: args.elf.clone(),
        args: args.args.clone(),
        store,
    })
}

/// parse済み`image list`引数と環境からstore pathを解決する。
pub fn resolve_list(args: &ImageListArgs, env: &dyn Environ) -> Result<ResolvedList, CliError> {
    let store = match &args.store {
        Some(path) => path.clone(),
        None => match env.store_override() {
            Some(path) => PathBuf::from(path),
            None => default_store_root(env)?,
        },
    };
    Ok(ResolvedList { store })
}

/// parse済み`image inspect`引数と環境からstore pathを解決する。
pub fn resolve_inspect(
    args: &ImageInspectArgs,
    env: &dyn Environ,
) -> Result<ResolvedInspect, CliError> {
    let store = match &args.store {
        Some(path) => path.clone(),
        None => match env.store_override() {
            Some(path) => PathBuf::from(path),
            None => default_store_root(env)?,
        },
    };
    Ok(ResolvedInspect {
        image: args.image.clone(),
        store,
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

    // Catches misrouting the bare help and version commands to run or
    // to an error.
    #[test]
    fn parses_help_and_version_commands() {
        assert_eq!(parse(["help"]), Ok(Command::Help));
        assert_eq!(parse(["--help"]), Ok(Command::Help));
        assert_eq!(parse(["--version"]), Ok(Command::Version));
    }

    // Catches silently accepting trailing tokens after a bare command,
    // which would hide a mistyped invocation.
    #[test]
    fn rejects_trailing_arguments_after_help_and_version() {
        assert_eq!(
            parse(["help", "extra"]),
            Err(CliError::UnexpectedArgument("extra".to_owned()))
        );
        assert_eq!(
            parse(["--help", "extra"]),
            Err(CliError::UnexpectedArgument("extra".to_owned()))
        );
        assert_eq!(
            parse(["--version", "extra"]),
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

    // Catches silently preferring the last of two timeouts, which can
    // replace a short timeout with a much longer one.
    #[test]
    fn rejects_duplicate_timeout_options_in_every_spelling() {
        assert_eq!(
            parse(["run", "--timeout-ms", "1", "--timeout-ms", "200", "hello"]),
            Err(CliError::DuplicateOption("--timeout-ms"))
        );
        assert_eq!(
            parse(["run", "--timeout-ms=1", "--timeout-ms=200", "hello"]),
            Err(CliError::DuplicateOption("--timeout-ms"))
        );
        assert_eq!(
            parse(["run", "--timeout-ms", "1", "--timeout-ms=200", "hello"]),
            Err(CliError::DuplicateOption("--timeout-ms"))
        );
        assert_eq!(
            parse(["run", "--timeout-ms=1", "--timeout-ms", "200", "hello"]),
            Err(CliError::DuplicateOption("--timeout-ms"))
        );
    }

    // Catches expanding equals syntax inside a consumed path value: a
    // kernel path beginning with `--` and containing `=` is opaque.
    #[test]
    fn keeps_equals_in_a_consumed_path_value_opaque() {
        assert_eq!(
            parse(["run", "--kernel", "--foo=bar", "hello"]),
            Ok(Command::Run(RunArgs {
                image: "hello".into(),
                store: None,
                kernel: Some(PathBuf::from("--foo=bar")),
                timeout: Duration::from_secs(5),
            }))
        );
        assert_eq!(
            parse(["run", "--store", "--foo=bar", "hello"]),
            Ok(Command::Run(RunArgs {
                image: "hello".into(),
                store: Some(PathBuf::from("--foo=bar")),
                kernel: None,
                timeout: Duration::from_secs(5),
            }))
        );
        assert_eq!(
            parse(["run", "--kernel", "--foo=bar"]),
            Err(CliError::MissingImage)
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

    // Catches interpreting undecodable trailing bytes after a bare
    // command as an accepted invocation.
    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_trailing_arguments_after_bare_commands() {
        use std::os::unix::ffi::OsStringExt;

        for command in ["help", "--help", "--version"] {
            let invalid = OsString::from_vec(vec![0xff]);
            assert_eq!(
                parse_os([OsString::from(command), invalid.clone()]),
                Err(CliError::NonUtf8Argument(invalid)),
                "{command} must reject undecodable trailing bytes"
            );
        }
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
        let Command::Run(args) = parsed else {
            panic!("expected a Run command");
        };
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
            "usage: minictr run [--store PATH] [--kernel PATH] [--timeout-ms N] IMAGE\nusage: minictr image build [--store PATH] [--arg VALUE]... IMAGE ELF\nusage: minictr image list [--store PATH]\nusage: minictr image inspect [--store PATH] IMAGE"
        );
    }

    // Catches drifting from the Task 18 acceptance syntax: `image build`
    // takes an image tag and an ELF path with no options by default.
    #[test]
    fn parses_image_build_with_defaults() {
        assert_eq!(
            parse(["image", "build", "hello", "./hello.elf"]),
            Ok(Command::Image(ImageCommand::Build(ImageBuildArgs {
                image: "hello".into(),
                elf: PathBuf::from("./hello.elf"),
                args: Vec::new(),
                store: None,
            })))
        );
    }

    // Catches rejecting a valid option order: options may precede, split,
    // or follow the IMAGE and ELF positionals.
    #[test]
    fn parses_image_build_options_in_any_order() {
        let expected = Command::Image(ImageCommand::Build(ImageBuildArgs {
            image: "hello".into(),
            elf: PathBuf::from("./hello.elf"),
            args: vec!["fast".to_owned()],
            store: Some(PathBuf::from("/data/store")),
        }));
        assert_eq!(
            parse([
                "image",
                "build",
                "--store",
                "/data/store",
                "--arg",
                "fast",
                "hello",
                "./hello.elf",
            ]),
            Ok(expected.clone())
        );
        assert_eq!(
            parse([
                "image",
                "build",
                "--arg=fast",
                "hello",
                "--store=/data/store",
                "./hello.elf",
            ]),
            Ok(expected.clone())
        );
        assert_eq!(
            parse([
                "image",
                "build",
                "hello",
                "./hello.elf",
                "--store",
                "/data/store",
                "--arg",
                "fast",
            ]),
            Ok(expected)
        );
    }

    // Catches keeping only the last `--arg`: every occurrence appends in order.
    #[test]
    fn accumulates_repeated_arg_options_in_order() {
        assert_eq!(
            parse([
                "image",
                "build",
                "--arg",
                "first",
                "--arg=second",
                "--arg",
                "third",
                "hello",
                "./hello.elf",
            ]),
            Ok(Command::Image(ImageCommand::Build(ImageBuildArgs {
                image: "hello".into(),
                elf: PathBuf::from("./hello.elf"),
                args: vec!["first".to_owned(), "second".to_owned(), "third".to_owned(),],
                store: None,
            })))
        );
    }

    // Catches silently preferring one of two `--store` values for `image build`.
    #[test]
    fn rejects_duplicate_store_for_image_build() {
        assert_eq!(
            parse([
                "image",
                "build",
                "--store",
                "/a",
                "--store",
                "/b",
                "hello",
                "./hello.elf",
            ]),
            Err(CliError::DuplicateOption("--store"))
        );
        assert_eq!(
            parse([
                "image",
                "build",
                "--store=/a",
                "--store=/b",
                "hello",
                "./hello.elf",
            ]),
            Err(CliError::DuplicateOption("--store"))
        );
    }

    // Catches running `image build` without an image tag or an ELF path,
    // or with a second unexpected positional.
    #[test]
    fn rejects_missing_image_elf_and_extra_positional_for_image_build() {
        assert_eq!(parse(["image", "build"]), Err(CliError::MissingBuildImage));
        assert_eq!(
            parse(["image", "build", "hello"]),
            Err(CliError::MissingElf)
        );
        assert_eq!(
            parse(["image", "build", "hello", "./hello.elf", "--arg"]),
            Err(CliError::MissingValue("--arg"))
        );
        assert_eq!(
            parse(["image", "build", "hello", "./a.elf", "./b.elf"]),
            Err(CliError::UnexpectedArgument("./b.elf".to_owned()))
        );
    }

    // Catches misrouting a bare `image` or an unknown subcommand to build.
    #[test]
    fn rejects_missing_and_unknown_image_subcommands() {
        assert_eq!(parse(["image"]), Err(CliError::MissingCommand));
        assert_eq!(
            parse(["image", "prune"]),
            Err(CliError::UnknownCommand("prune".to_owned()))
        );
        assert_eq!(
            parse(["image", "build", "--volume", "data", "hello", "./hello.elf"]),
            Err(CliError::UnknownOption("--volume".to_owned()))
        );
    }

    // Catches rejecting undecodable ELF and store paths while accepting an
    // undecodable image tag: only the tag must be UTF-8.
    #[cfg(unix)]
    #[test]
    fn keeps_non_utf8_build_paths_as_paths() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let raw = OsString::from_vec(vec![0x2f, 0x74, 0x6d, 0x70, 0xff]);
        let parsed = parse_os([
            OsString::from("image"),
            OsString::from("build"),
            OsString::from("hello"),
            raw.clone(),
        ])
        .unwrap();
        let Command::Image(ImageCommand::Build(args)) = parsed else {
            panic!("expected an image build command");
        };
        assert_eq!(args.elf.as_os_str().as_bytes(), raw.as_bytes());

        let parsed = parse_os([
            OsString::from("image"),
            OsString::from("build"),
            OsString::from("--store"),
            raw.clone(),
            OsString::from("hello"),
            OsString::from("./hello.elf"),
        ])
        .unwrap();
        let Command::Image(ImageCommand::Build(args)) = parsed else {
            panic!("expected an image build command");
        };
        assert_eq!(args.store.unwrap().as_os_str().as_bytes(), raw.as_bytes());

        assert_eq!(
            parse_os([
                OsString::from("image"),
                OsString::from("build"),
                raw.clone(),
                OsString::from("./hello.elf"),
            ]),
            Err(CliError::NonUtf8Argument(raw.clone()))
        );
        assert_eq!(
            parse_os([
                OsString::from("image"),
                OsString::from("build"),
                OsString::from("hello"),
                OsString::from("./hello.elf"),
                OsString::from("--arg"),
                raw.clone(),
            ]),
            Err(CliError::NonUtf8Argument(raw))
        );
    }

    // Catches resolving the build store from the wrong environment variable
    // or losing an explicit CLI path to an override.
    #[test]
    fn resolves_build_store_from_the_documented_environment() {
        let args = ImageBuildArgs {
            image: "hello".into(),
            elf: PathBuf::from("./hello.elf"),
            args: vec!["fast".to_owned()],
            store: None,
        };
        let resolved = resolve_build(&args, &home_env()).unwrap();
        assert_eq!(
            resolved,
            ResolvedBuild {
                image: "hello".into(),
                elf: PathBuf::from("./hello.elf"),
                args: vec!["fast".to_owned()],
                store: PathBuf::from("/home/test/.minicontainer"),
            }
        );

        let env = FakeEnv {
            store: Some(OsString::from("/env/store")),
            kernel: None,
            home: Some(OsString::from("/home/test")),
        };
        assert_eq!(
            resolve_build(&args, &env).unwrap().store,
            PathBuf::from("/env/store")
        );

        let explicit = ImageBuildArgs {
            store: Some(PathBuf::from("/cli/store")),
            ..args.clone()
        };
        assert_eq!(
            resolve_build(&explicit, &env).unwrap().store,
            PathBuf::from("/cli/store")
        );

        let env = FakeEnv {
            store: None,
            kernel: None,
            home: None,
        };
        assert_eq!(resolve_build(&args, &env), Err(CliError::MissingHome));
    }

    // Catches drifting from the Task 19 acceptance syntax: `image list`
    // takes no image and an optional store.
    #[test]
    fn parses_image_list_with_defaults() {
        assert_eq!(
            parse(["image", "list"]),
            Ok(Command::Image(ImageCommand::List(ImageListArgs {
                store: None
            })))
        );
        assert_eq!(
            parse(["image", "list", "--store", "/data/store"]),
            Ok(Command::Image(ImageCommand::List(ImageListArgs {
                store: Some(PathBuf::from("/data/store")),
            })))
        );
        assert_eq!(
            parse(["image", "list", "--store=/data/store"]),
            Ok(Command::Image(ImageCommand::List(ImageListArgs {
                store: Some(PathBuf::from("/data/store")),
            })))
        );
    }

    // Catches silently accepting positionals or repeated stores for `image list`.
    #[test]
    fn rejects_image_list_extras() {
        assert_eq!(
            parse(["image", "list", "hello"]),
            Err(CliError::UnexpectedArgument("hello".to_owned()))
        );
        assert_eq!(
            parse(["image", "list", "--store", "/a", "--store", "/b"]),
            Err(CliError::DuplicateOption("--store"))
        );
        assert_eq!(
            parse(["image", "list", "--volume", "data"]),
            Err(CliError::UnknownOption("--volume".to_owned()))
        );
        assert_eq!(
            parse(["image", "list", "--store"]),
            Err(CliError::MissingValue("--store"))
        );
    }

    // Catches drifting from the Task 19 acceptance syntax: `image inspect`
    // takes an image tag with an optional store in any order.
    #[test]
    fn parses_image_inspect_with_defaults() {
        assert_eq!(
            parse(["image", "inspect", "hello"]),
            Ok(Command::Image(ImageCommand::Inspect(ImageInspectArgs {
                image: "hello".into(),
                store: None,
            })))
        );
        assert_eq!(
            parse(["image", "inspect", "--store", "/data/store", "hello"]),
            Ok(Command::Image(ImageCommand::Inspect(ImageInspectArgs {
                image: "hello".into(),
                store: Some(PathBuf::from("/data/store")),
            })))
        );
        assert_eq!(
            parse(["image", "inspect", "hello", "--store=/data/store"]),
            Ok(Command::Image(ImageCommand::Inspect(ImageInspectArgs {
                image: "hello".into(),
                store: Some(PathBuf::from("/data/store")),
            })))
        );
    }

    // Catches running `image inspect` without an image tag or with extras.
    #[test]
    fn rejects_missing_image_and_extras_for_image_inspect() {
        assert_eq!(
            parse(["image", "inspect"]),
            Err(CliError::MissingInspectImage)
        );
        assert_eq!(
            parse(["image", "inspect", "hello", "extra"]),
            Err(CliError::UnexpectedArgument("extra".to_owned()))
        );
        assert_eq!(
            parse([
                "image", "inspect", "--store", "/a", "--store", "/b", "hello"
            ]),
            Err(CliError::DuplicateOption("--store"))
        );
        assert_eq!(
            parse(["image", "inspect", "--volume", "data", "hello"]),
            Err(CliError::UnknownOption("--volume".to_owned()))
        );
        assert_eq!(
            parse(["image", "inspect", "hello", "--store"]),
            Err(CliError::MissingValue("--store"))
        );
    }

    // Catches resolving the list and inspect stores from anything but the
    // documented explicit option, environment, and HOME order.
    #[test]
    fn resolves_list_and_inspect_stores_from_the_documented_environment() {
        let list = ImageListArgs { store: None };
        assert_eq!(
            resolve_list(&list, &home_env()).unwrap(),
            ResolvedList {
                store: PathBuf::from("/home/test/.minicontainer"),
            }
        );
        let inspect = ImageInspectArgs {
            image: "hello".into(),
            store: None,
        };
        assert_eq!(
            resolve_inspect(&inspect, &home_env()).unwrap(),
            ResolvedInspect {
                image: "hello".into(),
                store: PathBuf::from("/home/test/.minicontainer"),
            }
        );

        let env = FakeEnv {
            store: Some(OsString::from("/env/store")),
            kernel: None,
            home: Some(OsString::from("/home/test")),
        };
        assert_eq!(
            resolve_list(&list, &env).unwrap().store,
            PathBuf::from("/env/store")
        );
        assert_eq!(
            resolve_inspect(&inspect, &env).unwrap().store,
            PathBuf::from("/env/store")
        );

        let explicit_list = ImageListArgs {
            store: Some(PathBuf::from("/cli/store")),
        };
        assert_eq!(
            resolve_list(&explicit_list, &env).unwrap().store,
            PathBuf::from("/cli/store")
        );
        let explicit_inspect = ImageInspectArgs {
            image: "hello".into(),
            store: Some(PathBuf::from("/cli/store")),
        };
        assert_eq!(
            resolve_inspect(&explicit_inspect, &env).unwrap(),
            ResolvedInspect {
                image: "hello".into(),
                store: PathBuf::from("/cli/store"),
            }
        );

        let env = FakeEnv {
            store: None,
            kernel: None,
            home: None,
        };
        assert_eq!(resolve_list(&list, &env), Err(CliError::MissingHome));
        assert_eq!(resolve_inspect(&inspect, &env), Err(CliError::MissingHome));
    }
}
