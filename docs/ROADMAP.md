# Lecs: Local ECS Task Runner — 実装計画

## Context

ECS にデプロイするアプリをローカルで動かすとき、ECS が提供する「実行時契約」（メタデータエンドポイント、クレデンシャルプロバイダ、dependsOn、ヘルスチェック等）がないと正しくテストできない。AWS 公式の `amazon-ecs-local-container-endpoints` は Metadata/Credentials のモックを提供するが、task definition ネイティブな CLI 体験・ネットワーク自動構築・dependsOn/ヘルスチェック統合はない。

Lecs は「ECS control plane の再現」ではなく「ECS アプリが期待する実行時契約をローカルで満たす」ことに特化した CLI ツール。

---

## 開発エコシステム

### 主要依存クレート

| クレート | 用途 | Phase |
|---|---|---|
| `clap` (derive) | CLI フレームワーク | 0 |
| `tokio` | async ランタイム | 0 |
| `serde` / `serde_json` | JSON シリアライズ・デシリアライズ | 0 |
| `tracing` / `tracing-subscriber` | 構造化ログ | 0 |
| `anyhow` / `thiserror` | エラーハンドリング | 0 |
| `bollard` | コンテナランタイム API クライアント (Docker/Podman) | 1 |
| `futures-util` | bollard の Stream 処理 | 1 |
| `axum` | メタデータ/クレデンシャル HTTP サーバー | 3 |
| `aws-config` | AWS クレデンシャルチェーン | 3 |
| `aws-credential-types` | AWS クレデンシャル型定義 | 3 |
| `chrono` | 日時処理（クレデンシャル有効期限等） | 3 |
| `reqwest` (dev) | HTTP クライアント（テスト用） | 3 |
| `clap_complete` | シェル補完スクリプト生成 | 8 |

### ディレクトリ構成

```
src/
├── main.rs              # エントリポイント (clap CLI)
├── labels.rs            # コンテナラベルキー定数 (lecs.managed, lecs.task, ...)
├── cli/                 # CLI コマンド定義 (run, stop, ps, logs, init, validate, inspect, stats, completions, watch, exec, version)
├── taskdef/             # Task Definition JSON パーサ・型定義・診断・Terraform / CloudFormation 入力変換
├── container/           # OCI コンテナランタイムクライアント (Docker/Podman)
├── overrides/           # ローカルオーバーライド設定
├── secrets/             # Secrets ローカル差し替え
├── profile/             # 設定プロファイル解決 (.lecs.toml)
├── orchestrator/        # dependsOn DAG・ライフサイクル管理
├── metadata/            # ECS メタデータエンドポイントモック
├── credentials/         # クレデンシャルプロバイダモック
└── events/              # 構造化ライフサイクルイベントログ
```

---

## ロードマップ

### Phase 0: Skeleton ✅
**目標**: ビルド可能な CLI スケルトン + 開発エコシステム

- [x] `cargo init` + ディレクトリ作成 + 依存追加
- [x] clap による `lecs run --task-definition <file>` スケルトン
- [x] `lecs version` コマンド
- [x] Makefile, `rustfmt.toml`, CI workflow
- [x] `make check` が全て通ること

### Phase 1: Task Definition パース + 単一コンテナ実行 ✅
**目標**: `lecs run -f task-def.json` で単一コンテナが Docker/Podman 上で動く

- [x] Task Definition JSON パーサ（serde で `containerDefinitions` の主要フィールド対応）
  - `name`, `image`, `command`, `entryPoint`, `environment`, `portMappings`
  - `cpu`, `memory`, `memoryReservation`, `essential`
- [x] bollard でコンテナ作成・起動・ログストリーム表示
- [x] 専用 network 作成（`lecs-<task-name>`）
- [x] コンテナ名ベースの DNS 解決（bridge ネットワーク内）
- [x] `lecs stop` でクリーンアップ（コンテナ停止 + ネットワーク削除）

### Phase 2: ローカルオーバーライド + Secrets 差し替え ✅
**目標**: 本番 task definition をそのまま使いつつ、ローカル固有の設定を上書き

- [x] オーバーライドファイル（`lecs-override.json`）
  - 環境変数の追加・上書き
  - イメージタグの差し替え
  - ポートマッピング変更
- [x] Secrets 解決
  - `valueFrom` の ARN → ローカルマッピングファイルから値を引く
  - `secrets.local.json`: `{ "arn:aws:secretsmanager:...": "local-value" }`

### Phase 2.5: コンテナランタイム互換性強化
**目標**: Docker に加えて Podman をネイティブサポート

- [x] `docker` モジュールを `container` にリネーム（OCI ランタイム非依存の命名）
- [x] Podman ソケット自動検出（rootless → rootful）
- [x] `--host` CLI フラグ + `CONTAINER_HOST` 環境変数によるソケット明示指定
- [x] `unix://` / `tcp://` / 素パスの URL パース対応
- [x] 設計書の更新（OCI 準拠の記述に統一）

### Phase 3: Metadata + Credentials Sidecar ✅
**目標**: `ECS_CONTAINER_METADATA_URI_V4` と `AWS_CONTAINER_CREDENTIALS_FULL_URI` が動く

- [x] axum ベースのメタデータ HTTP サーバー（ランダムポートで起動）
  - `GET /v4/{container_name}` → コンテナメタデータ JSON
  - `GET /v4/{container_name}/task` → タスクメタデータ JSON
  - `GET /v4/{container_name}/stats` → 501 Not Implemented（将来対応）
  - `GET /credentials` → ローカル AWS credentials を返す
  - `GET /health` → ヘルスチェック
- [x] AWS クレデンシャルローダー（`aws-config` で完全なクレデンシャルチェーン対応）
- [x] 各アプリコンテナに環境変数を自動注入
  - `ECS_CONTAINER_METADATA_URI_V4=http://host.docker.internal:<port>/v4/<name>`
  - `AWS_CONTAINER_CREDENTIALS_FULL_URI=http://host.docker.internal:<port>/credentials`
- [x] `host.docker.internal:host-gateway` を extra_hosts として全コンテナに設定
- [x] `taskRoleArn` / `executionRoleArn` フィールド対応
- [x] `--no-metadata` フラグでサイドカー無効化

### Phase 4: dependsOn + Health Check
**目標**: マルチコンテナ task の起動順序と健全性を制御

- [x] `dependsOn` の DAG 解決（トポロジカルソート）
  - 条件: `START`, `COMPLETE`, `SUCCESS`, `HEALTHY`
  - 循環依存の検出・エラー
- [x] Health Check 実行・監視
  - `healthCheck.command` を Docker HEALTHCHECK として設定
  - `interval`, `timeout`, `retries`, `startPeriod` 対応
- [x] essential コンテナ停止時のタスク全体停止

### Phase 5: Volume + ログ + UX 改善 ✅
**目標**: 実用的な開発体験

- [x] Bind mount ベースの volume（`volumes` + `mountPoints`）
- [x] ログ統合（全コンテナのログを色分けマルチプレクス）
- [x] `lecs ps` — 実行中タスク一覧
- [x] `lecs logs <container>` — 特定コンテナのログ表示
- [x] Ctrl+C グレースフルシャットダウン（tokio signal handling）— Phase 1 で実装済み

### Phase 6: バリデーション + Init + Dry-run ✅
**目標**: コンテナ起動前にエラーを検出し、プロジェクト開始を高速化する

> Phase 3-5 と並行して着手可能（既存の `taskdef` / `overrides` モジュールのみに依存）

- [x] `lecs validate` — タスク定義の静的解析
  - イメージ名形式チェック、ポートマッピング競合検出
  - `dependsOn` 参照先の存在チェック、循環依存検出
  - Secret ARN 形式バリデーション
  - オーバーライドファイルのコンテナ名検証
  - よくあるミスへの警告（全コンテナ essential=false、ポートマッピングなし等）
- [x] `lecs init` — スターターファイル生成
  - 最小限のタスク定義 JSON、`lecs-override.json`、`secrets.local.json` のテンプレート
  - `--image` / `--family` フラグによる非対話生成
- [x] `--dry-run` フラグ（`lecs run`）
  - パース → バリデーション → オーバーライド適用 → Secrets 解決 → 構成表示（起動はしない）
  - コンテナ名、イメージ、環境変数（secrets 値は伏字）、ポート、ネットワーク名を出力
- [x] リッチなバリデーションエラーメッセージ
  - フィールドパス、期待される型、修正提案を含む人間向けの診断出力

### Phase 7: 可観測性 + 診断 ✅
**目標**: 実行中タスクの状態・リソース使用量・履歴を可視化し、ローカルデバッグを支援する

> Phase 5 完了後に着手（ログ基盤・ps コマンドの存在が前提）

- [x] 強化版 `lecs ps`
  - ヘルスチェック状態（HEALTHY/UNHEALTHY/UNKNOWN）、ポートマッピング、起動時間
  - CPU/メモリ使用量スナップショット（`docker stats` 相当）
  - 出力形式: table（デフォルト）、`--output json`
- [x] `lecs inspect <family>` — 実行中タスクの詳細表示
  - マージ済み実効設定（タスク定義 + オーバーライド + 解決済み Secrets、値は伏字）
  - ネットワーク構成、ポートマッピング、コンテナ ID・イメージ
- [x] `lecs stats [family]` — リソース使用量表示
  - CPU%、メモリ使用量、ネットワーク I/O、ブロック I/O をスナップショット表示
  - bollard の stats one-shot 利用
- [x] 構造化イベントログ
  - ライフサイクルイベント（作成、起動、ヘルスチェック通過/失敗、終了、クリーンアップ完了）
  - `--events` フラグで NDJSON 形式を stderr に出力（外部ツール連携用）

### Phase 8: ワークフロー高速化 ✅
**目標**: edit-run-debug サイクルを短縮する

> Phase 6 完了後に着手、Phase 7 と並行可能

- [x] `lecs watch` — ファイル変更監視 + 自動再起動
  - タスク定義、オーバーライド、secrets ファイルの変更を検知
  - 変更時: 停止 → 再パース → 再バリデーション → 再起動
  - デバウンス付き（デフォルト 500ms、`--debounce` で変更可能）
  - `--watch-path` でアプリソース等の追加監視パスを指定可能
- [x] 設定プロファイル（`--profile`）
  - `--profile dev` で `lecs-override.dev.json` / `secrets.dev.json` を自動ロード
  - `.lecs.toml` でデフォルトプロファイル・タスク定義パスを設定
- [x] `lecs completions <shell>` — シェル補完スクリプト生成
  - bash / zsh / fish 対応（`clap_complete` 利用）

### Phase 9: Terraform 互換性 ✅
**目標**: Terraform で管理された ECS タスク定義を直接利用可能にする

- [x] `--from-tf` フラグ — `terraform show -json` 出力を直接入力
  - Plan 出力（`planned_values`）と State 出力（`values`）の両方に対応
  - `resource_changes` フォールバック
  - 子モジュール内リソースの再帰的探索
- [x] `--tf-resource` フラグ — 複数 ECS リソースから1つを選択
- [x] 二重デシリアライゼーション — `container_definitions` JSON 文字列の変換
- [x] Volume 変換 — Terraform の `volume`（snake_case）を Lecs の `volumes`（camelCase）に変換
- [x] 対応コマンド: `lecs run`, `lecs validate`, `lecs watch`

### Phase 10a: タスク定義フィールド完全対応 ✅
**目標**: パース済みフィールドの実適用 + 高重要度フィールドの追加

> ECS CLI との機能比較で、パース済みだが未使用のフィールド（cpu/memory）や、ローカル開発で頻繁に必要となるフィールド（workingDirectory, user, stopTimeout 等）が未対応であることを確認。小工数で「本番との挙動差」を大幅に削減できる。

- [x] `cpu` / `memory` / `memoryReservation` をコンテナリソース制限に適用
  - `ContainerConfig` に `cpu_shares` / `memory` フィールド追加
  - `build_bollard_config` で `HostConfig` に反映
- [x] `workingDirectory` 対応
  - `ContainerDefinition` にフィールド追加、`Config::working_dir` に設定
- [x] `user` 対応
  - `ContainerDefinition` にフィールド追加、`Config::user` に設定
- [x] `stopTimeout` 対応（現在ハードコード 10 秒を置換）
  - `ContainerDefinition` にフィールド追加、`stop_container` のタイムアウト引数に反映
- [x] `dockerLabels` パススルー
  - `ContainerDefinition` にフィールド追加、lecs 管理ラベルとマージ（lecs ラベル優先）
- [x] `extraHosts` 対応
  - `ContainerDefinition` にフィールド追加、`host.docker.internal:host-gateway` と結合

### Phase 10b: CloudFormation / CDK 互換性 ✅

→ 詳細は後述の Phase 10b セクションを参照

### Phase 11: ECS Exec + 環境変数拡張 ✅
**目標**: デバッグ体験の向上

> `aws ecs execute-command` に相当する機能がなく、開発者は `docker exec` を直接叩く必要がある。`lecs exec` で Lecs 管理コンテナへの一貫したアクセスを提供する。

- [x] `lecs exec <container> [-- command]` — コンテナ内コマンド実行
  - 新規 `src/cli/exec.rs`、bollard の `exec_create` / `exec_start` 利用
  - stdout/stderr ストリーミング表示、デフォルト `/bin/sh`
- [x] `environmentFiles` 対応
  - `ContainerDefinition` にフィールド追加、ローカルファイル（.env 形式）を読み込み `environment` にマージ
- [x] `ulimits` 対応
  - `ContainerDefinition` にフィールド追加、`HostConfig::ulimits` に反映
- [x] `linuxParameters` 主要フィールド対応
  - `initProcessEnabled` → `HostConfig::init`
  - `tmpfs` → `HostConfig::tmpfs`
  - `sharedMemorySize` → `HostConfig::shm_size`

### Phase 12: サービスモード MVP ✅
**目標**: コンテナ障害時の自動再起動

> `docs/design/service-gap-analysis.md` に基づく。ECS Service の最小要件であるリスタートポリシーと、長時間稼働に必要なクレデンシャルローテーションを実装する。

- [x] リスタートポリシー
  - `RestartPolicy` enum（`None` / `OnFailure` / `Always`）+ 指数バックオフ（1s → 2s → ... → 300s）
  - essential コンテナ終了時の再起動判定ロジック
- [x] `lecs run --service` フラグ
  - reconciliation ループ有効化、Ctrl+C まで常時稼働
- [x] クレデンシャルローテーション
  - `tokio::spawn` でバックグラウンドリフレッシュ
  - `AppState` 内の credentials を `RwLock` で保護、TTL/2 間隔で更新

---

## 将来検討

- **Phase 13**: ネットワークモード拡張（`networkMode: host` / `none`）
- **Phase 14**: Docker Compose 互換（`lecs convert`, `lecs run --compose`）

---

### Phase 10b: CloudFormation / CDK 互換性 ✅
**目標**: CloudFormation テンプレート（CDK synth 出力含む）から直接タスク定義を読み込めるようにする

- [x] `--from-cfn` フラグ — CloudFormation テンプレート JSON を直接入力
  - PascalCase → camelCase キー変換による既存型との互換性確保
  - Intrinsic Function（`Ref`, `Fn::Sub` 等）の検出・エラー報告
- [x] `--cfn-resource` フラグ — 複数 ECS リソースから1つを論理IDで選択
- [x] 対応コマンド: `lecs run`, `lecs validate`, `lecs watch`
- [ ] YAML テンプレート対応（将来対応）
- [ ] `--from-cdk` による `cdk.out/` ディレクトリ自動探索（将来対応）

---

## 実装順序とPhase間の依存関係

```
Phase 0-2.5: ✅ 完了
    │
    ├── Phase 3 (Metadata + Credentials) ✅
    │       │
    ├── Phase 4 (dependsOn + Health Check) ✅
    │       │
    ├── Phase 5 (Volumes + Logs + ps) ✅
    │       │
    ├── Phase 6 (Validate/Init/Dry-run) ✅
    │       │
    │       ├── Phase 7 (可観測性) ✅
    │       │
    │       └── Phase 8 (ワークフロー高速化) ✅
    │               │
    │               └── Phase 9 (Terraform 互換性) ✅
    │                       │
    │                       ├── Phase 10a (タスク定義フィールド完全対応) ✅
    │                       │       │
    │                       │       └── Phase 11 (ECS Exec + 環境変数拡張) ✅
    │                       │
    │                       ├── Phase 10b (CloudFormation / CDK 互換性) ✅
    │                       │
    │                       └── Phase 12 (サービスモード MVP)
```

---

## 対象外（明示的に除外）

- Fargate 完全再現 / ENI 完全互換
- ALB / Cloud Map / Service Connect
- Capacity providers / Deployment circuit breaker
- FireLens 本番同等挙動
- Auto Scaling / ローリングデプロイ（Phase 12 MVP には含まず）
- コンテナイメージのビルド（Docker/Buildah/Kaniko の責務）
- Prometheus / Grafana 等の外部監視スタック連携
- awsvpc ネットワークモード完全再現
- Service Mesh / Service Connect
- ECR 認証付き push/pull（Docker CLI の責務、匿名 pull は自動実行）
- Cluster / Container Instance 管理（ローカルでは不要）
