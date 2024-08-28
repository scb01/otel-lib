// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![warn(clippy::all, clippy::pedantic)]

use std::net::SocketAddr;

use opentelemetry_proto::tonic::collector::{
    logs::v1::{
        logs_service_server::{LogsService, LogsServiceServer},
        ExportLogsServiceRequest, ExportLogsServiceResponse,
    },
    metrics::v1::{
        metrics_service_server::{MetricsService, MetricsServiceServer},
        ExportMetricsServiceRequest, ExportMetricsServiceResponse,
    },
};

use tokio::sync::mpsc::{self, Receiver, Sender};
use tonic::{async_trait, transport::Server, Request, Response, Status};

pub struct MockServer {
    pub socketaddr: String,
    pub endpoint: String,
    pub shutdown_tx: Sender<()>,
    pub metrics_rx: Receiver<ExportMetricsServiceRequest>,
    pub logs_rx: Receiver<ExportLogsServiceRequest>,
    pub server: OtlpServer,
}

impl MockServer {
    #[must_use]
    /// Create a new mock server
    ///
    /// # Panics
    ///
    /// Will panic if socketaddr parse fails
    pub fn new(port: u16) -> Self {
        // Setup mock otlp server
        let socketaddr = format!("127.0.0.1:{port}");
        let endpoint = format!("http://localhost:{port}");

        let (metrics_tx, metrics_rx) = mpsc::channel(10);
        let (logs_tx, logs_rx) = mpsc::channel(10);
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        let server = OtlpServer::new(
            socketaddr.parse().unwrap(),
            shutdown_rx,
            metrics_tx,
            logs_tx,
        );

        Self {
            socketaddr,
            endpoint,
            shutdown_tx,
            metrics_rx,
            logs_rx,
            server,
        }
    }
}

pub struct OtlpServer {
    endpoint: SocketAddr,
    shutdown_rx: Receiver<()>,
    echo_metric_tx: Sender<ExportMetricsServiceRequest>,
    echo_logs_tx: Sender<ExportLogsServiceRequest>,
}

impl OtlpServer {
    fn new(
        endpoint: SocketAddr,
        shutdown_rx: Receiver<()>,
        echo_metric_tx: Sender<ExportMetricsServiceRequest>,
        echo_logs_tx: Sender<ExportLogsServiceRequest>,
    ) -> Self {
        Self {
            endpoint,
            shutdown_rx,
            echo_metric_tx,
            echo_logs_tx,
        }
    }

    /// Run the server
    ///
    /// # Panics
    /// Will panic if the port is already in use
    ///
    pub async fn run(self) {
        let () = Server::builder()
            .add_service(MetricsServiceServer::new(MockMetricsService::new(
                self.echo_metric_tx,
            )))
            .add_service(LogsServiceServer::new(MockLogsService::new(
                self.echo_logs_tx,
            )))
            .serve_with_shutdown(self.endpoint, recv_wrapper(self.shutdown_rx))
            .await
            .unwrap();
    }
}

async fn recv_wrapper(mut receiver: Receiver<()>) {
    let _ = receiver.recv().await;
}

struct MockLogsService {
    echo_sender: Sender<ExportLogsServiceRequest>,
}

#[async_trait]
impl LogsService for MockLogsService {
    async fn export(
        &self,
        request: Request<ExportLogsServiceRequest>,
    ) -> Result<Response<ExportLogsServiceResponse>, Status> {
        // Echo received request over channel
        self.echo_sender.send(request.into_inner()).await.unwrap();
        let response = ExportLogsServiceResponse {
            partial_success: None,
        };
        Ok(Response::new(response))
    }
}

impl MockLogsService {
    fn new(echo_sender: Sender<ExportLogsServiceRequest>) -> Self {
        Self { echo_sender }
    }
}

#[derive(Debug)]
struct MockMetricsService {
    echo_sender: Sender<ExportMetricsServiceRequest>,
}

#[async_trait]
impl MetricsService for MockMetricsService {
    async fn export(
        &self,
        request: Request<ExportMetricsServiceRequest>,
    ) -> Result<Response<ExportMetricsServiceResponse>, Status> {
        // Echo received request over channel
        self.echo_sender.send(request.into_inner()).await.unwrap();
        let response = ExportMetricsServiceResponse {
            partial_success: None,
        };
        Ok(Response::new(response))
    }
}

impl MockMetricsService {
    fn new(echo_sender: Sender<ExportMetricsServiceRequest>) -> Self {
        Self { echo_sender }
    }
}
