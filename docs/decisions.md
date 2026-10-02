# 設計判断の記録（ADR）

kagemusha の設計判断を記録します。新しい判断は追記し、古い判断は消しません。

## ADR-001: エージェントは単一の静的バイナリとして、コンテナの起動役になる（採用）

- 状態: 採用（2026-09-26）
- 背景: MicroVM イメージは Dockerfile から作られ、`CMD`/`ENTRYPOINT` が起動されます。エージェントが起動役になると、フックの受け口と子プロセスの監督を 1 プロセスで担えます。
- 決定: `kagemusha <app command...>` の形で、アプリを子プロセスとして起動します。言語は Rust、musl 静的リンクを目標にします。
- 理由: メモリとスナップショットを小さく保てます。どのイメージにも単一バイナリで入れられます。OpenSSL に依存しない（rustls）ため、スナップショット由来の RNG 問題を避けられます。

## ADR-002: 実行時フックの失敗は利用者の処理を止めない（採用）

- 状態: 採用（2026-09-26）
- 背景: 実行時フック（run/resume/suspend/terminate）には 60 秒の上限があります。エージェントの障害で VM の利用者処理が止まるのは本末転倒です。
- 決定: エージェント側の処理（identity 修復、テレメトリ flush 等）が失敗しても、HTTP 応答は 200 を返します。アプリのフック応答の失敗は透過します。ビルド時フック（ready/validate）は正直な応答を返します — 届いたアプリ応答は失敗も透過し、到達不能は 502、タイムアウトは 504、超過ボディは 413 です（ADR-006/007）。fail-open で 200 に丸めるのは実行時フックだけです。

## ADR-003: メトリクスのラベルに個体 ID を入れない（採用）

- 状態: 採用（2026-09-26）
- 背景: microvmId や tenant/session ID をメトリクス次元に入れると、系列が無制限に増えます。
- 決定: 個体識別情報はログの属性にだけ付けます（trace exporter 導入時は span 属性にも）。メトリクス次元は hook 名・結果・image 名など有界の値に限ります。先行例（[ewhauser/eve-extensions#126](https://github.com/ewhauser/eve-extensions/pull/126)、2026-09-17）も同じ方針です。

## ADR-004: 使用量は事実だけを送る（採用）

- 状態: 採用（2026-09-26）
- 背景: 料金は AWS の単価で変わり、単価はエージェントが知るべき不変値ではありません。
- 決定: cgroup v2 の CPU・メモリ実測と稼働時間などの「事実」を送ります。費用の計算はクエリ側で行います。表示は必ず「推定」とします。

## ADR-005: フックの中継順序（採用）

- 状態: 採用（2026-09-26）
- 決定: 1 つの起動役が、フックごとに決まった順で処理します。
  - `run`: identity 修復 → 認証ヘッダ読み込み → `hooks.d/run` → アプリの `/run` 中継
  - `resume`: `hooks.d/resume` → アプリの `/resume` 中継
  - `suspend`: アプリの `/suspend` 中継（まずアプリを drain）→ `hooks.d/suspend` → テレメトリ flush（エージェントは最後）
  - `terminate`: アプリの `/terminate` 中継 → `hooks.d/terminate` → 最終 flush
  - `ready`: アプリの `/ready` 中継、未設定なら `app_ready_url` の GET、どちらも無ければ子プロセス起動済みで 200
  - `validate`: アプリの `/validate` 中継、未設定なら 200
- 理由: suspend/terminate では、アプリが先に仕事を止め、次に利用者のスクリプトが後始末をし、最後にエージェントがテレメトリを送り切るのが自然です。run/resume は逆に、環境を整えてからアプリを起こします。
- 応答: アプリのフック応答（ステータスと本文、本文の読み方を示す Content-Type・Content-Encoding）は透過します。実行時フックでエージェント内部処理が失敗しても 200 を返します（ADR-002）。

## ADR-006: 実行時フックの合成エラーは fail-open で 200 にする（採用）

- 状態: 採用（2026-09-26）
- 背景: アプリが応答を返した失敗（4xx/5xx）と、アプリに届かなかった失敗（接続拒否・タイムアウト）は別物です。`/terminate` や `/suspend` の時点でアプリが既に死んでいるのは正常系であり得ます。実行時フックで 502/504 を返すと、プラットフォームが失敗として扱い、利用者の処理を止めかねません。
- 決定: 実際に届いたアプリ応答はすべてのフックで透過します（status・body・Content-Type・Content-Encoding）。Timeout/Transport の合成失敗は、実行時フックでは 200（ADR-002 の fail-open）、ビルド時フックでは 502/504 を返します。
- 併記: `run`/`resume` では `hooks.d` の合計時間に中継用の予約（10 秒、`app_hook_base` 設定時のみ）を設け、ユーザースクリプトがアプリのフック時間を食い尽くさないようにします。`suspend`/`terminate` では flush 予算を**request body の読み取り・アプリ中継・hooks.d のすべてから**予約します（body の読み取りからの予約は 2026-09-29 追記）。「送り切る」ことがこのエージェントの存在理由なので、遅い body や遅い drain に flush を飢えさせません。

## ADR-007: 大きすぎる・遅すぎるフックボディの扱い（採用）

- 状態: 採用（2026-09-26）
- 決定: フックの request body は 64 KiB 上限（`runHookPayload` の実測上限 4 KiB より十分大きい）で、収集も hook の deadline 内に収めます。`suspend`/`terminate` では、flush 予算を残す時点で収集を打ち切ります（ADR-006）。超過・タイムアウト時は、実行時フックは 200 を返してフック全体をスキップ（fail-open）、ビルド時フックは 413 を返します。ただし `suspend`/`terminate` はスキップせず、`{}` のボディで処理を続けます（ADR-009 の「drain フックのボディ異常」を参照）。
- 理由: 壊れた・細切れの入力で hook が開きっぱなしになることを防ぎます。

## 脅威メモ: フック受信ポートとアプリの非信頼性（記録、採用 2026-09-26）

アプリは信頼できないコードとして扱います（AGENTS.md の原則）。対策済み:

- **バインド**: エージェントは `0.0.0.0:<hook_port>` にバインドします（プラットフォームからの到達のため）。同じ VM 内のアプリからもフックを打てます。アプリが自分で `/terminate` や `/suspend` を呼ぶのは自己完結の副作用に留まるため許容します。テナント境界の強制が必要になったら、プラットフォーム送信元の検証を別 ADR で扱います。VM の外からの経路もあります。MicroVM の認証トークンの `allowedPorts` がフックポートを含むと、エンドポイント経由で外からも届きます。そのため README の Security notes で、トークンをアプリのポートに絞るよう運用者に求めます。
- **接続フラッド**: 同時接続は 64 に制限し、超過分は切断します。ヘッダ読み取りは 10 秒でタイムアウトし、keep-alive は無効です（1 フック = 1 リクエスト）。さらに接続全体に `ヘッダ上限 + hook_budget + 書き込み 30 秒` の deadline を設け、読み取らないピアが接続スロットを永久占有するのを防ぎます。
- **残存リスク（接続枯渇）**: 接続上限はエージェントの資源を守りますが可用性までは保証しません。非信頼アプリが 64 スロットを全占有すれば、プラットフォーム本物のライフサイクル要求を締め出せます。送信元の区別が付かない同一ソケットでは防げないため受容します — プラットフォーム送信元の検証（接続の分離等）は将来の ADR で扱います。
- **リダイレクト**: relay クライアントはリダイレクトを辿りません。307/308 がフックボディ（`runHookPayload` は秘密を含みうる）をオリジン外へ再送するのを防ぎます。
- **ボディ爆弾**: リクエスト側は `Limited` ラップ、レスポンス側は `resp.chunk()` を 64 KiB 上限つきで逐次集積します。Content-Length の偽装や欠落にも耐えます。relay クライアントは圧縮を要求せず、アプリの応答を展開しないため、gzip 爆弾の経路もありません。圧縮された応答は、展開せずに Content-Encoding と一緒にそのまま中継します。
- **ヘッダ透過の範囲**: status・body と、本文の読み方を示す Content-Type・Content-Encoding だけを透過します。値は受け取ったバイト列のまま渡し、Content-Encoding は複数行ならすべての行を順に渡します。本文を展開せずに渡すため、Content-Encoding を落とすと、受け手は圧縮されたバイト列を読めません。`Location` 等の他ヘッダは意図的に落とします（リダイレクト誘導をプラットフォームへ伝えない）。
- **環境変数**: `hooks.d` スクリプトは `env_clear` で起動し、渡すのは明示リストだけです — `PATH`、`KAGEMUSHA_HOOK`、`KAGEMUSHA_HOOK_DIR`、設定済みなら `KAGEMUSHA_IMAGE_NAME`、`/run` 受理済みなら `KAGEMUSHA_MICROVM_ID`（後者 2 つは env 安全ゲート通過値のみ）。`OTEL_EXPORTER_OTLP_HEADERS` や AWS 資格情報はスクリプトに漏れません。
- **秘密情報の隔離**: 起動時に `scrub_secret_env` がエージェント自身の environ から資格系変数（`KAGEMUSHA_OTLP_HEADERS`/`_FILE`/`_ENDPOINT`、`OTEL_EXPORTER_OTLP_HEADERS`/`_ENDPOINT`、`KAGEMUSHA_APP_HOOK_BASE`/`_READY_URL`）を消します — 同一 uid のアプリが `/proc/1/environ` から読み戻す経路を塞ぎます。アプリの spawn 時にも同じリストで env を scrub し、`KAGEMUSHA_APP_UID`/`_GID` 設定時は権限降下します。
- **hooks.d の所有権（要件）**: スクリプトはエージェント自身の権限（通常 root）で実行されるため、`hooks_dir` はイメージ側で root 所有かつ非信頼の書き込み不可にする必要があります。アプリが `hooks_dir` に書き込める場合、次のフックで agent 権限のコード実行になります（Dockerfile 例で `root:root 0755` を明示します）。
- **識別ファイルの書き込み**: `/run` の修復は、エージェントの権限（通常 root）で `/run/machine-id`・`/etc/machine-id`・`/etc/hostname` に書き込みます。アプリは `/run` より前から動くため、パスの最後の要素にあるシンボリックリンクは辿りません（`O_NOFOLLOW`）。辿れない書き込みは記録して fail-open で進みます。
- **hooks.d の資源上限**: ディレクトリ走査は 1024 エントリ上限・実行は先頭 64 スクリプトまで・1 スクリプト 10 秒・フックの残り予算が 50ms 未満なら spawn しません。走査・stat は blocking pool 上で行い、止まったファイルシステムが Tokio ワーカーを占有しないようにします。kill 後の reap 待ちも共有予算内に収めます。
- **プロセス**: スクリプトは独自のプロセスグループで起動し、タイムアウト・キャンセル時はグループごと SIGKILL します（バックグラウンドジョブを残しません）。エージェントは PID 1 として動くため、孤児化した孫プロセスの zombie 回収は supervisor に `waitpid(-1, WNOHANG)` 型の汎用リーパーとして実装します。終了ステータスは記録時刻つきで保持し、PID 再利用で残った古いレコードが生存中スクリプトの終了を騙れないようにします（`take_since`）。
- **残存リスク（pgid 再利用）**: 回収済みアプリの pid が hooks.d スクリプトへ再利用されると、シグナル転送（pgid ベース）が別グループに当たる窓が残ります。`terminating` 監視で縮めていますが完全には消せません — 同一環境内の副作用に限定されるため受容します。

## ADR-008: supervisor と reap の集約（採用）

- 状態: 採用（2026-09-26）
- 背景: エージェントはコンテナの entrypoint（PID 1相当）として動きます。tokio::process や std::process の `Child::wait` を複数箇所で使うと、`waitpid` の競合で互いの終了ステータスを奪い合い（ECHILD）、監視が壊れます。また孤児化したプロセスは PID 1 に reparent され、回収者がいなければ zombie が溜まります。
- 決定:
  - すべての子プロセス（app・hooks.d）は `std::process::Command` + `process_group(0)` で起動し、tokio::process は使いません。
  - reap は supervisor の `waitpid(-1, WNOHANG)` 一箇所に集約し、終了ステータスは `ctx.reaper` の共有マップに記録します。hooks.d などの待ち側はマップをポーリングします（supervisor 非起動時は `try_wait` フォールバック）。
  - 捕捉可能シグナル（TERM/INT/QUIT/HUP/USR1/USR2）はアプリのプロセスグループ全体に転送します。
  - `/terminate` 応答後、300ms の送信猶予を置いてから SIGTERM→`shutdown_grace`→SIGKILL の順で停止し、最終 flush を行います。
  - アプリの終了コードをそのまま返します（シグナル死は 128+signal）。
- 補足: テスト環境では agent が PID 1 でないため孤児の reparent 検証はできません。本番では全孤児が agent に集まり、SIGCHLD 駆動 + 250ms バックストップの drain で回収します。

## ADR-009: /run での個体修復（採用）

- 状態: 採用（2026-09-26）
- 背景: イメージの `/etc/machine-id` はスナップショットに焼き込まれ、全 VM が同一 ID を共有します（[microvms-agentd の platform internals](https://laithalsaadoon.github.io/microvms-agentd/internals/platform/)、2026-09-23/24 実測）。systemd 系ツールや self-join 系テレメトリが全 VM で衝突します。`/run` フックで microvmId が届く唯一の機会に修復します。
- 決定:
  - machine-id は `microvmId` の 16 進部分（uuid 接尾＝32 hex）から導出します。suspend/resume で不変かつ制御面の ID と突き合わせ可能なためです。導出不能なら `/proc/sys/kernel/random/uuid`、それも駄目なら `getrandom(2)`/`getentropy` に落とします。
  - 書き込み先は `/run/machine-id`（コンテナ系ツールが bind-mount する経路）と `/etc/machine-id` の両方。片方だけ成功しても有効です。
  - hostname は `microvmId` を 63 文字の ldh 形式にサニタイズして `/etc/hostname` へ。`sethostname(2)` は `CAP_SYS_ADMIN` が無い環境では失敗するため fail-open（同実測で `additionalOsCapabilities: ["ALL"]` でも未付与の計測あり）。
  - 全ステップ fail-open。書き込み先 root は `KAGEMUSHA_IDENTITY_ROOT`（既定 `/`）で差し替え可能 — テスト・開発環境が実ホストの `/etc` を汚さないために必要で、`root != "/"` のとき `sethostname` は呼びません。
  - `microvmId` は `ctx.microvm_id`（OnceLock、有効な microvmId を含む最初の /run が勝つ）に保持し、hooks.d には `KAGEMUSHA_MICROVM_ID` として公開します。microvmId 以外の payload フィールドは環境変数に載せません（/proc/*/environ から読めるため。AGENTS.md の秘密情報原則、認証の正式経路は ADR-011 のヘッダ優先）。
  - `otlp_headers_file` は `/run` 内で読み込み、`ctx.otlp_headers` に保持します。パスはイメージ設定、中身は per-VM プロビジョニング — 認証情報をスナップショットに焼かないための正式経路です。
  - **single-shot**: `/run` の副作用（microvm_id 格納・identity repair）は、有効な microvmId を含む最初の `/run` だけが `ctx.run_seen` を claim して実行します。ID を含まない `/run` や解析できない `/run` は claim しないので、後から有効な ID を含む `/run` が届けば、そこで修復します。例外は `otlp_headers_file` の読み込みだけで、`headers_loaded` が立つまで `/run` ごとに再試行します（初回の読み込み失敗で OTLP 認証を永久に失わないため）。claim の後に届く鍛造 `/run`（非信頼アプリが同じポートに届く）が ID・machine-id・認証情報を再汚染することを防ぎます。残存: プラットフォームの真の `/run` より先に、アプリが有効な ID を付けて POST する spoof race は受容します（同一ソケット上で送信元の区別が付かない）。microvmId は ASCII printable ≤128 文字に検証済みのものだけ env 化します（NUL/制御文字を含む値は `spawn` を `InvalidInput` で失敗させ、hooks.d を丸ごと静かに止めるため — 実測 Rust 1.98.1）。検証を通らない値の `/run` は claim せず、修復も行いません（ID のない `/run` と同じ扱いです）。machine-id 導出は「埋め込み uuid 形状 → 末尾 32 hex」の順で、hex プレフィックスが uuid 接尾を押し出す誤導出を防ぎます。
  - **run パイプラインの一回性（2026-09-27 追記）**: `hooks.d/run` は `run_pipeline_fired` で厳密に 1 回 — body を読めた最初の `/run` POST（claim 有無を問わず、JSON として解析できなくても 1 回は流してフォーマット変更に耐える）だけがスクリプトを駆動します。body を読み切れない `/run`（64KiB 超・遅延・読み取り失敗）はパイプラインに入らず、発火もせずに 200 を返します（`/resume` も同じ）。以後の POST はスキップして 200。例外はアプリ中継だけ: 先行の unclaimed POST がパイプラインを発火済みでも、claim する `/run` は relay します — アプリが本物の runHookPayload を受け取れないと困るため（偽造先行で claim された spoof race は既に受容済みの残存リスク）。claim→修復→発火→中継の全区間は `run_pipeline_lock` で直列化し、修復中に鍛造 POST がパイプラインを先に発火して claim 側と二重中継する競合を閉じます（待機は各 POST の hook deadline 内、タイムアウトは 200）。claim は lock を取った後に行います。lock を待つ間に期限切れになった `/run` は claim を消費しないので、先行する ID なしの POST が遅いスクリプトや中継で lock を握っていても、次に届く有効な ID の `/run` が修復と中継を行います。
  - **drain フックのボディ異常（2026-09-28 追記）**: `/suspend`・`/terminate` の request body が超過・遅延・不正でも、パイプラインは `{}` で継続します — 同期 flush と `terminating` 立起を飛ばしたまま 200 を返すと、VM が「終了成功」のまま停止しない実害になるためです（`run`/`resume` は壊れた入力でスクリプトを起こさない方針を維持）。順序は通常と同じで、残りの予算の中でアプリ中継 → `hooks.d` → flush を試み、予算が尽きた段は実行しません。flush の予算は body の読み取りからも予約するので（ADR-006）、遅い body でも flush の時間は残ります（2026-09-29 追記）。

## ADR-010: usage meter は cgroup v2 の事実だけを採る（採用）

- 状態: 採用（2026-09-26）
- 背景: 課金形状の推定（ADR-004）と suspend 前の送り切りに、生のカウンタが必要です。kernel cgroup-v2 文書で確定した面: `cpu.stat`（usage_usec/user_usec/system_usec・帯域制御有効時に nr_throttled/throttled_usec）、`memory.current`、`memory.peak`、`memory.max`（"max" または bytes）。
- 決定:
  - 採るのはカーネルの生値のみ（rate・%は collector 側の仕事）。`throttled_usec`/`nr_throttled` は baseline を超えたバーストの直接証拠として必須です。
  - 定期サンプラーは `meter_interval`（既定 15s、1s..3600s に clamp）で `ctx.meter` の最新値を更新します。flush 経路は `sample_fresh` で「今」の値を取り直します（suspend 直前の 15s 前の値を送らない）。
  - cgroup root は `KAGEMUSHA_CGROUP_ROOT`（既定 `/sys/fs/cgroup`）で差し替え可能（テスト・開発環境向け）。
  - 全フィールド Option — cgroup 非搭載環境やコントローラ無効でも欠損のまま送り、決してメーターが処理を止めません。
  - メトリクスのラベルに microvmId/テナント/セッション等の個体 ID は入れません（ADR-003）。image_name のみ bounded で許可します。

## ADR-011: テレメトリは OTLP/HTTP JSON で同期送出する（採用）

- 状態: 採用（2026-09-26）
- 背景: suspend/terminate 前の「送り切り」はフック応答より先に完了する必要があります。gRPC/protobuf は追加依存が重いため、OTLP/HTTP の JSON マッピングを採ります（OpenTelemetry Collector など、OTLP/HTTP の JSON エンコードを受け付ける collector が受け取れます。OTLP の仕様は、受信側の JSON 対応を SHOULD に留めています）。
- 決定:
  - flush は budget の内訳を「採取 ≤25%（上限3s）→ export 残り全部」に分けます。procfs の読み取りがネットワーク送信を枯らさない構造です。
  - flush は 1 本ずつ、採取から export まで通して実行します（2026-09-30 追記）。`/suspend`・`/terminate`・supervisor の最終 flush は重なり得ます。並行させると、古い CUMULATIVE の点が新しい点より後に collector へ届き、カウンタのリセットに見えるためです。先行の flush を待つ時間も budget から引き、budget 内に始められなかった flush は何も送りません。
  - wire 形式: protobuf-JSON マッピングに従い int64/timeUnixNano は十進文字列、`aggregationTemporality: 2`（CUMULATIVE）+ `isMonotonic: true`。URL は `otlp_url("v1/metrics")`（OTEL_EXPORTER_OTLP_ENDPOINT 準拠のベース + `/v1/metrics`）。
  - メトリクス: `kagemusha.uptime`(ms)、`kagemusha.cpu.*_usec`/`nr_throttled`（カウンタ）、`kagemusha.memory.*_bytes`（ゲージ）。事実のみ（ADR-004/010）。
  - Resource attributes: `service.name`（cfg）、`telemetry.sdk.*`、bounded な `image_name` を `service.namespace` のみ許可。VM・テナント・セッション・リクエスト ID は一切含めません（ADR-003）。
  - ヘッダ優先順位: `/run` で読んだ `otlp_headers_file` の中身が `KAGEMUSHA_OTLP_HEADERS`（イメージ焼き込み）に勝ちます — 認証情報をスナップショットに焼かない正式経路です。
  - 送信は単発・fail-open。リトライは collector 側の仕事（suspend 中にリトライすると応答自体が遅れるため）。
  - startTimeUnixNano は `ctx.start_wall`（`AgentCtx::new` で `Instant` と同時に確保した `SystemTime`）から出します。export ごとの再計算は µs 単位でずれ、CUMULATIVE 系列で reset 誤検出を起こし得るため、起動時の一度だけアンカーします。

## ADR-012: panic 経路と共有状態は「経路ごと塞ぐ」設計で閉じる（採用）

- 状態: 採用（2026-09-26）
- 背景: `Command::env` に NUL/制御文字を含む値を渡しても panic はしませんが、`spawn` が `InvalidInput` で失敗します（実測 Rust 1.98.1）。microvm_id や image_name に制御文字が混ざると、以後の hooks.d スクリプトは全件 spawn 失敗になり、fail-open の下で静かに全滅します。payload 経路（ADR-009）だけでなく、設定値（`--config` の `image_name`）も同じ経路を通ります。
- 決定:
  - `Command::env` に渡る値はすべて env 安全ゲートを通します（`env_safe_microvm_id` に加え `env_safe_config_value` を新設。NUL/制御文字を含む `image_name` は `KAGEMUSHA_IMAGE_NAME` を載せず警告のみ）。panic ではなく「全スクリプトの spawn 失敗」が防ぐ対象です。
  - OTLP ヘッダは「ペア単位」で reqwest の `HeaderName`/`HeaderValue` として構築可能かをロード時に検査します。1 行不良でも残りを殺さない（不良ペアが request builder を全滅させ、テレメトリが静かに死ぬのを防ぐ）。
  - `otlp_headers_file` は metadata 事前検査ではなく `take(64KiB+1)` のバイト数締めで読みます（サイズ TOCTOU と fifo 類の肥大化を同時に防ぐ）。open は Unix で `O_NONBLOCK` 付き — writer の無い FIFO への open(2) は blocking pool スレッドを無限に占有し、claim 前の `/run` ごとに試行されるため、アプリが pool を枯渇させられました（読み取り 5 秒の timeout では塞げない: キャンセルしてもブロック中スレッドは返らない）。
  - Reaper は app 用の専用スロットを `take_app_status()` として API 分離します。hooks.d の purge が `take_since` で app ステータスを盗めない形にし、「drain→take が同期ペアである」という暗黙のタイミング前提に正しさを依存させません。
  - `/run` の payload ID は許可リスト（`LOGGABLE_PAYLOAD_KEYS`）経由で debug ログに載せます — 「ID はログ/trace に付ける」という原則をコードでも成立させます（生 payload は秘密を含み得るため決してログしません）。
