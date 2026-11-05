//! Logins are mailed by the Node module named in `[pass]`. It keeps its own
//! SMTP accounts, so this process only hands it a message.

use std::{
   process::Stdio,
   time::Duration,
};

use tokio::{
   io::AsyncWriteExt as _,
   process::Command,
};

use crate::config::PassConfig;

/// Run with `node -e`. Reads one message as JSON on stdin.
const SEND: &str = r#"
const chunks = [];
process.stdin.on("data", (chunk) => chunks.push(chunk));
process.stdin.on("end", async () => {
  try {
    const job = JSON.parse(Buffer.concat(chunks).toString("utf8"));
    const send = require(job.mailer)[job.export];
    if (typeof send !== "function") {
      throw new Error(`the mailer has no ${job.export} export`);
    }
    const log = (line) => console.error(line);
    await send(job.accounts, { to: job.to, subject: job.subject, text: job.text }, log);
    process.exit(0);
  } catch (err) {
    console.error(err && err.message ? err.message : String(err));
    process.exit(1);
  }
});
"#;

/// A sender may try several accounts, each with its own timeouts.
const SEND_TIMEOUT: Duration = Duration::from_secs(180);

pub const fn enabled(config: &PassConfig) -> bool {
   !config.mailer.is_empty() && !config.mailer_export.is_empty()
}

/// Queue a message and return. The outcome only reaches the log.
pub fn send(config: &PassConfig, to: &str, subject: &str, text: &str) {
   if !enabled(config) {
      return;
   }
   let job = serde_json::json!({
      "mailer": config.mailer,
      "export": config.mailer_export,
      "accounts": config.mail_accounts,
      "to": to,
      "subject": subject,
      "text": text,
   })
   .to_string();
   tokio::spawn(async move {
      match tokio::time::timeout(SEND_TIMEOUT, run(job)).await {
         Ok(Ok(())) => tracing::info!("search pass login emailed"),
         Ok(Err(err)) => tracing::warn!("search pass email failed: {err}"),
         Err(_) => tracing::warn!("search pass email timed out"),
      }
   });
}

async fn run(job: String) -> Result<(), String> {
   let mut child = Command::new("node")
      .arg("-e")
      .arg(SEND)
      .stdin(Stdio::piped())
      .stdout(Stdio::null())
      .stderr(Stdio::piped())
      .kill_on_drop(true)
      .spawn()
      .map_err(|err| format!("node did not start: {err}"))?;
   if let Some(mut stdin) = child.stdin.take() {
      stdin
         .write_all(job.as_bytes())
         .await
         .map_err(|err| format!("node stdin: {err}"))?;
   }
   let output = child
      .wait_with_output()
      .await
      .map_err(|err| format!("node: {err}"))?;
   if output.status.success() {
      return Ok(());
   }
   let stderr = String::from_utf8_lossy(&output.stderr);
   Err(stderr.lines().last().unwrap_or("no output").to_owned())
}
