//! 已知地址标签：交易所热钱包 / AMM 池子权限账户。
//! 最佳努力维护，不保证完整 —— 用于资金溯源时快速识别来源性质。

pub const TOKENKEG: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";
pub const WSOL: &str = "So11111111111111111111111111111111111111112";
pub const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
pub const USDT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";

pub fn label_for(addr: &str) -> Option<&'static str> {
    Some(match addr {
        // 交易所热钱包
        "5tzFkiKscXHK5ZXCGbXZxdw7gTjjD1mBwuoFbhUvuAi9" => "Binance",
        "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM" => "Binance 2",
        "2ojv9BAiHUrvsm9gxDe7fJSzbNZSJcxZvf8dqmWGHG8S" => "Binance Cold",
        "H8sMJSCQxfKiFTCfDR3DUMLPwcRbM61LGFJ8N4dK3WjS" => "Coinbase",
        "2AQdpHJ2JpcEgPiATUXjQxA8QmafFegfQwSLWSprPicm" => "Coinbase 2",
        "GJRs4FwHtemZ5ZE9x3FNvJ8TMwitKTh21yxdRPqn7npE" => "Coinbase Hot",
        "5VCwKtCXgCJ6kit5FybXjvriW3xELsFDhYrPSqtJNmcD" => "OKX",
        "AC5RDfQFmDS1deWZos921JfqscXdByf8BKHs5ACWjtW2" => "Bybit",
        "FWznbcNXWQuHTawe9RxvQ2LdCENssh12dsznf4RiouN5" => "Kraken",
        "u6PJ8DtQuPFnfmwHbGFULQ4u4EgjDiyYKjVEsynXq2w" => "Gate.io",
        "BmFdpraQhkiDQE6SnfG5omcA1VwzqfXrwtNYBwWTymy6" => "KuCoin",
        "ASTyfSima4LLAdDgoFGkgqoKowG1LZFDr9fAQrg7iaJZ" => "MEXC",
        // AMM / DEX
        "5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1" => "Raydium AMM V4",
        "GpMZbSM2GgvTKHJirzeGfMFoaZ8UR2X7F4v8vHTvxFbL" => "Raydium CPMM",
        _ => return None,
    })
}

pub fn is_exchange(label: &str) -> bool {
    matches!(
        label,
        l if l.starts_with("Binance")
            || l.starts_with("Coinbase")
            || l.starts_with("OKX")
            || l.starts_with("Bybit")
            || l.starts_with("Kraken")
            || l.starts_with("Gate")
            || l.starts_with("KuCoin")
            || l.starts_with("MEXC")
    )
}
