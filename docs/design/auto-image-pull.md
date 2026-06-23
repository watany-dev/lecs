# 自動イメージ pull 設計書

## 概要

コンテナ作成前にイメージを自動的に pull する機能。bollard の `create_image` API（Docker Engine API `POST /images/create`）を使用し、ユーザーが手動で `docker pull` する手間を排除する。

**対応要件**: FR-2.7

**解決する課題**: Docker Engine API の `create_container` はローカルにイメージが存在しない場合にエラーを返す（Docker CLI のように自動 pull しない）。開発者は `lecs run` の前に手動で `docker pull` する必要があり、特に初回起動やイメージ更新時にフリクションが発生していた。

**成果**: `lecs run` 実行時にイメージが自動的に pull され、ローカルにキャッシュ済みのイメージは高速な no-op で通過する。

**設計判断: orchestrator で pull する理由**:
CLI 層（`run.rs`）で一括 pull する方式も考えられるが、`dependsOn` DAG の各レイヤーでコンテナを作成する直前に pull することで、(1) 不要なイメージの先行 pull を避け、(2) pull 失敗時の部分起動状態を orchestrator の既存エラーハンドリング（partial `StartupResult` 返却）に統合できる。

---

## データフロー

### 初回起動（`orchestrate_startup`）

```
lecs run -f task-def.json
    │
    ▼
orchestrate_startup(specs)
    │
    ▼ DAG レイヤーごと
    │
    ├── create_and_start_container(spec)
    │     │
    │     ├── parse_image_reference(spec.config.image)
    │     │     → (repository, tag)
    │     │
    │     ├── client.pull_image(image)
    │     │     → bollard::create_image(CreateImageOptions { from_image, tag })
    │     │     → Stream<CreateImageInfo> を try_collect で消費
    │     │     → 失敗時: OrchestratorError::Runtime → 部分 StartupResult で返却
    │     │
    │     ├── emit(ImagePulled, family, container_name, image)
    │     │
    │     ├── client.create_container(config)
    │     │     → emit(Created)
    │     │
    │     └── client.start_container(id)
    │           → emit(Started)
    │
    └── wait_for_condition() [次レイヤーへの待機]
```

### サービスモード再起動（`restart_container`）

```
restart_container(client, name, old_id, config)
    │
    ├── emit(Restarting)
    │
    ├── client.pull_image(config.image)        ← ベストエフォート
    │     → 失敗時: tracing::warn のみ（再起動を中断しない）
    │
    ├── stop_container → remove_container
    │
    ├── create_container → start_container
    │
    └── update_container_id (metadata)
```

---

## 準拠する標準

| 標準 | 用途 |
|------|------|
| Docker Engine API `POST /images/create` | イメージ pull のプロトコル |
| OCI Distribution Specification | レジストリからのイメージ配布 |
| Docker イメージ参照形式 | `[registry/]repository[:tag\|@digest]` のパース |

---

## 技術選定

| 候補 | 判断 | 理由 |
|------|------|------|
| bollard `create_image` | **採用** | 既存の bollard 依存を活用。`Stream<CreateImageInfo>` でプログレスを受け取れる（将来の進捗表示に拡張可能）|
| Docker CLI ラップ (`docker pull`) | 不採用 | プロセス生成のオーバーヘッド、出力パースの脆弱性。bollard で統一すべき |
| `create_container` のエラーから pull にフォールバック | 不採用 | 二重のエラーパスが複雑。「常に pull」の方が一貫性がある（キャッシュヒット時は高速） |

新規依存クレートは不要（bollard 0.18 の既存 API を使用）。

---

## モジュール配置

| ファイル | 変更内容 |
|---------|---------|
| `src/container/mod.rs` | `pull_image` トレイトメソッド + `ContainerClient` 実装 + `parse_image_reference` ヘルパー + `MockContainerClient` 拡張 |
| `src/orchestrator/mod.rs` | `create_and_start_container` にイメージ pull を挿入 |
| `src/cli/task_lifecycle.rs` | `restart_container` にベストエフォート pull を追加 |
| `src/events/mod.rs` | `EventType::ImagePulled` バリアント追加 |

---

## 型定義

### `ContainerRuntime` トレイト拡張（`src/container/mod.rs`）

```rust
pub trait ContainerRuntime: Send + Sync {
    // ... 既存メソッド ...

    /// Pull a container image from a registry.
    ///
    /// The `image` parameter is the full image reference (e.g., `nginx:latest`,
    /// `registry.example.com/app:v1.0`). The implementation should handle
    /// splitting the reference into repository and tag components.
    async fn pull_image(&self, image: &str) -> Result<(), ContainerError>;
}
```

### `MockContainerClient` 拡張（`src/container/mod.rs`）

```rust
pub struct MockContainerClient {
    // ... 既存フィールド ...
    pub pull_image_results: Mutex<VecDeque<Result<(), ContainerError>>>,
}
```

### `EventType` 拡張（`src/events/mod.rs`）

```rust
pub enum EventType {
    /// Container image was pulled.
    ImagePulled,
    // ... 既存バリアント ...
}
```

---

## ヘルパー関数

### `parse_image_reference`（`src/container/mod.rs`）

```rust
/// Parse an image reference into (repository, tag) components.
fn parse_image_reference(image: &str) -> (&str, &str)
```

イメージ参照文字列を bollard `CreateImageOptions` に渡す `from_image` と `tag` に分離する。

**パースルール**:

| 入力 | `from_image` | `tag` | 説明 |
|------|-------------|-------|------|
| `nginx` | `nginx` | `latest` | タグ省略時はデフォルト `latest` |
| `nginx:1.25` | `nginx` | `1.25` | 基本形式 |
| `library/nginx:alpine` | `library/nginx` | `alpine` | 名前空間付き |
| `registry.example.com:5000/app` | `registry.example.com:5000/app` | `latest` | レジストリのポート番号をタグと誤認しない |
| `registry.example.com:5000/app:v1` | `registry.example.com:5000/app` | `v1` | レジストリのポート + タグ |
| `nginx@sha256:abc...` | `nginx@sha256:abc...` | `""` | digest 参照はそのまま（タグなし） |

**アルゴリズム**:
1. `@` を含む → digest 参照。`(image, "")` を返す
2. 最後の `/` の位置を特定（レジストリ部分のポート番号のコロンを除外）
3. `/` 以降の部分で最後の `:` を探す → タグの区切り
4. `:` が見つからない → `(image, "latest")`

---

## エラーハンドリング

| シナリオ | 挙動 | 理由 |
|----------|------|------|
| イメージが存在しない（404） | `ContainerError::Api` → `OrchestratorError::Runtime` → タスク起動中止 | 存在しないイメージでの起動は必ず失敗する |
| レジストリ接続エラー | 同上 | ネットワーク問題はリトライで解決しない可能性が高い |
| ローカルキャッシュのみ利用（レジストリ未到達）| Docker Engine がキャッシュから解決 | Docker Engine API の標準動作 |
| サービスモード再起動時の pull 失敗 | `tracing::warn` のみで続行 | リスタートをブロックすべきでない（イメージはローカルにある可能性が高い） |

### 初回起動 vs サービスモードの非対称設計

初回起動時はイメージが存在しない可能性が高いため、pull 失敗はハードエラーにする。一方、サービスモードの再起動時は直前まで同じイメージで動作していたため、pull 失敗しても既存キャッシュで `create_container` が成功する可能性が高い。このため、再起動時はベストエフォート（警告のみ）とする。

---

## テスト戦略

| テスト対象 | テスト数 | カテゴリ |
|-----------|---------|---------|
| `parse_image_reference` — 基本名 | 1 | ユニット |
| `parse_image_reference` — タグ付き | 1 | ユニット |
| `parse_image_reference` — レジストリ+ポート | 1 | ユニット |
| `parse_image_reference` — レジストリ+ポート+タグ | 1 | ユニット |
| `parse_image_reference` — digest 参照 | 1 | ユニット |
| `parse_image_reference` — 名前空間付き | 1 | ユニット |
| `orchestrate_startup` — image pull 失敗 | 1 | ユニット |
| `orchestrate_startup` — 正常起動（pull 成功込み） | 4 (既存修正) | ユニット |
| `restart_container` — pull 成功込み | 6 (既存修正) | ユニット |
| `run_task` — pull 成功込み | 3 (既存修正) | ユニット |

新規テスト: 7、既存テスト修正: 13（`pull_image_results` エンキュー追加）

合計テスト数: 585 → 592

---

## ライフサイクルイベント

`ImagePulled` イベントは `--events` フラグ有効時に NDJSON で出力される:

```json
{
  "timestamp": "2026-04-07T12:00:00.000Z",
  "event_type": "image_pulled",
  "container_name": "app",
  "family": "my-app",
  "details": "nginx:latest"
}
```

イベント発行タイミング: `pull_image()` 成功直後、`create_container()` 直前。

---

## 制限事項

- **レジストリ認証**: bollard `create_image` の `credentials` パラメータは `None` を渡しており、認証付きレジストリ（ECR、GCR 等）への pull は未対応。Docker/Podman のローカル認証情報（`~/.docker/config.json`）はランタイム側で処理される場合がある
- **プログレス表示**: `create_image` が返す `Stream<CreateImageInfo>` を `try_collect` で消費しており、レイヤーごとの進捗表示は行わない（将来の拡張ポイント）
- **並列 pull**: 同一レイヤー内の複数コンテナのイメージ pull は順次実行。並列化は将来の最適化候補
- **pull スキップオプション**: `--no-pull` のようなフラグでオフライン環境向けに pull をスキップする機能は未実装（将来対応）

---

## 将来の拡張ポイント

| 項目 | 優先度 | 説明 |
|------|--------|------|
| `--no-pull` フラグ | 中 | オフライン環境・CI でイメージ pull をスキップ |
| プログレス表示 | 低 | レイヤーごとの pull 進捗を stderr に表示 |
| 並列 pull | 低 | 同一レイヤー内の複数イメージを `tokio::join!` で並列 pull |
| レジストリ認証 | 低 | `executionRoleArn` を活用した ECR 認証（Docker CLI の責務と重複） |
