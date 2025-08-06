use axum::{
    extract::State,
    http::{ HeaderMap, StatusCode },
    response::IntoResponse,
    routing::post,
    Json,
    Router,
};
use once_cell::sync::OnceCell;
use rustls::{ ClientConfig, RootCertStore };
use rustls::pki_types::CertificateDer;
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use serde::{ Deserialize, Serialize };
use std::{
    collections::HashMap,
    env,
    fs::{ self, File },
    io::BufReader,
    net::SocketAddr,
    sync::Arc,
};
use thiserror::Error;
use tracing::{ error, info };

static SESSION: OnceCell<Arc<Session>> = OnceCell::new();
static TOKEN: OnceCell<String> = OnceCell::new();

#[derive(Debug, Deserialize)]
struct PluginInput {
    #[serde(default)]
    applicationSetName: Option<String>,
    #[serde(default)]
    input: InputWrapper,
}

#[derive(Debug, Deserialize, Default)]
struct InputWrapper {
    #[serde(default)]
    parameters: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct PluginResponse {
    output: Output,
}

#[derive(Debug, Serialize)]
struct Output {
    // One map per tenant
    parameters: Vec<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Error)]
enum AppError {
    #[error("forbidden")]
    Unauthorized,
    #[error("internal: {0}")] Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        match self {
            AppError::Unauthorized => (StatusCode::FORBIDDEN, "forbidden").into_response(),
            AppError::Internal(msg) => {
                error!("internal-error: {}", msg);
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Tracing
    tracing_subscriber
        ::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Load bearer token from a file that we mount via Secret
    let token_path = env
        ::var("PLUGIN_TOKEN_FILE")
        .unwrap_or_else(|_| "/var/run/argo/token".to_string());
    let token = fs
        ::read_to_string(&token_path)
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("failed to read plugin token: {e}"))?;
    TOKEN.set(token).ok();

    // Build and cache DB session
    let session = build_session().await.map_err(|e| format!("session build: {e}"))?;
    SESSION.set(Arc::new(session)).ok();

    // HTTP router
    let app = Router::new().route("/api/v1/getparams.execute", post(handler)).with_state(());

    let port: u16 = env
        ::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4355);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!("listening on {}", addr);

    axum::serve(tokio::net::TcpListener::bind(addr).await?, app).await?;
    Ok(())
}

async fn build_session() -> anyhow::Result<Session> {
    let region = env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".to_string());
    let node = format!("cassandra.{}.amazonaws.com:9142", region);

    // Load Starfield CA (unchanged)
    let cert_path = env
        ::var("KEYSPACES_ROOT_CERT")
        .unwrap_or_else(|_| "/certs/sf-class2-root.crt".to_string());
    let mut store = RootCertStore::empty();
    let mut rd = BufReader::new(File::open(cert_path)?);
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile
        ::certs(&mut rd)
        .collect::<Result<_, _>>()?;
    store.add_parsable_certificates(certs);

    // ⬇️ wrap in Arc
    let tls = Arc::new(ClientConfig::builder().with_root_certificates(store).with_no_client_auth());

    // Service-specific creds
    let user = env
        ::var("KEYSPACES_USERNAME")
        .map_err(|_| anyhow::anyhow!("missing env KEYSPACES_USERNAME"))?;
    let pass = env
        ::var("KEYSPACES_PASSWORD")
        .map_err(|_| anyhow::anyhow!("missing env KEYSPACES_PASSWORD"))?;

    let session = SessionBuilder::new()
        .known_node(node)
        .tls_context(Some(tls)) // now satisfies Into<TlsContext>
        .user(&user, &pass)
        .build().await?;

    Ok(session)
}

async fn handler(
    State(()): State<()>,
    headers: HeaderMap,
    Json(body): Json<PluginInput>
) -> Result<Json<PluginResponse>, AppError> {
    // Bearer check
    let Some(authz) = headers.get(axum::http::header::AUTHORIZATION) else {
        return Err(AppError::Unauthorized);
    };
    let authz = authz.to_str().unwrap_or_default();
    let expected = format!("Bearer {}", TOKEN.get().cloned().unwrap_or_default());
    if authz != expected {
        return Err(AppError::Unauthorized);
    }

    // Optional filters to trim the result set (e.g., by label)
    let filter_label_key = body.input.parameters
        .get("filterLabelKey")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let filter_label_val = body.input.parameters
        .get("filterLabelValue")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Query Keyspaces
    let session = SESSION.get().cloned().expect("session not initialized");

    let query =
        r#"
        SELECT tenant_id, namespace, target_cluster, repo_url, repo_path, labels, params
        FROM tenant_ops.tenant_configs
        WHERE enabled = true ALLOW FILTERING
    "#;

    // consider using Iterator API `query_iter`
    let qr = session
        .query_unpaged(
            r#"
                SELECT tenant_id, namespace, target_cluster, repo_url, repo_path, labels, params
                FROM tenant_ops.tenant_configs
                WHERE enabled = true ALLOW FILTERING
            "#,
            ()
        ).await
        .map_err(|e| AppError::Internal(e.to_string()))?;

    // Turn the QueryResult into a rows parser and iterate typed rows
    let rows_result = qr.into_rows_result().map_err(|e| AppError::Internal(e.to_string()))?;

    let mut out: Vec<HashMap<String, serde_json::Value>> = Vec::new();

    for row in rows_result
        .rows::<
            (
                String, // tenant_id
                String, // namespace
                String, // target_cluster
                String, // repo_url
                String, // repo_path
                Option<HashMap<String, String>>, // labels
                Option<HashMap<String, String>>, // params
            )
        >()
        .map_err(|e| AppError::Internal(e.to_string()))? {
        let (tenant_id, namespace, target_cluster, repo_url, repo_path, labels_opt, params_opt) =
            row.map_err(|e| AppError::Internal(e.to_string()))?;

        // optional label filter (unchanged)
        if
            let (Some(k), Some(v)) = (
                body.input.parameters.get("filterLabelKey").and_then(|x| x.as_str()),
                body.input.parameters.get("filterLabelValue").and_then(|x| x.as_str()),
            )
        {
            let pass = labels_opt
                .as_ref()
                .and_then(|m| m.get(k))
                .map(|val| val == v)
                .unwrap_or(false);
            if !pass {
                continue;
            }
        }

        let mut map = HashMap::new();
        map.insert("tenantId".into(), tenant_id.into());
        map.insert("namespace".into(), namespace.into());
        map.insert("cluster".into(), target_cluster.into());
        map.insert("repoURL".into(), repo_url.into());
        map.insert("path".into(), repo_path.into());

        if let Some(labels) = labels_opt {
            map.insert("labels".into(), serde_json::to_value(labels).unwrap());
        }
        if let Some(params) = params_opt {
            for (k, v) in &params {
                map.insert(k.clone(), serde_json::Value::String(v.clone()));
            }
            map.insert("params".into(), serde_json::to_value(params).unwrap());
        }

        out.push(map);
    }

    Ok(Json(PluginResponse { output: Output { parameters: out } }))
}
