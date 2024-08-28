// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![deny(rust_2018_idioms)]
#![warn(clippy::all, clippy::pedantic)]

use std::time::Duration;

use log::{debug, error, info};
use mocks::MockServer;
use opentelemetry::{global, logs::Severity, metrics::MeterProvider};
use opentelemetry_proto::tonic::common::v1::any_value::Value::{self, StringValue};
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue};
use opentelemetry_sdk::metrics::data::Temporality;
use otel_lib::{
    config::{Attribute, Config, LogsExportTarget, MetricsExportTarget, Prometheus},
    Otel,
};

use tokio::time::timeout;

mod mocks;

#[tokio::test]
async fn end_to_end_test() {
    // Setup mock otlp server for filtered logs
    let mut filtered_target = MockServer::new(4317);
    tokio::spawn(async {
        filtered_target.server.run().await;
    });

    // Setup mock otlp servier for unfiltered logs
    let mut unfiltered_target = MockServer::new(4318);
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
            url: filtered_target.endpoint,
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
        level: "info".to_owned(),
        service_name: "end_to_end_test".to_owned(),
        enterprise_number: Some("123".to_owned()),
        resource_attributes: Some(vec![sample_attribute.clone()]),
        prometheus_config,
        ..Config::default()
    };

    let otel_component = Otel::new(config);
    // Start the otel running task
    tokio::spawn(async move {
        otel_component.run().await;
    });

    let _ = timeout(Duration::from_secs(2), unfiltered_target.logs_rx.recv())
        .await;

    let meter = global::meter_provider().meter("end_to_end_test");
    let test_counter = meter.u64_counter("test_counter").init();
    test_counter.add(1, &[]);

    let metric_export_request = filtered_target.metrics_rx.recv().await.unwrap();
    assert_eq!(metric_export_request.resource_metrics.len(), 1);
    let kv = KeyValue {
        key: sample_attribute.key,
        value: Some(AnyValue {
            value: Some(StringValue(sample_attribute.value)),
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

    let error_log = "this is an error message";
    let info_log = "this is a test info message";
    info!("{info_log}");
    debug!("this is a test debug message");
    error!("{error_log}");
    let logs_export_request = timeout(Duration::from_secs(2), filtered_target.logs_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let log_message =  &logs_export_request.resource_logs.first().unwrap().scope_logs.first().unwrap().log_records.first().unwrap().body;
    let log_message = log_message.clone().unwrap().value.unwrap();
    assert_eq!(log_message, Value::StringValue(error_log.to_owned()));

    let logs_export_request = timeout(Duration::from_secs(2), unfiltered_target.logs_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let log_message =  &logs_export_request.resource_logs.first().unwrap().scope_logs.first().unwrap().log_records.first().unwrap().body;
    let log_message = log_message.clone().unwrap().value.unwrap();
    assert_eq!(log_message, Value::StringValue(info_log.to_owned()));

    // Shut it down
    filtered_target.shutdown_tx.send(()).await.unwrap();
    unfiltered_target.shutdown_tx.send(()).await.unwrap();
}
