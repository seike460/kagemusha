# kagemusha

kagemusha は、AWS Lambda MicroVMs の中でコンテナの起動役（PID 1）として動く小さなエージェントです。ライフサイクルフックを中継し、`/run` で個体識別を行い、停止と終了の前にテレメトリを同期送信し、コスト推定のための cgroup v2 の使用量を記録します。送り先は、OTLP/HTTP の JSON エンコードを受け付けるバックエンド（OpenTelemetry Collector など）です。

[English README](README.md)

## 機能

- **ライフサイクルフックの中継** — `ready`、`validate`、`run`、`resume`、`suspend`、`terminate` をアプリのフックサーバーへ順番に中継します。実行時フックは fail-open です。エージェント側の失敗は 200 を返し、アプリの実応答（エラー応答を含む）はそのまま透過します。透過するのは、status・本文と、本文の読み方を示す `Content-Type`・`Content-Encoding` です（圧縮された本文は展開せず、そのまま中継します）。ビルドフック（`ready`/`validate`）は正直に報告します。`ready` はアプリ未起動の間 503 を返し、合成した中継失敗は 502/504 です。
- **個体識別の修復** — 有効な `microvmId` を含む最初の `/run` で、`microvmId` から machine-id を導出し、`/run/machine-id`・`/etc/machine-id`・hostname を書き換えます。スナップショットに焼き込まれた共通の値を個体別に直します。あわせて VM ごとの OTLP 認証情報をファイルから読み込みます。
- **`hooks.d` ユーザースクリプト** — `<hooks_dir>/<hook>/` 直下の実行可能ファイルを、`run`/`resume`/`suspend`/`terminate` で名前順に実行します。独立したプロセスグループで、許可リスト済みの清潔な環境で動きます。スクリプトが見えるのは次だけです。`PATH`（継承）、`KAGEMUSHA_HOOK`（フック名）、`KAGEMUSHA_HOOK_DIR`（フック別 dir。例： `<hooks_dir>/run/`）、`KAGEMUSHA_IMAGE_NAME`（env 安全な場合）、`KAGEMUSHA_MICROVM_ID`（`/run` で確定後）。それ以外はすべて剥がされます（`env_clear`）。`KAGEMUSHA_HOOKS_DIR` をはじめとするエージェント設定変数も含みます。
- **テレメトリの同期送信** — OTLP の endpoint を設定していれば、`/suspend` と `/terminate` は、最新の使用量を期限付きの OTLP/HTTP POST で 1 回送ってから 200 を返します。endpoint が無ければ、何も送りません。POST が失敗したり時間切れになったりしたときは、警告をログに出し、フックには応答します（fail-open）。
- **プロセス監督** — アプリは独立したプロセスグループで起動し、TERM/INT/QUIT/HUP/USR1/USR2 をグループ全体へ転送します。ゾンビは一箇所で回収します（エージェントが PID 1 です）。`/terminate` は SIGTERM → 猶予 → SIGKILL → 最終 flush の順で、アプリの終了コードをそのまま返します。
- **コスト推定のための使用量** — cgroup v2 の `cpu.stat`（usage/user/system/throttled）、`memory.current`、`memory.peak`、`memory.max`、稼働時間を採ります。事実だけを送り、率や課金の計算は collector・クエリ側の仕事です。
- **エージェント内部は fail-open** — collector の故障、スクリプトの固まり、アプリ側フックの破損、cgroup 不在があっても、ライフサイクルの応答は止まりません。

## 使い方

```sh
cargo build --release
# VM の起動役として動かします。アプリは子プロセスとして監督されます:
./target/release/kagemusha -- /path/to/your-app arg1 arg2
```

フックサーバーは `0.0.0.0:9000` で待ち受け、`/aws/lambda-microvms/runtime/v1/{ready,validate,run,resume,suspend,terminate}` のプラットフォームのパスに応答します。

MicroVM の外（開発用のホストなど）で動かすときは、`KAGEMUSHA_IDENTITY_REPAIR=false` にするか、`KAGEMUSHA_IDENTITY_ROOT` を `$(mktemp -d)` などの作業用ディレクトリに向けてください。既定のままだと、有効な `microvmId` を含む最初の `/run` で、ホストの `/etc/machine-id`・`/run/machine-id`・`/etc/hostname` を書き換えます。エージェントを root で動かしていれば、ホスト名も変わります。

## 設定

すべて環境変数で設定します。`kagemusha --config FILE` で渡す JSON 設定ファイルでも、同じ値を設定できます。キーは、変数名から `KAGEMUSHA_` を除いて小文字にしたものです（例: `hook_budget_ms`、`app_hook_base`）。`identity_repair` は JSON の真偽値で書きます。`KAGEMUSHA_OTLP_HEADERS`、`OTEL_*` の変数、`RUST_LOG` には、ファイルのキーがありません。知らないキーがあると、起動時にエラーで止まります。環境変数は、ファイルの値より優先されます。

| 変数 | 既定 | 意味 |
|---|---|---|
| `KAGEMUSHA_HOOK_PORT` | `9000` | フック待受ポート |
| `KAGEMUSHA_APP_HOOK_BASE` | — | アプリのフックサーバーの base URL。例 `http://127.0.0.1:8080`。`<base>/aws/lambda-microvms/runtime/v1/<hook>` へ送るので、アプリ側は同じフックパスを serve します |
| `KAGEMUSHA_HOOKS_DIR` | `/etc/kagemusha/hooks.d` | フック別スクリプト dir の根（`<dir>/run/`、`<dir>/suspend/` など） |
| `KAGEMUSHA_OTLP_ENDPOINT` / `OTEL_EXPORTER_OTLP_ENDPOINT` | — | OTLP の base。`<base>/v1/metrics` へ POST します（シグナル別の `OTEL_EXPORTER_OTLP_METRICS_*` は読みません） |
| `KAGEMUSHA_OTLP_HEADERS` / `OTEL_EXPORTER_OTLP_HEADERS` | — | `k=v,k2=v2` の固定ヘッダ。**秘密を含まない値専用**（イメージ焼き込み。認証情報は `KAGEMUSHA_OTLP_HEADERS_FILE` へ。セキュリティ参照）。`OTEL_EXPORTER_OTLP_HEADERS` の値は、OpenTelemetry の仕様どおり percent-decode します（`Basic%20abc` → `Basic abc`）。`KAGEMUSHA_OTLP_HEADERS` の値はそのまま使います（percent-decode しません） |
| `KAGEMUSHA_OTLP_HEADERS_FILE` | — | `/run` で読む `k=v` 形式のファイル。**VM ごとの認証情報の正式経路**で、env より優先されます。値はそのまま使います（percent-decode しません） |
| `KAGEMUSHA_FLUSH_TIMEOUT_MS` | `8000` | 同期 flush の上限（200ms–120s。フック予算を丸ごと飲む設定のときは `HOOK_BUDGET` の 1/4 に収めます） |
| `KAGEMUSHA_HOOK_BUDGET_MS` | `55000` | フック処理全体の上限（1s–300s） |
| `KAGEMUSHA_METER_INTERVAL_MS` | `15000` | cgroup の定期サンプル間隔（1s–3600s） |
| `KAGEMUSHA_SHUTDOWN_GRACE_MS` | `10000` | terminate 時の SIGTERM→SIGKILL 猶予（100ms–30s） |
| `KAGEMUSHA_IDENTITY_REPAIR` | `true` | `/run` での machine-id/hostname 修復。`true`/`false`、`1`/`0`、`yes`/`no`、`on`/`off` を受け付けます。それ以外の値は警告を出して無視します |
| `KAGEMUSHA_IDENTITY_ROOT` | `/` | 識別ファイルの書き込み先の根（テスト・開発用） |
| `KAGEMUSHA_CGROUP_ROOT` | `/sys/fs/cgroup` | cgroup v2 のマウント位置 |
| `KAGEMUSHA_SERVICE_NAME` / `OTEL_SERVICE_NAME` | `kagemusha-app` | `service.name` 属性 |
| `KAGEMUSHA_IMAGE_NAME` | — | 有界のイメージ名ラベル → `service.namespace` |
| `KAGEMUSHA_APP_READY_URL` | — | `/ready` 内で呼ぶ readiness プローブ（任意）。`KAGEMUSHA_APP_HOOK_BASE` が未設定のときだけ使います（設定されていれば、`/ready` はアプリへ中継します） |
| `KAGEMUSHA_APP_UID` / `KAGEMUSHA_APP_GID` | — | アプリをこの uid/gid に降格（root 所有ファイルからアプリを隔離） |
| `RUST_LOG` | `info` | tracing のフィルタ。例 `debug`、`kagemusha=trace` |

`KAGEMUSHA_HOOK_BUDGET_MS` はプラットフォームに設定したフックのタイムアウトより短くしてください（実行時フックは 1–60 秒。既定の 55 秒は収まります。イメージフックは最大 3600 秒）。エージェントが必ずプラットフォームの窓内に応答するためです。

アプリは、`KAGEMUSHA_APP_HOOK_BASE`、`KAGEMUSHA_APP_READY_URL`、OTLP のエンドポイント・ヘッダ・ヘッダファイルの変数を引き継ぎません。OTLP の変数には、標準の `OTEL_EXPORTER_OTLP_ENDPOINT` と `OTEL_EXPORTER_OTLP_HEADERS` も含みます（セキュリティ参照）。アプリが OpenTelemetry SDK を使う場合は、シグナル別の変数（例: `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`）か、SDK 自身の設定で exporter を設定してください。シグナル別の変数は、アプリへそのまま渡ります。

## テレメトリ

flush ごとに OTLP/HTTP JSON の `ExportMetricsServiceRequest` を 1 本出します。flush は 1 本ずつ、採取から送信まで通して実行します。そのため、点は採取した順に collector へ届きます。先行の flush を待つうちに予算が尽きた flush は、何も送りません。カウンタ（`sum`・CUMULATIVE・単調増加）は `kagemusha.uptime`（ms）、`kagemusha.cpu.{usage,user,system,throttled}_usec`、`kagemusha.cpu.nr_throttled`。ゲージは `kagemusha.memory.{current,peak,max}_bytes` です。Resource 属性は `service.name`、`telemetry.sdk.*`、`image_name`（`service.namespace`）のみです。VM・テナント・セッション・リクエストの ID はメトリクスのラベルに入れません。個体識別はログにだけ付けます。詳しくは `docs/decisions.md` を参照してください。

**制約:** 同じイメージから起動した VM どうしは、メトリクスでは区別できません。VM ごとの属性を付けておらず、`startTimeUnixNano` もイメージのビルド中にエージェントが起動した時刻だからです。この値はスナップショットを通じて、全 VM で同じになります。そのため、同じイメージの VM が同時に動くと、各 VM の CUMULATIVE カウンタが 1 つの系列に書き込まれます。バックエンドには値が交互に届き、カウンタのリセットや値の飛びに見えます。この系列から求めた率や合計は、それらの VM の正しい使用量になりません。

## セキュリティ

脆弱性の報告は、[SECURITY.md](SECURITY.md) を見てください。公開の Issue には書かないでください。

- MicroVM の中のアプリは信頼できないコードとして扱います。フック body は 64KiB 上限で、アプリの*応答* body も 64KiB 上限です（超過は中継失敗扱い — ビルド時フックは 502/504、実行時フックは fail-open で 200）、hooks.d スクリプトは清潔な環境で実行し、リクエスト payload の中身は `microvmId` を除きスクリプトの環境変数に載せません（`/proc/*/environ` から読めるため）。エージェントの認証系 env（OTLP ヘッダ・ヘッダファイルのパス・OTLP エンドポイント・アプリのフック/readiness の URL — URL が userinfo 認証を含みうるため）は、アプリの継承環境とエージェント自身の `/proc/1/environ` の両方から除去します。この denylist は運用者が自分で追加した秘密（`AWS_*`、`DATABASE_URL` など）は捕捉できません。`KAGEMUSHA_APP_UID`/`GID` を設定してアプリを別ユーザーで起動し、root 所有ファイルから隔離してください。片方だけの設定はもう片方の ID が 0 のまま残るため起動時に警告します。権限降下では補助グループもクリアし、エージェントに `CAP_SETGID`/`CAP_SETUID` がない環境（非 root など）では権限を残したままアプリを起動するより spawn を失敗させる（fail-closed）設計です。
- フックポートは、認証なしでライフサイクルの指示を受け付けます。`/terminate` はアプリを止め、`/suspend` はエージェントの権限で `hooks.d` を実行します。このポートを、外から届く経路に載せないでください。MicroVM の認証トークンの `allowedPorts` は、アプリのポートだけに絞ります。すべてのポートを許すトークンでは、MicroVM のエンドポイント経由でフックポートにも届きます（[Networking](https://docs.aws.amazon.com/lambda/latest/dg/microvms-networking.html) を参照）。MicroVM の外（ローカル開発など）では、`0.0.0.0` で待ち受けるリスナーにネットワークから届きます。ファイアウォールでポートを閉じてください。
- hooks.d のスクリプトは冪等である必要があります。`resume`/`suspend`/`terminate` は共有フックポートへの POST ごとに再実行されます。`hooks.d/run` はちょうど 1 回だけ — body を読めた最初の `/run` POST（claim 可否を問わず、body が JSON として正しくなくても）で発火します。先に偽造 POST が届くと本物の `/run` 前に一度だけ起動し得ます（同一ソケットでは送信元を区別できない受容リスク）が、2 回目以降は実行されません。フック body が 64KiB 超・遅すぎる・読み取れないときは扱いが変わります。`/run` と `/resume` はすぐ 200 を返し、スクリプトもアプリへの中継も行いません（壊れた入力で特権のスクリプトを起こさないため）。`/suspend` と `/terminate` は、空の `{}` を body として処理を続けます。残りのフック予算の中で、アプリへの中継、`hooks.d` のスクリプト、テレメトリの flush の順に試みます。予算が尽きた段は実行しません。flush の時間（`KAGEMUSHA_FLUSH_TIMEOUT_MS`）は必ず残ります。body の読み取り・中継・スクリプトは、どれも flush の分を残して打ち切るので、遅い body でも flush の時間は削られません。`/terminate` は終了も始めます。
- エージェント自身の HTTP 通信（アプリへの中継・readiness プローブ・OTLP 送信）は `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` を使いません。これらの変数はアプリからは使えますが、フック body や OTLP ヘッダはプロキシを通りません。OTLP エンドポイントには直接届く必要があります。
- VM ごとの認証情報は `/run` で読む `KAGEMUSHA_OTLP_HEADERS_FILE` を使います。イメージの env や `runHookPayload` には入れません。このファイルは、プラットフォームの `/run` が届く前に置いてください。エージェントは `hooks.d/run` より前に読むので、`hooks.d/run` のスクリプトでは用意できません。ファイルが未配置・読み込み失敗・有効ヘッダなしの場合は、次の `/run` POST が届いたときだけ読み直し、非空のヘッダが得られるまで続けます。プラットフォームが送る `/run` は、通常 1 回です。読めるまでの間、送信には env の固定ヘッダだけが付きます。
- `/run` の識別情報は初回のみ有効（single-shot）です。有効な `microvmId`（1〜128 バイトの印字可能な ASCII）を body に含む最初の `/run` が、microvmId と machine-id 修復を確定します。使える ID を含まない `/run` は何も確定しません。そのため、後から有効な ID を含む `/run` が届けば、そこで修復します。確定した後の `/run`（偽造・プラットフォーム再送を問わず）では再汚染できません。識別ファイルは、パスの最後の要素がシンボリックリンクなら辿らずに書き込みません。`/run` より前に置かれたリンクで書き込み先を変えられないようにするためです。書き込めなかったパスは記録して先へ進みます（fail-open）。

## イメージ例

[`examples/Dockerfile`](examples/Dockerfile) を見てください。静的 musl バイナリとアプリを multi-stage で作り、`/sbin/kagemusha` を entrypoint にします。アプリは `KAGEMUSHA_APP_UID`/`GID` で、権限のない `app` ユーザー（uid/gid 10001）として動かします。ビルドは、リポジトリの直下をビルドコンテキストにして行います。

```sh
docker build -f examples/Dockerfile .
```

**対応環境:** 本番ターゲットは Linux（MicroVM のゲスト、PID 1）です。開発とテスト用に、macOS でもビルド・実行できます。フックの中継・シグナル転送・プロセスグループ kill・`sethostname`（root のとき）は、macOS でも同じコードで動きます。ただし macOS には cgroup v2 が無いので、送るメトリクスは `kagemusha.uptime` だけになります。また、エージェントが PID 1 ではないので、孤児のプロセスを回収しません。非 Linux ビルドでは、信頼できないワークロードを動かさないでください。Windows など unix 以外のターゲットには対応していません。ビルドはコンパイルエラーで止まります。

## 設計

設計決定の記録は [`docs/decisions.md`](docs/decisions.md)（ADR-001 以降）にあります。エージェント向けの約束は [AGENTS.md](AGENTS.md) にあります。

## ライセンス

[Apache License 2.0](LICENSE)
