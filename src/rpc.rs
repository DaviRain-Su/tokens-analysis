//! 轻量 Solana JSON-RPC 客户端：内置并发限流与 429/5xx 退避重试。

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::Semaphore;

pub struct Rpc {
    http: reqwest::Client,
    /// 大响应专用（getProgramAccounts 全量扫描可能要下载几十 MB）
    http_long: reqwest::Client,
    url: String,
    sem: Semaphore,
}

impl Rpc {
    pub fn new(url: &str, concurrency: usize) -> Self {
        let build = |secs| {
            reqwest::Client::builder()
                .timeout(Duration::from_secs(secs))
                .build()
                .expect("构建 HTTP 客户端失败")
        };
        Self {
            http: build(90),
            http_long: build(600),
            url: url.to_string(),
            sem: Semaphore::new(concurrency.max(1)),
        }
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.call_inner(method, params, false).await
    }

    /// 超大响应版本（10 分钟超时），用于全量账户扫描
    pub async fn call_long(&self, method: &str, params: Value) -> Result<Value> {
        self.call_inner(method, params, true).await
    }

    async fn call_inner(&self, method: &str, params: Value, long: bool) -> Result<Value> {
        let http = if long { &self.http_long } else { &self.http };
        let _permit = self.sem.acquire().await?;
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let mut delay = Duration::from_millis(400);
        let mut last_err: Option<anyhow::Error> = None;
        for _ in 0..10 {
            match http.post(&self.url).json(&body).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.as_u16() == 429 || status.is_server_error() {
                        // 限流时尊重服务端给的 Retry-After
                        if let Some(wait) = resp
                            .headers()
                            .get("retry-after")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| s.parse::<u64>().ok())
                        {
                            delay = delay.max(Duration::from_secs(wait.min(30)));
                        }
                        last_err = Some(anyhow!("{method} HTTP {status}"));
                    } else {
                        // 大响应可能下载中途断开，解析失败也要重试
                        match resp.json::<Value>().await {
                            Ok(v) => {
                                if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
                                    // JSON-RPC 层错误（方法被禁用、参数错误等）不重试
                                    bail!("{method} RPC 错误: {err}");
                                }
                                return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                            }
                            Err(e) => last_err = Some(anyhow!("{method} 响应解析失败: {e}")),
                        }
                    }
                }
                Err(e) => last_err = Some(e.into()),
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(20));
        }
        Err(last_err.unwrap_or_else(|| anyhow!("{method} 重试后仍然失败")))
    }

    /// 拉取地址签名（自动翻页，最新在前），最多 `max` 条。
    pub async fn signatures(&self, address: &str, max: usize) -> Result<Vec<Value>> {
        let mut out: Vec<Value> = Vec::new();
        let mut before: Option<String> = None;
        while out.len() < max {
            let limit = (max - out.len()).min(1000);
            let mut cfg = json!({"limit": limit, "commitment": "confirmed"});
            if let Some(b) = &before {
                cfg["before"] = json!(b);
            }
            let res = self
                .call("getSignaturesForAddress", json!([address, cfg]))
                .await?;
            let arr = res.as_array().cloned().unwrap_or_default();
            let n = arr.len();
            if n == 0 {
                break;
            }
            before = arr
                .last()
                .and_then(|s| s["signature"].as_str())
                .map(String::from);
            out.extend(arr);
            if n < limit {
                break;
            }
        }
        Ok(out)
    }

    pub async fn transaction(&self, signature: &str) -> Result<Value> {
        self.call(
            "getTransaction",
            json!([signature, {
                "encoding": "jsonParsed",
                "maxSupportedTransactionVersion": 0,
                "commitment": "confirmed"
            }]),
        )
        .await
    }
}

/// 解析 jsonParsed 的 uiTokenAmount。
pub fn ui_amount(v: &Value) -> f64 {
    if let Some(x) = v["uiAmount"].as_f64() {
        return x;
    }
    let dec = v["decimals"].as_u64().unwrap_or(0) as i32;
    v["amount"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|a| a / 10f64.powi(dec))
        .unwrap_or(0.0)
}
