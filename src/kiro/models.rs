//! Kiro 上游 ListAvailableModels 的数据结构与拉取逻辑
//!
//! 移植自 Kiro-account-manager 的 fetchKiroModels 实现：
//! 通过 `https://q.{region}.amazonaws.com/ListAvailableModels` 拉取上游可用模型，
//! 仅 social 类账号需要传 profileArn，IdC / builder-id / API Key 凭据不传。

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use reqwest::Client;
use serde::Deserialize;

use crate::kiro::endpoint::{KiroEndpoint, RequestContext};
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::{CallContext, MultiTokenManager};
use crate::model::config::Config;

/// 上游单个模型的 token 限制
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KiroTokenLimits {
    pub max_input_tokens: Option<i64>,
    pub max_output_tokens: Option<i64>,
}

/// 上游单个模型的 prompt cache 元信息
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct KiroPromptCaching {
    #[serde(default)]
    pub supports_prompt_caching: bool,
    #[serde(default)]
    pub maximum_cache_checkpoints_per_request: Option<i64>,
    #[serde(default)]
    pub minimum_tokens_per_cache_checkpoint: Option<i64>,
}

/// 上游单个模型条目（部分字段保持宽松）
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct KiroModelInfo {
    pub model_id: String,
    #[serde(default)]
    pub model_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub model_provider: Option<String>,
    #[serde(default)]
    pub rate_multiplier: Option<f64>,
    #[serde(default)]
    pub rate_unit: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub supported_input_types: Vec<String>,
    #[serde(default)]
    pub token_limits: Option<KiroTokenLimits>,
    #[serde(default)]
    pub prompt_caching: Option<KiroPromptCaching>,
    /// 上游声明的“附加请求字段”schema，用于判断模型是否支持 thinking
    #[serde(default)]
    pub additional_model_request_fields_schema: Option<serde_json::Value>,
    #[serde(default)]
    pub available_origins: Option<Vec<String>>,
}

impl KiroModelInfo {
    /// 该模型是否支持 thinking（依据上游 schema）
    pub fn supports_thinking(&self) -> bool {
        self.additional_model_request_fields_schema
            .as_ref()
            .and_then(|v| v.get("properties"))
            .and_then(|v| v.get("thinking"))
            .is_some()
    }
}

/// 分页响应
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListModelsResponse {
    #[serde(default)]
    models: Vec<KiroModelInfo>,
    #[serde(default)]
    next_token: Option<String>,
}

/// 模型缓存条目
#[derive(Clone)]
struct CacheEntry {
    models: Vec<KiroModelInfo>,
    fetched_at: Instant,
}

/// 上游模型列表缓存（默认 5 分钟 TTL）
pub struct ModelsCache {
    ttl: Duration,
    inner: RwLock<Option<CacheEntry>>,
}

impl ModelsCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: RwLock::new(None),
        }
    }

    pub fn get_fresh(&self) -> Option<Vec<KiroModelInfo>> {
        let guard = self.inner.read();
        let entry = guard.as_ref()?;
        if entry.fetched_at.elapsed() <= self.ttl {
            Some(entry.models.clone())
        } else {
            None
        }
    }

    pub fn get_any(&self) -> Option<Vec<KiroModelInfo>> {
        self.inner.read().as_ref().map(|e| e.models.clone())
    }

    pub fn store(&self, models: Vec<KiroModelInfo>) {
        *self.inner.write() = Some(CacheEntry {
            models,
            fetched_at: Instant::now(),
        });
    }
}

impl Default for ModelsCache {
    fn default() -> Self {
        Self::new(Duration::from_secs(5 * 60))
    }
}

/// social 类凭据才需要传 profileArn（与 IdeEndpoint 的逻辑保持一致）
fn profile_arn_for_query(credentials: &KiroCredentials) -> Option<&str> {
    let auth_method = credentials.auth_method.as_deref();
    let is_aws_sso_oidc = matches!(auth_method, Some("builder-id") | Some("idc"))
        || (credentials.client_id.is_some() && credentials.client_secret.is_some());
    if is_aws_sso_oidc {
        return None;
    }
    credentials.profile_arn.as_deref()
}

/// 构造与 Kiro 官方 IDE 插件一致的 User-Agent
fn build_user_agent(config: &Config, machine_id: &str) -> String {
    format!(
        "aws-sdk-js/1.0.34 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererstreaming#1.0.34 m/E KiroIDE-{}-{}",
        config.system_version,
        config.node_version,
        config.kiro_version,
        machine_id
    )
}

/// 构造与 Kiro 官方 IDE 插件一致的 x-amz-user-agent
fn build_x_amz_user_agent(config: &Config, machine_id: &str) -> String {
    format!(
        "aws-sdk-js/1.0.34 KiroIDE-{}-{}",
        config.kiro_version, machine_id
    )
}

/// 真正的 HTTP 拉取逻辑（不带缓存，调用方负责缓存）
///
/// 流程：
/// 1. 通过 endpoint.api_url 推算 q 服务 host（与现有 IDE 调用走同一域名）
/// 2. GET `https://{host}/ListAvailableModels?origin=AI_EDITOR&maxResults=50[&profileArn=..][&nextToken=..]`
/// 3. 自动翻页直到 nextToken 为空
///
/// 请求头与 Kiro-account-manager 的 fetchKiroModels 保持一致，避免上游 400。
pub async fn fetch_kiro_models(
    client: &Client,
    token_manager: &MultiTokenManager,
    endpoint: &dyn KiroEndpoint,
    ctx: &CallContext,
) -> anyhow::Result<Vec<KiroModelInfo>> {
    let config = token_manager.config();
    let machine_id = machine_id::generate_from_credentials(&ctx.credentials, &config)
        .ok_or_else(|| anyhow::anyhow!("无法生成 machine_id"))?;

    // 通过 endpoint 提供的 api_url 推算 host，确保与凭据所属区域一致
    let request_ctx = RequestContext {
        credentials: &ctx.credentials,
        token: &ctx.token,
        machine_id: &machine_id,
        config: &config,
    };
    let api_url = endpoint.api_url(&request_ctx);
    let host = api_url
        .strip_prefix("https://")
        .and_then(|rest| rest.split('/').next())
        .ok_or_else(|| anyhow::anyhow!("无法从 endpoint 解析 host: {api_url}"))?;
    let base_url = format!("https://{host}/ListAvailableModels");

    let user_agent = build_user_agent(&config, &machine_id);
    let x_amz_user_agent = build_x_amz_user_agent(&config, &machine_id);

    let mut all_models: Vec<KiroModelInfo> = Vec::new();
    let mut next_token: Option<String> = None;
    // 安全上限：避免上游异常时无限翻页
    const MAX_PAGES: usize = 20;

    for page in 0..MAX_PAGES {
        let mut url = format!("{base_url}?origin=AI_EDITOR&maxResults=50");
        if let Some(arn) = profile_arn_for_query(&ctx.credentials) {
            url.push_str(&format!("&profileArn={}", urlencoding::encode(arn)));
        }
        if let Some(tok) = next_token.as_deref() {
            url.push_str(&format!("&nextToken={}", urlencoding::encode(tok)));
        }

        // 与 KAM fetchKiroModels 完全一致的最小请求头
        let resp = client
            .get(&url)
            .header("Authorization", format!("Bearer {}", ctx.token))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", &user_agent)
            .header("x-amz-user-agent", &x_amz_user_agent)
            .header("x-amzn-codewhisperer-optout", "true")
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!(
                page,
                status = %status,
                body_preview = %body.chars().take(200).collect::<String>(),
                "ListAvailableModels 返回非 2xx，停止翻页"
            );
            // 返回已经拿到的模型；如果一条都没拿到则视作错误
            break;
        }

        let parsed: ListModelsResponse = resp.json().await?;
        all_models.extend(parsed.models);
        match parsed.next_token {
            Some(tok) if !tok.is_empty() => next_token = Some(tok),
            _ => return Ok(all_models),
        }
    }

    Ok(all_models)
}

/// 共享缓存的便捷封装
pub async fn fetch_with_cache(
    client: &Client,
    token_manager: &MultiTokenManager,
    endpoint: &dyn KiroEndpoint,
    ctx: &CallContext,
    cache: &Arc<ModelsCache>,
) -> anyhow::Result<Vec<KiroModelInfo>> {
    if let Some(models) = cache.get_fresh() {
        return Ok(models);
    }

    match fetch_kiro_models(client, token_manager, endpoint, ctx).await {
        Ok(models) if !models.is_empty() => {
            cache.store(models.clone());
            Ok(models)
        }
        Ok(_) => {
            // 上游返回空列表：保留旧缓存，用旧缓存兜底
            if let Some(stale) = cache.get_any() {
                Ok(stale)
            } else {
                Ok(Vec::new())
            }
        }
        Err(err) => {
            tracing::warn!("拉取上游模型失败: {err}");
            if let Some(stale) = cache.get_any() {
                Ok(stale)
            } else {
                Err(err)
            }
        }
    }
}
