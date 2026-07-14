//! Minimal Soroban RPC client used to verify the on-chain version of the
//! deployed `ttl_vault` contract at backend startup.
//!
//! This deliberately does *not* depend on the `soroban-client` crate: that
//! crate pulls in `reqwest` 0.11 with its default (native-tls/OpenSSL)
//! backend, which would require system OpenSSL headers that the production
//! Docker image doesn't otherwise install (this backend only links `reqwest`
//! 0.12 with `rustls-tls` everywhere else, see `Cargo.toml`). Instead, this
//! module hand-builds a single-operation, read-only `simulateTransaction`
//! request using `stellar-xdr` directly and posts it with the same `reqwest`
//! client used elsewhere in the backend.
//!
//! Read-only contract calls don't require a funded/real source account:
//! Soroban RPC's `simulateTransaction` doesn't validate the source account's
//! existence or the transaction's signatures (see
//! <https://developers.stellar.org/docs/data/apis/rpc/api-reference/methods/simulateTransaction>,
//! "This method can also be used to invoke read-only smart contract
//! functions for free"), so a throwaway random Ed25519 public key with
//! sequence number 0 is sufficient to build a structurally valid envelope.

use std::str::FromStr;
use std::time::Duration;

use rand::RngCore;
use serde::Deserialize;
use stellar_xdr::curr::{
    Hash, HostFunction, InvokeContractArgs, InvokeHostFunctionOp, Limits, Memo, MuxedAccount,
    Operation, OperationBody, Preconditions, ReadXdr, ScAddress, ScSymbol, ScVal, SequenceNumber,
    StringM, Transaction, TransactionEnvelope, TransactionExt, TransactionV1Envelope, Uint256,
    VecM, WriteXdr,
};

/// Contract method that reports the deployed contract's version.
/// See `contracts/ttl_vault/src/lib.rs::get_contract_version`.
const GET_CONTRACT_VERSION_METHOD: &str = "get_contract_version";

/// How long to wait for the Soroban RPC endpoint to respond before giving up.
const RPC_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize)]
struct JsonRpcEnvelope<T> {
    #[serde(default)]
    result: Option<T>,
    #[serde(default)]
    error: Option<JsonRpcErrorObject>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcErrorObject {
    code: i64,
    message: String,
}

#[derive(Debug, Default, Deserialize)]
struct SimulateTransactionResult {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    results: Option<Vec<SimulateHostFunctionResult>>,
}

#[derive(Debug, Deserialize)]
struct SimulateHostFunctionResult {
    xdr: String,
}

/// Queries the deployed `ttl_vault` contract's `get_contract_version()`
/// method over Soroban RPC and returns the major version component as a
/// `u32`, for comparison against `MIN_CONTRACT_VERSION`.
///
/// Never panics: all RPC, XDR, and parsing failures are surfaced as `Err`
/// so the caller (`check_contract_version`) can decide how to react.
pub async fn get_deployed_contract_version(
    rpc_url: &str,
    contract_id: &str,
) -> Result<u32, String> {
    let tx_xdr = build_simulate_request_xdr(contract_id, GET_CONTRACT_VERSION_METHOD)?;

    let client = reqwest::Client::builder()
        .timeout(RPC_TIMEOUT)
        .build()
        .map_err(|e| format!("failed to build Soroban RPC HTTP client: {e}"))?;

    let sc_val = simulate_transaction(&client, rpc_url, &tx_xdr).await?;
    parse_version_scval(&sc_val)
}

/// Builds the base64-encoded XDR of an unsigned `TransactionEnvelope`
/// containing a single `InvokeHostFunction` operation that calls `method`
/// on `contract_id` with no arguments.
fn build_simulate_request_xdr(contract_id: &str, method: &str) -> Result<String, String> {
    let contract_bytes = stellar_strkey::Contract::from_string(contract_id)
        .map(|c| c.0)
        .map_err(|e| format!("invalid contract id '{contract_id}': {e}"))?;

    let function_name = ScSymbol(
        StringM::from_str(method)
            .map_err(|e| format!("invalid contract method name '{method}': {e}"))?,
    );

    let invoke_args = InvokeContractArgs {
        contract_address: ScAddress::Contract(Hash(contract_bytes)),
        function_name,
        args: VecM::default(),
    };

    let op = Operation {
        source_account: None,
        body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
            host_function: HostFunction::InvokeContract(invoke_args),
            auth: VecM::default(),
        }),
    };

    // Throwaway source account: simulateTransaction doesn't check that this
    // account exists or is signed for, it only cares about the invoked
    // contract function's result.
    let mut source_key = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut source_key);

    let tx = Transaction {
        source_account: MuxedAccount::Ed25519(Uint256(source_key)),
        fee: 100,
        seq_num: SequenceNumber(0),
        cond: Preconditions::None,
        memo: Memo::None,
        operations: vec![op]
            .try_into()
            .map_err(|e| format!("failed to build transaction operations: {e}"))?,
        ext: TransactionExt::V0,
    };

    let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    });

    envelope
        .to_xdr_base64(Limits::none())
        .map_err(|e| format!("failed to encode transaction XDR: {e}"))
}

/// Posts a `simulateTransaction` JSON-RPC request and returns the decoded
/// `ScVal` the contract call returned.
async fn simulate_transaction(
    client: &reqwest::Client,
    rpc_url: &str,
    tx_xdr: &str,
) -> Result<ScVal, String> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "simulateTransaction",
        "params": { "transaction": tx_xdr },
    });

    let response = client
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("Soroban RPC request to {rpc_url} failed: {e}"))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("failed to read Soroban RPC response body: {e}"))?;

    if !status.is_success() {
        return Err(format!(
            "Soroban RPC at {rpc_url} returned HTTP {status}: {text}"
        ));
    }

    let envelope: JsonRpcEnvelope<SimulateTransactionResult> = serde_json::from_str(&text)
        .map_err(|e| format!("malformed Soroban RPC response: {e} (body: {text})"))?;

    if let Some(err) = envelope.error {
        return Err(format!("Soroban RPC error {}: {}", err.code, err.message));
    }

    let result = envelope
        .result
        .ok_or_else(|| "Soroban RPC response missing 'result'".to_string())?;

    if let Some(sim_error) = result.error {
        return Err(format!("contract simulation failed: {sim_error}"));
    }

    let return_xdr = result
        .results
        .as_ref()
        .and_then(|r| r.first())
        .map(|r| r.xdr.as_str())
        .ok_or_else(|| "Soroban RPC simulation returned no results".to_string())?;

    ScVal::from_xdr_base64(return_xdr, Limits::none())
        .map_err(|e| format!("failed to decode contract return value XDR: {e}"))
}

/// Interprets the `ScVal` returned by `get_contract_version()` as a `u32`.
///
/// The contract currently reports its version as a semver-ish `String`
/// (e.g. `"1.0.0"`, see `contracts/ttl_vault/src/lib.rs`); we take the major
/// component. A raw `ScVal::U32` is also accepted so this keeps working if
/// the contract's return type is ever simplified.
fn parse_version_scval(val: &ScVal) -> Result<u32, String> {
    let version_str = match val {
        ScVal::U32(v) => return Ok(*v),
        ScVal::String(s) => String::try_from(&s.0)
            .map_err(|e| format!("contract version value is not valid UTF-8: {e}"))?,
        other => {
            return Err(format!(
                "unexpected return type from get_contract_version: {other:?}"
            ))
        }
    };

    let major = version_str.split('.').next().unwrap_or(&version_str).trim();

    major.parse::<u32>().map_err(|e| {
        format!("could not parse contract version '{version_str}' as a version number: {e}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_version_scval(val: &ScVal) -> String {
        val.to_xdr_base64(Limits::none()).unwrap()
    }

    // --- parse_version_scval ---

    #[test]
    fn parses_semver_string_to_major_version() {
        let val = ScVal::String(StringM::from_str("1.0.0").unwrap().into());
        assert_eq!(parse_version_scval(&val), Ok(1));
    }

    #[test]
    fn parses_multi_digit_major_version() {
        let val = ScVal::String(StringM::from_str("12.3.4").unwrap().into());
        assert_eq!(parse_version_scval(&val), Ok(12));
    }

    #[test]
    fn parses_bare_u32_return_value() {
        let val = ScVal::U32(7);
        assert_eq!(parse_version_scval(&val), Ok(7));
    }

    #[test]
    fn rejects_non_numeric_version_string() {
        let val = ScVal::String(StringM::from_str("not-a-version").unwrap().into());
        let result = parse_version_scval(&val);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("could not parse"));
    }

    #[test]
    fn rejects_unexpected_scval_type() {
        let val = ScVal::Bool(true);
        let result = parse_version_scval(&val);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("unexpected return type"));
    }

    // --- build_simulate_request_xdr ---

    #[test]
    fn builds_valid_envelope_for_valid_contract_id() {
        let contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";
        let xdr = build_simulate_request_xdr(contract_id, "get_contract_version")
            .expect("should build a valid envelope");

        // Round-trip: decode it back and check the invoked function/contract.
        let envelope = TransactionEnvelope::from_xdr_base64(&xdr, Limits::none()).unwrap();
        let TransactionEnvelope::Tx(tx_envelope) = envelope else {
            panic!("expected a V1 transaction envelope");
        };
        assert!(tx_envelope.signatures.is_empty());
        let op = &tx_envelope.tx.operations[0];
        match &op.body {
            OperationBody::InvokeHostFunction(invoke_op) => match &invoke_op.host_function {
                HostFunction::InvokeContract(args) => {
                    assert_eq!(args.function_name.0.to_string(), "get_contract_version");
                    assert!(args.args.is_empty());
                }
                _ => panic!("expected InvokeContract host function"),
            },
            _ => panic!("expected InvokeHostFunction operation"),
        }
    }

    #[test]
    fn rejects_malformed_contract_id() {
        let result = build_simulate_request_xdr("not-a-contract-id", "get_contract_version");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid contract id"));
    }

    // --- end-to-end against a mocked Soroban RPC endpoint ---

    #[tokio::test]
    async fn get_deployed_contract_version_parses_successful_simulation() {
        let mut server = mockito::Server::new_async().await;
        let return_xdr =
            encode_version_scval(&ScVal::String(StringM::from_str("3.2.1").unwrap().into()));

        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {
                        "latestLedger": 12345,
                        "results": [{ "auth": [], "xdr": return_xdr }],
                    }
                })
                .to_string(),
            )
            .create_async()
            .await;

        let contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";
        let result = get_deployed_contract_version(&server.url(), contract_id).await;

        mock.assert_async().await;
        assert_eq!(result, Ok(3));
    }

    #[tokio::test]
    async fn get_deployed_contract_version_surfaces_simulation_error() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "result": {
                        "latestLedger": 12345,
                        "error": "HostError: Error(Contract, #48)"
                    }
                })
                .to_string(),
            )
            .create_async()
            .await;

        let contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";
        let result = get_deployed_contract_version(&server.url(), contract_id).await;

        mock.assert_async().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("contract simulation failed"));
    }

    #[tokio::test]
    async fn get_deployed_contract_version_surfaces_rpc_error() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "error": { "code": -32602, "message": "Invalid params" }
                })
                .to_string(),
            )
            .create_async()
            .await;

        let contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";
        let result = get_deployed_contract_version(&server.url(), contract_id).await;

        mock.assert_async().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid params"));
    }

    #[tokio::test]
    async fn get_deployed_contract_version_surfaces_malformed_response() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("not json")
            .create_async()
            .await;

        let contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";
        let result = get_deployed_contract_version(&server.url(), contract_id).await;

        mock.assert_async().await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .contains("malformed Soroban RPC response"));
    }

    #[tokio::test]
    async fn get_deployed_contract_version_surfaces_http_error_status() {
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("POST", "/")
            .with_status(503)
            .with_body("service unavailable")
            .create_async()
            .await;

        let contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";
        let result = get_deployed_contract_version(&server.url(), contract_id).await;

        mock.assert_async().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("HTTP 503"));
    }

    #[tokio::test]
    async fn get_deployed_contract_version_rejects_invalid_contract_id_without_network_call() {
        let result = get_deployed_contract_version(
            "http://127.0.0.1:1", // unroutable; would fail fast if actually dialed
            "not-a-real-contract-id",
        )
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid contract id"));
    }
}
