//! Codex CLI 0.153.4 / openai.chatgpt 26.5901.22334 native IPC envelopes.
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const REQUEST_BUDGET: Duration = Duration::from_secs(4);
pub const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

pub async fn read_frame(stream: &mut (impl AsyncRead + Unpin)) -> Result<Value> {
    // Idle registered clients may remain quiet indefinitely. Once a frame starts,
    // its remaining header and body must arrive within the request budget.
    let first = stream.read_u8().await?;
    tokio::time::timeout(REQUEST_BUDGET, async {
        let mut header = [first, 0, 0, 0];
        stream.read_exact(&mut header[1..]).await?;
        let length = u32::from_le_bytes(header) as usize;
        if length == 0 || length > MAX_FRAME_BYTES {
            bail!("invalid IPC frame length");
        }
        // Grow only as bytes arrive; an announced 256 MiB frame must not allocate
        // 256 MiB before its sender supplies any data.
        let mut payload = Vec::new();
        let mut limited = stream.take(length as u64);
        limited.read_to_end(&mut payload).await?;
        if payload.len() != length {
            bail!("truncated IPC frame");
        }
        let value: Value = serde_json::from_slice(&payload)?;
        if !value.is_object() || !value["type"].is_string() {
            bail!("invalid IPC envelope");
        }
        Ok(value)
    })
    .await?
}

pub async fn write_frame(stream: &mut (impl AsyncWrite + Unpin), value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_FRAME_BYTES {
        bail!("IPC frame too large");
    }
    tokio::time::timeout(REQUEST_BUDGET, async {
        stream
            .write_all(&(bytes.len() as u32).to_le_bytes())
            .await?;
        stream.write_all(&bytes).await?;
        stream.flush().await
    })
    .await??;
    Ok(())
}

pub fn error(request: &Value, error: &str) -> Value {
    json!({"type":"response", "requestId":request["requestId"], "resultType":"error", "error":error})
}

pub fn success(request: &Value, client: &str, result: Value) -> Value {
    json!({"type":"response", "requestId":request["requestId"], "resultType":"success",
        "method":request["method"], "handledByClientId":client, "result":result})
}

pub fn context_request(request: &Value) -> bool {
    request["method"] == "ide-context"
        && request.get("version").is_none_or(|v| v == 0)
        && request.get("hostId").is_none_or(Value::is_null)
        && request["params"]["workspaceRoot"].is_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn fragmented_frames_and_coalesced_frames() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/codex/exchanges.json"))
                .unwrap();
        let request = fixture["request"].clone();
        let bytes = serde_json::to_vec(&request).unwrap();
        let (mut tx, mut rx) = tokio::io::duplex(8);
        let writer = tokio::spawn(async move {
            for byte in (bytes.len() as u32).to_le_bytes().into_iter().chain(bytes) {
                tx.write_all(&[byte]).await.unwrap();
            }
            write_frame(&mut tx, &json!({"type":"broadcast"}))
                .await
                .unwrap();
        });
        assert_eq!(read_frame(&mut rx).await.unwrap(), request);
        assert_eq!(read_frame(&mut rx).await.unwrap()["type"], "broadcast");
        writer.await.unwrap();
    }
    #[tokio::test]
    async fn rejects_invalid_frames() {
        for bytes in [
            vec![0, 0, 0, 0],
            vec![255, 255, 255, 255],
            vec![1, 0, 0, 0, b'{'],
            vec![5, 0, 0, 0, b'{'],
        ] {
            assert!(read_frame(&mut bytes.as_slice()).await.is_err());
        }
    }
    #[test]
    fn pinned_success_envelope() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/codex/exchanges.json"))
                .unwrap();
        assert_eq!(
            success(
                &fixture["request"],
                "zed-provider",
                fixture["response"]["result"].clone()
            ),
            fixture["response"]
        );
        assert!(context_request(&fixture["request"]));
        let mut future = fixture["request"].clone();
        future["version"] = json!(1);
        assert!(!context_request(&future));
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    #[tokio::test]
    async fn partial_frame_has_a_deadline() {
        let (mut tx, mut rx) = tokio::io::duplex(8);
        tx.write_all(&[10]).await.unwrap();
        // Keep the sender alive: this must fail by deadline, not EOF.
        let result =
            tokio::time::timeout(REQUEST_BUDGET + Duration::from_secs(1), read_frame(&mut rx))
                .await;
        assert!(result.expect("frame deadline was not enforced").is_err());
        drop(tx);
    }
}
