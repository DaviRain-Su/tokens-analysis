//! 代币安全检查（蜜罐过滤）：跟买前用一次 getAccountInfo 排查
//! 增发权限、冻结权限、Token-2022 转账税/hook/永久代理等机制风险。

use crate::rpc::Rpc;
use anyhow::{Result, bail};
use serde_json::{Value, json};

#[derive(Clone, Debug, Default)]
pub struct SafetyReport {
    pub mint_authority: Option<String>,
    pub freeze_authority: Option<String>,
    pub transfer_fee_bps: Option<u32>,
    pub transfer_hook: bool,
    pub permanent_delegate: bool,
    pub default_frozen: bool,
    /// 人类可读的风险列表，空 = 通过
    pub risks: Vec<String>,
}

impl SafetyReport {
    pub fn is_safe(&self) -> bool {
        self.risks.is_empty()
    }

    pub fn summary(&self) -> String {
        if self.is_safe() {
            "✓ 通过 (无增发/冻结/转账税等机制风险)".into()
        } else {
            format!("✗ {} 项风险: {}", self.risks.len(), self.risks.join("; "))
        }
    }
}

pub async fn check_mint(rpc: &Rpc, mint: &str) -> Result<SafetyReport> {
    let acct = rpc
        .call("getAccountInfo", json!([mint, {"encoding": "jsonParsed"}]))
        .await?;
    let v = &acct["value"];
    if v.is_null() {
        bail!("mint 账户不存在: {mint}");
    }
    Ok(parse_report(v))
}

/// 从 jsonParsed 的 mint 账户解析安全报告（与网络解耦，便于测试）。
pub fn parse_report(v: &Value) -> SafetyReport {
    let info = &v["data"]["parsed"]["info"];
    let mut r = SafetyReport {
        mint_authority: info["mintAuthority"].as_str().map(String::from),
        freeze_authority: info["freezeAuthority"].as_str().map(String::from),
        ..Default::default()
    };
    for ext in info["extensions"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[])
    {
        let state = &ext["state"];
        match ext["extension"].as_str().unwrap_or("") {
            "transferFeeConfig" => {
                r.transfer_fee_bps = state["newerTransferFee"]["transferFeeBasisPoints"]
                    .as_u64()
                    .map(|x| x as u32);
            }
            "transferHook" => r.transfer_hook = state["programId"].as_str().is_some(),
            "permanentDelegate" => {
                r.permanent_delegate = state["delegate"].as_str().is_some();
            }
            "defaultAccountState" => {
                r.default_frozen = state["accountState"].as_str() == Some("frozen");
            }
            _ => {}
        }
    }

    if r.mint_authority.is_some() {
        r.risks.push("mint authority 未放弃 (可无限增发)".into());
    }
    if r.freeze_authority.is_some() {
        r.risks.push("freeze authority 存在 (可冻结你的账户使其无法卖出)".into());
    }
    if let Some(bps) = r.transfer_fee_bps {
        if bps > 100 {
            r.risks.push(format!("转账税 {bps} bps ({:.1}%)", bps as f64 / 100.0));
        }
    }
    if r.transfer_hook {
        r.risks.push("transfer hook (转账可被外部程序拦截)".into());
    }
    if r.permanent_delegate {
        r.risks.push("permanent delegate (发行方可直接没收代币)".into());
    }
    if r.default_frozen {
        r.risks.push("新账户默认冻结".into());
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renounced_spl_token_is_safe() {
        let v = json!({
            "owner": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
            "data": {"parsed": {"type": "mint", "info": {
                "decimals": 6, "mintAuthority": null, "freezeAuthority": null, "supply": "1"
            }}}
        });
        let r = parse_report(&v);
        assert!(r.is_safe(), "{:?}", r.risks);
    }

    #[test]
    fn authorities_flagged() {
        let v = json!({
            "data": {"parsed": {"info": {
                "decimals": 6, "mintAuthority": "Auth111", "freezeAuthority": "Auth222"
            }}}
        });
        let r = parse_report(&v);
        assert_eq!(r.risks.len(), 2);
        assert!(!r.is_safe());
    }

    #[test]
    fn token2022_extensions_flagged() {
        let v = json!({
            "data": {"parsed": {"info": {
                "decimals": 9, "mintAuthority": null, "freezeAuthority": null,
                "extensions": [
                    {"extension": "transferFeeConfig", "state": {"newerTransferFee": {"transferFeeBasisPoints": 500}}},
                    {"extension": "permanentDelegate", "state": {"delegate": "Evil111"}},
                    {"extension": "transferHook", "state": {"programId": "Hook111"}},
                ]
            }}}
        });
        let r = parse_report(&v);
        assert_eq!(r.transfer_fee_bps, Some(500));
        assert!(r.permanent_delegate && r.transfer_hook);
        assert_eq!(r.risks.len(), 3);
    }

    #[test]
    fn small_transfer_fee_tolerated() {
        let v = json!({
            "data": {"parsed": {"info": {
                "decimals": 9, "mintAuthority": null, "freezeAuthority": null,
                "extensions": [
                    {"extension": "transferFeeConfig", "state": {"newerTransferFee": {"transferFeeBasisPoints": 50}}}
                ]
            }}}
        });
        assert!(parse_report(&v).is_safe());
    }
}
