# コンテナランタイムクライアント設計書

## 概要

bollard クレートを通じて OCI 互換コンテナランタイム（Docker / Podman）と連携するモジュール。
コンテナ・ネットワークの作成・起動・停止・削除およびログストリームを提供する。
`src/container/mod.rs` に実装する。

## 設計方針

- bollard の API を薄くラップし、Lecs 固有のロジック（ラベル管理、命名規則）をカプセル化
- すべての操作は `async` で提供
- Docker と Podman の両方をサポート（Podman は Docker 互換 API を提供）
- ソケット接続は自動検出 + 明示指定の両方に対応

## 準拠する標準

| 標準 | 用途 |
|------|------|
| OCI Runtime Specification | コンテナ実行の標準仕様（Docker/Podman 共に準拠） |
| OCI Image Specification | コンテナイメージの標準仕様 |
| Docker Engine API | bollard が実装するデファクト標準（Podman も互換 API を提供） |
| `CONTAINER_HOST` 環境変数 | Podman 標準のソケット指定方法 |
| `DOCKER_HOST` 環境変数 | Docker 標準のソケット指定方法（bollard が処理） |
| XDG Base Directory Specification | rootless Podman ソケットパスの `$XDG_RUNTIME_DIR` 準拠 |

## 技術選定

| 候補 | 判断 | 理由 |
|------|------|------|
| `bollard` | **採用** | 純 Rust 実装の Docker Engine API クライアント。async/await 対応。活発にメンテナンスされている。Podman の Docker 互換 API とも動作する |
| `docker-api` | 不採用 | bollard より利用者が少なく、API カバレッジも限定的 |
| `shiplift` | 不採用 | メンテナンスが停滞（最終リリースが古い） |
| Docker CLI ラップ | 不採用 | プロセス生成のオーバーヘッド、出力パースの脆弱性、エラーハンドリングの困難さ |

`futures-util` は bollard が返す `Stream` を処理するために必要。

### 依存クレート

```toml
[dependencies]
bollard = "0.18"
futures-util = "0.3"
```

bollard `0.18` は本設計書作成時点（2026-03）の最新安定版。Docker Engine API v1.44+ に対応。

## ソケット接続優先順位

コンテナランタイムへの接続は以下の優先順位で試行する:

| 優先度 | ソース | 説明 |
|--------|--------|------|
| 1 | `--host` フラグ / `CONTAINER_HOST` 環境変数 | ユーザーの明示指定（clap `env` 属性で読み取り） |
| 2 | `DOCKER_HOST` 環境変数 | bollard の `connect_with_local_defaults` が処理 |
| 3 | Docker 標準ソケット | bollard のデフォルト動作（`/var/run/docker.sock` 等） |
| 4 | Rootless Podman ソケット | `$XDG_RUNTIME_DIR/podman/podman.sock` |
| 5 | Rootful Podman ソケット | `/run/podman/podman.sock` |

### ホスト URL フォーマット

`--host` フラグは以下の形式を受け付ける:

| 形式 | 例 | 接続方法 |
|------|-----|----------|
| `unix://` プレフィックス | `unix:///run/podman/podman.sock` | `Docker::connect_with_unix` |
| `tcp://` プレフィックス | `tcp://localhost:2375` | `Docker::connect_with_http` |
| 素パス | `/run/podman/podman.sock` | Unix ソケットとして扱う |

## ラベル戦略

Lecs が管理するリソースを識別するために、OCI ラベルを付与する:

| ラベルキー | 値 | 用途 |
|---|---|---|
| `lecs.managed` | `"true"` | Lecs が作成したリソースの識別 |
| `lecs.task` | `<family>` | タスクファミリー名 |
| `lecs.container` | `<container-name>` | コンテナ定義名（コンテナのみ） |

ネットワークにも `lecs.managed` と `lecs.task` ラベルを付与する。

## 型定義

```rust
use std::collections::HashMap;

use bollard::Docker;

/// Lecs 用コンテナランタイムクライアント
pub struct ContainerClient {
    docker: Docker,
}

/// コンテナ作成時の設定
pub struct ContainerConfig {
    /// コンテナ名（`<family>-<container_name>` 形式）
    pub name: String,
    /// OCI コンテナイメージ
    pub image: String,
    /// CMD
    pub command: Vec<String>,
    /// ENTRYPOINT
    pub entry_point: Vec<String>,
    /// 環境変数（`KEY=VALUE` 形式）
    pub env: Vec<String>,
    /// ポートマッピング
    pub port_mappings: Vec<PortMappingConfig>,
    /// 接続するネットワーク名
    pub network: String,
    /// ネットワーク内のエイリアス（コンテナ定義の name）
    pub network_aliases: Vec<String>,
    /// コンテナラベル
    pub labels: HashMap<String, String>,
    /// 追加ホストエントリ（Phase 3 で追加: `host.docker.internal:host-gateway` 等）
    pub extra_hosts: Vec<String>,
    /// ヘルスチェック設定（Phase 4 で追加）
    pub health_check: Option<HealthCheckConfig>,
}

/// ヘルスチェック設定（ナノ秒単位、bollard 互換）
pub struct HealthCheckConfig {
    pub test: Vec<String>,
    pub interval_ns: i64,
    pub timeout_ns: i64,
    pub retries: i64,
    pub start_period_ns: i64,
}

/// ポートマッピング設定
pub struct PortMappingConfig {
    pub host_port: u16,
    pub container_port: u16,
    pub protocol: String,
}

/// Lecs が管理するコンテナの情報
pub struct ContainerInfo {
    /// コンテナ ID
    pub id: String,
    /// コンテナ名
    pub name: String,
    /// タスクファミリー名（lecs.task ラベルの値）
    pub family: String,
    /// コンテナの状態（running, exited 等）
    pub state: String,
}

/// Lecs が管理するネットワークの情報
pub struct NetworkInfo {
    /// ネットワーク ID
    pub id: String,
    /// ネットワーク名
    pub name: String,
}

/// コンテナ検査結果（Phase 4 で追加）
pub struct ContainerInspection {
    pub id: String,
    pub state: ContainerState,
}

/// コンテナ状態（Phase 4 で追加）
pub struct ContainerState {
    pub status: String,
    pub running: bool,
    pub exit_code: Option<i64>,
    pub health_status: Option<String>,
}

/// コンテナ終了待機結果（Phase 4 で追加）
pub struct WaitResult {
    pub status_code: i64,
}
```

## エラー型

```rust
#[derive(Debug, thiserror::Error)]
pub enum ContainerError {
    /// コンテナランタイムに接続できない
    #[error("Container runtime is not running. Please start Docker or Podman and try again.")]
    RuntimeNotRunning,

    /// コンテナランタイム API エラー（bollard からの伝搬）
    #[error("Container runtime API error: {0}")]
    Api(#[from] bollard::errors::Error),
}
```

CLI 層で `ContainerError` を `anyhow` に変換して表示する。
`RuntimeNotRunning` はユーザーフレンドリーなメッセージを提供するために分離する。

## 公開 API

### `ContainerRuntime` トレイト

テスタビリティのため、コンテナランタイム操作を抽象化するトレイトを定義する。
`ContainerClient` はこのトレイトを実装し、テストでは `MockContainerClient` で差し替える。

Rust 1.93.0+ の RPITIT（Return Position Impl Trait in Traits）を使用し、`#[async_trait]` マクロは不要:

```rust
pub trait ContainerRuntime: Send + Sync {
    async fn create_network(&self, family: &str) -> Result<String, ContainerError>;
    async fn remove_network(&self, name: &str) -> Result<(), ContainerError>;
    async fn list_networks(&self, task_filter: Option<&str>) -> Result<Vec<NetworkInfo>, ContainerError>;
    async fn create_container(&self, config: &ContainerConfig) -> Result<String, ContainerError>;
    async fn start_container(&self, id: &str) -> Result<(), ContainerError>;
    async fn stop_container(&self, id: &str) -> Result<(), ContainerError>;
    async fn remove_container(&self, id: &str) -> Result<(), ContainerError>;
    async fn list_containers(&self, task_filter: Option<&str>) -> Result<Vec<ContainerInfo>, ContainerError>;
    async fn inspect_container(&self, id: &str) -> Result<ContainerInspection, ContainerError>;
    async fn wait_container(&self, id: &str) -> Result<WaitResult, ContainerError>;
    async fn pull_image(&self, image: &str) -> Result<(), ContainerError>;
}
```

### `ContainerClient` 実装

```rust
use futures_util::Stream;

impl ContainerClient {
    /// コンテナランタイムに接続し、ping で接続確認を行う
    /// 接続失敗時は ContainerError::RuntimeNotRunning を返す
    ///
    /// host: --host フラグまたは CONTAINER_HOST 環境変数からの値
    pub async fn connect(host: Option<&str>) -> Result<Self, ContainerError>;

    /// 指定された URL に直接接続する
    pub async fn connect_to_host(url: &str) -> Result<Self, ContainerError>;

    // --- ネットワーク ---

    /// Lecs 専用ネットワークを作成する
    /// ネットワーク名: `lecs-<family>`
    /// ドライバ: bridge（コンテナ名での DNS 解決が有効）
    /// 同名ネットワークが既存の場合はそのまま再利用する
    pub async fn create_network(
        &self,
        family: &str,
    ) -> Result<String, ContainerError>;

    /// ネットワークを削除する
    pub async fn remove_network(&self, name: &str) -> Result<(), ContainerError>;

    /// Lecs が管理するネットワーク一覧を取得する
    /// task_filter: Some(<family>) で特定タスクに絞り込み、None で全て
    pub async fn list_networks(
        &self,
        task_filter: Option<&str>,
    ) -> Result<Vec<NetworkInfo>, ContainerError>;

    // --- コンテナ ---

    /// コンテナを作成する（起動はしない）
    /// 戻り値はコンテナ ID
    pub async fn create_container(
        &self,
        config: &ContainerConfig,
    ) -> Result<String, ContainerError>;

    /// コンテナを起動する
    pub async fn start_container(&self, id: &str) -> Result<(), ContainerError>;

    /// コンテナを停止する（タイムアウト: 10秒、超過時は kill）
    pub async fn stop_container(&self, id: &str) -> Result<(), ContainerError>;

    /// コンテナを削除する
    pub async fn remove_container(&self, id: &str) -> Result<(), ContainerError>;

    /// Lecs が管理するコンテナ一覧を取得する
    /// task_filter: Some(<family>) で特定タスクに絞り込み、None で全て
    pub async fn list_containers(
        &self,
        task_filter: Option<&str>,
    ) -> Result<Vec<ContainerInfo>, ContainerError>;

    // --- ログ ---

    /// コンテナのログをストリームとして返す
    /// follow=true でリアルタイムストリーム
    pub fn stream_logs(
        &self,
        id: &str,
    ) -> impl Stream<Item = Result<String, ContainerError>> + '_;
}
```

### ヘルパー関数

```rust
/// ホスト URL を解析し、スキームとパスに分離する
fn parse_host_url(url: &str) -> (HostScheme, &str);

/// Podman ソケットの候補パスを返す（rootless → rootful の順）
fn podman_socket_candidates() -> Vec<String>;

/// ContainerConfig を bollard の Config に変換する純粋関数
/// extra_hosts が設定されている場合は HostConfig.extra_hosts に反映する
/// health_check が設定されている場合は bollard::models::HealthConfig に変換する
fn build_bollard_config(config: &ContainerConfig) -> Config<String>;
```

## ネットワーク設計

```
┌─────────────────────────────────────────┐
│        lecs-<family> (bridge)          │
│                                         │
│  ┌───────────┐     ┌───────────┐       │
│  │  app       │────▶│  redis    │       │
│  │ (container)│     │(container)│       │
│  └───────────┘     └───────────┘       │
│       DNS: app          DNS: redis      │
└─────────────────────────────────────────┘
         │
         ▼ port mapping
    host:8080 → app:80
```

- bridge ネットワーク内では、コンテナ名がそのまま DNS 名として解決される（Docker/Podman 共通）
- ECS の `awsvpc` ネットワークモードに近い挙動を bridge + DNS で再現
- コンテナ名は `<family>-<container_name>` 形式（例: `my-app-app`, `my-app-redis`）
- ネットワーク内のエイリアスとしてコンテナ定義の `name` をそのまま設定（例: `app`, `redis`）

### ネットワークエイリアスの設定

bollard API では `EndpointSettings::aliases` にエイリアスを設定する:

```rust
use bollard::models::EndpointSettings;
use std::collections::HashMap;

// コンテナ作成時の NetworkingConfig でエイリアスを指定
let endpoint_settings = EndpointSettings {
    aliases: Some(config.network_aliases.clone()),
    ..Default::default()
};

let networking_config = NetworkingConfig {
    endpoints_config: HashMap::from([
        (config.network.clone(), endpoint_settings),
    ]),
};
```

これにより、同一ネットワーク内の他コンテナから `app` や `redis` といったコンテナ定義名で名前解決が可能になる。

## コンテナ命名規則

| 項目 | 形式 | 例 |
|---|---|---|
| ネットワーク名 | `lecs-<family>` | `lecs-my-app` |
| コンテナ名 | `<family>-<container_name>` | `my-app-app` |
| ネットワークエイリアス | `<container_name>` | `app` |

## エラーハンドリング

| エラー状況 | ContainerError バリアント | 対応 |
|---|---|---|
| コンテナランタイム未起動 | `RuntimeNotRunning` | `connect()` で ping 失敗時に検出 |
| イメージ pull 失敗 | `Api(...)` | コンテナ作成前の `pull_image` で検出・報告 |
| ポート競合 | `Api(...)` | ランタイム API エラーをそのまま伝搬 |
| ネットワーク既存 | — | `create_network` 内で既存を検出し再利用（エラーにしない） |
| コンテナ停止タイムアウト | `Api(...)` | 10 秒後にランタイムが自動 kill |

## テスト戦略

コンテナランタイム API を直接呼ぶ統合テストは CI 環境に依存するため、Phase 1 では以下のアプローチ:

1. **`ContainerConfig` のビルドロジック**: `TaskDefinition` → `ContainerConfig` 変換のユニットテスト（`cli/run.rs` 側）
2. **ラベル・命名規則**: ラベル生成、コンテナ名・ネットワーク名の生成ロジックのユニットテスト
3. **ソケット検出ロジック**: `podman_socket_candidates()` と `parse_host_url()` のユニットテスト
4. **ランタイム API 呼び出し**: 手動テスト（Docker または Podman が利用可能な環境で `cargo run` による確認）

## Phase 3 で追加された機能

- `extra_hosts` フィールド: `ContainerConfig` に追加。`build_bollard_config()` で `HostConfig.extra_hosts` に反映
- メタデータサーバーアクセス用に `host.docker.internal:host-gateway` を全コンテナに設定

## Phase 4 で追加された機能

- `HealthCheckConfig` 構造体: `ContainerConfig` にヘルスチェック設定を追加
- `build_bollard_config()` で `HealthCheckConfig` → `bollard::models::HealthConfig` に変換（ナノ秒単位）
- `inspect_container()`: コンテナの状態とヘルスステータスを取得（`bollard::Docker::inspect_container` のラッパー）
- `wait_container()`: コンテナの終了を待機（`bollard::Docker::wait_container` の Stream から最初のアイテムを取得）
- `MockContainerClient` に `inspect_container_results` / `wait_container_results` キューを追加

## 自動イメージ pull

コンテナ作成前に `pull_image()` で自動的にイメージを pull する。

- bollard の `create_image` API（Docker Engine API `POST /images/create`）を使用
- イメージ参照を repository と tag に分離する `parse_image_reference()` ヘルパー
  - `nginx` → `("nginx", "latest")`
  - `nginx:1.25` → `("nginx", "1.25")`
  - `registry.example.com:5000/app:v1` → `("registry.example.com:5000/app", "v1")`
  - `nginx@sha256:abc...` → digest 参照としてそのまま渡す
- ローカルにキャッシュ済みのイメージは高速な no-op
- `orchestrate_startup()` の各コンテナ作成前に実行
- `restart_container()` でもベストエフォートで再 pull（失敗時は警告のみで続行）
- `ImagePulled` ライフサイクルイベントを発行

## 制限事項

- コンテナのリソース制限（CPU/メモリ）は設定するが、厳密な enforcement はランタイムに委ねる
- ボリュームマウントは未対応 → Phase 5
