//! Watch-only real-wallet tracking (read-only, Solana mainnet).
//!
//! This module can ONLY observe: it takes a **public address** and reads
//! balances through the public Solana JSON-RPC. There is no code path that
//! accepts, stores, or transmits a private key or seed phrase —
//! [`validate_watch_address`] rejects anything that is not a 32-byte
//! ed25519 public key, including 64-byte secret keys and seed phrases.
//! No live orders are involved here; (non-)execution is governed by
//! [`crate::livetrade`], disabled by default.

use reqwest::Client;

/// Official public Solana JSON-RPC (no key needed for balance reads).
/// Override with `SOLANA_RPC_URL` for a private/custom endpoint.
pub const DEFAULT_RPC_URL: &str = "https://api.mainnet-beta.solana.com";
/// SPL Token program: owner of every token account.
pub const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

/// Configured watch address (`WALLET_WATCH_ADDRESS`), if any.
pub fn watch_address() -> Option<String> {
    std::env::var("WALLET_WATCH_ADDRESS")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// RPC endpoint in use.
pub fn rpc_url() -> String {
    std::env::var("SOLANA_RPC_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_RPC_URL.to_string())
}

const B58: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

/// Decode a base58 string (Bitcoin/Solana alphabet, no external crate).
fn b58_decode(s: &str) -> Option<Vec<u8>> {
    if s.is_empty() {
        return None;
    }
    let mut num: Vec<u32> = vec![0];
    let leading_ones = s.bytes().take_while(|&b| b == b'1').count();
    // Leading '1's are zero bytes, not value: skip them in the loop,
    // then prepend exactly that many zeros.
    for c in s.bytes().skip(leading_ones) {
        let digit = B58.iter().position(|&b| b == c)? as u32;
        let mut carry = digit;
        for n in num.iter_mut() {
            let v = *n * 58 + carry;
            *n = v & 0xFF;
            carry = v >> 8;
        }
        while carry > 0 {
            num.push(carry & 0xFF);
            carry >>= 8;
        }
    }
    let mut be: Vec<u8> = num.into_iter().rev().map(|n| n as u8).collect();
    if be == [0] {
        be.clear(); // value zero contributes no bytes; '1's already counted
    }
    let mut full = vec![0u8; leading_ones];
    full.extend(be);
    Some(full)
}

/// Accept ONLY a 32-byte base58 public key. Rejects secret keys (64 bytes),
/// seed phrases (spaces / wrong alphabet), and anything malformed — with a
/// message that never echoes the input.
pub fn validate_watch_address(s: &str) -> Result<(), &'static str> {
    let s = s.trim();
    if s.contains(char::is_whitespace) {
        return Err("seed phrases are never accepted: paste the public address only");
    }
    if s.len() > 50 {
        return Err("too long for a public address: secret keys are never accepted");
    }
    match b58_decode(s) {
        Some(bytes) if bytes.len() == 32 => Ok(()),
        Some(_) => Err("not a 32-byte public address"),
        None => Err("not valid base58"),
    }
}

/// Short display form (`AbC1…XyZ9`), safe to render anywhere.
pub fn short_address(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    if c.len() <= 12 {
        return s.to_string();
    }
    let head: String = c[..4].iter().collect();
    let tail: String = c[c.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// One SPL token holding, human-readable amount.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenHolding {
    pub mint: String,
    pub ui_amount: f64,
}

/// Read-only snapshot of the watched address.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WalletSnapshot {
    pub sol: f64,
    pub tokens: Vec<TokenHolding>,
}

#[derive(Debug)]
pub enum WalletError {
    Transport(String),
    Rpc(String),
}

impl std::fmt::Display for WalletError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Never include addresses, keys, or response bodies.
            WalletError::Transport(_) => write!(f, "wallet RPC unreachable"),
            WalletError::Rpc(_) => write!(f, "wallet RPC error"),
        }
    }
}

/// Fetch SOL balance + parsed token accounts for a validated address.
pub async fn fetch_wallet(
    client: &Client,
    rpc: &str,
    address: &str,
) -> Result<WalletSnapshot, WalletError> {
    validate_watch_address(address).map_err(|e| WalletError::Rpc(e.to_string()))?;
    let sol_req = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "getBalance", "params": [address],
    });
    let sol_resp: serde_json::Value = client
        .post(rpc)
        .json(&sol_req)
        .send()
        .await
        .map_err(|_| WalletError::Transport("getBalance".to_string()))?
        .json()
        .await
        .map_err(|_| WalletError::Transport("getBalance body".to_string()))?;
    let lamports = sol_resp
        .pointer("/result/value")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| WalletError::Rpc("getBalance shape".to_string()))?;

    let tok_req = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "getTokenAccountsByOwner",
        "params": [address,
            {"programId": TOKEN_PROGRAM_ID},
            {"encoding": "jsonParsed"}],
    });
    let tok_resp: serde_json::Value = client
        .post(rpc)
        .json(&tok_req)
        .send()
        .await
        .map_err(|_| WalletError::Transport("getTokenAccountsByOwner".to_string()))?
        .json()
        .await
        .map_err(|_| WalletError::Transport("token accounts body".to_string()))?;
    let accounts = tok_resp
        .pointer("/result/value")
        .and_then(|v| v.as_array())
        .ok_or_else(|| WalletError::Rpc("token accounts shape".to_string()))?;

    let mut tokens = Vec::new();
    for a in accounts {
        let info = match a.pointer("/account/data/parsed/info") {
            Some(i) => i,
            None => continue,
        };
        let mint = info
            .get("mint")
            .and_then(|m| m.as_str())
            .unwrap_or_default();
        let ui = info
            .pointer("/tokenAmount/uiAmount")
            .and_then(|u| u.as_f64())
            .unwrap_or(0.0);
        if ui > 0.0 {
            tokens.push(TokenHolding {
                mint: short_address(mint),
                ui_amount: ui,
            });
        }
    }
    // Largest holdings first, cap the card.
    tokens.sort_by(|a, b| b.ui_amount.partial_cmp(&a.ui_amount).unwrap());
    tokens.truncate(8);
    Ok(WalletSnapshot {
        sol: lamports as f64 / 1_000_000_000.0,
        tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b58_decodes_system_program_to_32_zero_bytes() {
        // Well-known vector: 32 × '1' == 32 zero bytes.
        let bytes = b58_decode("11111111111111111111111111111111").expect("decodes");
        assert_eq!(bytes.len(), 32);
        assert!(bytes.iter().all(|&b| b == 0));
    }

    #[test]
    fn b58_rejects_bad_alphabet() {
        assert!(b58_decode("0OIl Treating humans").is_none());
        assert!(b58_decode("").is_none());
    }

    #[test]
    fn watch_accepts_real_pubkey_shape() {
        // Genuine mainnet address (Solana docs): 44 chars, 32 bytes.
        let addr = "vines1vzrYbzLMRdu58ou5XTNHP4Vv4qynWzstKGaCU";
        assert_eq!(b58_decode(addr).expect("decodes").len(), 32);
        assert!(validate_watch_address(addr).is_ok());
    }

    #[test]
    fn watch_rejects_secrets_and_phrases() {
        // 64-byte secret key (88 chars) must be refused, not truncated.
        let secret = "11111111111111111111111111111111111111111111111111111111111111111111111111111111";
        assert!(b58_decode(&secret[..44]).is_some()); // prefix alone looks fine…
        assert!(validate_watch_address(secret).is_err()); // …but the full input is refused
        assert!(validate_watch_address("word ".repeat(12).trim()).is_err());
        assert!(validate_watch_address("not base58!!").is_err());
        assert!(validate_watch_address("11111111111111111111111111111111").is_ok());
    }

    #[test]
    fn short_address_never_panics() {
        assert_eq!(short_address("AbC1xYz9QwErT"), "AbC1…wErT");
        assert_eq!(short_address("tiny"), "tiny");
        // Multibyte input must not panic on char boundaries.
        assert_eq!(short_address("éèêëāăąćĉċčďđéè"), "éèêë…ďđéè");
    }
}
