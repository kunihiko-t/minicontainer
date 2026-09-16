# 配布手順

`v*`タグのpushを契機に、macOS arm64用とLinux x86_64用の配布アーカイブを構築し、GitHub Releaseへ添付する。
各アーカイブとchecksumには、GitHub artifact attestationによる署名済みprovenanceを付ける。
notarizationは行わない。

## tag前の検証

tagを打つ前に、`main`の先端で次が成功していることを確認する。

```sh
cargo xtask setup
cargo xtask check-host
cargo xtask check
```

Ubuntu CIの`check`とmacOS CIの`check-host`も同じcommitで成功させる。
Ubuntu CIは配布archiveのsmokeも実行するため、その成功も確認する。
`check`のE2E末尾には反復・中断・強制終了後のcleanupを反復検証するstress節が含まれるため、resource漏れの退行もこの確認で担保する。
tag、Cargo package version、archive名のversionは一致させる。

releaseに含まれる契約変更は、release notesでbreakingと互換に分類して記載する。
分類の基準は[互換性と移行の方針](compatibility.md)に従い、Ubuntu CIの互換性fixture検証が同じcommitで成功していることを確認する。

## 配布内容

matrixは`macos-15`と`ubuntu-24.04`であり、成果物名のtargetは`aarch64-apple-darwin`と`x86_64-unknown-linux-gnu`である。
`<version>`はtag名 (`v0.2.0`など) そのままである。
archiveは`cargo xtask dist`が`--version`へtag名を渡してbuildする。各jobは次の二つのfileを作り、tagのReleaseへassetとして添付する。

- `minicontainer-<version>-<target>.tar.gz`
- `minicontainer-<version>-<target>.tar.gz.sha256`

archiveを展開すると次の配置になる。

```text
minicontainer-<version>-<target>/
  minictr
  kernel/minios.bin
  LICENSE-MIT
  LICENSE-APACHE
  MANIFEST.txt
  SHA256SUMS
```

`minictr`はrelease buildであり、実行bitを付けて格納する。
`kernel/minios.bin`は固定revision `9be99255a59d58d19db25b835af0e28a8d2a4036` のminiOSを`--release --locked --target riscv64gc-unknown-none-elf`でbuildしたものである。
配布kernelは[脅威モデル](threat-model.md)の信頼する計算基盤と同じ前提で扱う。
`MANIFEST.txt`はarchive version、target、各fileのmodeとSHA-256を記録する。
`SHA256SUMS`は`sha256sum -c`形式でpayloadと`MANIFEST.txt`を検査できる。

公開gateはxtaskと`minictr`のversion一致、release workflowの`cargo xtask dist`使用とtagからのversion受け渡し、attestationとasset添付の設定を検査する。
`dist`は`tar`や`sha256sum`を使わずRust toolchainだけでarchiveを組み立て、完成品を読み戻して検証してから成功を報告する。

## workflow権限

workflow全体の既定権限は`contents: read`であり、書き込みはjobごとに最小限だけ付与する。
`build-archives`はprovenance生成のため`attestations: write`と`id-token: write`だけを持ち、`publish-release`はRelease作成のため`contents: write`だけを持つ。
triggerはtag pushのみであり、forkやpull requestからrelease権限を得る経路はない。
使うactionはfull commit SHAでpinし、公開前検証のworkflow policy testがtrigger、権限、pin、attestation設定を検査する。

## checksumとprovenanceの確認

checksum fileは`<hash>  <file名>`の一行である。
Linuxでは`sha256sum`、macOSでは`shasum`で確認する。

```sh
sha256sum -c 'minicontainer-<version>-x86_64-unknown-linux-gnu.tar.gz.sha256'
shasum -a 256 -c 'minicontainer-<version>-aarch64-apple-darwin.tar.gz.sha256'
```

展開後はarchive内の`SHA256SUMS`でも内容を検査できる。

```sh
(cd minicontainer-<version>-x86_64-unknown-linux-gnu && sha256sum -c SHA256SUMS)
```

次に、artifactがこのrepositoryのrelease workflowから生成されたことを、GitHub CLIで検証する。

```sh
gh attestation verify 'minicontainer-<version>-x86_64-unknown-linux-gnu.tar.gz' --owner kunihiko-t
gh attestation verify 'minicontainer-<version>-aarch64-apple-darwin.tar.gz' --owner kunihiko-t
```

checksumはfileの完全性だけを示し、生成元の証明にはならない。
配布物の出所を確認する場合は、必ずattestationの検証まで行う。

## 導入と実行

展開したdirectoryから`minictr`を直接実行する。

```sh
./minictr --help
./minictr --version
./minictr run --store "$STORE" --kernel kernel/minios.bin hello
```

`--kernel`にはarchive内の`kernel/minios.bin`を渡す。
imageの登録と実行は`image build`、`image inspect`、`run`で行い、commandの形はREADMEのクイックスタートと同じである。
archiveには同梱ゲスト例のsourceを含まないため、利用者が用意した静的RISC-V 64 ELFを使う。

## 未対応事項

- 署名、notarization、Homebrew formulaは用意しない。配布物の真正性はattestationで確認する。
- Windows用とLinux arm64用のarchiveは作らない。
- 配布archive自体の再現可能build (bit一致) は保証しない。
- archiveにREADMEや導入手順書は含まない。導入手順はこの文書が正である。
