// Copyright 2021 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    net::{SocketAddr, TcpListener},
    sync::Arc,
    time::Duration,
};

use anyhow::Context;
use clap::Parser;
use futures::{future::TryFutureExt, stream::TryStreamExt};
use hyper::{header, Server, Version};
use mas_config::RootConfig;
use mas_email::{MailTransport, Mailer};
use mas_storage::MIGRATOR;
use mas_tasks::TaskQueue;
use mas_templates::Templates;
use opentelemetry::trace::TraceContextExt;
use opentelemetry_http::HeaderExtractor;
use tower::{make::Shared, ServiceBuilder};
use tower_http::{
    compression::CompressionLayer,
    sensitive_headers::SetSensitiveHeadersLayer,
    trace::{MakeSpan, OnResponse, TraceLayer},
};
use tracing::{error, field, info};

use super::RootCommand;

#[derive(Parser, Debug, Default)]
pub(super) struct ServerCommand {
    /// Automatically apply pending migrations
    #[clap(long)]
    migrate: bool,

    /// Watch for changes for templates on the filesystem
    #[clap(short, long)]
    watch: bool,
}

#[derive(Debug, Clone, Default)]
struct OtelMakeSpan;

impl<B> MakeSpan<B> for OtelMakeSpan {
    fn make_span(&mut self, request: &hyper::Request<B>) -> tracing::Span {
        // Extract the context from the headers
        let headers = request.headers();
        let extractor = HeaderExtractor(headers);

        let cx = opentelemetry::global::get_text_map_propagator(|propagator| {
            propagator.extract(&extractor)
        });

        let cx = if cx.span().span_context().is_remote() {
            cx
        } else {
            opentelemetry::Context::new()
        };

        // Attach the context so when the request span is created it gets properly
        // parented
        let _guard = cx.attach();

        let version = match request.version() {
            Version::HTTP_09 => "0.9",
            Version::HTTP_10 => "1.0",
            Version::HTTP_11 => "1.1",
            Version::HTTP_2 => "2.0",
            Version::HTTP_3 => "3.0",
            _ => "",
        };

        let span = tracing::info_span!(
            "request",
            http.method = %request.method(),
            http.target = %request.uri(),
            http.flavor = version,
            http.status_code = field::Empty,
            http.user_agent = field::Empty,
            otel.kind = "server",
            otel.status_code = field::Empty,
        );

        if let Some(user_agent) = headers
            .get(header::USER_AGENT)
            .and_then(|s| s.to_str().ok())
        {
            span.record("http.user_agent", &user_agent);
        }

        span
    }
}

#[derive(Debug, Clone, Default)]
struct OtelOnResponse;

impl<B> OnResponse<B> for OtelOnResponse {
    fn on_response(self, response: &hyper::Response<B>, _latency: Duration, span: &tracing::Span) {
        let s = response.status();
        let status = if s.is_success() {
            "ok"
        } else if s.is_client_error() || s.is_server_error() {
            "error"
        } else {
            "unset"
        };
        span.record("otel.status_code", &status);
        span.record("http.status_code", &s.as_u16());
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    // Wait for the CTRL+C signal
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install Ctrl+C signal handler");

    tracing::info!("Got Ctrl+C, shutting down");
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    // Wait for SIGTERM and SIGINT signals
    // This might panic but should be fine
    let mut term =
        signal(SignalKind::terminate()).expect("failed to install SIGTERM signal handler");
    let mut int = signal(SignalKind::interrupt()).expect("failed to install SIGINT signal handler");

    tokio::select! {
        _ = term.recv() => tracing::info!("Got SIGTERM, shutting down"),
        _ = int.recv() => tracing::info!("Got SIGINT, shutting down"),
    };
}

/// Watch for changes in the templates folders
async fn watch_templates(
    client: &watchman_client::Client,
    templates: &Templates,
) -> anyhow::Result<()> {
    use watchman_client::{
        fields::NameOnly,
        pdu::{QueryResult, SubscribeRequest},
        CanonicalPath, SubscriptionData,
    };

    let templates = templates.clone();

    // Find which roots we're supposed to watch
    let roots = templates.watch_roots().await;
    let mut streams = Vec::new();

    for root in roots {
        // For each root, create a subscription
        let resolved = client
            .resolve_root(CanonicalPath::canonicalize(root)?)
            .await?;

        // TODO: we could subscribe to less, properly filter here
        let (subscription, _) = client
            .subscribe::<NameOnly>(&resolved, SubscribeRequest::default())
            .await?;

        // Create a stream out of that subscription
        let stream = futures::stream::try_unfold(subscription, |mut sub| async move {
            let next = sub.next().await?;
            anyhow::Ok(Some((next, sub)))
        });

        streams.push(Box::pin(stream));
    }

    let files_changed_stream =
        futures::stream::select_all(streams).try_filter_map(|event| async move {
            match event {
                SubscriptionData::FilesChanged(QueryResult {
                    files: Some(files), ..
                }) => {
                    let files: Vec<_> = files.into_iter().map(|f| f.name.into_inner()).collect();
                    Ok(Some(files))
                }
                _ => Ok(None),
            }
        });

    let fut = files_changed_stream
        .try_for_each(move |files| {
            let templates = templates.clone();
            async move {
                info!(?files, "Files changed, reloading templates");

                templates
                    .clone()
                    .reload()
                    .await
                    .context("Could not reload templates")
            }
        })
        .inspect_err(|err| error!(%err, "Error while watching templates, stop watching"));

    tokio::spawn(fut);

    Ok(())
}

impl ServerCommand {
    pub async fn run(&self, root: &RootCommand) -> anyhow::Result<()> {
        let config: RootConfig = root.load_config()?;

        let addr: SocketAddr = config
            .http
            .address
            .parse()
            .context("could not parse listener address")?;
        let listener = TcpListener::bind(addr).context("could not bind address")?;

        // Connect to the mail server
        let mail_transport = MailTransport::from_config(&config.email.transport).await?;
        mail_transport.test_connection().await?;

        // Connect to the database
        let pool = config.database.connect().await?;

        if self.migrate {
            info!("Running pending migrations");
            MIGRATOR
                .run(&pool)
                .await
                .context("could not run migrations")?;
        }

        info!("Starting task scheduler");
        let queue = TaskQueue::default();
        queue.recuring(Duration::from_secs(15), mas_tasks::cleanup_expired(&pool));
        queue.start();

        // Initialize the key store
        let key_store = config
            .oauth2
            .key_store()
            .await
            .context("could not import keys from config")?;
        // Wrap the key store in an Arc
        let key_store = Arc::new(key_store);

        // Load and compile the templates
        let templates = Templates::load_from_config(&config.templates)
            .await
            .context("could not load templates")?;

        let mailer = Mailer::new(
            &templates,
            &mail_transport,
            &config.email.from,
            &config.email.reply_to,
        );

        // Watch for changes in templates if the --watch flag is present
        if self.watch {
            let client = watchman_client::Connector::new()
                .connect()
                .await
                .context("could not connect to watchman")?;

            watch_templates(&client, &templates)
                .await
                .context("could not watch for templates changes")?;
        }

        // Start the server
        let root = mas_handlers::root(&pool, &templates, &key_store, &mailer, &config);

        let warp_service = warp::service(root);

        let service = ServiceBuilder::new()
            // Add high level tracing/logging to all requests
            .layer(
                TraceLayer::new_for_http()
                    .make_span_with(OtelMakeSpan)
                    .on_response(OtelOnResponse),
            )
            // Set a timeout
            .timeout(Duration::from_secs(10))
            // Compress responses
            .layer(CompressionLayer::new())
            // Mark the `Authorization` and `Cookie` headers as sensitive so it doesn't show in logs
            .layer(SetSensitiveHeadersLayer::new(vec![
                header::AUTHORIZATION,
                header::COOKIE,
            ]))
            .service(warp_service);

        info!("Listening on http://{}", listener.local_addr().unwrap());

        Server::from_tcp(listener)?
            .serve(Shared::new(service))
            .with_graceful_shutdown(shutdown_signal())
            .await?;

        Ok(())
    }
}
