use crate::{
    api::{ApiError, ApiResult, HttpError},
    models::VerifySignatureRequest,
};
use async_std::sync::{Arc, RwLock};
use axum::{
    body::Body,
    extract::{multipart::Multipart, Extension, Json, Path},
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
};
use fuel_crypto::{Message, Signature};
use fuel_indexer_database::{
    queries,
    types::{IndexAsset, IndexAssetType},
    IndexerConnectionPool,
};
use fuel_indexer_lib::{
    config::{
        auth::{AuthenticationStrategy, Claims},
        IndexerConfig,
    },
    defaults,
    utils::{
        AssetReloadRequest, FuelNodeHealthResponse, IndexRevertRequest, IndexStopRequest,
        ServiceRequest, ServiceStatus,
    },
};
use fuel_indexer_schema::db::{
    graphql::GraphqlQueryBuilder, manager::SchemaManager, tables::Schema,
};
use hyper::Client;
use hyper_rustls::HttpsConnectorBuilder;
use jsonwebtoken::{encode, EncodingKey, Header};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    str::FromStr,
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc::Sender;
use tracing::error;

#[cfg(feature = "metrics")]
use fuel_indexer_metrics::{encode_metrics_response, METRICS};

#[derive(Clone, Debug, Deserialize)]
pub struct Query {
    pub query: String,
    #[allow(unused)] // TODO
    pub params: String,
}

pub(crate) async fn query_graph(
    Path((namespace, identifier)): Path<(String, String)>,
    Extension(pool): Extension<IndexerConnectionPool>,
    Extension(manager): Extension<Arc<RwLock<SchemaManager>>>,
    Json(query): Json<Query>,
) -> ApiResult<axum::Json<Value>> {
    match manager
        .read()
        .await
        .load_schema(&namespace, &identifier)
        .await
    {
        Ok(schema) => match run_query(query, schema, &pool).await {
            Ok(response) => Ok(axum::Json(response)),
            Err(e) => {
                error!("query_graph error: {e}");
                Err(e)
            }
        },
        Err(_e) => Err(ApiError::Http(HttpError::NotFound(format!(
            "The graph '{namespace}.{identifier}' was not found."
        )))),
    }
}

pub(crate) async fn get_fuel_status(config: &IndexerConfig) -> ServiceStatus {
    #[cfg(feature = "metrics")]
    METRICS.web.health.requests.inc();

    let https = HttpsConnectorBuilder::new()
        .with_native_roots()
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();

    let client = Client::builder().build::<_, hyper::Body>(https);
    match client
        .get(config.to_owned().fuel_node.health_check_uri())
        .await
    {
        Ok(r) => {
            let body_bytes = hyper::body::to_bytes(r.into_body())
                .await
                .unwrap_or_default();

            let fuel_node_health: FuelNodeHealthResponse =
                serde_json::from_slice(&body_bytes).unwrap_or_default();

            ServiceStatus::from(fuel_node_health)
        }
        Err(e) => {
            error!("Failed to fetch fuel /health status: {e}.");
            ServiceStatus::NotOk
        }
    }
}

pub(crate) async fn health_check(
    Extension(config): Extension<IndexerConfig>,
    Extension(pool): Extension<IndexerConnectionPool>,
    Extension(start_time): Extension<Arc<Instant>>,
) -> ApiResult<axum::Json<Value>> {
    let db_status = pool.is_connected().await.unwrap_or(ServiceStatus::NotOk);
    let uptime = start_time.elapsed().as_secs().to_string();
    let fuel_core_status = get_fuel_status(&config).await;

    Ok(Json(json!({
        "fuel_core_status": fuel_core_status,
        "uptime(seconds)": uptime,
        "database_status": db_status,
    })))
}

pub(crate) async fn stop_indexer(
    Path((namespace, identifier)): Path<(String, String)>,
    Extension(tx): Extension<Option<Sender<ServiceRequest>>>,
    Extension(pool): Extension<IndexerConnectionPool>,
    Extension(claims): Extension<Claims>,
) -> ApiResult<axum::Json<Value>> {
    if claims.is_unauthenticated() {
        return Err(ApiError::Http(HttpError::Unauthorized));
    }

    let mut conn = pool.acquire().await?;

    let _ = queries::start_transaction(&mut conn).await?;

    if let Err(e) = queries::remove_indexer(&mut conn, &namespace, &identifier).await {
        queries::revert_transaction(&mut conn).await?;

        error!("Failed to remove Indexer({namespace}.{identifier}): {e}");

        return Err(ApiError::Sqlx(sqlx::Error::RowNotFound));
    } else {
        queries::commit_transaction(&mut conn).await?;

        if let Some(tx) = tx {
            tx.send(ServiceRequest::IndexStop(IndexStopRequest {
                namespace,
                identifier,
            }))
            .await?;

            return Ok(Json(json!({
                "success": "true"
            })));
        }
    }

    Err(ApiError::default())
}

pub(crate) async fn revert_indexer(
    Path((namespace, identifier)): Path<(String, String)>,
    Extension(tx): Extension<Option<Sender<ServiceRequest>>>,
    Extension(pool): Extension<IndexerConnectionPool>,
    Extension(claims): Extension<Claims>,
) -> ApiResult<axum::Json<Value>> {
    if claims.is_unauthenticated() {
        return Err(ApiError::Http(HttpError::Unauthorized));
    }

    let mut conn = pool.acquire().await?;
    let asset = queries::penultimate_asset_for_index(
        &mut conn,
        &namespace,
        &identifier,
        IndexAssetType::Wasm,
    )
    .await?;

    if let Some(tx) = tx {
        tx.send(ServiceRequest::IndexRevert(IndexRevertRequest {
            penultimate_asset_id: asset.id,
            penultimate_asset_bytes: asset.bytes,
            namespace,
            identifier,
        }))
        .await?;

        return Ok(Json(json!({
            "success": "true"
        })));
    }

    Err(ApiError::default())
}

pub(crate) async fn register_indexer_assets(
    Path((namespace, identifier)): Path<(String, String)>,
    Extension(tx): Extension<Option<Sender<ServiceRequest>>>,
    Extension(schema_manager): Extension<Arc<RwLock<SchemaManager>>>,
    Extension(claims): Extension<Claims>,
    Extension(pool): Extension<IndexerConnectionPool>,
    multipart: Option<Multipart>,
) -> ApiResult<axum::Json<Value>> {
    if claims.is_unauthenticated() {
        return Err(ApiError::Http(HttpError::Unauthorized));
    }

    if let Some(mut multipart) = multipart {
        let mut conn = pool.acquire().await?;

        let _ = queries::start_transaction(&mut conn).await?;

        let mut assets: Vec<IndexAsset> = Vec::new();

        while let Some(field) = multipart.next_field().await.unwrap() {
            let name = field.name().unwrap_or("").to_string();
            let data = field.bytes().await.unwrap_or_default();
            let asset_type =
                IndexAssetType::from_str(&name).expect("Invalid asset type.");

            let asset: IndexAsset = match asset_type {
                IndexAssetType::Wasm | IndexAssetType::Manifest => {
                    queries::register_index_asset(
                        &mut conn,
                        &namespace,
                        &identifier,
                        data.to_vec(),
                        asset_type,
                        Some(&claims.sub),
                    )
                    .await?
                }
                IndexAssetType::Schema => {
                    match queries::register_index_asset(
                        &mut conn,
                        &namespace,
                        &identifier,
                        data.to_vec(),
                        IndexAssetType::Schema,
                        Some(&claims.sub),
                    )
                    .await
                    {
                        Ok(result) => {
                            schema_manager
                                .write()
                                .await
                                .new_schema(
                                    &namespace,
                                    &identifier,
                                    &String::from_utf8_lossy(&data),
                                    &mut conn,
                                )
                                .await?;

                            result
                        }
                        Err(e) => {
                            return Err(e.into());
                        }
                    }
                }
            };

            assets.push(asset);
        }

        let _ = queries::commit_transaction(&mut conn).await?;

        if let Some(tx) = tx {
            tx.send(ServiceRequest::AssetReload(AssetReloadRequest {
                namespace,
                identifier,
            }))
            .await?;
        }

        return Ok(Json(json!({
            "success": "true",
            "assets": assets,
        })));
    }

    Err(ApiError::default())
}

pub(crate) async fn get_nonce(
    Extension(pool): Extension<IndexerConnectionPool>,
) -> ApiResult<axum::Json<Value>> {
    let mut conn = pool.acquire().await?;
    let nonce = queries::create_nonce(&mut conn).await?;

    Ok(Json(json!(nonce)))
}

pub(crate) async fn verify_signature(
    Extension(config): Extension<IndexerConfig>,
    Extension(pool): Extension<IndexerConnectionPool>,
    Json(payload): Json<VerifySignatureRequest>,
) -> ApiResult<axum::Json<Value>> {
    if config.authentication.enabled {
        let mut conn = pool.acquire().await?;
        match config.authentication.strategy {
            Some(AuthenticationStrategy::JWT) => {
                let nonce = queries::get_nonce(&mut conn, &payload.message).await?;

                if nonce.is_expired() {
                    return Err(ApiError::Http(HttpError::Unauthorized));
                }

                let mut buff: [u8; 64] = [0u8; 64];
                buff.copy_from_slice(&payload.signature.as_bytes()[..64]);
                let sig = Signature::from_bytes(buff);
                let msg = Message::new(payload.message);
                let pk = sig.recover(&msg)?;

                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as usize;

                let claims = Claims {
                    sub: pk.to_string(),
                    iss: config.authentication.jwt_issuer.unwrap_or_default(),
                    iat: now,
                    exp: now
                        + config
                            .authentication
                            .jwt_expiry
                            .unwrap_or(defaults::JWT_EXPIRY_SECS),
                };

                if let Err(e) = sig.verify(&pk, &msg) {
                    error!("Failed to verify signature: {e}.");
                    return Err(ApiError::FuelCrypto(e));
                }

                let token = encode(
                    &Header::default(),
                    &claims,
                    &EncodingKey::from_secret(
                        config
                            .authentication
                            .jwt_secret
                            .unwrap_or_default()
                            .as_ref(),
                    ),
                )?;

                queries::delete_nonce(&mut conn, &nonce).await?;

                Ok(Json(json!({ "token": token })))
            }
            _ => {
                error!("Unsupported authentication strategy.");
                unimplemented!();
            }
        }
    } else {
        unreachable!()
    }
}

pub async fn run_query(
    query: Query,
    schema: Schema,
    pool: &IndexerConnectionPool,
) -> ApiResult<Value> {
    let builder = GraphqlQueryBuilder::new(&schema, &query.query)?;
    let query = builder.build()?;

    let queries = query.as_sql(&schema, pool.database_type()).join(";\n");

    let mut conn = pool.acquire().await?;

    match queries::run_query(&mut conn, queries).await {
        Ok(ans) => {
            let row: Value = serde_json::from_value(ans)?;
            Ok(row)
        }
        Err(e) => {
            error!("Error querying database: {e}.");
            Err(e.into())
        }
    }
}

pub async fn metrics(_req: Request<Body>) -> impl IntoResponse {
    #[cfg(feature = "metrics")]
    {
        match encode_metrics_response() {
            Ok((buff, fmt_type)) => Response::builder()
                .status(StatusCode::OK)
                .header(http::header::CONTENT_TYPE, &fmt_type)
                .body(Body::from(buff))
                .unwrap(),
            Err(_e) => Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("Error."))
                .unwrap(),
        }
    }
    #[cfg(not(feature = "metrics"))]
    {
        (StatusCode::NOT_FOUND, "Metrics collection disabled.")
    }
}
