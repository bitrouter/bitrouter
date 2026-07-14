//! `bitrouter cloud api` request assembly and streaming output.

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use bitrouter_cloud_sdk::api::{ApiRequest, CloudApiClient};
use futures::StreamExt;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode, Version};
use serde_json::{Map, Number, Value};

/// Arguments accepted by `bitrouter cloud api`.
#[derive(clap::Args)]
struct ParsedApiArgs {
    /// Relative API endpoint, for example `/v1/models`.
    pub endpoint: String,
    /// HTTP method. Defaults to GET, or POST when fields/input are present.
    #[arg(short = 'X', long, value_name = "METHOD")]
    pub method: Option<String>,
    /// Add an HTTP request header. May be repeated.
    #[arg(short = 'H', long = "header", value_name = "KEY:VALUE")]
    pub headers: Vec<String>,
    /// Add a string field to the JSON body or query string. May be repeated.
    #[arg(short = 'f', long = "raw-field", value_name = "KEY=VALUE")]
    pub raw_fields: Vec<String>,
    /// Add a typed field to the JSON body or query string. May be repeated.
    #[arg(short = 'F', long = "field", value_name = "KEY=VALUE")]
    pub fields: Vec<String>,
    /// Read the exact request body from a file, or `-` for stdin.
    #[arg(long, value_name = "FILE")]
    pub input: Option<PathBuf>,
    /// Include the response status line and headers in stdout.
    #[arg(short = 'i', long)]
    pub include: bool,
    /// Suppress the response body.
    #[arg(long, conflicts_with = "verbose")]
    pub silent: bool,
    /// Print redacted request and response details to stderr.
    #[arg(long, conflicts_with = "silent")]
    pub verbose: bool,
}

/// Arguments accepted by `bitrouter cloud api`.
pub struct ApiArgs {
    /// Relative API endpoint, for example `/v1/models`.
    pub endpoint: String,
    /// HTTP method. Defaults to GET, or POST when fields/input are present.
    pub method: Option<String>,
    /// Request headers supplied by `-H/--header`.
    pub headers: Vec<String>,
    /// String fields supplied by `-f/--raw-field`.
    pub raw_fields: Vec<String>,
    /// Typed fields supplied by `-F/--field`.
    pub fields: Vec<String>,
    /// Exact request body file, or `-` for stdin.
    pub input: Option<PathBuf>,
    /// Whether to include response status and headers.
    pub include: bool,
    /// Whether to suppress the response body.
    pub silent: bool,
    /// Whether to print redacted request and response details.
    pub verbose: bool,
    field_order: Vec<FieldKind>,
}

impl clap::FromArgMatches for ApiArgs {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        let field_order = parsed_field_order(matches);
        let parsed = <ParsedApiArgs as clap::FromArgMatches>::from_arg_matches(matches)?;
        Ok(Self::from_parsed(parsed, field_order))
    }

    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        *self = Self::from_arg_matches(matches)?;
        Ok(())
    }
}

impl clap::Args for ApiArgs {
    fn group_id() -> Option<clap::Id> {
        <ParsedApiArgs as clap::Args>::group_id()
    }

    fn augment_args(command: clap::Command) -> clap::Command {
        <ParsedApiArgs as clap::Args>::augment_args(command)
    }

    fn augment_args_for_update(command: clap::Command) -> clap::Command {
        <ParsedApiArgs as clap::Args>::augment_args_for_update(command)
    }
}

impl ApiArgs {
    fn from_parsed(parsed: ParsedApiArgs, field_order: Vec<FieldKind>) -> Self {
        Self {
            endpoint: parsed.endpoint,
            method: parsed.method,
            headers: parsed.headers,
            raw_fields: parsed.raw_fields,
            fields: parsed.fields,
            input: parsed.input,
            include: parsed.include,
            silent: parsed.silent,
            verbose: parsed.verbose,
            field_order,
        }
    }
}

impl std::fmt::Debug for ApiArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let endpoint = redacted_endpoint(&self.endpoint);
        f.debug_struct("ApiArgs")
            .field("endpoint", &endpoint)
            .field("method", &self.method)
            .field("header_count", &self.headers.len())
            .field("raw_field_count", &self.raw_fields.len())
            .field("field_count", &self.fields.len())
            .field("input", &self.input)
            .field("include", &self.include)
            .field("silent", &self.silent)
            .field("verbose", &self.verbose)
            .finish()
    }
}

fn redacted_endpoint(endpoint: &str) -> String {
    match endpoint.find(['?', '#']) {
        Some(index) => format!("{}<redacted>", &endpoint[..=index]),
        None => endpoint.to_owned(),
    }
}

#[derive(Clone, Copy)]
enum FieldKind {
    Raw,
    Typed,
}

fn parsed_field_order(matches: &clap::ArgMatches) -> Vec<FieldKind> {
    let raw = matches
        .indices_of("raw_fields")
        .into_iter()
        .flatten()
        .map(|index| (index, FieldKind::Raw));
    let typed = matches
        .indices_of("fields")
        .into_iter()
        .flatten()
        .map(|index| (index, FieldKind::Typed));
    let mut ordered = raw.chain(typed).collect::<Vec<_>>();
    ordered.sort_by_key(|(index, _)| *index);
    ordered.into_iter().map(|(_, kind)| kind).collect()
}

struct InputField {
    assignment: String,
    kind: FieldKind,
}

impl InputField {
    fn raw(assignment: impl Into<String>) -> Self {
        Self {
            assignment: assignment.into(),
            kind: FieldKind::Raw,
        }
    }

    fn typed(assignment: impl Into<String>) -> Self {
        Self {
            assignment: assignment.into(),
            kind: FieldKind::Typed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum KeyPart {
    Name(String),
    Array,
}

fn select_method(method: Option<&str>, has_fields: bool, has_input: bool) -> Result<Method> {
    if let Some(method) = method {
        return Method::from_bytes(method.to_ascii_uppercase().as_bytes())
            .with_context(|| format!("invalid HTTP method '{method}'"));
    }
    if has_fields || has_input {
        Ok(Method::POST)
    } else {
        Ok(Method::GET)
    }
}

fn build_fields(fields: &[InputField], stdin: &mut dyn Read) -> Result<Value> {
    let mut root = Value::Object(Map::new());
    let mut stdin_used = false;
    for field in fields {
        let (mut parts, raw_value) = parse_assignment(&field.assignment)?;
        if let Some(raw_value) = raw_value {
            let value = match field.kind {
                FieldKind::Raw => Value::String(raw_value.to_owned()),
                FieldKind::Typed => typed_value(raw_value, stdin, &mut stdin_used)?,
            };
            insert_value(&mut root, &parts, value)?;
        } else {
            parts.pop();
            insert_value(&mut root, &parts, Value::Array(Vec::new()))?;
        }
    }
    Ok(root)
}

fn parse_assignment(assignment: &str) -> Result<(Vec<KeyPart>, Option<&str>)> {
    if let Some((key, value)) = assignment.split_once('=') {
        return Ok((parse_key(key)?, Some(value)));
    }
    let parts = parse_key(assignment)?;
    if parts.last() != Some(&KeyPart::Array) {
        anyhow::bail!("field must use KEY=VALUE syntax or KEY[] for an empty array");
    }
    Ok((parts, None))
}

fn parse_key(key: &str) -> Result<Vec<KeyPart>> {
    let first_bracket = key.find('[').unwrap_or(key.len());
    let name = &key[..first_bracket];
    if name.is_empty() || name.contains(']') {
        anyhow::bail!("field key must start with a non-empty name");
    }
    let mut parts = vec![KeyPart::Name(name.to_owned())];
    let mut position = first_bracket;
    while position < key.len() {
        if !key[position..].starts_with('[') {
            anyhow::bail!("invalid nested field key '{key}'");
        }
        let content_start = position + 1;
        let remaining = &key[content_start..];
        let close_offset = remaining
            .find(']')
            .with_context(|| format!("unclosed bracket in field key '{key}'"))?;
        let content_end = content_start + close_offset;
        let content = &key[content_start..content_end];
        if content.contains('[') {
            anyhow::bail!("invalid nested field key '{key}'");
        }
        if content.is_empty() {
            parts.push(KeyPart::Array);
        } else {
            parts.push(KeyPart::Name(content.to_owned()));
        }
        position = content_end + 1;
    }
    Ok(parts)
}

fn typed_value(raw: &str, stdin: &mut dyn Read, stdin_used: &mut bool) -> Result<Value> {
    if raw == "@-" {
        if *stdin_used {
            anyhow::bail!("stdin can only be consumed by one field or --input");
        }
        *stdin_used = true;
        let mut value = String::new();
        stdin
            .read_to_string(&mut value)
            .context("reading field value from stdin")?;
        return Ok(Value::String(value));
    }
    if let Some(path) = raw.strip_prefix('@') {
        if path.is_empty() {
            anyhow::bail!("typed field file reference cannot be empty");
        }
        let value = std::fs::read_to_string(path)
            .with_context(|| format!("reading typed field from {path}"))?;
        return Ok(Value::String(value));
    }
    match raw {
        "true" => Ok(Value::Bool(true)),
        "false" => Ok(Value::Bool(false)),
        "null" => Ok(Value::Null),
        _ => {
            if let Ok(value) = raw.parse::<i64>() {
                return Ok(Value::Number(Number::from(value)));
            }
            if let Ok(value) = raw.parse::<u64>() {
                return Ok(Value::Number(Number::from(value)));
            }
            Ok(Value::String(raw.to_owned()))
        }
    }
}

fn insert_value(current: &mut Value, parts: &[KeyPart], value: Value) -> Result<()> {
    let (part, remaining) = parts.split_first().context("field key cannot be empty")?;
    match part {
        KeyPart::Name(name) => {
            if current.is_null() {
                *current = Value::Object(Map::new());
            }
            let object = current
                .as_object_mut()
                .with_context(|| format!("field '{name}' conflicts with an existing value"))?;
            if remaining.is_empty() {
                if object
                    .get(name)
                    .is_some_and(|existing| existing.is_array() || existing.is_object())
                {
                    anyhow::bail!("field '{name}' conflicts with an existing nested value");
                }
                object.insert(name.clone(), value);
                return Ok(());
            }
            let entry = object.entry(name.clone()).or_insert(Value::Null);
            insert_value(entry, remaining, value)
        }
        KeyPart::Array => {
            if current.is_null() {
                *current = Value::Array(Vec::new());
            }
            let array = current
                .as_array_mut()
                .context("array field conflicts with an existing value")?;
            if remaining.is_empty() {
                array.push(value);
                return Ok(());
            }
            if !array
                .last()
                .is_some_and(|candidate| can_accept(candidate, remaining))
            {
                array.push(container_for(&remaining[0]));
            }
            let target = array
                .last_mut()
                .context("array field could not allocate a nested value")?;
            insert_value(target, remaining, value)
        }
    }
}

fn can_accept(current: &Value, parts: &[KeyPart]) -> bool {
    let Some((part, remaining)) = parts.split_first() else {
        return false;
    };
    if current.is_null() {
        return true;
    }
    match part {
        KeyPart::Name(name) => {
            let Some(object) = current.as_object() else {
                return false;
            };
            match object.get(name) {
                None => true,
                Some(_) if remaining.is_empty() => false,
                Some(value) => can_accept(value, remaining),
            }
        }
        KeyPart::Array => current.is_array(),
    }
}

fn container_for(part: &KeyPart) -> Value {
    match part {
        KeyPart::Name(_) => Value::Object(Map::new()),
        KeyPart::Array => Value::Array(Vec::new()),
    }
}

struct PreparedRequest {
    method: Method,
    endpoint: String,
    headers: HeaderMap,
    body: Option<Vec<u8>>,
}

/// Execute `bitrouter cloud api` using the process standard streams.
pub async fn run(args: ApiArgs) -> Result<()> {
    let client = CloudApiClient::from_default_credentials()?;
    let stdout_is_terminal = std::io::stdout().is_terminal();
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    run_with_io(
        args,
        client,
        &mut stdin,
        &mut stdout,
        &mut stderr,
        stdout_is_terminal,
    )
    .await
}

/// Execute an API request with injectable streams for deterministic tests.
pub async fn run_with_io(
    args: ApiArgs,
    client: CloudApiClient,
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    stdout_is_terminal: bool,
) -> Result<()> {
    let prepared = prepare_request(&args, &client, stdin)?;
    if args.verbose {
        write_verbose_request(stderr, &client, &prepared)?;
    }
    let mut request = ApiRequest::new(prepared.method.clone(), prepared.endpoint.clone())
        .with_headers(prepared.headers.clone());
    if let Some(body) = prepared.body {
        request = request.with_body(body);
    }
    let response = client.execute(request).await?;
    let status = response.status();
    let version = response.version();
    let headers = response.headers().clone();
    if args.verbose {
        write_verbose_response(stderr, version, status, &headers)?;
    }
    if args.include {
        write_response_head(stdout, version, status, &headers)?;
    }
    let is_json = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            let media_type = value.split(';').next().unwrap_or_default().trim();
            media_type == "application/json" || media_type.ends_with("+json")
        });
    let mut body = response.into_response().bytes_stream();
    let buffer_json = stdout_is_terminal && is_json && !args.silent;
    let mut buffered = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.context("reading BitRouter Cloud response body")?;
        if args.silent {
            continue;
        }
        if buffer_json {
            buffered.extend_from_slice(&chunk);
        } else {
            stdout
                .write_all(&chunk)
                .context("writing BitRouter Cloud response body")?;
            stdout
                .flush()
                .context("flushing BitRouter Cloud response chunk")?;
        }
    }
    if buffer_json {
        match serde_json::from_slice::<Value>(&buffered) {
            Ok(value) => {
                serde_json::to_writer_pretty(&mut *stdout, &value)
                    .context("formatting JSON response")?;
                stdout.write_all(b"\n").context("writing JSON response")?;
            }
            Err(_) => stdout
                .write_all(&buffered)
                .context("writing BitRouter Cloud response body")?,
        }
    }
    stdout
        .flush()
        .context("flushing BitRouter Cloud response")?;
    if status.is_client_error() || status.is_server_error() {
        anyhow::bail!("BitRouter Cloud API request failed with status {status}");
    }
    Ok(())
}

fn prepare_request(
    args: &ApiArgs,
    client: &CloudApiClient,
    stdin: &mut dyn Read,
) -> Result<PreparedRequest> {
    let fields = ordered_fields(args);
    let method = select_method(
        args.method.as_deref(),
        !fields.is_empty(),
        args.input.is_some(),
    )?;
    let mut headers = parse_headers(&args.headers)?;
    let input_uses_stdin = args.input.as_deref() == Some(std::path::Path::new("-"));
    if input_uses_stdin
        && args.fields.iter().any(|field| {
            field
                .split_once('=')
                .is_some_and(|(_, value)| value == "@-")
        })
    {
        anyhow::bail!("stdin cannot be used by both --input and a typed field");
    }
    let mut body = match args.input.as_deref() {
        Some(path) => Some(read_input(path, stdin)?),
        None => None,
    };
    let fields_are_query = method == Method::GET || args.input.is_some();
    let endpoint = if fields_are_query && !fields.is_empty() {
        endpoint_with_query(client, &args.endpoint, &fields, stdin)?
    } else {
        args.endpoint.clone()
    };
    if !fields_are_query && !fields.is_empty() {
        let value = build_fields(&fields, stdin)?;
        body = Some(serde_json::to_vec(&value).context("serializing API fields")?);
        if !headers.contains_key(CONTENT_TYPE) {
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        }
    }
    Ok(PreparedRequest {
        method,
        endpoint,
        headers,
        body,
    })
}

fn ordered_fields(args: &ApiArgs) -> Vec<InputField> {
    if args.field_order.len() != args.raw_fields.len() + args.fields.len() {
        return args
            .raw_fields
            .iter()
            .cloned()
            .map(InputField::raw)
            .chain(args.fields.iter().cloned().map(InputField::typed))
            .collect();
    }

    let mut raw = args.raw_fields.iter();
    let mut typed = args.fields.iter();
    args.field_order
        .iter()
        .filter_map(|kind| match kind {
            FieldKind::Raw => raw.next().cloned().map(InputField::raw),
            FieldKind::Typed => typed.next().cloned().map(InputField::typed),
        })
        .collect()
}

fn parse_headers(values: &[String]) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for header in values {
        let (name, value) = header
            .split_once(':')
            .context("header must use KEY:VALUE syntax")?;
        let name = HeaderName::from_bytes(name.trim().as_bytes())
            .with_context(|| format!("invalid HTTP header name '{name}'"))?;
        let value = HeaderValue::from_str(value.trim())
            .with_context(|| format!("invalid value for HTTP header '{name}'"))?;
        headers.append(name, value);
    }
    Ok(headers)
}

fn read_input(path: &std::path::Path, stdin: &mut dyn Read) -> Result<Vec<u8>> {
    if path == std::path::Path::new("-") {
        let mut body = Vec::new();
        stdin
            .read_to_end(&mut body)
            .context("reading API request body from stdin")?;
        return Ok(body);
    }
    std::fs::read(path).with_context(|| format!("reading API request body from {}", path.display()))
}

fn endpoint_with_query(
    client: &CloudApiClient,
    endpoint: &str,
    fields: &[InputField],
    stdin: &mut dyn Read,
) -> Result<String> {
    let mut url = client.endpoint_url(endpoint)?;
    let mut stdin_used = false;
    {
        let mut query = url.query_pairs_mut();
        for field in fields {
            let (parts, raw_value) = parse_assignment(&field.assignment)?;
            let key = field
                .assignment
                .split_once('=')
                .map_or(field.assignment.as_str(), |(key, _)| key);
            let value = match (field.kind, raw_value) {
                (_, None) => String::new(),
                (FieldKind::Raw, Some(raw_value)) => raw_value.to_owned(),
                (FieldKind::Typed, Some(raw_value)) => {
                    query_value(typed_value(raw_value, stdin, &mut stdin_used)?)
                }
            };
            debug_assert!(!parts.is_empty());
            query.append_pair(key, &value);
        }
    }
    let mut relative = url.path().to_owned();
    if let Some(query) = url.query() {
        relative.push('?');
        relative.push_str(query);
    }
    Ok(relative)
}

fn query_value(value: Value) -> String {
    match value {
        Value::String(value) => value,
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

fn write_response_head(
    writer: &mut dyn Write,
    version: Version,
    status: StatusCode,
    headers: &HeaderMap,
) -> Result<()> {
    write!(
        writer,
        "{} {} {}\r\n",
        version_name(version),
        status.as_u16(),
        status.canonical_reason().unwrap_or_default()
    )
    .context("writing response status")?;
    for (name, value) in sorted_headers(headers) {
        writer
            .write_all(name.as_str().as_bytes())
            .context("writing response header")?;
        writer.write_all(b": ").context("writing response header")?;
        writer
            .write_all(value.as_bytes())
            .context("writing response header")?;
        writer
            .write_all(b"\r\n")
            .context("writing response header")?;
    }
    writer
        .write_all(b"\r\n")
        .context("writing response headers")?;
    Ok(())
}

fn write_verbose_request(
    writer: &mut dyn Write,
    client: &CloudApiClient,
    request: &PreparedRequest,
) -> Result<()> {
    let url = redacted_url(client.endpoint_url(&request.endpoint)?);
    writeln!(writer, "> {} {url}", request.method).context("writing verbose request")?;
    if !request.headers.contains_key(reqwest::header::AUTHORIZATION) {
        writeln!(writer, "> authorization: <redacted>").context("writing verbose request")?;
    }
    for (name, value) in sorted_headers(&request.headers) {
        write!(writer, "> {}: ", name.as_str()).context("writing verbose request")?;
        write_redacted_header_value(writer, name, value)?;
        writeln!(writer).context("writing verbose request")?;
    }
    Ok(())
}

fn redacted_url(mut url: reqwest::Url) -> reqwest::Url {
    let query = url
        .query_pairs()
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    if !query.iter().any(|(name, _)| sensitive_name(name)) {
        return url;
    }
    url.set_query(None);
    {
        let mut output = url.query_pairs_mut();
        for (name, value) in query {
            output.append_pair(
                &name,
                if sensitive_name(&name) {
                    "<redacted>"
                } else {
                    &value
                },
            );
        }
    }
    url
}

fn write_verbose_response(
    writer: &mut dyn Write,
    version: Version,
    status: StatusCode,
    headers: &HeaderMap,
) -> Result<()> {
    writeln!(
        writer,
        "< {} {} {}",
        version_name(version),
        status.as_u16(),
        status.canonical_reason().unwrap_or_default()
    )
    .context("writing verbose response")?;
    for (name, value) in sorted_headers(headers) {
        write!(writer, "< {}: ", name.as_str()).context("writing verbose response")?;
        write_redacted_header_value(writer, name, value)?;
        writeln!(writer).context("writing verbose response")?;
    }
    Ok(())
}

fn sorted_headers(headers: &HeaderMap) -> Vec<(&HeaderName, &HeaderValue)> {
    let mut values = headers.iter().collect::<Vec<_>>();
    values.sort_by(|(left_name, left_value), (right_name, right_value)| {
        left_name
            .as_str()
            .cmp(right_name.as_str())
            .then_with(|| left_value.as_bytes().cmp(right_value.as_bytes()))
    });
    values
}

fn write_redacted_header_value(
    writer: &mut dyn Write,
    name: &HeaderName,
    value: &HeaderValue,
) -> Result<()> {
    if sensitive_header(name) {
        writer
            .write_all(b"<redacted>")
            .context("writing redacted header")?;
    } else {
        writer
            .write_all(value.as_bytes())
            .context("writing header value")?;
    }
    Ok(())
}

fn sensitive_header(name: &HeaderName) -> bool {
    sensitive_name(name.as_str())
}

fn sensitive_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.contains("authorization")
        || name.contains("cookie")
        || name.contains("token")
        || name.contains("secret")
        || name.contains("api-key")
        || name.contains("api_key")
        || name.contains("password")
        || name.contains("signature")
        || name == "key"
}

fn version_name(version: Version) -> &'static str {
    match version {
        Version::HTTP_09 => "HTTP/0.9",
        Version::HTTP_10 => "HTTP/1.0",
        Version::HTTP_11 => "HTTP/1.1",
        Version::HTTP_2 => "HTTP/2",
        Version::HTTP_3 => "HTTP/3",
        _ => "HTTP/?",
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;

    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::routing::get;
    use bitrouter_cloud_sdk::api::CloudApiClient;
    use bitrouter_cloud_sdk::auth::credentials::{CredentialsStore, StoredCredential};
    use clap::Parser;
    use serde_json::json;
    use wiremock::matchers::{body_json, body_string, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    #[derive(Debug, Parser)]
    struct ApiHarness {
        #[command(flatten)]
        args: ApiArgs,
    }

    fn tmp_path(label: &str, filename: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-cloud-cli-api-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(filename)
    }

    fn api_args(endpoint: &str) -> ApiArgs {
        ApiArgs {
            endpoint: endpoint.to_owned(),
            method: None,
            headers: Vec::new(),
            raw_fields: Vec::new(),
            fields: Vec::new(),
            input: None,
            include: false,
            silent: false,
            verbose: false,
            field_order: Vec::new(),
        }
    }

    fn api_client(base_url: &str, label: &str) -> CloudApiClient {
        let path = tmp_path(label, "account-credentials.json");
        let mut store = CredentialsStore::load(&path).unwrap();
        store
            .save(StoredCredential::api_key(
                "brk_AAAAAAAAAAAAAAAA.stored-secret".to_owned(),
                base_url.to_owned(),
            ))
            .unwrap();
        CloudApiClient::from_credentials_path(path).unwrap()
    }

    async fn execute_for_test(
        args: ApiArgs,
        client: CloudApiClient,
        stdin_bytes: &[u8],
        stdout_is_terminal: bool,
    ) -> (Result<()>, Vec<u8>, Vec<u8>) {
        let mut stdin = std::io::Cursor::new(stdin_bytes);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let result = run_with_io(
            args,
            client,
            &mut stdin,
            &mut stdout,
            &mut stderr,
            stdout_is_terminal,
        )
        .await;
        (result, stdout, stderr)
    }

    #[derive(Clone)]
    struct SharedWriter(Arc<StdMutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let mut output = self
                .0
                .lock()
                .map_err(|_| std::io::Error::other("shared writer lock poisoned"))?;
            output.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    async fn delayed_sse_server() -> (
        String,
        Arc<tokio::sync::Notify>,
        tokio::task::JoinHandle<()>,
    ) {
        let release = Arc::new(tokio::sync::Notify::new());
        let handler_release = release.clone();
        let app = Router::new().route(
            "/v1/stream",
            get(move || {
                let release = handler_release.clone();
                async move {
                    let stream = futures::stream::unfold(0_u8, move |state| {
                        let release = release.clone();
                        async move {
                            match state {
                                0 => Some((
                                    Ok::<_, Infallible>(Bytes::from_static(b"data: first\n\n")),
                                    1,
                                )),
                                1 => {
                                    release.notified().await;
                                    Some((
                                        Ok::<_, Infallible>(Bytes::from_static(
                                            b"data: second\n\n",
                                        )),
                                        2,
                                    ))
                                }
                                _ => None,
                            }
                        }
                    });
                    (
                        [(reqwest::header::CONTENT_TYPE, "text/event-stream")],
                        Body::from_stream(stream),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}"), release, server)
    }

    #[test]
    fn parses_gh_api_style_flags() {
        let parsed = ApiHarness::try_parse_from([
            "test",
            "/v1/chat/completions",
            "-X",
            "post",
            "-H",
            "X-Test: one",
            "-H",
            "X-Test: two",
            "-f",
            "model=openai/gpt-5",
            "-F",
            "stream=true",
            "--include",
            "--verbose",
        ])
        .unwrap();

        assert_eq!(parsed.args.endpoint, "/v1/chat/completions");
        assert_eq!(parsed.args.method.as_deref(), Some("post"));
        assert_eq!(parsed.args.headers, ["X-Test: one", "X-Test: two"]);
        assert_eq!(parsed.args.raw_fields, ["model=openai/gpt-5"]);
        assert_eq!(parsed.args.fields, ["stream=true"]);
        assert!(parsed.args.include);
        assert!(parsed.args.verbose);
    }

    #[test]
    fn silent_conflicts_with_verbose() {
        let error = ApiHarness::try_parse_from(["test", "/v1/models", "--silent", "--verbose"])
            .unwrap_err();

        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn fields_or_input_default_to_post() {
        assert_eq!(
            select_method(None, false, false).unwrap(),
            reqwest::Method::GET
        );
        assert_eq!(
            select_method(None, true, false).unwrap(),
            reqwest::Method::POST
        );
        assert_eq!(
            select_method(None, false, true).unwrap(),
            reqwest::Method::POST
        );
        assert_eq!(
            select_method(Some("patch"), false, false).unwrap(),
            reqwest::Method::PATCH
        );
        assert!(select_method(Some("not a method"), false, false).is_err());
    }

    #[test]
    fn typed_and_raw_fields_build_nested_json() {
        let fields = vec![
            InputField::typed("model=openai/gpt-5"),
            InputField::typed("stream=true"),
            InputField::raw("messages[][role]=user"),
            InputField::raw("messages[][content]=Hello"),
            InputField::typed("max_tokens=256"),
            InputField::typed("metadata[nullable]=null"),
        ];

        let value = build_fields(&fields, &mut std::io::empty()).unwrap();

        assert_eq!(
            value,
            json!({
                "model": "openai/gpt-5",
                "stream": true,
                "messages": [{"role": "user", "content": "Hello"}],
                "max_tokens": 256,
                "metadata": {"nullable": null}
            })
        );
    }

    #[test]
    fn repeated_array_properties_start_a_new_object() {
        let fields = vec![
            InputField::raw("messages[][role]=user"),
            InputField::raw("messages[][content]=Hello"),
            InputField::raw("messages[][role]=assistant"),
            InputField::raw("messages[][content]=Hi"),
        ];

        let value = build_fields(&fields, &mut std::io::empty()).unwrap();

        assert_eq!(
            value,
            json!({
                "messages": [
                    {"role": "user", "content": "Hello"},
                    {"role": "assistant", "content": "Hi"}
                ]
            })
        );
    }

    #[test]
    fn empty_array_field_builds_an_empty_array() {
        let fields = vec![InputField::raw("stop[]")];

        let value = build_fields(&fields, &mut std::io::empty()).unwrap();

        assert_eq!(value, json!({"stop": []}));
    }

    #[test]
    fn parsed_raw_and_typed_fields_keep_command_line_order() {
        let parsed = ApiHarness::try_parse_from([
            "test",
            "/v1/messages",
            "-F",
            "messages[][priority]=1",
            "-f",
            "messages[][content]=first",
            "-F",
            "messages[][priority]=2",
            "-f",
            "messages[][content]=second",
        ])
        .unwrap();
        let client = api_client("https://api.bitrouter.ai", "field-order");

        let request = prepare_request(&parsed.args, &client, &mut std::io::empty()).unwrap();
        let body = serde_json::from_slice::<Value>(request.body.as_deref().unwrap()).unwrap();

        assert_eq!(
            body,
            json!({
                "messages": [
                    {"priority": 1, "content": "first"},
                    {"priority": 2, "content": "second"}
                ]
            })
        );
    }

    #[test]
    fn conflicting_nested_shapes_are_rejected() {
        let fields = vec![InputField::raw("a=value"), InputField::raw("a[b]=value")];
        assert!(build_fields(&fields, &mut std::io::empty()).is_err());

        let reverse = vec![InputField::raw("a[b]=value"), InputField::raw("a=value")];
        assert!(build_fields(&reverse, &mut std::io::empty()).is_err());
    }

    #[test]
    fn typed_field_reads_stdin_once() {
        let fields = vec![InputField::typed("input=@-")];
        let mut stdin = std::io::Cursor::new("from stdin");
        assert_eq!(
            build_fields(&fields, &mut stdin).unwrap(),
            json!({"input": "from stdin"})
        );

        let repeated = vec![
            InputField::typed("first=@-"),
            InputField::typed("second=@-"),
        ];
        assert!(build_fields(&repeated, &mut std::io::Cursor::new("once")).is_err());
    }

    #[tokio::test]
    async fn fields_build_an_implicit_post_json_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("content-type", "application/json"))
            .and(body_json(json!({
                "model": "openai/gpt-5",
                "stream": true
            })))
            .respond_with(ResponseTemplate::new(200).set_body_raw("ok", "text/plain"))
            .expect(1)
            .mount(&server)
            .await;
        let mut args = api_args("/v1/chat/completions");
        args.raw_fields = vec!["model=openai/gpt-5".to_owned()];
        args.fields = vec!["stream=true".to_owned()];

        let (result, stdout, stderr) =
            execute_for_test(args, api_client(&server.uri(), "post"), b"", false).await;

        result.unwrap();
        assert_eq!(stdout, b"ok");
        assert!(stderr.is_empty());
    }

    #[tokio::test]
    async fn explicit_get_sends_fields_as_query_parameters() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(query_param("existing", "yes"))
            .and(query_param("owned", "true"))
            .and(query_param("limit", "25"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let mut args = api_args("/v1/models?existing=yes");
        args.method = Some("GET".to_owned());
        args.fields = vec!["owned=true".to_owned(), "limit=25".to_owned()];

        let (result, _, _) =
            execute_for_test(args, api_client(&server.uri(), "get-query"), b"", false).await;

        result.unwrap();
    }

    #[tokio::test]
    async fn input_preserves_exact_body_and_moves_fields_to_query() {
        let server = MockServer::start().await;
        let input = tmp_path("input", "request.json");
        std::fs::write(&input, b"{\n  \"exact\": true\n}\n").unwrap();
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .and(query_param("trace", "true"))
            .and(body_string("{\n  \"exact\": true\n}\n"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let mut args = api_args("/v1/responses");
        args.input = Some(input);
        args.fields = vec!["trace=true".to_owned()];

        let (result, _, _) =
            execute_for_test(args, api_client(&server.uri(), "input-body"), b"", false).await;

        result.unwrap();
    }

    #[tokio::test]
    async fn stdin_cannot_be_both_input_and_a_typed_field() {
        let server = MockServer::start().await;
        let mut args = api_args("/v1/responses");
        args.input = Some(PathBuf::from("-"));
        args.fields = vec!["extra=@-".to_owned()];

        let (result, _, _) = execute_for_test(
            args,
            api_client(&server.uri(), "stdin-conflict"),
            b"body",
            false,
        )
        .await;

        assert!(result.unwrap_err().to_string().contains("stdin"));
        assert!(
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn non_tty_sse_bytes_are_preserved() {
        let server = MockServer::start().await;
        let body = "event: message\ndata: {\"delta\":\"hi\"}\n\ndata: [DONE]\n\n";
        Mock::given(method("GET"))
            .and(path("/v1/stream"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
            .mount(&server)
            .await;

        let (result, stdout, _) = execute_for_test(
            api_args("/v1/stream"),
            api_client(&server.uri(), "sse"),
            b"",
            false,
        )
        .await;

        result.unwrap();
        assert_eq!(stdout, body.as_bytes());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn non_tty_sse_is_written_before_the_stream_completes() {
        let (base_url, release, server) = delayed_sse_server().await;
        let output = Arc::new(StdMutex::new(Vec::new()));
        let shared_output = output.clone();
        let client = api_client(&base_url, "delayed-sse");
        let local = tokio::task::LocalSet::new();

        local
            .run_until(async move {
                let task_output = shared_output.clone();
                let request = tokio::task::spawn_local(async move {
                    let mut stdin = std::io::empty();
                    let mut stdout = SharedWriter(task_output);
                    let mut stderr = Vec::new();
                    run_with_io(
                        api_args("/v1/stream"),
                        client,
                        &mut stdin,
                        &mut stdout,
                        &mut stderr,
                        false,
                    )
                    .await
                });
                let first_chunk_arrived = tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        let has_first = shared_output
                            .lock()
                            .map(|bytes| bytes.as_slice() == b"data: first\n\n")
                            .unwrap_or(false);
                        if has_first {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .is_ok();
                release.notify_one();
                request.await.unwrap().unwrap();
                assert!(first_chunk_arrived, "first SSE chunk was buffered");
            })
            .await;

        assert_eq!(
            output.lock().unwrap().as_slice(),
            b"data: first\n\ndata: second\n\n"
        );
        server.abort();
    }

    #[tokio::test]
    async fn tty_json_is_pretty_printed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("{\"data\":[{\"id\":\"m\"}]}", "application/json"),
            )
            .mount(&server)
            .await;

        let (result, stdout, _) = execute_for_test(
            api_args("/v1/models"),
            api_client(&server.uri(), "tty-json"),
            b"",
            true,
        )
        .await;

        result.unwrap();
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            "{\n  \"data\": [\n    {\n      \"id\": \"m\"\n    }\n  ]\n}\n"
        );
    }

    #[tokio::test]
    async fn include_silent_and_verbose_obey_output_contracts() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer override-secret"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-api-key", "response-secret")
                    .set_body_raw("hidden", "text/plain"),
            )
            .mount(&server)
            .await;
        let mut args = api_args("/v1/models");
        args.headers = vec!["Authorization: Bearer override-secret".to_owned()];
        args.include = true;
        args.silent = true;

        let (result, stdout, stderr) = execute_for_test(
            args,
            api_client(&server.uri(), "include-silent"),
            b"",
            false,
        )
        .await;

        result.unwrap();
        let output = String::from_utf8(stdout).unwrap();
        assert!(output.starts_with("HTTP/1.1 200 OK\r\n"), "{output:?}");
        assert!(!output.contains("hidden"));
        assert!(stderr.is_empty());

        let mut verbose_args = api_args("/v1/models?api_key=query-secret&visible=yes");
        verbose_args.headers = vec!["Authorization: Bearer override-secret".to_owned()];
        verbose_args.verbose = true;
        let (result, _, stderr) = execute_for_test(
            verbose_args,
            api_client(&server.uri(), "verbose"),
            b"",
            false,
        )
        .await;
        result.unwrap();
        let verbose = String::from_utf8(stderr).unwrap();
        assert!(verbose.contains("GET"));
        assert!(verbose.contains("<redacted>"));
        assert!(!verbose.contains("override-secret"));
        assert!(!verbose.contains("response-secret"));
        assert!(!verbose.contains("stored-secret"));
        assert!(!verbose.contains("query-secret"));
        assert!(verbose.contains("visible=yes"));
    }

    #[tokio::test]
    async fn error_body_is_written_before_status_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(429)
                    .set_body_raw("{\"error\":\"rate_limited\"}", "application/json"),
            )
            .mount(&server)
            .await;

        let (result, stdout, _) = execute_for_test(
            api_args("/v1/models"),
            api_client(&server.uri(), "http-error"),
            b"",
            false,
        )
        .await;

        assert_eq!(stdout, b"{\"error\":\"rate_limited\"}");
        assert!(result.unwrap_err().to_string().contains("429"));
    }
}
