# guest-hello

MiniContainerで動く最小のゲスト例である。
Guest ABIの`write`と`exit`だけを使い、標準出力へ`hello from guest`、標準エラー出力へ`guest stderr`と書いて終了コード42で終わる。

## build

リポジトリ直下から実行する。

```sh
cargo build --manifest-path examples/guest-hello/Cargo.toml --target riscv64gc-unknown-none-elf --release --locked --config 'target.riscv64gc-unknown-none-elf.rustflags=["-C", "link-arg=-Tlinker.ld"]' --config 'build.target-dir="target/guest-hello"'
```

成果物は`target/guest-hello/riscv64gc-unknown-none-elf/release/guest-hello`である。
このdirectory内で実行する場合は、`cargo build --release --locked`の短縮形も使える。

## ABIの読み方

system call番号とfile descriptorは[source](src/main.rs)内の`minios_abi::syscall`参照だけが定義元である。
register配置とlink addressの説明は[第3章](../../docs/guide/03-static-elf-abi.md)を参照する。
