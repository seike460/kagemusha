# AGENTS.md — kagemusha

このファイルは、kagemusha のリポジトリで作業する AI エージェントと人のための約束です。

## このリポジトリ

- kagemusha は、AWS Lambda MicroVMs の中で動く小さなエージェントです。コンテナの起動役として振る舞い、ライフサイクルフックを中継し、個体識別を行い、停止・終了の前にテレメトリを同期送信し、コスト推定のための使用量を記録します。
- 名前の由来は「影武者」です。アプリ本体ではないが、起動役（entrypoint）としてその影に立つ者、という意味です。

## 言語と道具

- すべて Rust で書きます（VM 内で動く単一の静的バイナリのため）。
- 道具の版は mise で固定します。

## 設計の約束（崩さないこと）

- **実行時にエージェントが失敗しても、利用者の処理を止めません（fail-open）。** 実行時フック（run/resume/suspend/terminate）でエージェント側の処理が失敗しても、最終的な HTTP 応答は 200 を返します。ただしアプリのフック応答が失敗の場合は、その失敗を透過します。
- **ビルド時は静止し、`/run` で起動します。** ID・乱数の種・接続・秘密情報を、スナップショットの前に作りません。identity の修復（machine-id/hostname）は `/run` フックで行います。
- **送信は応答の前に、同期で試みます。** `/suspend` と `/terminate` では、OTLP の endpoint が設定されていれば、設定された残り時間の中で flush（期限付きの送信 1 回）を終えてから 200 を返します。送信に失敗しても、警告を出して続けます（fail-open）。collector に届いたことまでは保証しません。
- **ID はメトリクスのラベルに入れません。** microvmId・tenant・session の ID は、ログの属性にだけ付けます（trace exporter を載せたら span 属性にも）。メトリクスの次元は有界のもの（hook 名、結果、image 名）に限ります。
- **使用量の事実だけを送り、単価はクエリ側で掛けます。** エージェントに単価を埋め込みません。
- **秘密情報をイメージの環境変数に入れません。** VM ごとの OTLP 認証情報は `KAGEMUSHA_OTLP_HEADERS_FILE`（`/run` で `hooks.d/run` より前に読む `k=v` ファイル。プラットフォームの `/run` より前に置く。読めなかったときは、非空のヘッダが得られるまで、後から届く `/run` ごとに再試行）が正式経路です。`runHookPayload` の中身はヘッダにも環境変数にも載せません。
- **MicroVM の中のアプリは、信頼できないコードとして扱います。**

## 実測に基づく設計入力（出典つき）

- `runHookPayload` は二重包装で届きます（`{"runHookPayload": "{\"...\"}"}`。実測 2026-08-05、[microvms-agentd の platform internals](https://laithalsaadoon.github.io/microvms-agentd/internals/platform/)）。
- `runHookPayload` の実測上限は 4096B です（API モデルの記載 16384 より小さい。実測 2026-08-07、同上）。
- デーモン起動時の identity 修復はスナップショットに焼き込まれ全 VM で共有されます。`/run` で修復すると個体別になります（実測 2026-09-24、同上）。
- `additionalOsCapabilities: ["ALL"]` でも CAP_SYS_ADMIN が無い計測があります（実測 2026-09-12、同上）。identity 修復は best-effort にし、結果を報告します。
- ゲストの `MemTotal` は baseline ではなく provisioned ceiling を報告します（実測 2026-08-07、同上）。使用量の計測は cgroup v2 を使います。

## Git

- Conventional Commits（feat / fix / chore / docs / refactor / test）を使います。
- 秘密情報を commit しません。

## 書いてはいけない情報

- AWS アカウント ID、エンドポイントの URL、トークン、認証情報

## 検証

- `cargo fmt --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo test` を通します。CI（`.github/workflows/ci.yml`）も同じ 3 本を、Linux と macOS の両方で走らせます。CI の clippy と test には `--locked` を付け、`Cargo.lock` と食い違えば失敗させます。
- CI は、ほかに次の 3 つも確かめます。
  - `cargo build --release --locked` — リリースプロファイル（LTO・`panic = "abort"`）でのビルド。Linux だけで行います。
  - `rust-version`（1.88）の toolchain での `cargo check --all-targets --all-features --locked` — 宣言した MSRV。`rust-version` を変えたら、`msrv` ジョブの toolchain も合わせます。
  - `docker build -f examples/Dockerfile .` — musl の静的バイナリとイメージ例。
- `cargo deny check advisories` で、依存に RustSec の advisory が無いことを確かめます。`.github/workflows/audit.yml` が、push・PR と毎週の定期実行で走らせます。設定はリポジトリ直下の `deny.toml` です。CI が走らせるのは advisories の検査だけなので、効くのは `[advisories]` の節だけです。
- ワークフローの action は、コミットの SHA で固定し、タグ名をコメントで添えます。更新は Dependabot（`.github/dependabot.yml`）で追います。
- 修正は end-to-end で確かめます。部品だけで「完了」としません。
  - `crates/kagemusha/tests/hooks.rs` — in-process のフック中継・順序・fail-open・identity・OTLP 経路。
  - `crates/kagemusha/tests/supervisor.rs` — 実バイナリでのシグナル転送・終了コード・graceful shutdown。
  - `crates/kagemusha/tests/e2e.rs` — 実バイナリ × hook シミュレータ × mock OTLP collector のフルライフサイクル。
  - `examples/Dockerfile` — イメージ焼き込みの形を示します。変えたら `docker build -f examples/Dockerfile .` で確かめます。
