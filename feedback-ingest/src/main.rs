use anyhow::Context;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::env;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

// ---------------------------------------------------------------------------
// Request / Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct FeedbackRequest {
    category: String,
    description: Option<String>,
    #[serde(default)]
    include_logs: bool,
    session_id: Option<String>,
    session_events: Option<serde_json::Value>,
    app_version: Option<String>,
    os: Option<String>,
    model: Option<String>,
    install_id: Option<String>,
}

#[derive(Serialize)]
struct HealthResponse {
    ok: bool,
}

// ---------------------------------------------------------------------------
// Allowed category values
// ---------------------------------------------------------------------------

const ALLOWED_CATEGORIES: &[&str] = &[
    "bug",
    "bad_result",
    "good_result",
    "safety_check",
    "other",
];

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { ok: true })
}

async fn post_feedback(
    State(pool): State<PgPool>,
    Json(body): Json<FeedbackRequest>,
) -> impl IntoResponse {
    if !ALLOWED_CATEGORIES.contains(&body.category.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!(
                    "category must be one of: {}",
                    ALLOWED_CATEGORIES.join(", ")
                )
            })),
        );
    }

    // Only store session_events when include_logs is true
    let session_events = if body.include_logs {
        body.session_events
    } else {
        None
    };

    let result: Result<uuid::Uuid, sqlx::Error> = sqlx::query_scalar(
        r#"
        INSERT INTO feedback (
            category, description, include_logs,
            session_id, session_events,
            app_version, os, model, install_id
        ) VALUES (
            $1, $2, $3,
            $4, $5,
            $6, $7, $8, $9
        )
        RETURNING id
        "#,
    )
    .bind(&body.category)
    .bind(&body.description)
    .bind(body.include_logs)
    .bind(&body.session_id)
    .bind(&session_events)
    .bind(&body.app_version)
    .bind(&body.os)
    .bind(&body.model)
    .bind(&body.install_id)
    .fetch_one(&pool)
    .await;

    match result {
        Ok(id) => (
            StatusCode::OK,
            Json(serde_json::json!({ "id": id })),
        ),
        Err(e) => {
            // Log the detail server-side; return a generic message so we don't
            // leak DB/internal implementation details to clients.
            tracing::error!("DB insert failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "internal server error" })),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Migration
// ---------------------------------------------------------------------------

async fn run_migrations(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::raw_sql(
        r#"
create extension if not exists "pgcrypto";
create table if not exists feedback (
  id uuid primary key default gen_random_uuid(),
  created_at timestamptz not null default now(),
  category text not null,
  description text,
  include_logs boolean not null default false,
  session_id text,
  session_events jsonb,
  app_version text,
  os text,
  model text,
  install_id text
);
alter table feedback add column if not exists id uuid default gen_random_uuid();
alter table feedback add column if not exists created_at timestamptz not null default now();
alter table feedback add column if not exists category text;
alter table feedback add column if not exists description text;
alter table feedback add column if not exists include_logs boolean not null default false;
alter table feedback add column if not exists session_id text;
alter table feedback add column if not exists session_events jsonb;
alter table feedback add column if not exists app_version text;
alter table feedback add column if not exists os text;
alter table feedback add column if not exists model text;
alter table feedback add column if not exists install_id text;
        "#,
    )
    .execute(pool)
    .await
    .context("running migrations")?;

    info!("migrations complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let database_url = env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
    let port: u16 = env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse()
        .context("PORT must be a valid u16")?;

    info!("connecting to postgres");
    let pool = PgPool::connect(&database_url)
        .await
        .context("connecting to postgres")?;

    run_migrations(&pool).await?;

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([axum::http::Method::GET, axum::http::Method::POST])
        .allow_headers(Any);

    // Cap request bodies to bound abuse and the size of an uploaded session.
    const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

    let app = Router::new()
        .route("/health", get(health))
        .route("/feedback", post(post_feedback))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(cors)
        .with_state(pool);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    info!("listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
