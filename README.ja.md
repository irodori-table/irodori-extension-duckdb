<!-- i18n: language-switcher -->
[English](README.md) | [日本語](README.ja.md)

# DuckDBコネクタ

Irodoriテーブルコネクタのネイティブ拡張機能 for DuckDB。

このクレートは、コネクタのメタデータ、ネイティブABIエクスポート、およびIrodori拡張マーケットプレイスで使用されるドライバ実装をパッケージ化しています。

## コネクタ

- 拡張機能ID: `irodori.duckdb`
- エンジンID: `duckdb`
- ワイヤープロトコル: `duckdb`
- デフォルトポート: `0`
- ネイティブABI: `irodori.connector.native.v1`
- ドライバリンク済み: `はい`
- マーケットプレイスの表示: `公開`
- パッケージバージョン: `0.1.3`

このパッケージには、`db/duck.rs`からのデスクトップアダプタのソーススナップショットが含まれています。

コネクタのメタデータは`connector.config.json`と`irodori.extension.json`に格納されています。
Rustクレートは`src/lib.rs`からネイティブABIをエクスポートし、`irodori-connector-abi`を使用して共有JSON/バッファヘルパーを提供し、コネクタの動作は`src/driver.rs`に保持されています。

## 接続メタデータ

- エンドポイントモード: `localFile`, `inMemory`, `connectionString`
- トランスポートモード: `localFile`, `direct`
- TLS対応: `いいえ`
- デフォルトでTLS必要: `いいえ`
- カスタムドライバオプション: `はい`

### エンドポイントフィールド

| フィールド | ラベル | 型 | 必須 |
| --- | --- | --- | --- |
| `database` | データベースファイルまたは:memory: | `path` | いいえ |

## 認証

コネクタはこれらの認証モードを宣伝しており、クライアントは適切な資格情報フィールドをレンダリングできます。必要に応じて、`options`を通じてドライバ固有またはプロバイダ固有の値を渡すことも可能です。

| 認証方法 | ラベル | 種類 | シークレットの用途 |
| --- | --- | --- | --- |
| `none` | 認証なし | `none` | なし |
| `connectionString` | 接続文字列 / DSN | `connectionString` | なし |
| `extensionCredential` | 拡張機能またはリモートストレージトークン | `token` | `token` |
| `customDriverOptions` | カスタムドライバオプション | `custom` | `password`, `token`, `privateKey`, `privateKeyPassphrase` |

## ネイティブABI呼び出し

| メソッド | 応答 |
| --- | --- |
| `health` | コネクタのヘルス状態、エンジンID、ABIバージョン、ドライバの状態を返します。 |
| `describe` | 埋め込みマニフェストとコネクタ設定を返します。 |
| `manifest` | 生の`irodori.extension.json`を返します。 |
| `config` | 生の`connector.config.json`を返します。 |
| `connect` | ネイティブコネクタ接続を開き、検証します。 |
| `query` | コネクタクエリを実行し、構造化された行またはJSON結果を返します。 |
| `metadata` | スキーマ、テーブル、カラム、インデックス、コレクション、または同等のメタデータを読み取ります。 |
| `close` | キャッシュされたネイティブ接続を閉じて削除します。 |

## 開発

このチェックアウト内のすべての拡張クレートは`../target`を共有しており、依存関係は兄弟リポジトリ間で一度だけコンパイルされます。

```sh
make check
make build
```

リリースパッケージは、プラットフォーム固有のネイティブアーティファクトを`dist/native`に配置します。

## ライセンス

0BSD。ほぼすべての目的でこのプロジェクトを使用、コピー、修正、配布できます。