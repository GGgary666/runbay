use crate::adapters::{AdapterLaunchContext, PreparedLaunch, RuntimeAdapter};
use crate::models::{AdapterCapabilities, CapabilityTier};
use anyhow::{bail, Context, Result};
use runbay_sandbox::SandboxRunOptions;
use std::path::PathBuf;

const MODEL: &str = "deepseek-flash";
const NOTE_PATH: &str = "paper-note.md";
const QUESTION_ENV: &str = "RUNBAY_COWIKI_QUESTION";

pub struct CowikiAdapter {
    self_binary: PathBuf,
}

impl CowikiAdapter {
    pub fn new() -> Result<Self> {
        Ok(Self {
            self_binary: std::env::current_exe()?,
        })
    }
}

impl RuntimeAdapter for CowikiAdapter {
    fn name(&self) -> &'static str {
        "cowiki"
    }

    fn capabilities(&self) -> AdapterCapabilities {
        let linux = cfg!(target_os = "linux");
        AdapterCapabilities {
            runtime: self.name().to_string(),
            tier: CapabilityTier::OneShot,
            supports_streaming: false,
            supports_sandbox: linux,
            supports_gateway: linux,
            supports_native_resume: false,
            supports_hot_process: false,
            available: linux,
            binary_path: Some(self.self_binary.to_string_lossy().to_string()),
            notes: format!(
                "Linux one-shot Q&A over {NOTE_PATH} with {MODEL} through the Unix socket gateway"
            ),
        }
    }

    fn prepare_launch(&self, ctx: &AdapterLaunchContext) -> Result<PreparedLaunch> {
        if !cfg!(target_os = "linux") {
            bail!("ADAPTER_UNAVAILABLE: cowiki requires the Linux Unix socket gateway");
        }
        let socket = ctx
            .gateway_unix_socket
            .as_ref()
            .context("cowiki requires a bound Unix socket gateway")?;
        let token = ctx
            .gateway_run_token
            .as_ref()
            .context("cowiki requires a gateway run token")?;

        Ok(PreparedLaunch {
            command: vec![
                self.self_binary.to_string_lossy().into_owned(),
                "__internal-cowiki-worker".to_string(),
            ],
            sandbox_opts: SandboxRunOptions {
                allow_network: false,
                gateway_unix_socket: Some(socket.clone()),
                env_overrides: [
                    (QUESTION_ENV.to_string(), ctx.prompt.clone()),
                    (
                        "RUNBAY_GATEWAY_SOCKET".to_string(),
                        socket.to_string_lossy().into_owned(),
                    ),
                    ("RUNBAY_RUN_TOKEN".to_string(), token.clone()),
                ]
                .into(),
                ..Default::default()
            },
            network_isolated: true,
            secretless_gateway: true,
            gateway_transport: Some("linux_bwrap_unshare_net_unix_socket".to_string()),
        })
    }
}

#[cfg(target_os = "linux")]
pub async fn run_worker() -> Result<()> {
    let question = std::env::var(QUESTION_ENV).context("missing CoWiki question")?;
    let socket = std::env::var_os("RUNBAY_GATEWAY_SOCKET").context("missing gateway socket")?;
    let token = std::env::var("RUNBAY_RUN_TOKEN").context("missing gateway run token")?;
    let note_path = runbay_mvcc::safe_resolve_under_root(&std::env::current_dir()?, NOTE_PATH)?;
    let note = std::fs::read_to_string(note_path).context("failed to read paper-note.md")?;
    if note.trim().is_empty() {
        bail!("paper-note.md is empty");
    }

    let response = reqwest::Client::builder()
        .unix_socket(PathBuf::from(socket))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(std::time::Duration::from_secs(75))
        .build()?
        .post("http://localhost/v1/chat/completions")
        .bearer_auth(token)
        .json(&serde_json::json!({
            "model": MODEL,
            "stream": false,
            "reasoning_effort": "none",
            "max_tokens": 192,
            "messages": [
                {
                    "role": "system",
                    "content": "Answer concisely in the question's language using only facts in the supplied CoWiki note. If the answer is absent from the note, say so.",
                },
                {
                    "role": "user",
                    "content": format!("Question:\n{question}\n\nCoWiki note ({NOTE_PATH}):\n{note}"),
                },
            ],
        }))
        .send()
        .await
        .context("CoWiki model request failed")?;
    let status = response.status();
    if !status.is_success() {
        bail!("CoWiki model gateway returned HTTP {status}");
    }
    let completion: serde_json::Value = response.json().await.context("invalid model response")?;
    let answer = completion["choices"][0]["message"]["content"]
        .as_str()
        .map(str::trim)
        .filter(|answer| !answer.is_empty())
        .context("model response contained no answer")?;

    println!(
        "{}",
        serde_json::json!({
            "type": "cowiki.answer",
            "answer": answer,
            "requested_model": MODEL,
            "model": completion["model"].as_str(),
            "http_status": status.as_u16(),
            "sources": [{"path": NOTE_PATH, "start_line": 1, "end_line": note.lines().count()}],
        })
    );
    Ok(())
}
