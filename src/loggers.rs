// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::time::{Duration, SystemTime};

use crate::{
    config::{Config, RegexFilter},
    filtered_log_processor::{FilteredBatchConfig, FilteredBatchLogProcessor},
    syslog_writer, SERVICE_NAME_KEY,
};
use log::Level;
use opentelemetry::{
    logs::{AnyValue, LogRecordBuilder, Logger, Severity},
    KeyValue,
};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    logs::{BatchConfigBuilder, BatchLogProcessor, LoggerProvider},
    runtime, Resource,
};
use regex::Regex;

pub(crate) struct OtelLogBridge<P, L>
where
    P: opentelemetry::logs::LoggerProvider<Logger = L> + Send + Sync,
    L: Logger + Send + Sync,
{
    logger: L,
    std_err_enabled: bool,
    host_name: String,
    service_name: String,
    regex_disallow_filters: Vec<(Regex, Regex)>,
    _phantom: std::marker::PhantomData<P>, // P is not used in this struct
}

impl<P, L> log::Log for OtelLogBridge<P, L>
where
    P: opentelemetry::logs::LoggerProvider<Logger = L> + Send + Sync,
    L: Logger + Send + Sync,
{
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        let _ = metadata;
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let body = record.args().to_string();
        for (module_regex, log_text_regx) in &self.regex_disallow_filters {
            if !module_regex.is_match(record.target()) {
                continue;
            }
            if log_text_regx.is_match(&body) {
                return;
            }
        }

        let timestamp = SystemTime::now();

        if self.std_err_enabled {
            syslog_writer::write_syslog_format(
                record,
                &self.service_name,
                &self.host_name,
                &timestamp,
            );
        }

        // Propagate to otel logger
        // TODO: Also emit user-defined attributes as provided by the kv feature of the log crate.
        self.logger.emit(
            LogRecordBuilder::new()
                .with_severity_number(to_otel_severity(record.level()))
                .with_severity_text(record.level().as_str())
                .with_timestamp(timestamp)
                .with_body(AnyValue::from(body))
                .build(),
        );
    }

    fn flush(&self) {}
}

impl<P, L> OtelLogBridge<P, L>
where
    P: opentelemetry::logs::LoggerProvider<Logger = L> + Send + Sync,
    L: Logger + Send + Sync,
{
    pub(crate) fn new(
        provider: &P,
        service_name: String,
        std_err_enabled: bool,
        host_name: String,
        regex_filters: Option<Vec<RegexFilter>>,
    ) -> Self {
        let mut regex_disallow_filters = Vec::new();

        if let Some(regex_filters) = regex_filters {
            for regex_filter in regex_filters {
                let module_filter = match Regex::new(&regex_filter.module_regex) {
                    Ok(re) => re,
                    Err(e) => {
                        eprintln!("unable to create regex filter for pattern {} due to error: {e:?}, ignoring", regex_filter.module_regex);
                        continue;
                    }
                };

                let log_text_filter = match Regex::new(&regex_filter.log_text_regex) {
                    Ok(re) => match regex_filter.action {
                        crate::config::FilterAction::DISALLOW => re,
                    },
                    Err(e) => {
                        eprintln!("unable to create regex filter for pattern {} due to error: {e:?}, ignoring", regex_filter.log_text_regex);
                        continue;
                    }
                };
                regex_disallow_filters.push((module_filter, log_text_filter));
            }
        }

        OtelLogBridge {
            logger: provider.versioned_logger(service_name.clone(), None, None, None),
            std_err_enabled,
            host_name,
            service_name,
            regex_disallow_filters,
            _phantom: Default::default(),
        }
    }
}

const fn to_otel_severity(level: Level) -> Severity {
    match level {
        Level::Error => Severity::Error,
        Level::Warn => Severity::Warn,
        Level::Info => Severity::Info,
        Level::Debug => Severity::Debug,
        Level::Trace => Severity::Trace,
    }
}

pub(crate) fn init_logs(config: Config) -> Result<LoggerProvider, log::SetLoggerError> {
    let mut keys = vec![KeyValue::new(SERVICE_NAME_KEY, config.service_name.clone())];
    if let Some(resource_attributes) = config.resource_attributes {
        for attribute in resource_attributes {
            keys.push(KeyValue::new(attribute.key, attribute.value));
        }
    }
    let mut logger_provider_builder = LoggerProvider::builder()
        .with_config(opentelemetry_sdk::logs::Config::default().with_resource(Resource::new(keys)));

    let host_name = nix::unistd::gethostname()
        .map(|hostname| {
            hostname
                .into_string()
                .unwrap_or_else(|hostname| hostname.to_string_lossy().into_owned())
        })
        .unwrap_or_default();

    if let Some(export_target_list) = config.log_export_targets {
        for export_target in export_target_list {
            let exporter = match opentelemetry_otlp::new_exporter()
                .tonic()
                .with_endpoint(export_target.url.clone())
                .build_log_exporter()
            {
                Ok(exporter) => exporter,
                Err(e) => {
                    // log error using eprintln as the logger framework is not setup yet!
                    eprintln!(
                        "unable to create exporter for target [{}]: {:?}",
                        export_target.url, e
                    );
                    continue;
                }
            };

            if let Some(export_severity) = export_target.export_severity {
                let filtered_batch_config = FilteredBatchConfig {
                    export_severity,
                    scheduled_delay: Duration::from_secs(export_target.interval_secs),
                    max_export_timeout: Duration::from_secs(export_target.timeout),
                    ..Default::default()
                };

                let filtered_log_processor =
                    FilteredBatchLogProcessor::builder(exporter, runtime::Tokio)
                        .with_batch_config(filtered_batch_config)
                        .build();
                logger_provider_builder =
                    logger_provider_builder.with_log_processor(filtered_log_processor);
            } else {
                let batch_log_processor = BatchLogProcessor::builder(exporter, runtime::Tokio)
                    .with_batch_config(
                        BatchConfigBuilder::default()
                            .with_scheduled_delay(Duration::from_secs(export_target.interval_secs))
                            .with_max_export_timeout(Duration::from_secs(export_target.timeout))
                            .build(),
                    )
                    .build();
                logger_provider_builder =
                    logger_provider_builder.with_log_processor(batch_log_processor);
            }
        }
    }

    let logger_provider = logger_provider_builder.build();

    // Setup Log Bridge to OTEL
    let otel_log_bridge = OtelLogBridge::new(
        &logger_provider,
        config.service_name,
        config.emit_logs_to_stderr,
        host_name,
        config.regex_filters,
    );

    // Setup filtering
    let env_filter = env_filter::Builder::new()
        .parse(config.level.as_str())
        .build();
    let level_filter = env_filter.filter();

    log::set_boxed_logger(Box::new(env_filter::FilteredLog::new(
        otel_log_bridge,
        env_filter,
    )))?;
    log::set_max_level(level_filter);

    Ok(logger_provider)
}
