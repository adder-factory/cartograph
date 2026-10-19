//! Poll and cancel an explicitly started job on the existing project owner.

use super::owner::OwnerClient;
use cartograph_mcp::{ToolCall, ToolCallContext, ToolResult};
use serde_json::{Map, Value};
use std::{path::Path, time::Duration};

const POLL: Duration = Duration::from_millis(100);
const DRAIN: Duration = Duration::from_secs(30);

pub(crate) async fn run(
    root: &Path,
    initial: ToolCall,
    context: ToolCallContext,
    mut progress: impl FnMut(&Value),
) -> Result<ToolResult, String> {
    let observe_only = is_status_request(&initial);
    let (owner, result) = call(root, initial, context.clone(), None).await?;
    if observe_only {
        return Ok(result);
    }
    let Some((job, status)) = identity(&result) else {
        return Ok(result);
    };
    if !active(status) {
        return Ok(result);
    }
    let mut previous = None;
    loop {
        if context.should_stop() {
            return stop(root, job, &owner).await;
        }
        tokio::select! {
            biased;
            ()=context.cancellation().cancelled()=>return stop(root,job,&owner).await,
            ()=tokio::time::sleep_until(context.deadline())=>return stop(root,job,&owner).await,
            ()=tokio::time::sleep(POLL)=>{},
        }
        let outcome = call(
            root,
            request("status", job),
            context.clone(),
            Some(owner.clone()),
        )
        .await
        .map(|(_, result)| result);
        let result = match outcome {
            Ok(result) => result,
            Err(_) if context.should_stop() => return stop(root, job, &owner).await,
            Err(error) => return Err(error),
        };
        let value = payload(&result)?;
        if let Some(observed) = value.get("progress")
            && previous.as_ref() != Some(observed)
        {
            progress(observed);
            previous = Some(observed.clone());
        }
        let (_, status) = identity(&result)
            .ok_or_else(|| "native owner returned malformed job status".to_owned())?;
        if !active(status) {
            return Ok(result);
        }
    }
}

pub(crate) fn is_status_request(call: &ToolCall) -> bool {
    call.name == "cartograph_admin"
        && call.arguments.get("action").and_then(Value::as_str) == Some("status")
}

async fn stop(root: &Path, job: u64, owner: &str) -> Result<ToolResult, String> {
    let context = ToolCallContext::local(DRAIN);
    let mut result = call(
        root,
        request("cancel", job),
        context.clone(),
        Some(owner.to_owned()),
    )
    .await
    .map(|(_, result)| result)
    .map_err(|_| {
        "native job cancellation could not be acknowledged; the owner retains the admitted job"
            .to_owned()
    })?;
    loop {
        let (_, status) = identity(&result)
            .ok_or_else(|| "native owner returned malformed cancellation status".to_owned())?;
        if !active(status) {
            return Ok(result);
        }
        if context.should_stop() {
            return Err("native job cancellation is still draining; the owner retains the admitted job and cleanup is unverified".to_owned());
        }
        tokio::time::sleep(POLL).await;
        result = call(
            root,
            request("status", job),
            context.clone(),
            Some(owner.to_owned()),
        )
        .await
        .map(|(_, result)| result)
        .map_err(|_| {
            "native job cleanup could not be verified; the owner retains the admitted job"
                .to_owned()
        })?;
    }
}

async fn call(
    root: &Path,
    call: ToolCall,
    context: ToolCallContext,
    expected_owner: Option<String>,
) -> Result<(String, ToolResult), String> {
    OwnerClient::connect(root)
        .await?
        .ok_or_else(|| "native project owner became unavailable".to_owned())?
        .call_owned(call, context, expected_owner)
        .await
        .map_err(|error| error.wire_message().to_owned())
}

fn request(action: &str, job: u64) -> ToolCall {
    ToolCall {
        name: "cartograph_admin".to_owned(),
        arguments: Map::from_iter([
            ("action".to_owned(), Value::from(action)),
            ("jobId".to_owned(), Value::from(job)),
        ]),
    }
}
fn payload(result: &ToolResult) -> Result<Value, String> {
    serde_json::from_str(
        result
            .primary_text()
            .ok_or_else(|| "native job returned no report".to_owned())?,
    )
    .map_err(|_| "native job returned malformed report".to_owned())
}
fn identity(result: &ToolResult) -> Option<(u64, &str)> {
    let data = result.structured_content()?;
    Some((data.get("jobId")?.as_u64()?, data.get("status")?.as_str()?))
}
fn active(status: &str) -> bool {
    matches!(status, "running" | "cancelling")
}

/// A job status is a successful MCP request even when the job itself failed.
/// Preserve its full report while giving command-line callers a failure exit.
pub(crate) fn failed(result: &ToolResult) -> bool {
    result.is_error()
        || identity(result).is_some_and(|(_, status)| matches!(status, "failed" | "cancelled"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_job_failure_is_distinct_from_successful_status_transport() {
        for (status, expected) in [("succeeded", false), ("failed", true), ("cancelled", true)] {
            let report = serde_json::json!({"jobId": 7, "status": status, "cleanupFailure": {"code": "fixture"}});
            let result = ToolResult::text(report.to_string())
                .with_structured_content(report.as_object().cloned().unwrap_or_default());
            assert!(!result.is_error());
            assert_eq!(failed(&result), expected);
            assert!(
                result
                    .structured_content()
                    .is_some_and(|value| value.contains_key("cleanupFailure"))
            );
        }
    }
}
