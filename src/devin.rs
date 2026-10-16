use crate::host;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use rand::{rngs::OsRng, RngCore};
use reqwest::{
    blocking::Client,
    header::{HeaderMap, HeaderName, HeaderValue},
    redirect::Policy,
    Proxy,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::Read,
    sync::{OnceLock, RwLock},
    time::Duration,
};

const DEFAULT_BASE_URL: &str = "https://server.codeium.com";
const MODEL_CONFIGS_PATH: &str = "/exa.api_server_pb.ApiServerService/GetCliModelConfigs";
const DEFAULT_CLIENT_NAME: &str = "chisel";
const DEFAULT_CLIENT_VERSION: &str = "3000.10.21";
const FINGERPRINT_HEX_LEN: usize = 732;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_COMPLETION_TOKENS: i64 = 64_000;

#[derive(Clone, Debug, Serialize)]
struct ModelInfo {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "Object")]
    object: String,
    #[serde(rename = "OwnedBy")]
    owned_by: String,
    #[serde(rename = "Type")]
    model_type: String,
    #[serde(rename = "DisplayName")]
    display_name: String,
    #[serde(rename = "InputTokenLimit")]
    input_token_limit: i64,
    #[serde(rename = "OutputTokenLimit")]
    output_token_limit: i64,
    #[serde(rename = "SupportedGenerationMethods")]
    supported_generation_methods: Vec<String>,
    #[serde(rename = "ContextLength")]
    context_length: i64,
    #[serde(rename = "MaxCompletionTokens")]
    max_completion_tokens: i64,
    #[serde(rename = "SupportedInputModalities")]
    supported_input_modalities: Vec<String>,
    #[serde(rename = "SupportedOutputModalities")]
    supported_output_modalities: Vec<String>,
    #[serde(rename = "Thinking", skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingSupport>,
}

#[derive(Clone, Debug, Serialize)]
struct ThinkingSupport {
    #[serde(rename = "Levels")]
    levels: Vec<String>,
}

#[derive(Clone, Debug)]
struct UpstreamModel {
    uid: String,
    label: String,
    context_length: i64,
    multimodal: bool,
    vendor_id: u64,
}

#[derive(Clone, Debug)]
struct RouteVariant {
    target_model: String,
    effort: String,
    bare: bool,
}

#[derive(Clone, Debug, Default)]
struct RouteCatalog {
    by_base: HashMap<String, Vec<RouteVariant>>,
    exact: HashMap<String, String>,
}

struct DiscoveredModels {
    models: Vec<ModelInfo>,
    routes: RouteCatalog,
}

#[derive(Default)]
struct Credentials {
    api_key: String,
    base_url: String,
    device_seed: String,
    proxy_url: String,
    headers: BTreeMap<String, Vec<String>>,
}

struct Catalog {
    models: Vec<ModelInfo>,
    by_id: HashMap<String, ModelInfo>,
}

#[derive(Deserialize)]
struct CatalogPayload {
    devin: Vec<CatalogModel>,
}

#[derive(Deserialize)]
struct CatalogModel {
    id: String,
    #[serde(default)]
    object: String,
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    owned_by: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    context_length: i64,
    #[serde(default)]
    max_completion_tokens: i64,
    #[serde(default, rename = "supportedInputModalities")]
    supported_input_modalities: Vec<String>,
    #[serde(default, rename = "supportedOutputModalities")]
    supported_output_modalities: Vec<String>,
    #[serde(default)]
    thinking: Option<CatalogThinking>,
}

#[derive(Deserialize)]
struct CatalogThinking {
    #[serde(default)]
    levels: Vec<String>,
}

static CATALOG: OnceLock<Catalog> = OnceLock::new();
static ROUTES: OnceLock<RwLock<HashMap<String, RouteCatalog>>> = OnceLock::new();

pub fn models_for_auth(request: &Value) -> Value {
    let auth_id = string_field(request, &["AuthID", "auth_id"]).unwrap_or_default();
    let callback_id =
        string_field(request, &["host_callback_id", "HostCallbackID"]).unwrap_or_default();
    let models = match fetch_models(request, &callback_id) {
        Ok(discovered) if !discovered.models.is_empty() => {
            replace_auth_routes(&auth_id, discovered.routes);
            discovered.models
        }
        Ok(_) => {
            let error = "上游返回空模型目录";
            clear_auth_routes(&auth_id);
            host::warn(&callback_id, &auth_id, error);
            fallback_models()
        }
        Err(error) => {
            clear_auth_routes(&auth_id);
            host::warn(&callback_id, &auth_id, &error);
            fallback_models()
        }
    };
    json!({"Provider": "devin", "Models": models})
}

pub fn route_model(request: &Value) -> Value {
    let requested_model =
        string_field(request, &["RequestedModel", "requested_model"]).unwrap_or_default();
    if !has_devin_prefix(&requested_model) {
        return json!({"Handled": false});
    }
    let requested_effort = request_effort(request);
    let Some(target_model) = resolve_route(&requested_model, &requested_effort) else {
        return json!({"Handled": false});
    };
    json!({
        "Handled": true,
        "TargetKind": "provider",
        "Target": "devin",
        "TargetModel": target_model,
        "Reason": "cpa-devin resolved the discovered model to an upstream chat_model_uid"
    })
}

pub fn intercept_response(request: &Value) -> Value {
    if string_field(request, &["Model", "model"]).is_some_and(|value| !value.is_empty())
        || string_field(request, &["RequestedModel", "requested_model"])
            .is_some_and(|value| !value.is_empty())
    {
        return json!({});
    }
    let hidden_models = hidden_route_models();
    if hidden_models.is_empty() {
        return json!({});
    }
    let Some(body) = bytes_field(request, &["Body", "body"]) else {
        return json!({});
    };
    let Some(filtered) = filter_model_catalog_response(&body, &hidden_models) else {
        return json!({});
    };
    json!({
        "Body": STANDARD.encode(filtered),
        "ClearHeaders": ["Content-Length"]
    })
}

pub fn clear_routes() {
    if let Some(routes) = ROUTES.get() {
        if let Ok(mut routes) = routes.write() {
            routes.clear();
        }
    }
}

fn fetch_models(request: &Value, callback_id: &str) -> Result<DiscoveredModels, String> {
    let credentials = resolve_credentials(request);
    if credentials.api_key.is_empty() {
        return Err("缺少 api_key、session_token 或 token".to_string());
    }
    let url = format!(
        "{}{}",
        credentials.base_url.trim_end_matches('/'),
        MODEL_CONFIGS_PATH
    );
    let body = build_model_configs_request(&credentials.api_key, &credentials.device_seed);
    let response = if credentials.proxy_url.is_empty() {
        host_http_post(callback_id, &url, &credentials.headers, &body)?
    } else {
        direct_http_post(&url, &credentials.headers, &body, &credentials.proxy_url)?
    };
    if response.0 != 200 {
        return Err(format!("上游返回 HTTP {}", response.0));
    }
    let raw_models = parse_model_configs(&response.1)?;
    if raw_models.is_empty() {
        return Err("上游返回空模型目录".to_string());
    }
    Ok(DiscoveredModels {
        models: build_models_from_upstream(&raw_models),
        routes: build_route_catalog(&raw_models),
    })
}

fn resolve_credentials(request: &Value) -> Credentials {
    let attributes = object_field(request, &["Attributes", "attributes"])
        .cloned()
        .unwrap_or_default();
    let metadata = object_field(request, &["Metadata", "metadata"])
        .cloned()
        .unwrap_or_default();
    let storage = bytes_field(request, &["StorageJSON", "storage_json"])
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or_else(|| json!({}));
    let attribute_value = Value::Object(attributes.clone());
    let metadata_value = Value::Object(metadata);
    let sources = [&attribute_value, &metadata_value, &storage];

    let api_key =
        lookup_sources(&sources, &["api_key", "session_token", "token"]).unwrap_or_default();
    let base_url = lookup_sources(&sources, &["base_url"])
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let device_seed = lookup_sources(&sources, &["device_seed"]).unwrap_or_default();
    let proxy_url =
        lookup_sources(&sources, &["proxy_url", "proxy-url", "proxy"]).unwrap_or_default();

    let mut headers = BTreeMap::from([
        (
            "Authorization".to_string(),
            vec![format!("Basic {api_key}-{api_key}")],
        ),
        (
            "Content-Type".to_string(),
            vec!["application/proto".to_string()],
        ),
        (
            "Connect-Protocol-Version".to_string(),
            vec!["1".to_string()],
        ),
        ("Accept".to_string(), vec!["*/*".to_string()]),
        ("User-Agent".to_string(), vec![String::new()]),
    ]);
    for (key, value) in attributes {
        let Some(name) = key.strip_prefix("header:") else {
            continue;
        };
        let name = name.trim();
        let Some(value) = value.as_str().map(str::trim) else {
            continue;
        };
        if name.is_empty() || value.is_empty() || value.starts_with('$') {
            continue;
        }
        headers.insert(name.to_string(), vec![value.to_string()]);
    }

    Credentials {
        api_key,
        base_url,
        device_seed,
        proxy_url: proxy_url.trim().to_string(),
        headers,
    }
}

fn host_http_post(
    callback_id: &str,
    url: &str,
    headers: &BTreeMap<String, Vec<String>>,
    body: &[u8],
) -> Result<(u16, Vec<u8>), String> {
    let result = host::call(
        "host.http.do",
        &json!({
            "host_callback_id": callback_id,
            "method": "POST",
            "url": url,
            "headers": headers,
            "body": STANDARD.encode(body)
        }),
    )?;
    let status = number_field(&result, &["StatusCode", "status_code"])
        .ok_or_else(|| "宿主 HTTP 响应缺少状态码".to_string())?;
    let body = bytes_field(&result, &["Body", "body"]).unwrap_or_default();
    if body.len() > MAX_RESPONSE_BYTES {
        return Err("上游模型目录响应超过 8 MiB".to_string());
    }
    Ok((status as u16, body))
}

fn direct_http_post(
    url: &str,
    headers: &BTreeMap<String, Vec<String>>,
    body: &[u8],
    proxy_url: &str,
) -> Result<(u16, Vec<u8>), String> {
    let mut builder = Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(Policy::none());
    if proxy_url.eq_ignore_ascii_case("direct") || proxy_url.eq_ignore_ascii_case("none") {
        builder = builder.no_proxy();
    } else {
        let proxy =
            Proxy::all(proxy_url).map_err(|error| format!("凭证 proxy_url 无效：{error}"))?;
        builder = builder.proxy(proxy);
    }
    let client = builder
        .build()
        .map_err(|error| format!("创建凭证代理客户端失败：{error}"))?;
    let mut request_headers = HeaderMap::new();
    for (name, values) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| format!("自定义请求头名称无效：{error}"))?;
        for value in values {
            let value = HeaderValue::from_str(value)
                .map_err(|error| format!("自定义请求头值无效：{error}"))?;
            request_headers.append(name.clone(), value);
        }
    }
    let response = client
        .post(url)
        .headers(request_headers)
        .body(body.to_vec())
        .send()
        .map_err(|error| format!("读取上游模型目录失败：{error}"))?;
    let status = response.status().as_u16();
    let mut limited = response.take((MAX_RESPONSE_BYTES + 1) as u64);
    let mut response_body = Vec::new();
    limited
        .read_to_end(&mut response_body)
        .map_err(|error| format!("读取上游模型目录响应失败：{error}"))?;
    if response_body.len() > MAX_RESPONSE_BYTES {
        return Err("上游模型目录响应超过 8 MiB".to_string());
    }
    Ok((status, response_body))
}

fn build_model_configs_request(api_key: &str, device_seed: &str) -> Vec<u8> {
    let metadata = build_client_metadata(api_key, device_seed);
    let mut body = Vec::new();
    append_key(&mut body, 1, 2);
    append_bytes(&mut body, &metadata);
    body
}

fn build_client_metadata(session_token: &str, device_seed: &str) -> Vec<u8> {
    let fingerprint = device_fingerprint(device_seed);
    let mut body = Vec::new();
    append_string_field(&mut body, 1, DEFAULT_CLIENT_NAME);
    append_string_field(&mut body, 2, DEFAULT_CLIENT_VERSION);
    append_string_field(&mut body, 3, session_token);
    append_string_field(&mut body, 4, "en");
    append_string_field(&mut body, 5, std::env::consts::OS);
    append_string_field(&mut body, 7, DEFAULT_CLIENT_VERSION);
    append_string_field(&mut body, 12, DEFAULT_CLIENT_NAME);
    append_string_field(&mut body, 31, &fingerprint);
    body
}

fn device_fingerprint(seed: &str) -> String {
    if seed.is_empty() {
        let mut bytes = [0u8; FINGERPRINT_HEX_LEN / 2];
        OsRng.fill_bytes(&mut bytes);
        return hex::encode(bytes);
    }
    let mut output = String::with_capacity(FINGERPRINT_HEX_LEN);
    let mut counter = 0;
    while output.len() < FINGERPRINT_HEX_LEN {
        let digest = Sha256::digest(format!("{seed}-{counter}").as_bytes());
        output.push_str(&hex::encode(digest));
        counter += 1;
    }
    output.truncate(FINGERPRINT_HEX_LEN);
    output
}

fn append_string_field(output: &mut Vec<u8>, field_number: u64, value: &str) {
    append_key(output, field_number, 2);
    append_bytes(output, value.as_bytes());
}

fn append_key(output: &mut Vec<u8>, field_number: u64, wire_type: u64) {
    append_varint(output, (field_number << 3) | wire_type);
}

fn append_bytes(output: &mut Vec<u8>, value: &[u8]) {
    append_varint(output, value.len() as u64);
    output.extend_from_slice(value);
}

fn append_varint(output: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        output.push((value as u8) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn parse_model_configs(input: &[u8]) -> Result<Vec<UpstreamModel>, String> {
    let mut position = 0;
    let mut models = Vec::new();
    while position < input.len() {
        let tag = read_varint(input, &mut position)?;
        let field_number = tag >> 3;
        let wire_type = tag & 7;
        if field_number == 1 && wire_type == 2 {
            let submessage = read_bytes(input, &mut position)?;
            let model = parse_model_config(submessage)?;
            if !model.uid.is_empty() {
                models.push(model);
            }
        } else {
            skip_value(input, &mut position, wire_type)?;
        }
    }
    Ok(models)
}

fn parse_model_config(input: &[u8]) -> Result<UpstreamModel, String> {
    let mut position = 0;
    let mut model = UpstreamModel {
        uid: String::new(),
        label: String::new(),
        context_length: 0,
        multimodal: false,
        vendor_id: 0,
    };
    while position < input.len() {
        let tag = read_varint(input, &mut position)?;
        let field_number = tag >> 3;
        let wire_type = tag & 7;
        match (field_number, wire_type) {
            (1, 2) => {
                model.label =
                    String::from_utf8_lossy(read_bytes(input, &mut position)?).into_owned()
            }
            (5, 0) => model.multimodal = read_varint(input, &mut position)? == 1,
            (10, 0) => model.vendor_id = read_varint(input, &mut position)?,
            (18, 0) => {
                model.context_length =
                    read_varint(input, &mut position)?.min(i64::MAX as u64) as i64
            }
            (22, 2) => {
                model.uid = String::from_utf8_lossy(read_bytes(input, &mut position)?).into_owned()
            }
            _ => skip_value(input, &mut position, wire_type)?,
        }
    }
    Ok(model)
}

fn read_varint(input: &[u8], position: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let byte = *input
            .get(*position)
            .ok_or_else(|| "protobuf varint 被截断".to_string())?;
        *position += 1;
        if shift == 63 && byte > 1 {
            return Err("protobuf varint 溢出".to_string());
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("protobuf varint 过长".to_string())
}

fn read_bytes<'a>(input: &'a [u8], position: &mut usize) -> Result<&'a [u8], String> {
    let len = read_varint(input, position)? as usize;
    let end = position
        .checked_add(len)
        .ok_or_else(|| "protobuf 长度溢出".to_string())?;
    let value = input
        .get(*position..end)
        .ok_or_else(|| "protobuf 字段被截断".to_string())?;
    *position = end;
    Ok(value)
}

fn skip_value(input: &[u8], position: &mut usize, wire_type: u64) -> Result<(), String> {
    match wire_type {
        0 => {
            read_varint(input, position)?;
        }
        1 => advance(input, position, 8)?,
        2 => {
            let len = read_varint(input, position)? as usize;
            advance(input, position, len)?;
        }
        3 => loop {
            let tag = read_varint(input, position)?;
            let nested_wire = tag & 7;
            if nested_wire == 4 {
                break;
            }
            skip_value(input, position, nested_wire)?;
        },
        4 => return Err("protobuf 出现意外的 end-group".to_string()),
        5 => advance(input, position, 4)?,
        _ => return Err(format!("不支持的 protobuf wire type：{wire_type}")),
    }
    Ok(())
}

fn advance(input: &[u8], position: &mut usize, count: usize) -> Result<(), String> {
    let end = position
        .checked_add(count)
        .ok_or_else(|| "protobuf 长度溢出".to_string())?;
    if end > input.len() {
        return Err("protobuf 字段被截断".to_string());
    }
    *position = end;
    Ok(())
}

fn build_models_from_upstream(raw: &[UpstreamModel]) -> Vec<ModelInfo> {
    let mut variants = Vec::new();
    let mut seen = HashSet::new();
    for raw_model in raw {
        let uid = raw_model.uid.trim().to_lowercase();
        if uid.is_empty() || !seen.insert(uid.clone()) {
            continue;
        }
        let mut input_modalities = vec!["text".to_string()];
        if raw_model.multimodal {
            input_modalities.push("image".to_string());
        }
        variants.push(ModelInfo {
            id: uid.clone(),
            object: "model".to_string(),
            owned_by: vendor_name(raw_model.vendor_id, &uid).to_string(),
            model_type: "devin".to_string(),
            display_name: raw_model.label.trim().to_string(),
            input_token_limit: 0,
            output_token_limit: 0,
            supported_generation_methods: Vec::new(),
            context_length: raw_model.context_length,
            max_completion_tokens: 0,
            supported_input_modalities: input_modalities,
            supported_output_modalities: vec!["text".to_string()],
            thinking: None,
        });
    }

    let catalog = catalog();
    let mut models = aggregate_models(variants);
    for model in &mut models {
        if let Some(curated) = catalog.by_id.get(&model.id) {
            if let Some(thinking) = &curated.thinking {
                let mut levels = thinking.levels.clone();
                if let Some(upstream_thinking) = &model.thinking {
                    levels.extend(upstream_thinking.levels.clone());
                }
                model.thinking = Some(ThinkingSupport {
                    levels: sort_levels(levels),
                });
            }
            if curated.max_completion_tokens > 0 {
                model.max_completion_tokens = curated.max_completion_tokens;
            }
        }
        if model.max_completion_tokens == 0 {
            model.max_completion_tokens = DEFAULT_COMPLETION_TOKENS;
        }
        model.output_token_limit = model.max_completion_tokens;
    }
    models
}

fn build_route_catalog(raw: &[UpstreamModel]) -> RouteCatalog {
    let mut catalog = RouteCatalog::default();
    for raw_model in raw {
        let uid = raw_model.uid.trim();
        if uid.is_empty() {
            continue;
        }
        let clean_uid = uid.to_lowercase();
        let (mut base_id, effort) = split_model_id(&clean_uid);
        if base_id.is_empty() {
            base_id = clean_uid.clone();
        }
        let target_model = routed_model(&base_id, &effort);
        catalog
            .exact
            .entry(clean_uid.clone())
            .or_insert_with(|| target_model.clone());
        let variants = catalog.by_base.entry(base_id.clone()).or_default();
        if variants
            .iter()
            .any(|variant| variant.target_model.eq_ignore_ascii_case(&target_model))
        {
            continue;
        }
        variants.push(RouteVariant {
            target_model,
            effort,
            bare: clean_uid == base_id,
        });
    }
    catalog
}

fn routed_model(base_id: &str, effort: &str) -> String {
    if effort.is_empty() {
        format!("devin/{base_id}")
    } else {
        format!("devin/{base_id}({effort})")
    }
}

fn routes() -> &'static RwLock<HashMap<String, RouteCatalog>> {
    ROUTES.get_or_init(|| RwLock::new(HashMap::new()))
}

fn replace_auth_routes(auth_id: &str, catalog: RouteCatalog) {
    let auth_id = auth_id.trim();
    if auth_id.is_empty() {
        return;
    }
    if let Ok(mut routes) = routes().write() {
        if catalog.by_base.is_empty() {
            routes.remove(auth_id);
        } else {
            routes.insert(auth_id.to_string(), catalog);
        }
    }
}

fn clear_auth_routes(auth_id: &str) {
    let auth_id = auth_id.trim();
    if auth_id.is_empty() {
        return;
    }
    if let Some(routes) = ROUTES.get() {
        if let Ok(mut routes) = routes.write() {
            routes.remove(auth_id);
        }
    }
}

fn hidden_route_models() -> HashSet<String> {
    let Some(routes) = ROUTES.get() else {
        return HashSet::new();
    };
    let Ok(routes) = routes.read() else {
        return HashSet::new();
    };
    routes
        .values()
        .flat_map(|catalog| catalog.exact.keys())
        .filter_map(|uid| {
            let (base_id, _) = split_model_id(uid);
            (!base_id.eq_ignore_ascii_case(uid)).then(|| format!("devin/{uid}").to_lowercase())
        })
        .collect()
}

fn filter_model_catalog_response(body: &[u8], hidden_models: &HashSet<String>) -> Option<Vec<u8>> {
    let mut payload = serde_json::from_slice::<Value>(body).ok()?;
    let root_is_list = payload
        .get("object")
        .and_then(Value::as_str)
        .is_some_and(|value| value.eq_ignore_ascii_case("list"));
    let mut changed = false;

    if let Some(data) = payload.get_mut("data").and_then(Value::as_array_mut) {
        let is_model_catalog = root_is_list || data.iter().any(is_model_catalog_entry);
        if is_model_catalog {
            changed |= remove_hidden_models(data, hidden_models);
        }
    }
    if let Some(models) = payload.get_mut("models").and_then(Value::as_array_mut) {
        changed |= remove_hidden_models(models, hidden_models);
    }

    changed.then(|| serde_json::to_vec(&payload).ok()).flatten()
}

fn remove_hidden_models(models: &mut Vec<Value>, hidden_models: &HashSet<String>) -> bool {
    let original_len = models.len();
    models.retain(|model| {
        model_catalog_entry_id(model).is_none_or(|id| !hidden_models.contains(&id))
    });
    models.len() != original_len
}

fn is_model_catalog_entry(value: &Value) -> bool {
    string_field(value, &["object", "type"])
        .is_some_and(|value| value.eq_ignore_ascii_case("model"))
        || string_field(value, &["name"])
            .is_some_and(|value| value.to_lowercase().starts_with("models/"))
}

fn model_catalog_entry_id(value: &Value) -> Option<String> {
    let id = string_field(value, &["id", "slug", "name"])?;
    let lower = id.trim().to_lowercase();
    let normalized = lower.strip_prefix("models/").unwrap_or(&lower).to_string();
    (!normalized.is_empty()).then_some(normalized)
}

fn resolve_route(requested_model: &str, requested_effort: &str) -> Option<String> {
    let routes = ROUTES.get()?.read().ok()?;
    select_route(routes.values(), requested_model, requested_effort)
}

fn select_route<'a>(
    catalogs: impl Iterator<Item = &'a RouteCatalog>,
    requested_model: &str,
    requested_effort: &str,
) -> Option<String> {
    let clean_model = strip_devin_prefix(requested_model)?.trim();
    if clean_model.is_empty() {
        return None;
    }
    let exact_key = clean_model.to_lowercase();
    let catalogs = catalogs.collect::<Vec<_>>();

    let mut exact_targets = BTreeMap::<String, (String, usize)>::new();
    for catalog in &catalogs {
        if let Some(target) = catalog.exact.get(&exact_key) {
            let key = target.to_lowercase();
            let entry = exact_targets
                .entry(key)
                .or_insert_with(|| (target.clone(), 0));
            entry.1 += 1;
        }
    }
    if let Some(target) = choose_most_supported(exact_targets.into_values()) {
        return Some(target);
    }

    let (base_id, suffix_effort) = split_requested_model(clean_model);
    let base_id = base_id.to_lowercase();
    let mut choices = BTreeMap::<String, (RouteVariant, usize)>::new();
    for catalog in &catalogs {
        let Some(variants) = catalog.by_base.get(&base_id) else {
            continue;
        };
        for variant in variants {
            let key = variant.target_model.to_lowercase();
            let entry = choices.entry(key).or_insert_with(|| (variant.clone(), 0));
            entry.1 += 1;
        }
    }
    if choices.is_empty() {
        return None;
    }

    let requested_effort = normalize_effort(if suffix_effort.is_empty() {
        requested_effort
    } else {
        &suffix_effort
    });
    let choices = choices.into_values().collect::<Vec<_>>();
    if !requested_effort.is_empty() {
        if let Some(target) = choose_variant_with_effort(&choices, &requested_effort) {
            return Some(target);
        }
        if let Some(clamped) = closest_effort(&requested_effort, &choices) {
            if let Some(target) = choose_variant_with_effort(&choices, &clamped) {
                return Some(target);
            }
        }
    } else {
        if let Some(target) = choose_bare_variant(&choices) {
            return Some(target);
        }
        let default_effort = default_route_effort(&base_id, &choices);
        if !default_effort.is_empty() {
            if let Some(target) = choose_variant_with_effort(&choices, &default_effort) {
                return Some(target);
            }
        }
    }

    let empty_effort = choices
        .iter()
        .filter(|(variant, _)| variant.effort.is_empty())
        .cloned()
        .collect::<Vec<_>>();
    if let Some(target) = choose_route_variant(empty_effort.into_iter()) {
        return Some(target);
    }
    choose_route_variant(choices.into_iter())
}

fn choose_most_supported(values: impl Iterator<Item = (String, usize)>) -> Option<String> {
    let mut values = values.collect::<Vec<_>>();
    values.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| left.0.to_lowercase().cmp(&right.0.to_lowercase()))
    });
    values.into_iter().next().map(|(target, _)| target)
}

fn choose_route_variant(values: impl Iterator<Item = (RouteVariant, usize)>) -> Option<String> {
    let mut values = values.collect::<Vec<_>>();
    values.sort_by(|left, right| {
        right.1.cmp(&left.1).then_with(|| {
            left.0
                .target_model
                .to_lowercase()
                .cmp(&right.0.target_model.to_lowercase())
        })
    });
    values
        .into_iter()
        .next()
        .map(|(variant, _)| variant.target_model)
}

fn choose_bare_variant(choices: &[(RouteVariant, usize)]) -> Option<String> {
    choose_route_variant(choices.iter().filter(|(variant, _)| variant.bare).cloned())
}

fn choose_variant_with_effort(choices: &[(RouteVariant, usize)], effort: &str) -> Option<String> {
    choose_route_variant(
        choices
            .iter()
            .filter(|(variant, _)| variant.effort == effort)
            .cloned(),
    )
}

fn closest_effort(requested: &str, choices: &[(RouteVariant, usize)]) -> Option<String> {
    if requested == "none" {
        return None;
    }
    let requested_rank = effort_rank(requested)?;
    let mut efforts = choices
        .iter()
        .filter_map(|(variant, _)| (!variant.effort.is_empty()).then_some(variant.effort.clone()))
        .collect::<Vec<_>>();
    efforts.sort();
    efforts.dedup();
    efforts.into_iter().min_by(|left, right| {
        let left_rank = effort_rank(left).unwrap_or(usize::MAX);
        let right_rank = effort_rank(right).unwrap_or(usize::MAX);
        left_rank
            .abs_diff(requested_rank)
            .cmp(&right_rank.abs_diff(requested_rank))
            .then_with(|| right_rank.cmp(&left_rank))
            .then_with(|| left.cmp(right))
    })
}

fn default_route_effort(base_id: &str, choices: &[(RouteVariant, usize)]) -> String {
    let efforts = choices
        .iter()
        .map(|(variant, _)| variant.effort.as_str())
        .filter(|effort| !effort.is_empty())
        .collect::<HashSet<_>>();
    if base_id.contains("swe-2") && efforts.contains("high") {
        return "high".to_string();
    }
    if base_id.starts_with("gpt-5") && efforts.contains("none") && efforts.contains("low") {
        return "low".to_string();
    }
    if efforts.contains("high")
        && ["gemini", "grok", "glm", "deepseek", "kimi", "nemotron"]
            .iter()
            .any(|family| base_id.contains(family))
    {
        return "high".to_string();
    }
    for effort in ["medium", "high", "low", "minimal", "xhigh", "max", "none"] {
        if efforts.contains(effort) {
            return effort.to_string();
        }
    }
    String::new()
}

fn effort_rank(effort: &str) -> Option<usize> {
    match effort {
        "none" => Some(0),
        "minimal" => Some(1),
        "low" => Some(2),
        "medium" => Some(3),
        "high" => Some(4),
        "xhigh" => Some(5),
        "max" => Some(6),
        "fast" => Some(7),
        _ => None,
    }
}

fn split_requested_model(clean_model: &str) -> (String, String) {
    let trimmed = clean_model.trim();
    if let Some(without_closing) = trimmed.strip_suffix(')') {
        if let Some(index) = without_closing.rfind('(') {
            let effort = normalize_effort(&without_closing[index + 1..]);
            if !effort.is_empty() {
                return (without_closing[..index].trim().to_string(), effort);
            }
        }
    }
    if let Some(index) = trimmed.rfind(':') {
        let effort = normalize_effort(&trimmed[index + 1..]);
        if !effort.is_empty() {
            return (trimmed[..index].trim().to_string(), effort);
        }
    }
    (trimmed.to_string(), String::new())
}

fn request_effort(request: &Value) -> String {
    let Some(body) = bytes_field(request, &["Body", "body"]) else {
        return String::new();
    };
    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return String::new();
    };
    for value in [
        body.get("reasoning_effort"),
        body.get("reasoning").and_then(|value| value.get("effort")),
        body.get("reasoning").and_then(|value| value.get("level")),
        body.get("thinking").and_then(|value| value.get("level")),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(value) = value.as_str() {
            let effort = normalize_effort(value);
            if !effort.is_empty() {
                return effort;
            }
        }
    }
    String::new()
}

fn normalize_effort(value: &str) -> String {
    match value.trim().to_lowercase().as_str() {
        "none" | "off" | "disabled" => "none".to_string(),
        "minimal" => "minimal".to_string(),
        "low" => "low".to_string(),
        "medium" => "medium".to_string(),
        "high" | "auto" | "adaptive" | "thinking" => "high".to_string(),
        "xhigh" => "xhigh".to_string(),
        "max" => "max".to_string(),
        "fast" => "fast".to_string(),
        _ => String::new(),
    }
}

fn has_devin_prefix(model: &str) -> bool {
    model
        .trim()
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("devin/"))
}

fn strip_devin_prefix(model: &str) -> Option<&str> {
    let model = model.trim();
    has_devin_prefix(model).then(|| &model[6..])
}

fn aggregate_models(models: Vec<ModelInfo>) -> Vec<ModelInfo> {
    struct Aggregate {
        model: ModelInfo,
        levels: HashSet<String>,
    }

    let mut order = Vec::new();
    let mut aggregated: HashMap<String, Aggregate> = HashMap::new();
    for model in models {
        let clean_id = model
            .id
            .trim()
            .to_lowercase()
            .strip_prefix("devin/")
            .unwrap_or(model.id.trim())
            .to_string();
        let (mut base_id, effort) = split_model_id(&clean_id);
        if base_id.is_empty() {
            base_id = clean_id.clone();
        }
        let namespaced = format!("devin/{base_id}");
        let is_base = base_id == clean_id;
        if !aggregated.contains_key(&namespaced) {
            let mut clone = model.clone();
            clone.id = namespaced.clone();
            let cleaned = clean_display_name(&model.display_name);
            if !cleaned.is_empty() {
                clone.display_name = cleaned;
            }
            aggregated.insert(
                namespaced.clone(),
                Aggregate {
                    model: clone,
                    levels: HashSet::new(),
                },
            );
            order.push(namespaced.clone());
        }

        let entry = aggregated.get_mut(&namespaced).expect("aggregate exists");
        if is_base {
            if !model.display_name.is_empty() {
                entry.model.display_name = clean_display_name(&model.display_name);
            }
            if !model.owned_by.is_empty() {
                entry.model.owned_by = model.owned_by.clone();
            }
        }
        entry.model.context_length = entry.model.context_length.max(model.context_length);
        entry.model.max_completion_tokens = entry
            .model
            .max_completion_tokens
            .max(model.max_completion_tokens);
        entry.model.input_token_limit = entry.model.input_token_limit.max(model.input_token_limit);
        entry.model.output_token_limit =
            entry.model.output_token_limit.max(model.output_token_limit);
        extend_unique(
            &mut entry.model.supported_input_modalities,
            &model.supported_input_modalities,
        );
        extend_unique(
            &mut entry.model.supported_output_modalities,
            &model.supported_output_modalities,
        );
        extend_unique(
            &mut entry.model.supported_generation_methods,
            &model.supported_generation_methods,
        );
        if let Some(thinking) = &model.thinking {
            for level in &thinking.levels {
                if !level.is_empty() && level != "priority" {
                    entry.levels.insert(level.clone());
                }
            }
        }
        if !effort.is_empty() && effort != "priority" {
            entry.levels.insert(effort);
        }
    }

    let mut output = Vec::with_capacity(order.len());
    for id in order {
        let Some(mut entry) = aggregated.remove(&id) else {
            continue;
        };
        if !entry.levels.is_empty() {
            entry.model.thinking = Some(ThinkingSupport {
                levels: sort_levels(entry.levels.into_iter().collect()),
            });
        }
        normalize_model(&mut entry.model);
        output.push(entry.model);
    }
    output
}

fn normalize_model(model: &mut ModelInfo) {
    if !model.id.to_lowercase().starts_with("devin/") {
        model.id = format!("devin/{}", model.id);
    }
    model.id = model.id.to_lowercase();
    if model.object.is_empty() {
        model.object = "model".to_string();
    }
    if model.model_type.is_empty() {
        model.model_type = "devin".to_string();
    }
    if model.supported_input_modalities.is_empty() {
        model.supported_input_modalities = vec!["text".to_string()];
    }
    if model.supported_output_modalities.is_empty() {
        model.supported_output_modalities = vec!["text".to_string()];
    }
    if model.input_token_limit == 0 && model.context_length > 0 {
        model.input_token_limit = model.context_length;
    }
    if model.output_token_limit == 0 && model.max_completion_tokens > 0 {
        model.output_token_limit = model.max_completion_tokens;
    }
    if model.supported_generation_methods.is_empty() {
        model.supported_generation_methods =
            vec!["generateContent".to_string(), "countTokens".to_string()];
    }
}

fn split_model_id(clean_id: &str) -> (String, String) {
    if clean_id == "swe-1-6-slow" {
        return (clean_id.to_string(), String::new());
    }
    if clean_id == "swe-1-6-fast" {
        return ("swe-1-6".to_string(), "fast".to_string());
    }

    let upper = clean_id.to_uppercase();
    for (suffix, effort) in [
        ("_NONE", "none"),
        ("_MINIMAL", "minimal"),
        ("_LOW", "low"),
        ("_MEDIUM", "medium"),
        ("_HIGH", "high"),
        ("_XHIGH", "xhigh"),
        ("_MAX", "max"),
        ("_THINKING", "high"),
    ] {
        if upper.ends_with(suffix) {
            return (
                clean_id[..clean_id.len() - suffix.len()].to_string(),
                effort.to_string(),
            );
        }
    }

    for (suffix, effort, readd) in [
        ("-low-fast", "low", ""),
        ("-medium-fast", "medium", ""),
        ("-high-fast", "high", ""),
        ("-xhigh-fast", "xhigh", ""),
        ("-max-fast", "max", ""),
        ("-none-fast", "none", ""),
        ("-low-priority", "low", ""),
        ("-medium-priority", "medium", ""),
        ("-high-priority", "high", ""),
        ("-xhigh-priority", "xhigh", ""),
        ("-max-priority", "max", ""),
        ("-none-priority", "none", ""),
        ("-thinking-1m", "high", ""),
        ("-thinking", "high", ""),
        ("-max-1m", "max", ""),
        ("-none-1m", "none", ""),
        ("-fast", "fast", ""),
    ] {
        if clean_id.ends_with(suffix) {
            let mut base = clean_id[..clean_id.len() - suffix.len()].to_string();
            base.push_str(readd);
            return (base, effort.to_string());
        }
    }

    for (suffix, effort) in [
        ("-none", "none"),
        ("-minimal", "minimal"),
        ("-low", "low"),
        ("-medium", "medium"),
        ("-high", "high"),
        ("-xhigh", "xhigh"),
        ("-max", "max"),
    ] {
        if clean_id.ends_with(suffix) {
            return (
                clean_id[..clean_id.len() - suffix.len()].to_string(),
                effort.to_string(),
            );
        }
    }
    for base in ["claude-opus-4-6", "claude-sonnet-4-6", "glm-5-2"] {
        if clean_id == format!("{base}-1m") {
            return (base.to_string(), String::new());
        }
    }
    (clean_id.to_string(), String::new())
}

fn clean_display_name(name: &str) -> String {
    const SUFFIXES: &[&str] = &[
        " Low Fast",
        " Medium Fast",
        " High Fast",
        " XHigh Fast",
        " Max Fast",
        " Low Thinking Fast",
        " Medium Thinking Fast",
        " High Thinking Fast",
        " XHigh Thinking Fast",
        " Max Thinking Fast",
        " No Thinking Fast",
        " Low Thinking",
        " Medium Thinking",
        " High Thinking",
        " XHigh Thinking",
        " Max Thinking",
        " No Thinking",
        " Low",
        " Medium",
        " High",
        " XHigh",
        " Max",
        " None",
        " Minimal",
        " Thinking",
        " Fast",
    ];
    let mut value = name.trim().to_string();
    loop {
        let lower = value.to_lowercase();
        let Some(suffix) = SUFFIXES
            .iter()
            .find(|suffix| lower.ends_with(&suffix.to_lowercase()))
        else {
            break;
        };
        value.truncate(value.len() - suffix.len());
        value = value.trim().to_string();
    }
    value
}

fn sort_levels(mut levels: Vec<String>) -> Vec<String> {
    levels.retain(|level| !level.is_empty() && level != "priority");
    levels.sort_by(|left, right| {
        level_rank(left)
            .cmp(&level_rank(right))
            .then_with(|| left.cmp(right))
    });
    levels.dedup();
    levels
}

fn level_rank(level: &str) -> u8 {
    match level {
        "none" => 0,
        "minimal" => 1,
        "low" => 2,
        "medium" => 3,
        "high" => 4,
        "xhigh" => 5,
        "max" => 6,
        "fast" => 7,
        "priority" => 8,
        _ => 99,
    }
}

fn vendor_name(vendor_id: u64, uid: &str) -> &'static str {
    match vendor_id {
        1 => "cognition",
        2 => "openai",
        3 => "anthropic",
        4 => "google",
        6 => "deepseek",
        7 => "moonshot",
        9 => "zhipu",
        11 => "nvidia",
        _ if uid.to_lowercase().contains("grok") => "xai",
        _ => "devin",
    }
}

fn fallback_models() -> Vec<ModelInfo> {
    catalog().models.clone()
}

fn catalog() -> &'static Catalog {
    CATALOG.get_or_init(|| {
        let parsed =
            serde_json::from_str::<CatalogPayload>(include_str!("../models/devin_models.json"));
        let mut models = match parsed {
            Ok(payload) => payload
                .devin
                .into_iter()
                .map(catalog_model)
                .collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        };
        models = aggregate_models(models);
        let by_id = models
            .iter()
            .cloned()
            .map(|model| (model.id.clone(), model))
            .collect();
        Catalog { models, by_id }
    })
}

fn catalog_model(model: CatalogModel) -> ModelInfo {
    let mut result = ModelInfo {
        id: model.id,
        object: model.object,
        owned_by: model.owned_by,
        model_type: model.r#type,
        display_name: model.display_name,
        input_token_limit: model.context_length,
        output_token_limit: model.max_completion_tokens,
        supported_generation_methods: Vec::new(),
        context_length: model.context_length,
        max_completion_tokens: model.max_completion_tokens,
        supported_input_modalities: model.supported_input_modalities,
        supported_output_modalities: model.supported_output_modalities,
        thinking: model.thinking.map(|thinking| ThinkingSupport {
            levels: thinking.levels,
        }),
    };
    normalize_model(&mut result);
    result
}

fn extend_unique(target: &mut Vec<String>, values: &[String]) {
    for value in values {
        if !target.contains(value) {
            target.push(value.clone());
        }
    }
}

fn object_field<'a>(value: &'a Value, names: &[&str]) -> Option<&'a Map<String, Value>> {
    field(value, names)?.as_object()
}

fn string_field(value: &Value, names: &[&str]) -> Option<String> {
    field(value, names)?
        .as_str()
        .map(|value| value.trim().to_string())
}

fn number_field(value: &Value, names: &[&str]) -> Option<u64> {
    field(value, names)?.as_u64()
}

fn bytes_field(value: &Value, names: &[&str]) -> Option<Vec<u8>> {
    let value = field(value, names)?;
    if let Some(encoded) = value.as_str() {
        return STANDARD.decode(encoded).ok();
    }
    value.as_array().map(|items| {
        items
            .iter()
            .filter_map(Value::as_u64)
            .map(|value| value as u8)
            .collect()
    })
}

fn field<'a>(value: &'a Value, names: &[&str]) -> Option<&'a Value> {
    let object = value.as_object()?;
    object.iter().find_map(|(key, value)| {
        let normalized = normalize_key(key);
        names
            .iter()
            .any(|name| normalize_key(name) == normalized)
            .then_some(value)
    })
}

fn lookup_sources(sources: &[&Value], keys: &[&str]) -> Option<String> {
    sources
        .iter()
        .find_map(|source| lookup_string(source, keys, 0))
}

fn lookup_string(value: &Value, keys: &[&str], depth: usize) -> Option<String> {
    if depth > 2 {
        return None;
    }
    let object = value.as_object()?;
    for (key, value) in object {
        let normalized = normalize_key(key);
        if keys
            .iter()
            .any(|candidate| normalize_key(candidate) == normalized)
        {
            if let Some(value) = value.as_str() {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    for nested in ["attributes", "metadata", "credentials", "auth"] {
        if let Some(value) = object
            .iter()
            .find_map(|(key, value)| (normalize_key(key) == nested).then_some(value))
        {
            if let Some(found) = lookup_string(value, keys, depth + 1) {
                return Some(found);
            }
        }
    }
    None
}

fn normalize_key(value: &str) -> String {
    value.trim().to_lowercase().replace('-', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(uid: &str) -> UpstreamModel {
        UpstreamModel {
            uid: uid.to_string(),
            label: uid.to_string(),
            context_length: 1_000_000,
            multimodal: true,
            vendor_id: 3,
        }
    }

    #[test]
    fn bundled_catalog_preserves_anthropic_thinking_models() {
        let models = fallback_models();
        for id in [
            "devin/claude-haiku-5-5",
            "devin/claude-sonnet-5-5",
            "devin/claude-opus-5-5",
        ] {
            let model = models
                .iter()
                .find(|model| model.id == id)
                .unwrap_or_else(|| panic!("missing bundled model: {id}"));
            assert_eq!(model.context_length, 1_000_000, "{id}");
            assert_eq!(model.max_completion_tokens, 64_000, "{id}");
            assert_eq!(
                model.thinking.as_ref().expect("thinking levels").levels,
                ["low", "medium", "high", "xhigh", "max"],
                "{id}"
            );
        }
    }

    #[test]
    fn discovered_models_only_publish_canonical_names() {
        let raw = vec![
            upstream("claude-sonnet-5-5-low"),
            upstream("claude-sonnet-5-5-high"),
            upstream("claude-sonnet-4-6-thinking"),
            upstream("claude-sonnet-4-6-thinking-1m"),
            upstream("swe-1-6-fast"),
        ];

        let models = build_models_from_upstream(&raw);
        let ids = models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<HashSet<_>>();

        assert!(ids.contains("devin/claude-sonnet-5-5"));
        assert!(ids.contains("devin/claude-sonnet-4-6"));
        assert!(ids.contains("devin/swe-1-6"));
        assert!(!ids.iter().any(|id| {
            id.ends_with("-low")
                || id.ends_with("-high")
                || id.contains("-thinking")
                || id.ends_with("-fast")
                || id.ends_with("-slow")
        }));
    }

    #[test]
    fn routes_use_auth_safe_base_model_suffixes() {
        let raw = vec![
            upstream("claude-sonnet-5-5-low"),
            upstream("claude-sonnet-5-5-high"),
            upstream("claude-sonnet-4-6-thinking"),
            upstream("claude-sonnet-4-6-thinking-1m"),
            upstream("swe-1-6-fast"),
        ];
        let routes = build_route_catalog(&raw);

        assert_eq!(
            select_route(std::iter::once(&routes), "devin/claude-sonnet-5-5", "high"),
            Some("devin/claude-sonnet-5-5(high)".to_string())
        );
        assert_eq!(
            select_route(std::iter::once(&routes), "devin/claude-sonnet-5-5-low", ""),
            Some("devin/claude-sonnet-5-5(low)".to_string())
        );
        assert_eq!(
            select_route(
                std::iter::once(&routes),
                "devin/claude-sonnet-4-6-thinking-1m",
                ""
            ),
            Some("devin/claude-sonnet-4-6(high)".to_string())
        );
        assert_eq!(
            select_route(std::iter::once(&routes), "devin/swe-1-6", "fast"),
            Some("devin/swe-1-6(fast)".to_string())
        );
    }
}
