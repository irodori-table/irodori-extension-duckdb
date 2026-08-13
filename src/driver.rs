use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::abi::{self, IrodoriConnectorBuffer};
use crate::{ABI_VERSION, CONFIG_JSON, ENGINE, MANIFEST_JSON};

static CONNECTIONS: OnceLock<Mutex<HashMap<String, duckdb::Connection>>> = OnceLock::new();

#[derive(Default)]
struct ObjectMeta {
    schema: String,
    name: String,
    kind: String,
    columns: Vec<Value>,
}

type QueryRows = Vec<Vec<Value>>;
type QueryOutput = (Vec<String>, QueryRows, bool);

fn connections() -> &'static Mutex<HashMap<String, duckdb::Connection>> {
    CONNECTIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn call_json(request: IrodoriConnectorBuffer) -> IrodoriConnectorBuffer {
    let request = match abi::parse_request(request) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let method = match abi::request_method(request.as_ref()) {
        Ok(method) => method,
        Err(response) => return response,
    };

    match method {
        "health" | "ping" => abi::ok(serde_json::Map::from_iter([
            ("engine".to_string(), Value::String(ENGINE.to_string())),
            ("abiVersion".to_string(), json!(ABI_VERSION)),
            ("driverLinked".to_string(), Value::Bool(true)),
        ])),
        "describe" | "capabilities" => abi::ok(serde_json::Map::from_iter([
            ("engine".to_string(), Value::String(ENGINE.to_string())),
            ("abiVersion".to_string(), json!(ABI_VERSION)),
            ("driverLinked".to_string(), Value::Bool(true)),
            (
                "manifest".to_string(),
                serde_json::from_str(MANIFEST_JSON).unwrap_or(Value::Null),
            ),
            (
                "config".to_string(),
                serde_json::from_str(CONFIG_JSON).unwrap_or(Value::Null),
            ),
        ])),
        "manifest" => abi::owned_buffer(MANIFEST_JSON.to_string()),
        "config" => abi::owned_buffer(CONFIG_JSON.to_string()),
        "connect" => connect(request.as_ref().expect("connect has request")),
        "query" => query(request.as_ref().expect("query has request")),
        "metadata" => metadata(request.as_ref().expect("metadata has request")),
        "close" => close(request.as_ref().expect("close has request")),
        other => abi::error(
            "connector.unknownMethod",
            format!("unknown connector method: {other}"),
        ),
    }
}

/// The object-store credentials a profile implies, as DuckDB `CREATE SECRET`
/// statements.
///
/// The connector used to forward a handful of `SET s3_*` settings, which covers
/// exactly one of the credential sources `connector.config.json` declares:
/// a static access key. DuckDB's secret manager is the surface for the rest —
/// the AWS credential chain (and with it SSO and web identity), and the Azure
/// providers.
///
/// Pure on purpose. The statements are the whole behaviour worth testing, and
/// asserting on them needs no DuckDB, no network, and no credentials.
fn secret_statements(request: &Value) -> Vec<String> {
    let mut statements = Vec::new();
    if let Some(s3) = s3_secret(request) {
        statements.push(s3);
    }
    if let Some(azure) = azure_secret(request) {
        statements.push(azure);
    }
    statements
}

/// `CREATE SECRET` for S3-compatible storage, or `None` when the profile names
/// no S3 credential source and DuckDB's own defaults should stand.
fn s3_secret(request: &Value) -> Option<String> {
    let mut params: Vec<(&str, String)> = Vec::new();

    let key_id = option_string(
        request,
        &["s3AccessKeyId", "accessKeyId", "awsAccessKeyId", "user"],
    )
    .filter(|value| looks_like_access_key_id(value));
    let secret = option_string(
        request,
        &[
            "s3SecretAccessKey",
            "secretAccessKey",
            "awsSecretAccessKey",
            "password",
        ],
    );
    let session_token = option_string(
        request,
        &["s3SessionToken", "sessionToken", "awsSessionToken"],
    );
    let profile = option_string(request, &["awsProfile", "profile"]);
    let chain = option_string(request, &["awsCredentialChain", "credentialChain"]);
    let use_chain = option_string(request, &["awsUseCredentialChain", "useCredentialChain"])
        .is_some_and(|value| matches!(value.trim(), "1" | "true" | "yes"));

    // A static key pair is an explicit instruction; anything else that names a
    // credential source falls through to the chain, which is what carries SSO,
    // web identity, ECS/IMDS, and a profile's own `role_arn`.
    match (key_id, secret) {
        (Some(key_id), Some(secret)) => {
            params.push(("PROVIDER", "config".to_string()));
            params.push(("KEY_ID", sql_string(&key_id)));
            params.push(("SECRET", sql_string(&secret)));
            if let Some(token) = session_token {
                params.push(("SESSION_TOKEN", sql_string(&token)));
            }
        }
        _ if use_chain || profile.is_some() || chain.is_some() => {
            params.push(("PROVIDER", "credential_chain".to_string()));
            if let Some(chain) = chain {
                params.push(("CHAIN", sql_string(&chain)));
            }
            if let Some(profile) = profile {
                params.push(("PROFILE", sql_string(&profile)));
            }
        }
        _ => return None,
    }

    for (option, key) in [
        (["s3Region", "region", "awsRegion"].as_slice(), "REGION"),
        (["s3Endpoint", "endpoint"].as_slice(), "ENDPOINT"),
        (["s3UrlStyle", "urlStyle"].as_slice(), "URL_STYLE"),
    ] {
        if let Some(value) = option_string(request, option) {
            params.push((key, sql_string(&value)));
        }
    }

    Some(create_secret("irodori_s3", "s3", &params))
}

/// `CREATE SECRET` for Azure Blob / ADLS, or `None` when the profile names no
/// Azure credential source.
fn azure_secret(request: &Value) -> Option<String> {
    let mut params = azure_provider_params(request)?;
    if let Some(account) = option_string(request, &["azureAccountName", "accountName"]) {
        params.push(("ACCOUNT_NAME", sql_string(&account)));
    }
    Some(create_secret("irodori_azure", "azure", &params))
}

/// Which Azure provider the profile is asking for, and its parameters.
///
/// Split out so each branch can answer for itself; a profile that names an
/// Azure credential source but cannot complete it (a service principal with
/// neither a secret nor a certificate) answers `None` rather than emitting a
/// secret that cannot authenticate — that would displace whatever DuckDB could
/// otherwise have used and fail later and less clearly.
fn azure_provider_params(request: &Value) -> Option<Vec<(&'static str, String)>> {
    // A SAS token is delivered as a connection string; DuckDB's config provider
    // is the only one that accepts one.
    if let Some(connection_string) = option_string(
        request,
        &["azureConnectionString", "azureSasToken", "sasToken"],
    ) {
        return Some(vec![
            ("PROVIDER", "config".to_string()),
            ("CONNECTION_STRING", sql_string(&connection_string)),
        ]);
    }

    let tenant_id = option_string(request, &["azureTenantId", "tenantId"]);
    let client_id = option_string(request, &["azureClientId", "clientId"]);
    if let (Some(tenant_id), Some(client_id)) = (tenant_id, client_id) {
        // Secret or certificate — a service principal authenticates with one or
        // the other, never neither.
        let credential = match (
            option_string(request, &["azureClientSecret", "clientSecret"]),
            option_string(
                request,
                &["azureClientCertificatePath", "clientCertificatePath"],
            ),
        ) {
            (Some(secret), _) => ("CLIENT_SECRET", sql_string(&secret)),
            (None, Some(path)) => ("CLIENT_CERTIFICATE_PATH", sql_string(&path)),
            (None, None) => return None,
        };
        return Some(vec![
            ("PROVIDER", "service_principal".to_string()),
            ("TENANT_ID", sql_string(&tenant_id)),
            ("CLIENT_ID", sql_string(&client_id)),
            credential,
        ]);
    }

    // `cli`, `managed_identity`, `env`, … — this is what carries Azure AD
    // interactive login and managed identity.
    let chain = option_string(request, &["azureCredentialChain", "azureChain"])?;
    Some(vec![
        ("PROVIDER", "credential_chain".to_string()),
        ("CHAIN", sql_string(&chain)),
    ])
}

fn create_secret(name: &str, kind: &str, params: &[(&str, String)]) -> String {
    let body = params
        .iter()
        .map(|(key, value)| format!("    {key} {value}"))
        .collect::<Vec<_>>()
        .join(",\n");
    format!("create or replace secret {name} (\n    TYPE {kind},\n{body}\n);")
}

/// An AWS access key id is 20 uppercase alphanumerics beginning with `A`
/// (`AKIA…` long-term, `ASIA…` temporary). The connection form overloads `user`
/// for both a profile name and an access key id, so the shape is what tells
/// them apart.
fn looks_like_access_key_id(value: &str) -> bool {
    value.len() == 20
        && value.starts_with('A')
        && value
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// Resolve a field from anywhere the host may put it.
///
/// `abi::profile_field` looks at the request and its `profile`, but connector
/// options arrive under `profile.options`, so a credential supplied as an
/// option would be invisible to it.
fn option_string(request: &Value, fields: &[&str]) -> Option<String> {
    let containers = [
        Some(request),
        request.get("profile"),
        request.get("options"),
        request.get("secrets"),
        request.get("profile").and_then(|p| p.get("options")),
        request.get("profile").and_then(|p| p.get("secrets")),
    ];
    containers.into_iter().flatten().find_map(|container| {
        fields.iter().find_map(|field| {
            container
                .get(*field)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        })
    })
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn connect(request: &Value) -> IrodoriConnectorBuffer {
    let connection_id = abi::connection_id(Some(request));
    let database =
        abi::profile_field(request, "database").or_else(|| abi::profile_field(request, "url"));
    let conn = match database.map(str::trim) {
        None | Some("") | Some(":memory:") => duckdb::Connection::open_in_memory(),
        Some(path) => duckdb::Connection::open(path),
    };
    let conn = match conn {
        Ok(conn) => conn,
        Err(err) => return abi::error("connector.connectFailed", format!("connect failed: {err}")),
    };
    // Credentials for the remote stores DuckDB can read through httpfs. Applied
    // before anything runs, so a query against s3:// or az:// in the very first
    // statement is authenticated.
    for statement in secret_statements(request) {
        if let Err(err) = conn.execute_batch(&statement) {
            return abi::error(
                "connector.invalidRequest",
                format!("DuckDB credential setup failed: {err}"),
            );
        }
    }
    let server_version = duckdb_version(&conn).unwrap_or_else(|| "unknown".to_string());
    if should_seed_sample(request, &connection_id) {
        if let Err(err) = seed_sample(&conn) {
            return abi::error("connector.seedFailed", err);
        }
    }
    let mut guard = match connections().lock() {
        Ok(guard) => guard,
        Err(_) => {
            return abi::error(
                "connector.statePoisoned",
                "Connector connection state is poisoned.",
            )
        }
    };
    guard.insert(connection_id.clone(), conn);
    abi::ok(serde_json::Map::from_iter([
        ("engine".to_string(), Value::String(ENGINE.to_string())),
        ("connectionId".to_string(), Value::String(connection_id)),
        ("serverVersion".to_string(), Value::String(server_version)),
        ("driverLinked".to_string(), Value::Bool(true)),
    ]))
}

fn query(request: &Value) -> IrodoriConnectorBuffer {
    let connection_id = abi::connection_id(Some(request));
    let Some(sql) = abi::string_field(request, "sql") else {
        return abi::error(
            "connector.invalidRequest",
            "query requires a string sql field.",
        );
    };
    let mut guard = match connections().lock() {
        Ok(guard) => guard,
        Err(_) => {
            return abi::error(
                "connector.statePoisoned",
                "Connector connection state is poisoned.",
            )
        }
    };
    let Some(conn) = guard.get_mut(&connection_id) else {
        return abi::error(
            "connector.connectionNotFound",
            format!("no open connection: {connection_id}"),
        );
    };
    match run_query(conn, sql, abi::max_rows(request)) {
        Ok((columns, rows, truncated)) => abi::ok(serde_json::Map::from_iter([
            ("connectionId".to_string(), Value::String(connection_id)),
            (
                "columns".to_string(),
                Value::Array(columns.into_iter().map(Value::String).collect()),
            ),
            (
                "rows".to_string(),
                Value::Array(
                    rows.into_iter()
                        .map(|row| Value::Array(row.into_iter().collect()))
                        .collect(),
                ),
            ),
            ("truncated".to_string(), Value::Bool(truncated)),
        ])),
        Err(err) => abi::error("connector.queryFailed", err),
    }
}

fn metadata(request: &Value) -> IrodoriConnectorBuffer {
    let connection_id = abi::connection_id(Some(request));
    let mut guard = match connections().lock() {
        Ok(guard) => guard,
        Err(_) => {
            return abi::error(
                "connector.statePoisoned",
                "Connector connection state is poisoned.",
            )
        }
    };
    let Some(conn) = guard.get_mut(&connection_id) else {
        return abi::error(
            "connector.connectionNotFound",
            format!("no open connection: {connection_id}"),
        );
    };
    match load_metadata(conn) {
        Ok(metadata) => abi::ok(serde_json::Map::from_iter([
            ("connectionId".to_string(), Value::String(connection_id)),
            ("metadata".to_string(), metadata),
        ])),
        Err(err) => abi::error("connector.metadataFailed", err),
    }
}

fn close(request: &Value) -> IrodoriConnectorBuffer {
    let connection_id = abi::connection_id(Some(request));
    let mut guard = match connections().lock() {
        Ok(guard) => guard,
        Err(_) => {
            return abi::error(
                "connector.statePoisoned",
                "Connector connection state is poisoned.",
            )
        }
    };
    let existed = guard.remove(&connection_id).is_some();
    abi::ok(serde_json::Map::from_iter([
        ("connectionId".to_string(), Value::String(connection_id)),
        ("closed".to_string(), Value::Bool(existed)),
    ]))
}

fn duckdb_version(conn: &duckdb::Connection) -> Option<String> {
    conn.query_row("select version()", [], |row| row.get::<_, String>(0))
        .ok()
}

fn should_seed_sample(request: &Value, connection_id: &str) -> bool {
    request
        .get("seedSample")
        .or_else(|| {
            request
                .get("profile")
                .and_then(|profile| profile.get("seedSample"))
        })
        .and_then(Value::as_bool)
        .unwrap_or(matches!(
            connection_id,
            "duckdb-memory" | "motherduck-memory"
        ))
}

fn seed_sample(conn: &duckdb::Connection) -> Result<(), String> {
    conn.execute_batch("create table if not exists customers (id integer, name varchar);")
        .map_err(|err| format!("duckdb sample schema failed: {err}"))?;
    let existing = conn
        .query_row("select count(*) from customers", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap_or(0);
    if existing == 0 {
        conn.execute_batch("insert into customers values (1, 'Kawase Foods'), (2, 'Minato Labs');")
            .map_err(|err| format!("duckdb sample data failed: {err}"))?;
    }
    Ok(())
}

fn run_query(conn: &duckdb::Connection, sql: &str, cap: usize) -> Result<QueryOutput, String> {
    let lead = sql.trim_start().to_ascii_lowercase();
    let is_query = [
        "select", "with", "show", "pragma", "explain", "describe", "values", "table", "call",
    ]
    .iter()
    .any(|keyword| lead.starts_with(keyword));
    if !is_query {
        conn.execute(sql, [])
            .map_err(|err| format!("query failed: {err}"))?;
        return Ok((Vec::new(), Vec::new(), false));
    }

    let mut stmt = conn
        .prepare(sql)
        .map_err(|err| format!("query failed: {err}"))?;
    let mut duck_rows = stmt
        .query([])
        .map_err(|err| format!("query failed: {err}"))?;
    let columns: Vec<String> = match duck_rows.as_ref() {
        Some(stmt) => stmt
            .column_names()
            .iter()
            .map(|column| column.to_string())
            .collect(),
        None => Vec::new(),
    };
    let column_count = columns.len();
    let mut rows = Vec::new();
    let mut truncated = false;
    while let Some(row) = duck_rows
        .next()
        .map_err(|err| format!("query failed: {err}"))?
    {
        if rows.len() >= cap {
            truncated = true;
            break;
        }
        rows.push(
            (0..column_count)
                .map(|index| cell_to_json(row, index))
                .collect(),
        );
    }
    Ok((columns, rows, truncated))
}

fn load_metadata(conn: &duckdb::Connection) -> Result<Value, String> {
    let mut objects: BTreeMap<(String, String), ObjectMeta> = BTreeMap::new();
    let mut stmt = conn
        .prepare(
            "select table_schema, table_name, table_type \
             from information_schema.tables \
             where table_schema not in ('information_schema', 'pg_catalog') \
             order by table_schema, table_name",
        )
        .map_err(|err| format!("metadata objects failed: {err}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|err| format!("metadata objects failed: {err}"))?;
    for row in rows {
        let (schema, name, table_type) =
            row.map_err(|err| format!("metadata objects failed: {err}"))?;
        let kind = if table_type.eq_ignore_ascii_case("VIEW") {
            "view"
        } else {
            "table"
        };
        objects.insert(
            (schema.clone(), name.clone()),
            ObjectMeta {
                schema,
                name,
                kind: kind.to_string(),
                columns: Vec::new(),
            },
        );
    }

    let mut stmt = conn
        .prepare(
            "select table_schema, table_name, column_name, data_type, is_nullable, ordinal_position \
             from information_schema.columns \
             where table_schema not in ('information_schema', 'pg_catalog') \
             order by table_schema, table_name, ordinal_position",
        )
        .map_err(|err| format!("metadata columns failed: {err}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i32>(5)?,
            ))
        })
        .map_err(|err| format!("metadata columns failed: {err}"))?;
    for row in rows {
        let (schema, table, name, data_type, nullable, ordinal) =
            row.map_err(|err| format!("metadata columns failed: {err}"))?;
        if let Some(object) = objects.get_mut(&(schema, table)) {
            object.columns.push(json!({
                "name": name,
                "dataType": data_type,
                "nullable": nullable.eq_ignore_ascii_case("YES"),
                "ordinal": ordinal
            }));
        }
    }

    let mut schemas: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for object in objects.into_values() {
        schemas
            .entry(object.schema.clone())
            .or_default()
            .push(json!({
                "schema": object.schema,
                "name": object.name,
                "kind": object.kind,
                "columns": object.columns
            }));
    }
    Ok(json!({
        "schemas": schemas
            .into_iter()
            .map(|(name, objects)| json!({ "name": name, "objects": objects }))
            .collect::<Vec<_>>()
    }))
}

fn cell_to_json(row: &duckdb::Row, index: usize) -> Value {
    use duckdb::types::Value as DuckValue;
    match row.get::<usize, DuckValue>(index) {
        Ok(DuckValue::Null) => Value::Null,
        Ok(DuckValue::Boolean(value)) => Value::Bool(value),
        Ok(DuckValue::TinyInt(value)) => json!(value),
        Ok(DuckValue::SmallInt(value)) => json!(value),
        Ok(DuckValue::Int(value)) => json!(value),
        Ok(DuckValue::BigInt(value)) => json!(value),
        Ok(DuckValue::UTinyInt(value)) => json!(value),
        Ok(DuckValue::USmallInt(value)) => json!(value),
        Ok(DuckValue::UInt(value)) => json!(value),
        Ok(DuckValue::UBigInt(value)) => json!(value),
        Ok(DuckValue::Float(value)) => json!(value as f64),
        Ok(DuckValue::Double(value)) => json!(value),
        Ok(DuckValue::Text(value)) => Value::String(value),
        Ok(DuckValue::Blob(value)) => Value::String(format!("\\x{}", hex_encode(&value))),
        Ok(other) => Value::String(format!("{other:?}")),
        Err(_) => Value::Null,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    use crate::{
        irodori_connector_call_json, irodori_connector_free_buffer, IrodoriConnectorBuffer,
    };

    fn buffer_from_str(value: &'static str) -> IrodoriConnectorBuffer {
        IrodoriConnectorBuffer {
            ptr: value.as_ptr(),
            len: value.len(),
        }
    }

    fn buffer_to_json(buffer: IrodoriConnectorBuffer) -> Value {
        let bytes = unsafe { std::slice::from_raw_parts(buffer.ptr, buffer.len) };
        let value = serde_json::from_slice(bytes).unwrap();
        irodori_connector_free_buffer(buffer);
        value
    }

    fn call(request: &'static str) -> Value {
        buffer_to_json(irodori_connector_call_json(buffer_from_str(request)))
    }

    #[test]
    fn connect_query_metadata_and_close_use_real_duckdb_driver() {
        let connected = call(r#"{"method":"connect","connectionId":"test","database":":memory:"}"#);
        assert_eq!(connected["ok"], true);
        assert_eq!(connected["driverLinked"], true);

        assert_eq!(
            call(
                r#"{"method":"query","connectionId":"test","sql":"create table numbers (n integer, label varchar)"}"#
            )["ok"],
            true
        );
        assert_eq!(
            call(
                r#"{"method":"query","connectionId":"test","sql":"insert into numbers values (1, 'one'), (2, 'two')"}"#
            )["ok"],
            true
        );
        let result = call(
            r#"{"method":"query","connectionId":"test","sql":"select n, label from numbers order by n","maxRows":10}"#,
        );
        assert_eq!(result["ok"], true);
        assert_eq!(result["columns"], json!(["n", "label"]));
        assert_eq!(result["rows"], json!([[1, "one"], [2, "two"]]));

        let metadata = call(r#"{"method":"metadata","connectionId":"test"}"#);
        assert_eq!(metadata["ok"], true);
        let schemas = metadata["metadata"]["schemas"].as_array().unwrap();
        assert!(schemas.iter().any(|schema| schema["objects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|object| object["name"] == "numbers")));

        assert_eq!(
            call(r#"{"method":"close","connectionId":"test"}"#)["closed"],
            true
        );
        let missing = call(r#"{"method":"query","connectionId":"test","sql":"select 1"}"#);
        assert_eq!(missing["ok"], false);
        assert_eq!(missing["error"]["code"], "connector.connectionNotFound");
    }

    #[test]
    fn query_reports_driver_errors() {
        let _ = call(r#"{"method":"connect","connectionId":"errors","database":":memory:"}"#);
        let response = call(
            r#"{"method":"query","connectionId":"errors","sql":"select * from missing_table"}"#,
        );
        assert_eq!(response["ok"], false);
        assert_eq!(response["error"]["code"], "connector.queryFailed");
    }

    fn request(options: Value) -> Value {
        json!({ "profile": { "options": options } })
    }

    #[test]
    fn a_profile_with_no_credential_source_emits_no_secret() {
        // DuckDB's own defaults should stand; emitting an empty secret would
        // override them with nothing.
        assert!(secret_statements(&request(json!({}))).is_empty());
        assert!(secret_statements(&request(json!({ "region": "ap-northeast-1" }))).is_empty());
    }

    #[test]
    fn a_static_key_pair_becomes_a_config_secret() {
        let statements = secret_statements(&request(json!({
            "accessKeyId": "AKIAIOSFODNN7EXAMPLE",
            "secretAccessKey": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "sessionToken": "FQoGZXIvYXdzEBYa",
            "region": "ap-northeast-1"
        })));
        assert_eq!(statements.len(), 1);
        let sql = &statements[0];
        assert!(sql.contains("TYPE s3"), "{sql}");
        assert!(sql.contains("PROVIDER config"), "{sql}");
        assert!(sql.contains("KEY_ID 'AKIAIOSFODNN7EXAMPLE'"), "{sql}");
        assert!(sql.contains("SESSION_TOKEN 'FQoGZXIvYXdzEBYa'"), "{sql}");
        assert!(sql.contains("REGION 'ap-northeast-1'"), "{sql}");
    }

    #[test]
    fn a_profile_name_becomes_a_credential_chain_secret() {
        // This is the path that carries SSO, web identity, ECS/IMDS, and a
        // profile's own role_arn — none of which a `SET s3_*` can express.
        let statements = secret_statements(&request(json!({ "awsProfile": "analytics" })));
        assert_eq!(statements.len(), 1);
        let sql = &statements[0];
        assert!(sql.contains("PROVIDER credential_chain"), "{sql}");
        assert!(sql.contains("PROFILE 'analytics'"), "{sql}");
    }

    #[test]
    fn an_explicit_chain_is_passed_through() {
        let statements = secret_statements(&request(json!({
            "awsCredentialChain": "sso;env;instance"
        })));
        assert!(
            statements[0].contains("CHAIN 'sso;env;instance'"),
            "{:?}",
            statements
        );
    }

    #[test]
    fn a_static_key_pair_wins_over_the_chain() {
        // Explicit credentials are an instruction, not a hint.
        let statements = secret_statements(&request(json!({
            "accessKeyId": "AKIAIOSFODNN7EXAMPLE",
            "secretAccessKey": "secret",
            "awsProfile": "analytics"
        })));
        assert!(
            statements[0].contains("PROVIDER config"),
            "{:?}",
            statements
        );
        assert!(
            !statements[0].contains("credential_chain"),
            "{:?}",
            statements
        );
    }

    #[test]
    fn a_user_that_is_not_an_access_key_id_is_not_a_credential() {
        // The form overloads `user` for both a profile name and an access key
        // id; only the access-key shape may become KEY_ID.
        let statements = secret_statements(&request(json!({
            "user": "analytics", "password": "secret"
        })));
        assert!(statements.is_empty(), "{statements:?}");
        assert!(looks_like_access_key_id("ASIAIOSFODNN7EXAMPLE"));
        assert!(!looks_like_access_key_id("analytics"));
    }

    #[test]
    fn an_azure_service_principal_becomes_a_service_principal_secret() {
        let statements = secret_statements(&request(json!({
            "azureTenantId": "tenant",
            "azureClientId": "client",
            "azureClientSecret": "shh",
            "azureAccountName": "lakehouse"
        })));
        let sql = statements.last().expect("an azure secret");
        assert!(sql.contains("TYPE azure"), "{sql}");
        assert!(sql.contains("PROVIDER service_principal"), "{sql}");
        assert!(sql.contains("CLIENT_SECRET 'shh'"), "{sql}");
        assert!(sql.contains("ACCOUNT_NAME 'lakehouse'"), "{sql}");
    }

    #[test]
    fn an_azure_service_principal_can_authenticate_with_a_certificate() {
        let statements = secret_statements(&request(json!({
            "azureTenantId": "tenant",
            "azureClientId": "client",
            "azureClientCertificatePath": "/etc/ssl/sp.pem"
        })));
        let sql = statements.last().expect("an azure secret");
        assert!(
            sql.contains("CLIENT_CERTIFICATE_PATH '/etc/ssl/sp.pem'"),
            "{sql}"
        );
        assert!(!sql.contains("CLIENT_SECRET"), "{sql}");
    }

    #[test]
    fn an_azure_service_principal_without_a_credential_emits_nothing() {
        // Emitting a secret that cannot authenticate would replace whatever
        // DuckDB could otherwise have used, and fail later and less clearly.
        let statements = secret_statements(&request(json!({
            "azureTenantId": "tenant", "azureClientId": "client"
        })));
        assert!(statements.is_empty(), "{statements:?}");
    }

    #[test]
    fn an_azure_sas_token_becomes_a_connection_string_secret() {
        let statements = secret_statements(&request(json!({
            "azureSasToken": "BlobEndpoint=https://x.blob.core.windows.net/;SharedAccessSignature=sv=2022"
        })));
        let sql = statements.last().expect("an azure secret");
        assert!(sql.contains("PROVIDER config"), "{sql}");
        assert!(sql.contains("SharedAccessSignature=sv=2022"), "{sql}");
    }

    #[test]
    fn an_azure_chain_carries_managed_identity_and_cli_login() {
        let statements = secret_statements(&request(json!({
            "azureCredentialChain": "managed_identity;cli"
        })));
        let sql = statements.last().expect("an azure secret");
        assert!(sql.contains("PROVIDER credential_chain"), "{sql}");
        assert!(sql.contains("CHAIN 'managed_identity;cli'"), "{sql}");
    }

    #[test]
    fn s3_and_azure_credentials_coexist() {
        // A table can live in one store and its catalog in another.
        let statements = secret_statements(&request(json!({
            "awsProfile": "analytics",
            "azureCredentialChain": "cli"
        })));
        assert_eq!(statements.len(), 2, "{statements:?}");
        assert!(statements[0].contains("TYPE s3"));
        assert!(statements[1].contains("TYPE azure"));
    }

    #[test]
    fn secret_values_are_quoted_against_injection() {
        let statements = secret_statements(&request(json!({
            "accessKeyId": "AKIAIOSFODNN7EXAMPLE",
            "secretAccessKey": "it's a secret"
        })));
        assert!(
            statements[0].contains("SECRET 'it''s a secret'"),
            "{:?}",
            statements
        );
    }
}
