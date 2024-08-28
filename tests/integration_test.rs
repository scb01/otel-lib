// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![deny(rust_2018_idioms)]
#![warn(clippy::all, clippy::pedantic)]

use std::time::Duration;

use log::{debug, error, info, warn};
use mocks::MockServer;
use opentelemetry::{global, logs::Severity, metrics::MeterProvider};
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value::{self, StringValue};
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_sdk::metrics::data::Temporality;
use otel_lib::{
    config::{Attribute, Config, LogsExportTarget, MetricsExportTarget, Prometheus},
    Otel,
};

use tokio::sync::mpsc::Receiver;
use tokio::time::timeout;

mod mocks;

#[tokio::test]
async fn end_to_end_test() {
    // Setup mock otlp server for filtered logs
    let filtered_target = MockServer::new(4317);
    tokio::spawn(async {
        filtered_target.server.run().await;
    });

    // Setup mock otlp server for unfiltered logs
    let unfiltered_target = MockServer::new(4318);
    tokio::spawn(async {
        unfiltered_target.server.run().await;
    });

    // Instrument and test
    let prometheus_config = Some(Prometheus { port: 9090 });

    let metric_targets = vec![MetricsExportTarget {
        url: filtered_target.endpoint.clone(),
        interval_secs: 1,
        timeout: 5,
        temporality: Some(Temporality::Cumulative),
    }];
    let logs_targets = vec![
        LogsExportTarget {
            url: filtered_target.endpoint.clone(),
            interval_secs: 1,
            timeout: 5,
            export_severity: Some(Severity::Error),
        },
        LogsExportTarget {
            url: unfiltered_target.endpoint,
            interval_secs: 1,
            timeout: 5,
            export_severity: None,
        },
    ];

    let sample_attribute = Attribute {
        key: "resource_key1".to_owned(),
        value: "1".to_owned(),
    };

    let config = Config {
        emit_metrics_to_stdout: false,
        metrics_export_targets: Some(metric_targets),
        log_export_targets: Some(logs_targets),
        level: "warn".to_owned(),
        service_name: "end_to_end_test".to_owned(),
        enterprise_number: Some("123".to_owned()),
        resource_attributes: Some(vec![sample_attribute.clone()]),
        prometheus_config,
        ..Config::default()
    };

    let otel_component = Otel::new(config);
    let otel_long_running_task = otel_component.run();
    let run_tests_task = run_tests(
        filtered_target.metrics_rx,
        filtered_target.logs_rx,
        unfiltered_target.logs_rx,
        &sample_attribute,
    );

    tokio::select! {
        () = otel_long_running_task => {
            panic!("Otel component ended unexpectedly");
        },
        () = run_tests_task => {
        }
    }

    // TODO: troubleshoot why calling `otel_component.shutdown()` blocks test execution here.

    filtered_target.shutdown_tx.send(()).await.unwrap();
    unfiltered_target.shutdown_tx.send(()).await.unwrap();
}

async fn run_tests(
    mut filtered_metrics_rx: Receiver<ExportMetricsServiceRequest>,
    mut filtered_logs_rx: Receiver<ExportLogsServiceRequest>,
    mut unfiltered_logs_rx: Receiver<ExportLogsServiceRequest>,
    sample_attribute: &Attribute,
) {
    let meter = global::meter_provider().meter("end_to_end_test");
    let test_counter = meter.u64_counter("test_counter").init();
    test_counter.add(1, &[]);

    let metric_export_request = filtered_metrics_rx.recv().await.unwrap();
    assert_eq!(metric_export_request.resource_metrics.len(), 1);
    let kv = KeyValue {
        key: sample_attribute.key.clone(),
        value: Some(AnyValue {
            value: Some(StringValue(sample_attribute.value.clone())),
        }),
    };
    assert!(
        <Option<opentelemetry_proto::tonic::resource::v1::Resource> as Clone>::clone(
            &metric_export_request
                .resource_metrics
                .first()
                .unwrap()
                .resource
        )
        .unwrap()
        .attributes
        .contains(&kv)
    );

    let error_log = "this is a test error message";
    let info_log = "this is a test info message";
    let debug_log = "this is a test debug message";
    let warn_log = "this is a test warn message";
    warn!("{warn_log}");
    info!("{info_log}");
    debug!("{debug_log}");
    error!("{error_log}");
    let logs_export_request = timeout(Duration::from_secs(2), filtered_logs_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let log_message = &logs_export_request
        .resource_logs
        .first()
        .unwrap()
        .scope_logs
        .first()
        .unwrap()
        .log_records
        .first()
        .unwrap()
        .body;
    let log_message = log_message.clone().unwrap().value.unwrap();
    assert_eq!(log_message, Value::StringValue(error_log.to_owned()));

    let logs_export_request = timeout(Duration::from_secs(2), unfiltered_logs_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let log_message = &logs_export_request
        .resource_logs
        .first()
        .unwrap()
        .scope_logs
        .first()
        .unwrap()
        .log_records
        .first()
        .unwrap()
        .body;
    let log_message = log_message.clone().unwrap().value.unwrap();
    assert_eq!(log_message, Value::StringValue(warn_log.to_owned()));

    let body = reqwest::get("http://127.0.0.1:9090/metrics")
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // TODO: Parse the response using a prometheus parser and assert both format compliance
    // and expected metrics.
    assert!(
        body.contains("test_counter"),
        "did not find expected metric test_counter in server response",
    );
}
