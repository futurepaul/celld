//! Host ops behind `ctx.container`.
//!
//! JavaScript owns the surface and every validation message, copied from
//! workerd's `api/container.c++`; these ops perform the effects through
//! `crate::container`. Each takes the cell scope as its first argument,
//! the way the storage ops do, and an exec process is addressed by the id
//! `__container_exec` returned.

use super::*;
use crate::container::{
    self, CellContainer, ContainerEngine, ExecParams, Instance, InterceptRule, OutputMode,
    ServiceRoute, StartParams,
};
use std::time::Duration;

async fn cell_for(scope: &str) -> Result<(Arc<ContainerEngine>, Arc<CellContainer>), String> {
    let engine = container::engine()
        .await
        .map_err(|error| format!("{error:#}"))?;
    let cell = engine
        .cell(scope)
        .ok_or_else(|| "this Durable Object has no container".to_string())?;
    Ok((engine, cell))
}

fn ready_cell(scope: &str) -> Option<Arc<CellContainer>> {
    container::engine_if_ready().and_then(|engine| engine.cell(scope))
}

fn string_arg(
    scope: &mut v8::PinScope,
    args: &v8::FunctionCallbackArguments,
    index: i32,
) -> String {
    args.get(index).to_rust_string_lossy(scope)
}

fn number_arg(scope: &mut v8::PinScope, args: &v8::FunctionCallbackArguments, index: i32) -> u64 {
    args.get(index).integer_value(scope).unwrap_or(0).max(0) as u64
}

pub(super) fn op_container_running(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let cell = string_arg(scope, &args, 0);
    let running = ready_cell(&cell).is_some_and(|cell| cell.running());
    rv.set(v8::Boolean::new(scope, running).into());
}

/// `host:port` the node dials for `getTcpPort(port)`, or a throw.
pub(super) fn op_container_address(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let cell = string_arg(scope, &args, 0);
    let port = number_arg(scope, &args, 1) as u16;
    let address = ready_cell(&cell)
        .ok_or_else(|| "this Durable Object has no container".to_string())
        .and_then(|cell| cell.address(port));
    match address {
        Ok(address) => rv.set(v8::String::new(scope, &address).unwrap().into()),
        Err(message) => loader_throw(scope, &message),
    }
}

/// `inspect()`'s answer as JSON text, `null` when nothing runs.
pub(super) fn op_container_inspect(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let cell = string_arg(scope, &args, 0);
    let info = ready_cell(&cell).and_then(|cell| cell.inspect());
    let text = info.map_or_else(|| "null".to_string(), |info| info.to_string());
    rv.set(v8::String::new(scope, &text).unwrap().into());
}

/// `ctx.container.images` as JSON text.
pub(super) fn op_container_images(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let cell = string_arg(scope, &args, 0);
    let class = cell.split(':').next().unwrap_or_default();
    let images = ready_cell(&cell)
        .map(|cell| cell.images().clone())
        .or_else(|| container::spec(class).map(|spec| spec.images.clone()))
        .unwrap_or_default();
    let text = serde_json::to_string(&images).unwrap_or_else(|_| "{}".into());
    rv.set(v8::String::new(scope, &text).unwrap().into());
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum InstanceArg {
    Named(String),
    #[serde(rename_all = "camelCase")]
    Custom {
        vcpu: f64,
        memory_mib: u64,
        disk_mb: u64,
    },
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartArgs {
    entrypoint: Option<Vec<String>>,
    #[serde(default)]
    env: Vec<(String, String)>,
    #[serde(default)]
    enable_internet: bool,
    #[serde(default)]
    labels: Vec<(String, String)>,
    image: Option<String>,
    container_snapshot: Option<String>,
    instance: Option<InstanceArg>,
}

pub(super) fn op_container_start(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let cell = string_arg(scope, &args, 0);
    let raw = string_arg(scope, &args, 1);
    let request: StartArgs = match serde_json::from_str(&raw) {
        Ok(request) => request,
        Err(error) => return loader_throw(scope, &format!("start(): {error}")),
    };
    // The run opens here, on the calling thread, so the monitor() that
    // follows this call in the same turn waits for this start's exit.
    let run = ready_cell(&cell).map(|cell| cell.begin_run());
    let async_id = asyncrt::enqueue_io_context(async move {
        let (engine, cell) = cell_for(&cell).await?;
        let run = run.unwrap_or_else(|| cell.begin_run());
        engine
            .start(
                &cell,
                run,
                StartParams {
                    image: request.image,
                    snapshot: request.container_snapshot,
                    instance: request.instance.map(|i| match i {
                        InstanceArg::Named(name) => Instance::Named(name),
                        InstanceArg::Custom {
                            vcpu,
                            memory_mib,
                            disk_mb,
                        } => Instance::Custom {
                            vcpu,
                            memory_mib,
                            disk_mb,
                        },
                    }),
                    entrypoint: request.entrypoint,
                    env: request.env,
                    enable_internet: request.enable_internet,
                    labels: request.labels,
                },
            )
            .await
            .map_err(|error| format!("{error:#}"))?;
        Ok::<String, String>(String::new())
    });
    rv.set(promise_for(scope, async_id));
}

/// Resolves with `{"code", "destroyed"}` once the current run ends.
/// Unrefed: a handler can await it, but once the handler has answered it
/// cannot keep the event, and so the cell, alive. A drain would otherwise
/// wait on a promise that settles only when the container exits. After the
/// event, `running` reports the host's state.
pub(super) fn op_container_monitor(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let cell = string_arg(scope, &args, 0);
    let run = ready_cell(&cell).map_or(0, |cell| cell.current_run());
    let async_id = asyncrt::enqueue_unrefed(async move {
        let (engine, cell) = cell_for(&cell).await?;
        let exit = engine.monitor(&cell, run).await?;
        Ok::<String, String>(
            serde_json::json!({ "code": exit.code, "destroyed": exit.destroyed }).to_string(),
        )
    });
    rv.set(promise_for(scope, async_id));
}

pub(super) fn op_container_destroy(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let cell = string_arg(scope, &args, 0);
    let async_id = asyncrt::enqueue(async move {
        let (engine, cell) = cell_for(&cell).await?;
        engine
            .destroy(&cell)
            .await
            .map_err(|error| format!("{error:#}"))?;
        Ok::<String, String>(String::new())
    });
    rv.set(promise_for(scope, async_id));
}

pub(super) fn op_container_signal(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let cell = string_arg(scope, &args, 0);
    let signal = number_arg(scope, &args, 1) as u32;
    let async_id = asyncrt::enqueue(async move {
        let (engine, cell) = cell_for(&cell).await?;
        engine
            .signal(&cell, signal)
            .await
            .map_err(|error| format!("{error:#}"))?;
        Ok::<String, String>(String::new())
    });
    rv.set(promise_for(scope, async_id));
}

pub(super) fn op_container_inactivity(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let cell = string_arg(scope, &args, 0);
    let duration = number_arg(scope, &args, 1);
    let async_id = asyncrt::enqueue(async move {
        let (_, cell) = cell_for(&cell).await?;
        cell.set_inactivity(Duration::from_millis(duration));
        Ok::<String, String>(String::new())
    });
    rv.set(promise_for(scope, async_id));
}

/// `snapshotContainer({name})`: resolves with the snapshot's JSON.
pub(super) fn op_container_snapshot(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let cell = string_arg(scope, &args, 0);
    let name = args.get(1);
    let name = (!name.is_null_or_undefined()).then(|| name.to_rust_string_lossy(scope));
    let async_id = asyncrt::enqueue(async move {
        let (engine, cell) = cell_for(&cell).await?;
        let snapshot = engine
            .snapshot(&cell, name)
            .await
            .map_err(|error| format!("{error:#}"))?;
        Ok::<String, String>(snapshot.to_string())
    });
    rv.set(promise_for(scope, async_id));
}

#[derive(serde::Deserialize)]
struct InterceptArgs {
    scheme: String,
    target: String,
    script: String,
    entrypoint: Option<String>,
}

/// `interceptOutbound*`: the rule, and the binding's route (its props as
/// structured-clone bytes in the third argument).
pub(super) fn op_container_intercept(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let cell = string_arg(scope, &args, 0);
    let raw = string_arg(scope, &args, 1);
    let props = view_bytes(args.get(2)).unwrap_or_default();
    let request: InterceptArgs = match serde_json::from_str(&raw) {
        Ok(request) => request,
        Err(error) => return loader_throw(scope, &format!("intercept: {error}")),
    };
    let scheme = match request.scheme.as_str() {
        "http" => "http",
        "https" => "https",
        _ => return loader_throw(scope, "intercept: the scheme is http or https"),
    };
    let route = ServiceRoute {
        generation: current_generation(scope),
        script: request.script,
        entrypoint: request.entrypoint,
        props,
    };
    let rule = InterceptRule {
        scheme,
        target: request.target,
    };
    let async_id = asyncrt::enqueue(async move {
        let (engine, cell) = cell_for(&cell).await?;
        engine
            .intercept(&cell, rule, route)
            .await
            .map_err(|error| format!("{error:#}"))?;
        Ok::<String, String>(String::new())
    });
    rv.set(promise_for(scope, async_id));
}

#[derive(serde::Deserialize)]
struct ExecArgs {
    cmd: Vec<String>,
    #[serde(default)]
    env: Vec<(String, String)>,
    cwd: Option<String>,
    user: Option<String>,
    #[serde(default)]
    stdin: bool,
    stdout: OutputMode,
    stderr: OutputMode,
    /// `[cols, rows]`.
    pty: Option<(u16, u16)>,
}

pub(super) fn op_container_exec(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let cell = string_arg(scope, &args, 0);
    let raw = string_arg(scope, &args, 1);
    let request: ExecArgs = match serde_json::from_str(&raw) {
        Ok(request) => request,
        Err(error) => return loader_throw(scope, &format!("exec(): {error}")),
    };
    let async_id = asyncrt::enqueue(async move {
        let (engine, cell) = cell_for(&cell).await?;
        let process = engine
            .exec(
                &cell,
                ExecParams {
                    cmd: request.cmd,
                    env: request.env,
                    cwd: request.cwd,
                    user: request.user,
                    stdin: request.stdin,
                    stdout: request.stdout,
                    stderr: request.stderr,
                    pty: request.pty,
                },
            )
            .await
            .map_err(|error| format!("{error:#}"))?;
        Ok::<String, String>(
            serde_json::json!({ "id": process.id, "pid": process.pid }).to_string(),
        )
    });
    rv.set(promise_for(scope, async_id));
}

// The process ops below are unrefed like `monitor()`: a handler awaits
// them, and a process left running when the handler answers does not pin
// the cell.
fn process_arg(
    scope: &mut v8::PinScope,
    args: &v8::FunctionCallbackArguments,
) -> Result<Arc<container::ExecProcess>, String> {
    let id = number_arg(scope, args, 0);
    container::process(id).ok_or_else(|| "the process is closed".to_string())
}

/// One chunk of stdout (1) or stderr (2); an empty chunk is the end.
pub(super) fn op_container_exec_read(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let process = process_arg(scope, &args);
    let which = number_arg(scope, &args, 1) as u8;
    let async_id = asyncrt::enqueue_unrefed(async move {
        let process = process?;
        Ok::<Vec<u8>, String>(process.read(which).await.to_vec())
    });
    rv.set(promise_for(scope, async_id));
}

pub(super) fn op_container_exec_write(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let process = process_arg(scope, &args);
    let Some(bytes) = view_bytes(args.get(1)) else {
        return loader_throw(scope, "stdin write needs bytes");
    };
    let async_id = asyncrt::enqueue_unrefed(async move {
        process?.write(&bytes).await?;
        Ok::<String, String>(String::new())
    });
    rv.set(promise_for(scope, async_id));
}

pub(super) fn op_container_exec_close(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let process = process_arg(scope, &args);
    let async_id = asyncrt::enqueue_unrefed(async move {
        process?.close_stdin().await;
        Ok::<String, String>(String::new())
    });
    rv.set(promise_for(scope, async_id));
}

pub(super) fn op_container_exec_wait(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let process = process_arg(scope, &args);
    let async_id = asyncrt::enqueue_unrefed(async move {
        let code = process?
            .wait()
            .await
            .map_err(|error| format!("{error:#}"))?;
        Ok::<String, String>(code.to_string())
    });
    rv.set(promise_for(scope, async_id));
}

pub(super) fn op_container_exec_kill(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let process = process_arg(scope, &args);
    let signal = number_arg(scope, &args, 1) as u32;
    let async_id = asyncrt::enqueue_unrefed(async move {
        process?
            .kill(signal)
            .await
            .map_err(|error| format!("{error:#}"))?;
        Ok::<String, String>(String::new())
    });
    rv.set(promise_for(scope, async_id));
}

pub(super) fn op_container_exec_resize(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue<v8::Value>,
) {
    let process = process_arg(scope, &args);
    let cols = number_arg(scope, &args, 1).min(u16::MAX as u64) as u16;
    let rows = number_arg(scope, &args, 2).min(u16::MAX as u64) as u16;
    let async_id = asyncrt::enqueue_unrefed(async move {
        process?
            .resize(cols, rows)
            .await
            .map_err(|error| format!("{error:#}"))?;
        Ok::<String, String>(String::new())
    });
    rv.set(promise_for(scope, async_id));
}

pub(super) fn op_container_exec_drop(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let id = number_arg(scope, &args, 0);
    container::drop_process(id);
}
