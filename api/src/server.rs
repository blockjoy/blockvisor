//! [`axum`]-specific logic for offering a REST API

use crate::handlers;
use crate::{config::AppConfig, db};
use anyhow::Result;
use axum::{
    body::BoxBody, extract::Extension, http::Request, response::Response, routing::get, Router,
    Server,
};
use hyper::Body;
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::signal::unix::{signal, SignalKind};
use tower_http::trace::TraceLayer;
use tracing::{debug_span, field, info, span, Span};

/// Internal helper for [`tower_http::trace::TraceLayer`] to create
/// [`tracing::Span`]s around a request.
fn make_span(_request: &Request<Body>) -> Span {
    #[cfg(feature = "otel")]
    {
        debug_span!(
            "http-request",
            request_duration = tracing::field::Empty,
            status_code = tracing::field::Empty,
            traceID = tracing::field::Empty,
        )
    }
    #[cfg(not(feature = "otel"))]
    {
        debug_span!(
            "http-request",
            request_duration = tracing::field::Empty,
            status_code = tracing::field::Empty,
        )
    }
}

/// Internal helper for [`tower_http::trace::TraceLayer`] to emit a structured [`tracing::Span`] with specific recorded fields.
///
/// Uses a `Loki`-friendly `traceID` that can correlate to `Tempo` distributed traces.
fn emit_response_trace_with_id(response: &Response<BoxBody>, latency: Duration, span: &Span) {
    #[cfg(feature = "otel")]
    {
        // https://github.com/kube-rs/controller-rs/blob/b99ad0bfbf4ae75f03323bff2796572d4257bd96/src/telemetry.rs#L4-L8
        use opentelemetry::trace::TraceContextExt;
        use tracing_opentelemetry::OpenTelemetrySpanExt;
        let trace_id = span.context().span().span_context().trace_id().to_string();
        span.record("traceID", &field::display(&trace_id));
    }

    span.record("request_duration", &field::display(latency.as_micros()));
    span.record("status_code", &field::display(response.status().as_u16()));

    tracing::debug!("response generated");
}

/// Opens an HTTP server on the indicated address and port from an [`AppConfig`].
///
/// Relies on [`axum::Server`] for the primary behavior. Also launches a
/// [`tokio::signal`]-based task to listen for OS kill signals to allow
/// in-flight requests to finish first, via
/// [`axum::Server::with_graceful_shutdown`]. Currently also comprehensively
/// defines all HTTP routes.
pub async fn start(config: &AppConfig) -> Result<()> {
    let root_span = span!(tracing::Level::TRACE, "api_start");
    let _enter = root_span.enter();

    let pool = db::new_pool(config).await?;

    let app = Router::new()
        .route("/health", get(handlers::health))
        .layer(Extension(pool))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(make_span)
                .on_response(emit_response_trace_with_id),
        );

    let addr = SocketAddr::new(
        IpAddr::V4(config.http.listen_address),
        config.http.listen_port,
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let signal_handler = tokio::spawn(async {
        tokio::pin! {
          let interrupt = signal(SignalKind::interrupt()).expect("could not open SIGINT channel");
          let quit = signal(SignalKind::quit()).expect("could not open SIGQUIT channel");
          let term = signal(SignalKind::terminate()).expect("could not open SIGTERM channel");
        };

        loop {
            tokio::select! {
              _ = (&mut interrupt).recv() => {
                  info!("SIGINT received");
                  break;
              }
              _ = (&mut quit).recv() => {
                  info!("SIGQUIT received");
                  break;
              }
              _ = (&mut term).recv() => {
                  info!("SIGTERM received");
                  break;
              }
            }
        }

        shutdown_tx
            .send(())
            .expect("could not send shutdown signal");
    });

    info!(port = ?addr.port(), address = ?addr.ip(), "Listening on http://{}/", addr);
    info!("Waiting for SIGTERM/SIGQUIT for graceful shutdown");

    Server::bind(&addr)
        .serve(app.into_make_service())
        .with_graceful_shutdown(async {
            shutdown_rx.await.ok();
        })
        .await
        .expect("could not launch HTTP server on port 8080");

    signal_handler
        .await
        .expect("error with shutdown handler task");

    Ok(())
}
