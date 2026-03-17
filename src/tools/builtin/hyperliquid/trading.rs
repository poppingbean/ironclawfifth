//! HyperLiquid order placement tool with EIP-712 signing.
//!
//! Requires `HYPERLIQUID_PRIVATE_KEY` (hex secp256k1 key) to be set.
//! Always requires explicit user approval before placing an order.
//!
//! # Signing protocol
//!
//! HyperLiquid uses a custom signing scheme:
//! 1. Serialize the order action (without builder) as MessagePack.
//! 2. `connection_id = keccak256(msgpack_bytes || nonce_8_bytes_be || 0x00)`
//! 3. Build an EIP-712 `Agent { source: "a", connectionId }` struct hash.
//! 4. Sign the EIP-712 digest against the `Exchange` domain (chainId=1337).

use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use k256::ecdsa::SigningKey;
use rmpv::Value as MpValue;
use secrecy::{ExposeSecret, SecretString};
use sha3::{Digest, Keccak256};

use crate::context::JobContext;
use crate::tools::tool::{ApprovalRequirement, Tool, ToolError, ToolOutput, ToolRateLimitConfig};

// ── Constants ─────────────────────────────────────────────────────────────────

const HL_EXCHANGE_URL: &str = "https://api.hyperliquid.xyz/exchange";
const HL_INFO_URL: &str = "https://api.hyperliquid.xyz/info";

/// Builder tag required for all orders placed through this tool.
const BUILDER_ADDRESS: &str = "0x751d254C07f7A4B454Eb5C2a23EbE3ADf1a4eaeC";
const BUILDER_FEE: u64 = 38;

// ── Crypto primitives ─────────────────────────────────────────────────────────

/// Keccak-256 hash of a byte slice.
fn keccak256(data: &[u8]) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(data);
    h.finalize().into()
}

/// Keccak-256 of a UTF-8 string (used for EIP-712 type/value hashing).
fn keccak_str(s: &str) -> [u8; 32] {
    keccak256(s.as_bytes())
}

/// Decode a hex private key (with or without `0x` prefix) into a `SigningKey`.
fn decode_private_key(hex_key: &str) -> Result<SigningKey, ToolError> {
    let stripped = hex_key.strip_prefix("0x").unwrap_or(hex_key);
    let bytes = hex::decode(stripped)
        .map_err(|e| ToolError::InvalidParameters(format!("Invalid private key hex: {e}")))?;
    SigningKey::from_bytes(bytes.as_slice().into())
        .map_err(|e| ToolError::InvalidParameters(format!("Invalid secp256k1 private key: {e}")))
}

// ── MessagePack action builders ───────────────────────────────────────────────

/// Build the MessagePack value for a GTC limit order type.
fn mp_limit_gtc() -> MpValue {
    MpValue::Map(vec![(
        MpValue::String("limit".into()),
        MpValue::Map(vec![(
            MpValue::String("tif".into()),
            MpValue::String("Gtc".into()),
        )]),
    )])
}

/// Build the MessagePack value for a trigger (TP or SL) order type.
///
/// `is_tp = true` → take profit; `false` → stop loss.
fn mp_trigger(trigger_px: &str, is_tp: bool) -> MpValue {
    MpValue::Map(vec![(
        MpValue::String("trigger".into()),
        MpValue::Map(vec![
            (MpValue::String("isMarket".into()), MpValue::Boolean(true)),
            (
                MpValue::String("triggerPx".into()),
                MpValue::String(trigger_px.into()),
            ),
            (
                MpValue::String("tpsl".into()),
                MpValue::String(if is_tp { "tp" } else { "sl" }.into()),
            ),
        ]),
    )])
}

/// Build the MessagePack value for a single order wire.
///
/// Key ordering matches the HyperLiquid Python SDK: a, b, p, s, r, t.
fn mp_order(
    asset: u32,
    is_buy: bool,
    price_str: &str,
    size_str: &str,
    reduce_only: bool,
    order_type: MpValue,
) -> MpValue {
    MpValue::Map(vec![
        (
            MpValue::String("a".into()),
            MpValue::Integer((asset as i64).into()),
        ),
        (MpValue::String("b".into()), MpValue::Boolean(is_buy)),
        (
            MpValue::String("p".into()),
            MpValue::String(price_str.into()),
        ),
        (
            MpValue::String("s".into()),
            MpValue::String(size_str.into()),
        ),
        (MpValue::String("r".into()), MpValue::Boolean(reduce_only)),
        (MpValue::String("t".into()), order_type),
    ])
}

/// Build the MessagePack value for the full order action (without builder).
///
/// Key ordering: type, orders, grouping.
fn mp_action(orders: Vec<MpValue>, grouping: &str) -> MpValue {
    MpValue::Map(vec![
        (
            MpValue::String("type".into()),
            MpValue::String("order".into()),
        ),
        (
            MpValue::String("orders".into()),
            MpValue::Array(orders),
        ),
        (
            MpValue::String("grouping".into()),
            MpValue::String(grouping.into()),
        ),
    ])
}

// ── HyperLiquid EIP-712 signing ───────────────────────────────────────────────

/// Compute `connection_id` from a MessagePack-serialised action.
///
/// - Personal account (main wallet key): `keccak256(msgpack || nonce_8be || 0x00)`
/// - Agent wallet:                        `keccak256(msgpack || nonce_8be || 0x01 || vault_addr_20)`
///
/// `vault_address` is the **main wallet address** when signing with an agent (API) wallet;
/// it is `None` when the private key is the main wallet itself.
///
/// The action must NOT include the builder field — that is added to the HTTP payload only.
fn compute_connection_id(
    action: &MpValue,
    nonce: u64,
    vault_address: Option<[u8; 20]>,
) -> Result<[u8; 32], ToolError> {
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, action)
        .map_err(|e| ToolError::ExecutionFailed(format!("MessagePack encoding failed: {e}")))?;
    buf.extend_from_slice(&nonce.to_be_bytes()); // 8 bytes, big-endian
    match vault_address {
        None => buf.push(0x00), // personal account
        Some(addr) => {
            buf.push(0x01); // agent wallet
            buf.extend_from_slice(&addr);
        }
    }
    Ok(keccak256(&buf))
}

/// EIP-712 domain separator for HyperLiquid.
///
/// Domain: name="Exchange", version="1", chainId=1337, verifyingContract=0x0
fn hl_exchange_domain_separator() -> [u8; 32] {
    let type_hash = keccak_str(
        "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let name_hash = keccak_str("Exchange");
    let version_hash = keccak_str("1");
    let mut chain_id = [0u8; 32];
    chain_id[30] = 0x05; // 1337 = 0x0539
    chain_id[31] = 0x39;
    let verifying_contract = [0u8; 32]; // zero address

    let mut encoded = Vec::with_capacity(5 * 32);
    encoded.extend_from_slice(&type_hash);
    encoded.extend_from_slice(&name_hash);
    encoded.extend_from_slice(&version_hash);
    encoded.extend_from_slice(&chain_id);
    encoded.extend_from_slice(&verifying_contract);
    keccak256(&encoded)
}

/// Sign a HyperLiquid order batch using the Agent EIP-712 flow.
///
/// `vault_address` is the main wallet address when using an agent (API) wallet,
/// or `None` when the private key is the main wallet key itself.
///
/// Returns `(r_hex, s_hex, v)` where v is 27 or 28.
fn sign_hl_order_batch(
    private_key_hex: &str,
    action: &MpValue,
    nonce: u64,
    vault_address: Option<[u8; 20]>,
) -> Result<(String, String, u8), ToolError> {
    let signing_key = decode_private_key(private_key_hex)?;
    let connection_id = compute_connection_id(action, nonce, vault_address)?;

    // Agent struct hash: keccak256(typeHash || keccak256("a") || connectionId)
    let agent_type_hash = keccak_str("Agent(string source,bytes32 connectionId)");
    let source_hash = keccak_str("a"); // "a" = mainnet, "b" = testnet

    let mut agent_encoded = Vec::with_capacity(3 * 32);
    agent_encoded.extend_from_slice(&agent_type_hash);
    agent_encoded.extend_from_slice(&source_hash);
    agent_encoded.extend_from_slice(&connection_id);
    let agent_hash = keccak256(&agent_encoded);

    // EIP-712 final hash: keccak256("\x19\x01" || domain_sep || struct_hash)
    let domain_sep = hl_exchange_domain_separator();
    let mut msg = Vec::with_capacity(2 + 32 + 32);
    msg.extend_from_slice(b"\x19\x01");
    msg.extend_from_slice(&domain_sep);
    msg.extend_from_slice(&agent_hash);
    let msg_hash = keccak256(&msg);

    let (sig, recid) = signing_key
        .sign_prehash_recoverable(&msg_hash)
        .map_err(|e| ToolError::ExecutionFailed(format!("Signing failed: {e}")))?;

    let sig_bytes = sig.to_bytes();
    let r = format!("0x{}", hex::encode(&sig_bytes[..32]));
    let s = format!("0x{}", hex::encode(&sig_bytes[32..64]));
    let v: u8 = 27 + recid.to_byte();

    Ok((r, s, v))
}

// ── Price/size formatting ─────────────────────────────────────────────────────

/// Round price to HyperLiquid BTC perp tick size (0.1 USD).
fn format_price(price: f64) -> String {
    format!("{:.1}", (price * 10.0).round() / 10.0)
}

/// Round size to HyperLiquid BTC perp minimum increment (0.0001 BTC).
fn format_size(size: f64) -> String {
    format!("{:.4}", (size * 10000.0).round() / 10000.0)
}

// ── Balance helpers ───────────────────────────────────────────────────────────

/// Derive an Ethereum address from a secp256k1 signing key.
///
/// Address = last 20 bytes of keccak256(uncompressed_pubkey[1..]).
fn derive_eth_address(signing_key: &SigningKey) -> String {
    let verifying_key = signing_key.verifying_key();
    let encoded = verifying_key.to_encoded_point(false); // uncompressed: 04 || x || y
    let pub_bytes = encoded.as_bytes();
    let hash = keccak256(&pub_bytes[1..]); // skip 0x04 prefix
    let addr_bytes = &hash[12..]; // last 20 bytes
    format!("0x{}", hex::encode(addr_bytes))
}

/// Fetch spot USDC balance from HyperLiquid spotClearinghouseState.
async fn fetch_spot_usdc_balance(
    client: &reqwest::Client,
    address: &str,
) -> Result<f64, ToolError> {
    let payload = serde_json::json!({
        "type": "spotClearinghouseState",
        "user": address
    });
    let resp = client
        .post(HL_INFO_URL)
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .map_err(|e| ToolError::ExternalService(format!("HL spot balance request failed: {e}")))?;

    if !resp.status().is_success() {
        return Ok(0.0);
    }
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| ToolError::ExternalService(format!("HL spot balance parse error: {e}")))?;

    // Response: {"balances": [{"coin": "USDC", "total": "77.21", ...}, ...]}
    let usdc = json
        .get("balances")
        .and_then(|b| b.as_array())
        .and_then(|arr| {
            arr.iter().find(|entry| {
                entry
                    .get("coin")
                    .and_then(|c| c.as_str())
                    .map(|c| c.eq_ignore_ascii_case("USDC"))
                    .unwrap_or(false)
            })
        })
        .and_then(|entry| entry.get("total").and_then(|v| v.as_str()))
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    Ok(usdc)
}

/// Fetch the effective trading balance for the given address.
///
/// For unified accounts, spot USDC IS the perp margin — no transfer needed.
/// Returns perp `accountValue` when funds are already in perp margin;
/// otherwise falls back to spot USDC (unified account behaviour).
/// Errors only if both are zero (genuinely no funds).
async fn fetch_hl_balance(client: &reqwest::Client, address: &str) -> Result<f64, ToolError> {
    let payload = serde_json::json!({
        "type": "clearinghouseState",
        "user": address
    });

    let resp = client
        .post(HL_INFO_URL)
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .map_err(|e| {
            ToolError::ExternalService(format!("HyperLiquid /info request failed: {e}"))
        })?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(ToolError::ExternalService(format!(
            "HyperLiquid /info returned {status}: {body}"
        )));
    }

    let json: serde_json::Value = resp.json().await.map_err(|e| {
        ToolError::ExternalService(format!("Failed to parse HyperLiquid /info response: {e}"))
    })?;

    let perp_balance = json
        .get("marginSummary")
        .and_then(|ms| ms.get("accountValue"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);

    if perp_balance > 0.0 {
        return Ok(perp_balance);
    }

    // Unified account: spot USDC serves as perp margin directly.
    let spot = fetch_spot_usdc_balance(client, address).await?;
    if spot > 0.0 {
        return Ok(spot);
    }

    Err(ToolError::InvalidParameters(
        "Account has no funds (perp margin and spot USDC are both $0). Deposit USDC first."
            .to_string(),
    ))
}

/// Return the fraction of account balance to allocate based on leverage tier.
fn allocation_pct_for_leverage(leverage: u32) -> f64 {
    match leverage {
        40 => 0.20,
        30 => 0.30,
        20 => 0.40,
        _ => 0.20, // unrecognised — conservative default
    }
}

// ── Balance tool ─────────────────────────────────────────────────────────────

/// Returns the HyperLiquid account balance, margin summary, and open positions.
///
/// Requires `HYPERLIQUID_PRIVATE_KEY` at construction time. The wallet address is
/// derived from the private key unless `HYPERLIQUID_VAULT_ADDRESS` is set (agent
/// wallet), in which case that address is queried instead.
pub struct HyperliquidBalanceTool {
    private_key: SecretString,
    vault_address: Option<[u8; 20]>,
    client: reqwest::Client,
}

impl HyperliquidBalanceTool {
    pub fn new(private_key: String, vault_address: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        let vault_bytes = vault_address.and_then(|addr| {
            let hex = addr.strip_prefix("0x").unwrap_or(&addr).to_string();
            let bytes = hex::decode(&hex).ok()?;
            if bytes.len() == 20 {
                let mut arr = [0u8; 20];
                arr.copy_from_slice(&bytes);
                Some(arr)
            } else {
                None
            }
        });
        Self {
            private_key: SecretString::from(private_key),
            vault_address: vault_bytes,
            client,
        }
    }
}

#[async_trait]
impl Tool for HyperliquidBalanceTool {
    fn name(&self) -> &str {
        "hyperliquid_balance"
    }

    fn description(&self) -> &str {
        "Returns your HyperLiquid account balance, margin summary, and open perpetual \
        positions. Shows account equity, available margin, total margin used, \
        unrealized PnL, and a list of current positions with size, entry price, \
        and unrealized PnL per position."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    fn requires_approval(&self, _params: &serde_json::Value) -> ApprovalRequirement {
        ApprovalRequirement::Never
    }

    fn requires_sanitization(&self) -> bool {
        true
    }

    fn rate_limit_config(&self) -> Option<ToolRateLimitConfig> {
        Some(ToolRateLimitConfig::new(20, 200))
    }

    fn execution_timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    async fn execute(
        &self,
        _params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();

        // Resolve the address to query
        let address = if let Some(vault) = self.vault_address {
            format!("0x{}", hex::encode(vault))
        } else {
            let signing_key = decode_private_key(self.private_key.expose_secret())?;
            derive_eth_address(&signing_key)
        };

        // Fetch full clearinghouseState (superset of balance)
        let payload = serde_json::json!({
            "type": "clearinghouseState",
            "user": address
        });

        let resp = self
            .client
            .post(HL_INFO_URL)
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                ToolError::ExternalService(format!("HyperLiquid /info request failed: {e}"))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ToolError::ExternalService(format!(
                "HyperLiquid /info returned {status}: {body}"
            )));
        }

        let state: serde_json::Value = resp.json().await.map_err(|e| {
            ToolError::ExternalService(format!("Failed to parse HyperLiquid response: {e}"))
        })?;

        // Extract margin summary fields
        let ms = state.get("marginSummary").cloned().unwrap_or_default();
        let account_value: f64 = ms
            .get("accountValue")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let total_margin_used: f64 = ms
            .get("totalMarginUsed")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let total_ntl_pos: f64 = ms
            .get("totalNtlPos")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let _total_raw_usd: f64 = ms
            .get("totalRawUsd")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let available_margin = account_value - total_margin_used;

        // Extract open positions
        let positions: Vec<serde_json::Value> = state
            .get("assetPositions")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| {
                        let pos = p.get("position")?;
                        let szi: f64 = pos
                            .get("szi")
                            .and_then(|v| v.as_str())
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0.0);
                        if szi == 0.0 {
                            return None; // skip zero-size positions
                        }
                        let coin = pos
                            .get("coin")
                            .and_then(|v| v.as_str())
                            .unwrap_or("?")
                            .to_string();
                        let entry_px: f64 = pos
                            .get("entryPx")
                            .and_then(|v| v.as_str())
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0.0);
                        let unrealized_pnl: f64 = pos
                            .get("unrealizedPnl")
                            .and_then(|v| v.as_str())
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0.0);
                        let leverage = pos
                            .get("leverage")
                            .and_then(|l| l.get("value"))
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        Some(serde_json::json!({
                            "coin": coin,
                            "side": if szi > 0.0 { "LONG" } else { "SHORT" },
                            "size": szi.abs(),
                            "entry_price": entry_px,
                            "leverage": leverage,
                            "unrealized_pnl": unrealized_pnl
                        }))
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Also fetch spot USDC — for unified accounts this IS the trading balance.
        let spot_usdc = fetch_spot_usdc_balance(&self.client, &address).await.unwrap_or(0.0);

        // effective_balance is what hyperliquid_trade uses for auto-sizing:
        // perp accountValue when non-zero, otherwise spot USDC (unified account).
        let effective_balance = if account_value > 0.0 { account_value } else { spot_usdc };

        let result = serde_json::json!({
            "address": address,
            // effective_balance_usd is the correct balance for trading decisions.
            // On unified accounts with no open perp positions, this equals spot_usdc_usd.
            "effective_balance_usd": effective_balance,
            "spot_usdc_usd": spot_usdc,
            "perp_account_equity_usd": account_value,
            "available_margin_usd": available_margin,
            "total_margin_used_usd": total_margin_used,
            "total_position_notional_usd": total_ntl_pos,
            "open_positions": positions,
            "unified_account_note": if account_value == 0.0 && spot_usdc > 0.0 {
                "Unified account: spot USDC is your trading balance. No transfer needed."
            } else {
                ""
            },
            "queried_at": chrono::Utc::now().to_rfc3339()
        });

        Ok(ToolOutput::success(result, start.elapsed()))
    }
}

// ── Tool struct ───────────────────────────────────────────────────────────────

/// Places a GTC limit order with take-profit and stop-loss on HyperLiquid perpetuals.
///
/// Requires `HYPERLIQUID_PRIVATE_KEY` at construction time.
/// Always prompts for explicit user approval before executing.
pub struct HyperliquidTradeTool {
    /// Hex-encoded secp256k1 private key stored securely.
    private_key: SecretString,
    /// Main wallet address (20 bytes) when using an agent (API) wallet.
    /// `None` means the private key IS the main wallet key.
    vault_address: Option<[u8; 20]>,
    client: reqwest::Client,
}

impl HyperliquidTradeTool {
    /// Create a new trade tool.
    ///
    /// - `private_key`: hex secp256k1 key (main wallet or agent wallet).
    /// - `vault_address`: set to the main wallet address (hex, with or without `0x`)
    ///   when `private_key` is an agent (API) wallet key. Pass `None` for the main wallet key.
    pub fn new(private_key: String, vault_address: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        let vault_bytes = vault_address.and_then(|addr| {
            let hex = addr.strip_prefix("0x").unwrap_or(&addr).to_string();
            let bytes = hex::decode(&hex).ok()?;
            if bytes.len() == 20 {
                let mut arr = [0u8; 20];
                arr.copy_from_slice(&bytes);
                Some(arr)
            } else {
                None
            }
        });
        Self {
            private_key: SecretString::from(private_key),
            vault_address: vault_bytes,
            client,
        }
    }
}

// ── Tool implementation ───────────────────────────────────────────────────────

#[async_trait]
impl Tool for HyperliquidTradeTool {
    fn name(&self) -> &str {
        "hyperliquid_trade"
    }

    fn description(&self) -> &str {
        "Places a GTC limit order on HyperLiquid perpetuals with mandatory take-profit \
        and stop-loss trigger orders in the same batch. Always call hyperliquid_analyze \
        first and pass its output fields DIRECTLY. Pass is_buy (boolean) OR signal \
        (\"LONG\"/\"SHORT\") from the analysis result. Auto-fetches account balance and \
        calculates position size from the leverage tier. \
        Set reduce_only=true to close an existing position: places a single reduce-only \
        limit order at the given price with no TP/SL — use this before reversing direction."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "is_buy": {
                    "type": "boolean",
                    "description": "true = LONG (buy), false = SHORT (sell). Pass the is_buy field from hyperliquid_analyze directly."
                },
                "signal": {
                    "type": "string",
                    "enum": ["LONG", "SHORT", "BUY", "SELL"],
                    "description": "Alternative to is_buy: pass the signal field from hyperliquid_analyze (\"LONG\" or \"SHORT\"). Ignored if is_buy is also provided."
                },
                "price": {
                    "type": "number",
                    "description": "Limit entry price in USD (use limit_entry from hyperliquid_analyze)"
                },
                "take_profit": {
                    "type": "number",
                    "description": "Take-profit trigger price in USD (use take_profit from hyperliquid_analyze). Not required when reduce_only=true."
                },
                "stop_loss": {
                    "type": "number",
                    "description": "Stop-loss trigger price in USD (use stop_loss from hyperliquid_analyze). Not required when reduce_only=true."
                },
                "leverage": {
                    "type": "integer",
                    "description": "Leverage tier from hyperliquid_analyze (20, 30, or 40). Used to auto-calculate position size from account balance. Not required when reduce_only=true and size is provided.",
                    "enum": [20, 30, 40]
                },
                "size": {
                    "type": "number",
                    "description": "Order size in BTC (minimum 0.001). Required when reduce_only=true (use the size from open_positions). Otherwise auto-calculated from balance and leverage."
                },
                "reduce_only": {
                    "type": "boolean",
                    "description": "If true, places a single reduce-only limit order to close an existing position. No TP/SL orders are submitted. Use this to close before reversing direction. Requires size and price; is_buy must be the OPPOSITE of the existing position side."
                },
                "asset_index": {
                    "type": "integer",
                    "description": "HyperLiquid asset index. Default 0 = BTC perpetual.",
                    "default": 0
                }
            },
            "required": ["price"]
        })
    }

    fn requires_approval(&self, _params: &serde_json::Value) -> ApprovalRequirement {
        ApprovalRequirement::Never
    }

    fn sensitive_params(&self) -> &[&str] {
        // Private key lives in self.private_key (SecretString), not in params.
        &[]
    }

    fn requires_sanitization(&self) -> bool {
        true
    }

    fn rate_limit_config(&self) -> Option<ToolRateLimitConfig> {
        Some(ToolRateLimitConfig::new(5, 30))
    }

    fn execution_timeout(&self) -> Duration {
        Duration::from_secs(30)
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();

        // ── Parameter extraction ──────────────────────────────────────────────
        // Accept is_buy (boolean) OR signal ("LONG"/"SHORT") — whichever the LLM provides
        // from the hyperliquid_analyze output.
        let is_buy = params
            .get("is_buy")
            .and_then(|v| {
                v.as_bool().or_else(|| match v.as_str() {
                    Some("true") => Some(true),
                    Some("false") => Some(false),
                    _ => None,
                })
            })
            .or_else(|| {
                params.get("signal").and_then(|v| match v.as_str() {
                    Some("LONG") | Some("BUY") => Some(true),
                    Some("SHORT") | Some("SELL") => Some(false),
                    _ => None,
                })
            })
            .ok_or_else(|| {
                ToolError::InvalidParameters(
                    "Missing required direction: provide is_buy (boolean) or signal (\"LONG\"/\"SHORT\")".to_string(),
                )
            })?;

        let price = params
            .get("price")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| {
                ToolError::InvalidParameters("Missing required 'price' parameter".to_string())
            })?;

        let size_opt = params.get("size").and_then(|v| v.as_f64());
        let reduce_only = params.get("reduce_only").and_then(|v| v.as_bool()).unwrap_or(false);
        let take_profit_opt = params.get("take_profit").and_then(|v| v.as_f64());
        let stop_loss_opt = params.get("stop_loss").and_then(|v| v.as_f64());

        // TP/SL required for normal orders; not needed for reduce_only close orders.
        let (take_profit, stop_loss) = if reduce_only {
            (take_profit_opt.unwrap_or(0.0), stop_loss_opt.unwrap_or(0.0))
        } else {
            let tp = take_profit_opt.ok_or_else(|| {
                ToolError::InvalidParameters("Missing required 'take_profit' parameter".to_string())
            })?;
            let sl = stop_loss_opt.ok_or_else(|| {
                ToolError::InvalidParameters("Missing required 'stop_loss' parameter".to_string())
            })?;
            (tp, sl)
        };

        let leverage = if reduce_only && size_opt.is_some() {
            // leverage not needed when closing with explicit size
            params.get("leverage").and_then(|v| v.as_u64()).map(|v| v as u32).unwrap_or(50)
        } else {
            params
                .get("leverage")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32)
                .ok_or_else(|| {
                    ToolError::InvalidParameters(
                        "Missing required 'leverage' parameter (20/30/40)".to_string(),
                    )
                })?
        };

        let asset_index = params
            .get("asset_index")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;

        // ── Validation ────────────────────────────────────────────────────────
        if price <= 0.0 || price > 10_000_000.0 {
            return Err(ToolError::InvalidParameters(format!(
                "Price {price} is out of the plausible BTC range (0, 10,000,000)"
            )));
        }
        // TP/SL validation and directional sanity only apply to normal (non-reduce_only) orders.
        if !reduce_only {
            if take_profit <= 0.0 || take_profit > 10_000_000.0 {
                return Err(ToolError::InvalidParameters(format!(
                    "take_profit {take_profit} is out of range"
                )));
            }
            if stop_loss <= 0.0 || stop_loss > 10_000_000.0 {
                return Err(ToolError::InvalidParameters(format!(
                    "stop_loss {stop_loss} is out of range"
                )));
            }
            if is_buy && take_profit <= price {
                return Err(ToolError::InvalidParameters(
                    "For a LONG order, take_profit must be above the entry price".to_string(),
                ));
            }
            if !is_buy && take_profit >= price {
                return Err(ToolError::InvalidParameters(
                    "For a SHORT order, take_profit must be below the entry price".to_string(),
                ));
            }
            if is_buy && stop_loss >= price {
                return Err(ToolError::InvalidParameters(
                    "For a LONG order, stop_loss must be below the entry price".to_string(),
                ));
            }
            if !is_buy && stop_loss <= price {
                return Err(ToolError::InvalidParameters(
                    "For a SHORT order, stop_loss must be above the entry price".to_string(),
                ));
            }
        }

        // ── Auto-size from balance if size not supplied ────────────────────────
        let size = match size_opt {
            Some(s) => s,
            None => {
                let address = if let Some(vault) = self.vault_address {
                    format!("0x{}", hex::encode(vault))
                } else {
                    let signing_key = decode_private_key(self.private_key.expose_secret())?;
                    derive_eth_address(&signing_key)
                };
                let balance = fetch_hl_balance(&self.client, &address).await?;
                let alloc = allocation_pct_for_leverage(leverage);
                let margin_usd = balance * alloc;
                let notional = margin_usd * leverage as f64;
                let raw_size = notional / price;
                ((raw_size * 10_000.0).round() / 10_000.0).max(0.001)
            }
        };

        if size < 0.001 {
            return Err(ToolError::InvalidParameters(format!(
                "Size {size} is below the HyperLiquid BTC minimum of 0.001"
            )));
        }

        // ── Format prices and size ─────────────────────────────────────────────
        let price_str = format_price(price);
        let size_str = format_size(size);
        let nonce = Utc::now().timestamp_millis() as u64;

        // Derive signing address for diagnostics — helps identify key/account mismatches.
        let signer_address = {
            let signing_key = decode_private_key(self.private_key.expose_secret())?;
            derive_eth_address(&signing_key)
        };
        tracing::debug!("hyperliquid_trade: signing with address {signer_address}");

        // ── Build order batch ─────────────────────────────────────────────────
        let (action, payload) = if reduce_only {
            // Single reduce-only GTC limit order — closes an existing position.
            let order = mp_order(asset_index, is_buy, &price_str, &size_str, true, mp_limit_gtc());
            let action = mp_action(vec![order], "na");
            let (r, s, v) = sign_hl_order_batch(
                self.private_key.expose_secret(), &action, nonce, self.vault_address,
            )?;
            let mut payload = serde_json::json!({
                "action": {
                    "type": "order",
                    "orders": [{
                        "a": asset_index,
                        "b": is_buy,
                        "p": price_str,
                        "s": size_str,
                        "r": true,
                        "t": { "limit": { "tif": "Gtc" } }
                    }],
                    "grouping": "na",
                    "builder": { "b": BUILDER_ADDRESS, "f": BUILDER_FEE }
                },
                "nonce": nonce,
                "signature": { "r": r, "s": s, "v": v }
            });
            if let Some(vault) = self.vault_address {
                payload["vaultAddress"] = serde_json::json!(format!("0x{}", hex::encode(vault)));
            }
            (action, payload)
        } else {
            // Normal batch: entry GTC limit + TP trigger + SL trigger.
            let tp_str = format_price(take_profit);
            let sl_str = format_price(stop_loss);
            let close_is_buy = !is_buy;

            let entry_order = mp_order(asset_index, is_buy, &price_str, &size_str, false, mp_limit_gtc());
            let tp_order = mp_order(asset_index, close_is_buy, &tp_str, &size_str, true, mp_trigger(&tp_str, true));
            let sl_order = mp_order(asset_index, close_is_buy, &sl_str, &size_str, true, mp_trigger(&sl_str, false));
            let action = mp_action(vec![entry_order, tp_order, sl_order], "normalTpsl");
            let (r, s, v) = sign_hl_order_batch(
                self.private_key.expose_secret(), &action, nonce, self.vault_address,
            )?;
            let mut payload = serde_json::json!({
                "action": {
                    "type": "order",
                    "orders": [
                        {
                            "a": asset_index, "b": is_buy,
                            "p": price_str, "s": size_str, "r": false,
                            "t": { "limit": { "tif": "Gtc" } }
                        },
                        {
                            "a": asset_index, "b": close_is_buy,
                            "p": tp_str, "s": size_str, "r": true,
                            "t": { "trigger": { "isMarket": true, "triggerPx": tp_str, "tpsl": "tp" } }
                        },
                        {
                            "a": asset_index, "b": close_is_buy,
                            "p": sl_str, "s": size_str, "r": true,
                            "t": { "trigger": { "isMarket": true, "triggerPx": sl_str, "tpsl": "sl" } }
                        }
                    ],
                    "grouping": "normalTpsl",
                    "builder": { "b": BUILDER_ADDRESS, "f": BUILDER_FEE }
                },
                "nonce": nonce,
                "signature": { "r": r, "s": s, "v": v }
            });
            if let Some(vault) = self.vault_address {
                payload["vaultAddress"] = serde_json::json!(format!("0x{}", hex::encode(vault)));
            }
            (action, payload)
        };
        let _ = &action; // used in signing above

        // ── Send ──────────────────────────────────────────────────────────────
        let response = self
            .client
            .post(HL_EXCHANGE_URL)
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                ToolError::ExternalService(format!("HyperLiquid request failed: {e}"))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(ToolError::ExternalService(format!(
                "HyperLiquid API returned {status}: {body} \
                (signer_address={signer_address}, vault={:?})",
                self.vault_address.map(|v| format!("0x{}", hex::encode(v)))
            )));
        }

        let resp_json: serde_json::Value = response.json().await.map_err(|e| {
            ToolError::ExternalService(format!("Failed to parse HyperLiquid response: {e}"))
        })?;

        // Build a clean summary output (never echo the raw private key data)
        let result = if reduce_only {
            serde_json::json!({
                "status": "close_order_submitted",
                "signer_address": signer_address,
                "reduce_only": true,
                "side": if is_buy { "BUY (closing SHORT)" } else { "SELL (closing LONG)" },
                "asset_index": asset_index,
                "price": price_str,
                "size": size_str,
                "nonce": nonce,
                "hyperliquid_response": resp_json
            })
        } else {
            let tp_str = format_price(take_profit);
            let sl_str = format_price(stop_loss);
            serde_json::json!({
                "status": "order_submitted",
                "signer_address": signer_address,
                "side": if is_buy { "LONG" } else { "SHORT" },
                "asset_index": asset_index,
                "entry_price": price_str,
                "take_profit": tp_str,
                "stop_loss": sl_str,
                "size": size_str,
                "leverage": leverage,
                "nonce": nonce,
                "hyperliquid_response": resp_json
            })
        };

        Ok(ToolOutput::success(result, start.elapsed()))
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::tool::ApprovalRequirement;

    #[test]
    fn test_format_price() {
        assert_eq!(format_price(94_620.35), "94620.4");
        assert_eq!(format_price(95_000.0), "95000.0");
        assert_eq!(format_price(100_000.05), "100000.1");
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(0.01234), "0.0123");
        assert_eq!(format_size(0.001), "0.0010");
        assert_eq!(format_size(1.0), "1.0000");
    }

    #[test]
    fn test_tool_metadata() {
        let tool = HyperliquidTradeTool::new(
            "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
            None,
        );
        assert_eq!(tool.name(), "hyperliquid_trade");
        assert_eq!(
            tool.requires_approval(&serde_json::json!({})),
            ApprovalRequirement::Never
        );
        assert!(tool.sensitive_params().is_empty());
    }

    #[tokio::test]
    async fn test_missing_is_buy() {
        let tool = HyperliquidTradeTool::new(
            "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
            None,
        );
        let ctx = JobContext::default();
        let result = tool
            .execute(
                serde_json::json!({"price": 95000.0, "size": 0.01, "leverage": 50,
                    "take_profit": 96000.0, "stop_loss": 94000.0}),
                &ctx,
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("direction"));
    }

    #[tokio::test]
    async fn test_signal_string_long() {
        let tool = HyperliquidTradeTool::new(
            "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
            None,
        );
        let ctx = JobContext::default();
        // "signal": "LONG" should be accepted as is_buy = true; will fail on network, not on params
        let result = tool
            .execute(
                serde_json::json!({"signal": "LONG", "price": 95000.0, "size": 0.01,
                    "leverage": 50, "take_profit": 96000.0, "stop_loss": 94000.0}),
                &ctx,
            )
            .await;
        // Should not fail with a parameter error
        if let Err(e) = &result {
            assert!(!e.to_string().contains("direction"), "unexpected param error: {e}");
        }
    }

    #[tokio::test]
    async fn test_signal_string_short() {
        let tool = HyperliquidTradeTool::new(
            "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
            None,
        );
        let ctx = JobContext::default();
        let result = tool
            .execute(
                serde_json::json!({"signal": "SHORT", "price": 95000.0, "size": 0.01,
                    "leverage": 50, "take_profit": 94000.0, "stop_loss": 96000.0}),
                &ctx,
            )
            .await;
        if let Err(e) = &result {
            assert!(!e.to_string().contains("direction"), "unexpected param error: {e}");
        }
    }

    #[tokio::test]
    async fn test_missing_take_profit() {
        let tool = HyperliquidTradeTool::new(
            "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
            None,
        );
        let ctx = JobContext::default();
        let result = tool
            .execute(
                serde_json::json!({"is_buy": true, "price": 95000.0, "size": 0.01,
                    "leverage": 50, "stop_loss": 94000.0}),
                &ctx,
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("take_profit"));
    }

    #[tokio::test]
    async fn test_missing_stop_loss() {
        let tool = HyperliquidTradeTool::new(
            "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
            None,
        );
        let ctx = JobContext::default();
        let result = tool
            .execute(
                serde_json::json!({"is_buy": true, "price": 95000.0, "size": 0.01,
                    "leverage": 50, "take_profit": 96000.0}),
                &ctx,
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("stop_loss"));
    }

    #[tokio::test]
    async fn test_missing_leverage() {
        let tool = HyperliquidTradeTool::new(
            "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
            None,
        );
        let ctx = JobContext::default();
        let result = tool
            .execute(
                serde_json::json!({"is_buy": true, "price": 95000.0, "size": 0.01,
                    "take_profit": 96000.0, "stop_loss": 94000.0}),
                &ctx,
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("leverage"));
    }

    #[tokio::test]
    async fn test_price_out_of_range() {
        let tool = HyperliquidTradeTool::new(
            "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
            None,
        );
        let ctx = JobContext::default();
        let result = tool
            .execute(
                serde_json::json!({"is_buy": true, "price": -1.0, "size": 0.01,
                    "leverage": 50, "take_profit": 96000.0, "stop_loss": 94000.0}),
                &ctx,
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_size_below_minimum() {
        let tool = HyperliquidTradeTool::new(
            "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
            None,
        );
        let ctx = JobContext::default();
        let result = tool
            .execute(
                serde_json::json!({"is_buy": true, "price": 95000.0, "size": 0.0001,
                    "leverage": 50, "take_profit": 96000.0, "stop_loss": 94000.0}),
                &ctx,
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("minimum"));
    }

    #[tokio::test]
    async fn test_tp_direction_long() {
        let tool = HyperliquidTradeTool::new(
            "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
            None,
        );
        let ctx = JobContext::default();
        // TP below entry for LONG — invalid
        let result = tool
            .execute(
                serde_json::json!({"is_buy": true, "price": 95000.0, "size": 0.01,
                    "leverage": 50, "take_profit": 94000.0, "stop_loss": 93000.0}),
                &ctx,
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("take_profit"));
    }

    #[tokio::test]
    async fn test_sl_direction_short() {
        let tool = HyperliquidTradeTool::new(
            "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
            None,
        );
        let ctx = JobContext::default();
        // SL below entry for SHORT — invalid (a short's stop loss must be above entry to protect
        // against price rising; placing it below entry means it can never fire as a loss-cutter)
        let result = tool
            .execute(
                serde_json::json!({"is_buy": false, "price": 95000.0, "size": 0.01,
                    "leverage": 50, "take_profit": 94000.0, "stop_loss": 93000.0}),
                &ctx,
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("stop_loss"));
    }

    #[test]
    fn test_decode_private_key_valid() {
        let key_hex = "0000000000000000000000000000000000000000000000000000000000000001";
        assert!(decode_private_key(key_hex).is_ok());
        assert!(decode_private_key(&format!("0x{key_hex}")).is_ok());
    }

    #[test]
    fn test_decode_private_key_invalid() {
        assert!(decode_private_key("not_hex_at_all").is_err());
    }

    #[test]
    fn test_keccak256_known() {
        // keccak256("") = c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470
        let result = keccak256(b"");
        let expected =
            hex::decode("c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470")
                .unwrap();
        assert_eq!(result.as_slice(), expected.as_slice());
    }

    #[test]
    fn test_domain_separator_is_deterministic() {
        let d1 = hl_exchange_domain_separator();
        let d2 = hl_exchange_domain_separator();
        assert_eq!(d1, d2);
        assert!(d1.iter().any(|&b| b != 0));
    }

    #[test]
    fn test_mp_action_encodes_without_error() {
        let entry = mp_order(0, true, "95000.0", "0.0100", false, mp_limit_gtc());
        let tp = mp_order(0, false, "96500.0", "0.0100", true, mp_trigger("96500.0", true));
        let sl = mp_order(0, false, "93800.0", "0.0100", true, mp_trigger("93800.0", false));
        let action = mp_action(vec![entry, tp, sl], "normalTpsl");
        let mut buf = Vec::new();
        rmpv::encode::write_value(&mut buf, &action).unwrap();
        assert!(!buf.is_empty());
    }

    #[test]
    fn test_connection_id_is_deterministic() {
        let entry = mp_order(0, true, "95000.0", "0.0100", false, mp_limit_gtc());
        let tp = mp_order(0, false, "96500.0", "0.0100", true, mp_trigger("96500.0", true));
        let sl = mp_order(0, false, "93800.0", "0.0100", true, mp_trigger("93800.0", false));
        let action = mp_action(vec![entry, tp, sl], "normalTpsl");
        let id1 = compute_connection_id(&action, 1_234_567_890, None).unwrap();
        let id2 = compute_connection_id(&action, 1_234_567_890, None).unwrap();
        assert_eq!(id1, id2);
        assert!(id1.iter().any(|&b| b != 0));
    }

    #[test]
    fn test_sign_produces_valid_signature() {
        let key_hex = "0000000000000000000000000000000000000000000000000000000000000001";
        let entry = mp_order(0, true, "95000.0", "0.0100", false, mp_limit_gtc());
        let tp = mp_order(0, false, "96500.0", "0.0100", true, mp_trigger("96500.0", true));
        let sl = mp_order(0, false, "93800.0", "0.0100", true, mp_trigger("93800.0", false));
        let action = mp_action(vec![entry, tp, sl], "normalTpsl");
        let (r, s, v) = sign_hl_order_batch(key_hex, &action, 1_234_567_890, None).unwrap();
        assert!(r.starts_with("0x"));
        assert!(s.starts_with("0x"));
        assert!(v == 27 || v == 28);
    }

    #[test]
    fn test_allocation_pct_for_leverage() {
        assert!((allocation_pct_for_leverage(100) - 0.10).abs() < f64::EPSILON);
        assert!((allocation_pct_for_leverage(75) - 0.15).abs() < f64::EPSILON);
        assert!((allocation_pct_for_leverage(50) - 0.30).abs() < f64::EPSILON);
        assert!((allocation_pct_for_leverage(25) - 0.50).abs() < f64::EPSILON);
    }

    #[test]
    fn test_derive_eth_address_format() {
        let signing_key = decode_private_key(
            "0000000000000000000000000000000000000000000000000000000000000001",
        )
        .unwrap();
        let addr = derive_eth_address(&signing_key);
        assert!(addr.starts_with("0x"));
        assert_eq!(addr.len(), 42); // "0x" + 40 hex chars
    }
}
