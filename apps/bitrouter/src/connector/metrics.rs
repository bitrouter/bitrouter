//! Narrow vLLM V1 Prometheus text adapter. No fallback to legacy GPU metrics.
//!
//! Mapping and label identity follow vLLM's production metrics documentation and
//! V1 logger (`model_name`, `engine`):
//! https://docs.vllm.ai/en/stable/usage/metrics/
//! https://github.com/vllm-project/vllm/blob/main/vllm/v1/metrics/loggers.py

use std::collections::{BTreeMap, BTreeSet};

use super::config::MemberConfig;

const METRICS: [&str; 3] = [
    "vllm:num_requests_running",
    "vllm:num_requests_waiting",
    "vllm:kv_cache_usage_perc",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MetricError {
    MissingMetric,
    ScopeMismatch,
    MalformedMetric,
    NonFiniteMetric,
    OutOfRangeMetric,
    StaleSample,
    FutureSample,
    AmbiguousMetric,
}

#[derive(Debug)]
pub(super) struct Reading {
    pub running: u64,
    pub waiting: u64,
    pub kv_fraction_max: f64,
    pub oldest_sample_at_ms: Option<i64>,
    pub all_samples_timestamped: bool,
    pub process_started_at_seconds: Option<f64>,
}

struct Sample {
    labels: BTreeMap<String, String>,
    value: f64,
    timestamp_ms: Option<i64>,
}

/// Every configured engine must expose exactly one of each gauge. Extra engines,
/// models or labels fail closed rather than silently selecting a subset.
pub(super) fn parse(
    text: &str,
    model: &str,
    member: &MemberConfig,
    now_ms: i64,
    max_age_ms: u64,
) -> Result<Reading, MetricError> {
    let expected: BTreeSet<&str> = member.engines.iter().map(String::as_str).collect();
    let mut by_metric: [BTreeMap<String, Sample>; 3] = std::array::from_fn(|_| BTreeMap::new());
    let mut epoch = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(comment) = line.strip_prefix('#') {
            let mut parts = comment.split_whitespace();
            if parts.next() == Some("TYPE")
                && let Some(name) = parts.next()
                && METRICS.contains(&name)
                && (parts.next() != Some("gauge") || parts.next().is_some())
            {
                return Err(MetricError::MalformedMetric);
            }
            continue;
        }
        let name_end = line
            .find(|character: char| character == '{' || character.is_whitespace())
            .map_or(line.len(), |position| position);
        let name = &line[..name_end];
        let metric_index = METRICS.iter().position(|candidate| *candidate == name);
        if metric_index.is_none() && name != "process_start_time_seconds" {
            continue;
        }
        let sample = sample(&line[name_end..])?;
        if !sample.value.is_finite() {
            return Err(MetricError::NonFiniteMetric);
        }
        if let Some(timestamp) = sample.timestamp_ms {
            if timestamp > now_ms {
                return Err(MetricError::FutureSample);
            }
            if timestamp < 0 || now_ms.saturating_sub(timestamp) as u64 > max_age_ms {
                return Err(MetricError::StaleSample);
            }
        }
        if let Some(index) = metric_index {
            if sample.labels.len() != 2
                || sample.labels.get("model_name").map(String::as_str) != Some(model)
            {
                return Err(MetricError::ScopeMismatch);
            }
            let engine = sample
                .labels
                .get("engine")
                .ok_or(MetricError::ScopeMismatch)?
                .clone();
            if !expected.contains(engine.as_str()) {
                return Err(MetricError::ScopeMismatch);
            }
            if sample.value < 0.0
                || (index == 2 && sample.value > 1.0)
                || (index != 2 && (sample.value.fract() != 0.0 || sample.value > u32::MAX as f64))
            {
                return Err(MetricError::OutOfRangeMetric);
            }
            if by_metric[index].insert(engine, sample).is_some() {
                return Err(MetricError::AmbiguousMetric);
            }
        } else {
            if !sample.labels.is_empty() || sample.value <= 0.0 || epoch.is_some() {
                return Err(MetricError::MalformedMetric);
            }
            epoch = Some(sample.value);
        }
    }
    if by_metric
        .iter()
        .any(|values| values.len() != expected.len())
    {
        return Err(MetricError::MissingMetric);
    }
    let samples: Vec<&Sample> = by_metric
        .iter()
        .flat_map(|values| values.values())
        .collect();
    let reading = Reading {
        running: by_metric[0]
            .values()
            .map(|sample| sample.value as u64)
            .sum(),
        waiting: by_metric[1]
            .values()
            .map(|sample| sample.value as u64)
            .sum(),
        kv_fraction_max: by_metric[2]
            .values()
            .map(|sample| sample.value)
            .fold(0.0, f64::max),
        oldest_sample_at_ms: samples
            .iter()
            .filter_map(|sample| sample.timestamp_ms)
            .min(),
        all_samples_timestamped: samples.iter().all(|sample| sample.timestamp_ms.is_some()),
        process_started_at_seconds: epoch,
    };
    Ok(reading)
}

fn sample(input: &str) -> Result<Sample, MetricError> {
    let mut parser = LabelParser { input, position: 0 };
    let mut labels = BTreeMap::new();
    if parser.peek() == Some('{') {
        parser.advance();
        loop {
            parser.spaces();
            if parser.peek() == Some('}') {
                parser.advance();
                break;
            }
            let start = parser.position;
            while parser
                .peek()
                .is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
            {
                parser.advance();
            }
            let name = &input[start..parser.position];
            if name.is_empty() || name.as_bytes()[0].is_ascii_digit() {
                return Err(MetricError::MalformedMetric);
            }
            parser.spaces();
            parser.consume('=')?;
            parser.spaces();
            let value = parser.quoted()?;
            if labels.insert(name.to_string(), value).is_some() {
                return Err(MetricError::AmbiguousMetric);
            }
            parser.spaces();
            match parser.peek() {
                Some(',') => parser.advance(),
                Some('}') => {
                    parser.advance();
                    break;
                }
                _ => return Err(MetricError::MalformedMetric),
            }
        }
    }
    if !parser.peek().is_some_and(char::is_whitespace) {
        return Err(MetricError::MalformedMetric);
    }
    let mut fields = input[parser.position..].split_whitespace();
    let value = fields
        .next()
        .ok_or(MetricError::MalformedMetric)?
        .parse::<f64>()
        .map_err(|_| MetricError::MalformedMetric)?;
    let timestamp_ms = fields
        .next()
        .map(|raw| raw.parse::<i64>().map_err(|_| MetricError::MalformedMetric))
        .transpose()?;
    if fields.next().is_some() {
        return Err(MetricError::MalformedMetric);
    }
    Ok(Sample {
        labels,
        value,
        timestamp_ms,
    })
}

struct LabelParser<'a> {
    input: &'a str,
    position: usize,
}

impl LabelParser<'_> {
    fn peek(&self) -> Option<char> {
        self.input[self.position..].chars().next()
    }

    fn advance(&mut self) {
        if let Some(character) = self.peek() {
            self.position += character.len_utf8();
        }
    }

    fn spaces(&mut self) {
        while self
            .peek()
            .is_some_and(|character| matches!(character, ' ' | '\t'))
        {
            self.advance();
        }
    }

    fn consume(&mut self, expected: char) -> Result<(), MetricError> {
        if self.peek() != Some(expected) {
            return Err(MetricError::MalformedMetric);
        }
        self.advance();
        Ok(())
    }

    fn quoted(&mut self) -> Result<String, MetricError> {
        self.consume('"')?;
        let mut value = String::new();
        loop {
            let character = self.peek().ok_or(MetricError::MalformedMetric)?;
            self.advance();
            match character {
                '"' => return Ok(value),
                '\\' => {
                    let escaped = self.peek().ok_or(MetricError::MalformedMetric)?;
                    self.advance();
                    value.push(match escaped {
                        'n' => '\n',
                        '\\' => '\\',
                        '"' => '"',
                        _ => return Err(MetricError::MalformedMetric),
                    });
                }
                '\n' | '\r' => return Err(MetricError::MalformedMetric),
                _ => value.push(character),
            }
        }
    }
}
