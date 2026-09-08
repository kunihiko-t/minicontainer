# v0.1.0配布手順

`v0.1.0`タグのpushを契機に、macOS arm64用とLinux x86_64用の配布アーカイブを構築する。
配布物はworkflow artifactとして保存し、GitHub Releaseの作成、署名、notarizationは行わない。

## tag前の検証

tagを打つ前に、`main`の先端で次が成功していることを確認する。

```sh
cargo xtask setup
cargo xtask check-host
cargo xtask check
```

Ubuntu CIの`check`とmacOS CIの`check-host`も同じcommitで成功させる。
tag、Cargo package version、archive名のversionは`0.1.0`で一致させる。

## 配布内容

matrixは`macos-15`と`ubuntu-24.04`であり、成果物名のtargetは`aarch64-apple-darwin`と`x86_64-unknown-linux-gnu`である。
各jobは次の二つのfileをuploadする。

- `minicontainer-v0.1.0-<target>.tar.gz`
- `minicontainer-v0.1.0-<target>.tar.gz.sha256`

archiveを展開すると次の配置になる。

```text
minicontainer-v0.1.0-<target>/
  bin/minictr
  share/minicontainer/minios-kernel
  README.md
  LICENSE-MIT
  LICENSE-APACHE
```

`minictr`はrelease buildであり、実行bitを付けて格納する。
`minios-kernel`は固定revision `9be99255a59d58d19db25b835af0e28a8d2a4036` のminiOSを`--release --locked --target riscv64gc-unknown-none-elf`でbuildしたものである。
配布kernelは[脅威モデル](threat-model.md)の信頼する計算基盤と同じ前提で扱う。

## checksumの確認

checksum fileは`<hash>  <file名>`の一行である。
Linuxでは`sha256sum`、macOSでは`shasum`で確認する。

```sh
sha256sum -c minicontainer-v0.1.0-x86_64-unknown-linux-gnu.tar.gz.sha256
shasum -a 256 -c minicontainer-v0.1.0-aarch64-apple-darwin.tar.gz.sha256
```

## 導入と実行

展開したdirectoryから`bin/minictr`を直接実行する。

```sh
bin/minictr --help
bin/minictr --version
bin/minictr run --store "$STORE" --kernel share/minicontainer/minios-kernel hello
```

`--kernel`にはarchive内の`share/minicontainer/minios-kernel`を渡す。
imageの登録と実行は`image build`、`image inspect`、`run`で行い、commandの形は同梱READMEのクイックスタートと同じである。
archiveには同梱ゲスト例のsourceを含まないため、利用者が用意した静的RISC-V 64 ELFを使う。

## 未対応事項

- GitHub Releaseの作成と配布物の添付は行わない。
- 署名、notarization、Homebrew formulaは用意しない。
- Windows用とLinux arm64用のarchiveは作らない。
- 配布archive自体の再現可能build (bit一致) は保証しない。
