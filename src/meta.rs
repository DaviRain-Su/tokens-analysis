//! 代币元数据（symbol/name）解析：
//! 优先读 Metaplex Metadata PDA，回退 Token-2022 的 tokenMetadata 扩展。

use crate::rpc::Rpc;
use base64::Engine;
use serde_json::json;
use sha2::{Digest, Sha256};

const METADATA_PROGRAM: &str = "metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s";

#[derive(Clone, Debug)]
pub struct TokenMeta {
    pub symbol: String,
    /// 暂未在界面展示，保留给后续 TUI 使用
    #[allow(dead_code)]
    pub name: String,
}

pub async fn fetch_meta(rpc: &Rpc, mint: &str) -> Option<TokenMeta> {
    if let Some(m) = fetch_metaplex(rpc, mint).await {
        return Some(m);
    }
    fetch_token2022_ext(rpc, mint).await
}

async fn fetch_metaplex(rpc: &Rpc, mint: &str) -> Option<TokenMeta> {
    let program = bs58::decode(METADATA_PROGRAM).into_vec().ok()?;
    let mint_bytes = bs58::decode(mint).into_vec().ok()?;
    let pda = find_program_address(&[b"metadata", &program, &mint_bytes], &program)?;
    let res = rpc
        .call(
            "getAccountInfo",
            json!([bs58::encode(&pda).into_string(), {"encoding": "base64"}]),
        )
        .await
        .ok()?;
    let data_b64 = res["value"]["data"][0].as_str()?;
    let data = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .ok()?;
    parse_metaplex(&data)
}

/// Metaplex Metadata 布局: key(1) + update_authority(32) + mint(32)
/// + name(borsh string) + symbol(borsh string) + ...
fn parse_metaplex(data: &[u8]) -> Option<TokenMeta> {
    let mut off = 1 + 32 + 32;
    let name = read_borsh_string(data, &mut off)?;
    let symbol = read_borsh_string(data, &mut off)?;
    if symbol.is_empty() && name.is_empty() {
        return None;
    }
    Some(TokenMeta { symbol, name })
}

fn read_borsh_string(data: &[u8], off: &mut usize) -> Option<String> {
    if data.len() < *off + 4 {
        return None;
    }
    let len = u32::from_le_bytes(data[*off..*off + 4].try_into().ok()?) as usize;
    *off += 4;
    if len > 256 || data.len() < *off + len {
        return None;
    }
    let s = String::from_utf8_lossy(&data[*off..*off + len])
        .trim_end_matches('\0')
        .trim()
        .to_string();
    *off += len;
    Some(s)
}

async fn fetch_token2022_ext(rpc: &Rpc, mint: &str) -> Option<TokenMeta> {
    let res = rpc
        .call("getAccountInfo", json!([mint, {"encoding": "jsonParsed"}]))
        .await
        .ok()?;
    let exts = res["value"]["data"]["parsed"]["info"]["extensions"].as_array()?;
    for ext in exts {
        if ext["extension"].as_str() == Some("tokenMetadata") {
            let s = &ext["state"];
            return Some(TokenMeta {
                symbol: s["symbol"].as_str().unwrap_or_default().to_string(),
                name: s["name"].as_str().unwrap_or_default().to_string(),
            });
        }
    }
    None
}

/// Solana PDA 推导：从 bump=255 向下找第一个不在 ed25519 曲线上的哈希。
fn find_program_address(seeds: &[&[u8]], program_id: &[u8]) -> Option<[u8; 32]> {
    for bump in (0..=255u8).rev() {
        let mut h = Sha256::new();
        for s in seeds {
            h.update(s);
        }
        h.update([bump]);
        h.update(program_id);
        h.update(b"ProgramDerivedAddress");
        let hash: [u8; 32] = h.finalize().into();
        let on_curve = curve25519_dalek::edwards::CompressedEdwardsY(hash)
            .decompress()
            .is_some();
        if !on_curve {
            return Some(hash);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pda_derivation_matches_known() {
        // USDC 的 Metaplex metadata PDA（链上已知值）
        let program = bs58::decode(METADATA_PROGRAM).into_vec().unwrap();
        let mint = bs58::decode("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v")
            .into_vec()
            .unwrap();
        let pda = find_program_address(&[b"metadata", &program, &mint], &program).unwrap();
        assert_eq!(
            bs58::encode(&pda).into_string(),
            "5x38Kp4hvdomTCnCrAny4UtMUt5rQBdB6px2K1Ui45Wq"
        );
    }

    #[test]
    fn borsh_string_parsing() {
        // key(1) + authority(32) + mint(32) + name(带 \0 填充) + symbol
        let mut data = vec![0u8; 65];
        data.extend(4u32.to_le_bytes());
        data.extend(b"AB\0\0");
        data.extend(1u32.to_le_bytes());
        data.extend(b"X");
        let m = parse_metaplex(&data).unwrap();
        assert_eq!(m.name, "AB");
        assert_eq!(m.symbol, "X");
    }
}
