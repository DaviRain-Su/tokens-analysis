//! 本地密钥加载与交易签名。
//!
//! 签名只操作交易"信封"：序列化的 VersionedTransaction 结构是
//! `[签名数量(shortvec)] [签名×N] [消息字节]`，对消息字节做 ed25519
//! 签名后填入 fee payer 的签名槽位即可，不需要理解消息内部结构。
//! 签名前会校验消息的第一个账户（fee payer）确实是本钱包，
//! 防止给非预期的交易签名。

use anyhow::{Result, bail};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};

pub struct Wallet {
    signing: SigningKey,
    pub pubkey: String,
}

impl Wallet {
    /// 加载 Solana CLI 格式的密钥文件（64 字节 JSON 数组）
    pub fn load(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("读取密钥文件 {path} 失败: {e}"))?;
        let bytes: Vec<u8> = serde_json::from_str(&raw)?;
        if bytes.len() != 64 {
            bail!("密钥文件格式错误: 期望 64 字节, 实际 {}", bytes.len());
        }
        let signing = SigningKey::from_bytes(bytes[..32].try_into().unwrap());
        let verifying = signing.verifying_key();
        if verifying.as_bytes() != &bytes[32..] {
            bail!("密钥文件损坏: 公钥与私钥不匹配");
        }
        Ok(Self {
            pubkey: bs58::encode(verifying.as_bytes()).into_string(),
            signing,
        })
    }

    /// 给 base64 编码的 VersionedTransaction 签名（fee payer 槽位），返回 base64。
    pub fn sign_transaction_b64(&self, tx_b64: &str) -> Result<String> {
        let engine = base64::engine::general_purpose::STANDARD;
        let mut data = engine.decode(tx_b64)?;
        let (nsig, sig_array_start) = decode_shortvec(&data)?;
        if nsig == 0 {
            bail!("交易没有签名槽位");
        }
        let msg_start = sig_array_start + nsig * 64;
        if data.len() <= msg_start {
            bail!("交易数据过短");
        }
        // 校验消息的 fee payer 是本钱包
        let fee_payer = extract_fee_payer(&data[msg_start..])?;
        if fee_payer != self.pubkey {
            bail!("交易的 fee payer ({fee_payer}) 不是本钱包 ({})", self.pubkey);
        }
        let sig = self.signing.sign(&data[msg_start..]);
        data[sig_array_start..sig_array_start + 64].copy_from_slice(&sig.to_bytes());
        Ok(engine.encode(&data))
    }
}

/// 解析 compact-u16 (shortvec) 长度前缀，返回 (值, 数据起始偏移)
fn decode_shortvec(data: &[u8]) -> Result<(usize, usize)> {
    let mut value = 0usize;
    for (i, &b) in data.iter().take(3).enumerate() {
        value |= ((b & 0x7f) as usize) << (7 * i);
        if b & 0x80 == 0 {
            return Ok((value, i + 1));
        }
    }
    bail!("shortvec 编码无效")
}

/// 从消息字节中取第一个静态账户（fee payer）。
/// 兼容 legacy 与 v0 消息：v0 首字节最高位为 1（版本号），后跟
/// 3 字节 header + 账户数量(shortvec) + 账户列表。
fn extract_fee_payer(msg: &[u8]) -> Result<String> {
    let mut off = 0;
    if msg[0] & 0x80 != 0 {
        off += 1; // 版本字节
    }
    off += 3; // header: num_required_signatures, num_readonly_signed, num_readonly_unsigned
    let (nkeys, len) = decode_shortvec(&msg[off..])?;
    off += len;
    if nkeys == 0 || msg.len() < off + 32 {
        bail!("消息账户列表无效");
    }
    Ok(bs58::encode(&msg[off..off + 32]).into_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use ed25519_dalek::Verifier;

    fn test_wallet() -> Wallet {
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        Wallet {
            pubkey: bs58::encode(signing.verifying_key().as_bytes()).into_string(),
            signing,
        }
    }

    fn fake_tx(fee_payer: &[u8; 32]) -> Vec<u8> {
        // 1 个签名槽位（置零） + v0 消息: 版本字节 + header + 2 个账户
        let mut tx = vec![1u8];
        tx.extend([0u8; 64]);
        tx.push(0x80); // v0
        tx.extend([1, 0, 1]); // header
        tx.push(2); // 账户数
        tx.extend(fee_payer);
        tx.extend([9u8; 32]);
        tx.extend([0u8; 40]); // 其余消息内容（recent_blockhash + instructions 占位）
        tx
    }

    #[test]
    fn sign_and_verify() {
        let w = test_wallet();
        let pubkey_bytes: [u8; 32] = bs58::decode(&w.pubkey).into_vec().unwrap().try_into().unwrap();
        let tx = fake_tx(&pubkey_bytes);
        let engine = base64::engine::general_purpose::STANDARD;
        let signed_b64 = w.sign_transaction_b64(&engine.encode(&tx)).unwrap();
        let signed = engine.decode(&signed_b64).unwrap();
        // 验证签名对消息字节有效
        let sig = ed25519_dalek::Signature::from_bytes(signed[1..65].try_into().unwrap());
        let msg = &signed[65..];
        w.signing.verifying_key().verify(msg, &sig).unwrap();
    }

    #[test]
    fn reject_wrong_fee_payer() {
        let w = test_wallet();
        let tx = fake_tx(&[3u8; 32]); // 别人的 fee payer
        let engine = base64::engine::general_purpose::STANDARD;
        assert!(w.sign_transaction_b64(&engine.encode(&tx)).is_err());
    }

    #[test]
    fn shortvec_multibyte() {
        assert_eq!(decode_shortvec(&[0x05]).unwrap(), (5, 1));
        assert_eq!(decode_shortvec(&[0x80, 0x01]).unwrap(), (128, 2));
        assert_eq!(decode_shortvec(&[0xff, 0x01]).unwrap(), (255, 2));
    }
}
