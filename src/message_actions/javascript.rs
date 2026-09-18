use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail, ensure};
use rquickjs::{CatchResultExt as _, Context, Function, Runtime, Value};
use serde_json::json;
use tokio::sync::Semaphore;

use super::{ActionContext, ImageResult, MessageActionOutcome};

const EXECUTION_LIMIT: Duration = Duration::from_secs(1);
const MEMORY_LIMIT: usize = 64 * 1024 * 1024;
const MAX_CODE_BYTES: usize = 64 * 1024;
static SCRIPT_SLOTS: Semaphore = Semaphore::const_new(4);

pub(super) async fn execute(
    context: Arc<ActionContext>,
    code: String,
    reason: String,
    role_id: Option<i64>,
) -> Result<MessageActionOutcome> {
    execute_with_input(code, reason, role_id, move || {
        Ok(serde_json::to_vec(&message_values(&context))?)
    })
    .await
}

pub(crate) async fn execute_with_input<F>(
    code: String,
    reason: String,
    role_id: Option<i64>,
    input: F,
) -> Result<MessageActionOutcome>
where
    F: FnOnce() -> Result<Vec<u8>> + Send + 'static,
{
    run_worker(code, role_id, move || Ok(Some((input()?, reason)))).await
}

/// Check a rule's syntax and synchronous, single-argument function shape in a
/// bounded runtime. Does not call its body or prove its behavior for every input.
pub async fn validate(code: String) -> Result<()> {
    run_worker(code, None, || Ok(None)).await.map(|_| ())
}

async fn run_worker<F>(code: String, role_id: Option<i64>, input: F) -> Result<MessageActionOutcome>
where
    F: FnOnce() -> Result<Option<(Vec<u8>, String)>> + Send + 'static,
{
    ensure!(
        code.len() <= MAX_CODE_BYTES,
        "JavaScript rule exceeds 64 KiB"
    );
    let permit = SCRIPT_SLOTS.acquire().await?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let _cancel_on_drop = CancelOnDrop(Arc::clone(&cancelled));
    // QuickJS stays entirely on a blocking thread; it cannot stall Tokio's
    // gateway, storage, or other action tasks. Hold the slot until it really exits.
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        run(input()?, &code, role_id, cancelled)
    })
    .await
    .context("JavaScript worker failed")?
}

fn run(
    input: Option<(Vec<u8>, String)>,
    code: &str,
    role_id: Option<i64>,
    cancelled: Arc<AtomicBool>,
) -> Result<MessageActionOutcome> {
    let deadline = Instant::now() + EXECUTION_LIMIT;
    let runtime = Runtime::new().context("failed to create JavaScript runtime")?;
    runtime.set_memory_limit(MEMORY_LIMIT);
    runtime.set_max_stack_size(256 * 1024);
    runtime.set_interrupt_handler(Some(Box::new({
        let cancelled = Arc::clone(&cancelled);
        move || cancelled.load(Ordering::Relaxed) || Instant::now() >= deadline
    })));
    // No module loader or host bindings: scripts cannot access the filesystem,
    // network, environment, database, or API clients.
    let js = Context::full(&runtime).context("failed to create JavaScript context")?;
    let code = code.trim().trim_end_matches(';');
    let result = js.with(|ctx| -> Result<MessageActionOutcome> {
        // Capture intrinsics before evaluating user source so it cannot replace
        // the checks. Async and generator functions have different prototypes.
        let validate: Function = ctx.eval(
            "(() => { const getProto = Object.getPrototypeOf; const proto = Function.prototype;
                return fn => getProto(fn) === proto && fn.length === 1; })()",
        )?;
        let function: Function = ctx
            .eval(format!("({code}\n)"))
            .catch(&ctx)
            .map_err(|error| anyhow::anyhow!("invalid JavaScript function: {error}"))?;
        let Some((input, reason)) = input else {
            ensure!(
                validate
                    .call::<_, bool>((function,))
                    .catch(&ctx)
                    .map_err(|error| anyhow::anyhow!("invalid JavaScript function: {error}"))?,
                "JavaScript rule must be a synchronous function taking one argument"
            );
            return Ok(MessageActionOutcome::Ignore);
        };
        // Parse data separately, never interpolate Discord content into source.
        let messages = ctx
            .json_parse(input)
            .catch(&ctx)
            .map_err(|error| anyhow::anyhow!("invalid JavaScript input: {error}"))?;
        let result: Value = function
            .call((messages,))
            .catch(&ctx)
            .map_err(|error| anyhow::anyhow!("JavaScript rule failed: {error}"))?;
        if result.is_null() {
            return Ok(MessageActionOutcome::Ignore);
        }
        let Some(result) = result.as_string() else {
            bail!("JavaScript rule must return \"BAN\", \"KICK\", \"STRIKE\", \"GIVE_ROLE\", \"REVOKE_ROLE\", or null");
        };
        Ok(match result.to_string()?.as_str() {
            "BAN" => MessageActionOutcome::Ban(reason),
            "KICK" => MessageActionOutcome::Kick(reason),
            "STRIKE" => MessageActionOutcome::Strike(reason),
            "GIVE_ROLE" => MessageActionOutcome::role(role_id, true, reason)?,
            "REVOKE_ROLE" => MessageActionOutcome::role(role_id, false, reason)?,
            _ => bail!("JavaScript rule must return \"BAN\", \"KICK\", \"STRIKE\", \"GIVE_ROLE\", \"REVOKE_ROLE\", or null"),
        })
    });
    ensure!(
        !cancelled.load(Ordering::Relaxed),
        "JavaScript rule was cancelled"
    );
    ensure!(
        Instant::now() < deadline,
        "JavaScript rule exceeded its 1-second execution limit"
    );
    result
}

fn message_values(context: &ActionContext) -> Vec<serde_json::Value> {
    let mut messages = context.history.iter().map(|message| json!({
        "id": message.id.to_string(),
        "guild_id": message.guild_id.to_string(),
        "channel_id": message.channel_id.to_string(),
        "author_id": message.author_id.to_string(),
        "content": message.content,
        "timestamp": message.timestamp.unix_timestamp_nanos() / 1_000_000,
        "edited_timestamp": message.edited_timestamp.map(|value| value.unix_timestamp_nanos() / 1_000_000),
        "images": image_values(&message.images),
    })).collect::<Vec<_>>();
    let message = &context.message;
    messages.push(json!({
        "id": message.id,
        "guild_id": message.guild_id,
        "channel_id": message.channel_id,
        "author_id": message.author.id,
        "content": message.content,
        "timestamp": message.timestamp.as_micros() / 1000,
        "edited_timestamp": message.edited_timestamp.map(|value| value.as_micros() / 1000),
        "images": image_values(&context.images),
    }));
    messages
}

fn image_values(images: &[ImageResult]) -> Vec<serde_json::Value> {
    images
        .iter()
        .map(|image| {
            json!({
                // Discord snowflakes can exceed JavaScript's exact integer range.
                "attachment_id": image.attachment_id.to_string(),
                "url": image.url,
                "mime_type": image.mime_type,
                "description": image.description,
                "description_error": image.description_error,
            })
        })
        .collect()
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests;
