//! 分析结果缓存：避免对同一 mint + 参数重复打 RPC。
//!
//! 分析逻辑升级时递增 `CACHE_VERSION`，旧缓存自动失效。

use crate::types::{Analysis, fmt_time};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 分析算法或口径变更时递增，使旧缓存失效。
pub const CACHE_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheParams {
    pub top: usize,
    pub tx_limit: usize,
    pub funding_scan: usize,
    pub hops: usize,
    pub holders_mode: String,
    pub owners: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct AnalysisCache {
    version: u32,
    analyzed_at: i64,
    params: CacheParams,
    analysis: Analysis,
}

fn path_for(mint: &str) -> String {
    format!("cache/{mint}/analysis.json")
}

pub fn load(mint: &str, params: &CacheParams) -> Result<Option<Analysis>> {
    let path = path_for(mint);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let cached: AnalysisCache =
        serde_json::from_str(&raw).with_context(|| format!("解析缓存失败: {path}"))?;
    if cached.version != CACHE_VERSION {
        eprintln!(
            "ℹ 缓存版本过旧 (v{} → v{})，将重新分析",
            cached.version, CACHE_VERSION
        );
        return Ok(None);
    }
    if cached.params != *params {
        eprintln!("ℹ 分析参数与缓存不一致，将重新分析");
        return Ok(None);
    }
    eprintln!(
        "✓ 使用缓存分析 ({})，加 --refresh 可强制重拉",
        fmt_time(Some(cached.analyzed_at))
    );
    Ok(Some(cached.analysis))
}

pub fn save(mint: &str, params: &CacheParams, analysis: &Analysis) -> Result<()> {
    let path = path_for(mint);
    if let Some(dir) = path.rsplit_once('/') {
        std::fs::create_dir_all(dir.0)?;
    }
    let cached = AnalysisCache {
        version: CACHE_VERSION,
        analyzed_at: chrono::Utc::now().timestamp(),
        params: params.clone(),
        analysis: analysis.clone(),
    };
    std::fs::write(&path, serde_json::to_string(&cached)?)?;
    eprintln!("✓ 分析已缓存: {path}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Distribution, TokenInfo};

    fn sample_analysis(mint: &str) -> Analysis {
        Analysis {
            token: TokenInfo {
                mint: mint.into(),
                symbol: Some("TEST".into()),
                program: "Tokenkeg".into(),
                decimals: 6,
                supply: 1_000_000.0,
                holder_count: 1,
                holders_complete: true,
            },
            holders: vec![],
            dist: Distribution::default(),
            pnl: vec![],
            flows: vec![],
            clusters: vec![],
            last_price_sol: None,
            last_price_time: None,
            sol_usd: None,
            safety: None,
            transfer_links: vec![],
            snapshot_diff: None,
            upstream: Default::default(),
        }
    }

    #[test]
    fn round_trip_and_param_mismatch() {
        let mint = "CacheMint1111111111111111111111111111111";
        let path = path_for(mint);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(format!("cache/{mint}"));

        let params = CacheParams {
            top: 10,
            tx_limit: 60,
            funding_scan: 25,
            hops: 1,
            holders_mode: "auto".into(),
            owners: vec![],
        };
        let analysis = sample_analysis(mint);
        save(mint, &params, &analysis).unwrap();

        let loaded = load(mint, &params).unwrap().expect("cache hit");
        assert_eq!(loaded.token.mint, mint);

        let other = CacheParams { top: 20, ..params.clone() };
        assert!(load(mint, &other).unwrap().is_none());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(format!("cache/{mint}"));
    }
}