use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

use crate::context::PluginContext;
use crate::state::{StateStore, ViewerMapping, ViewerPlacement};
use crate::{Error, PLUGIN_ID, Result};

pub trait Herdr {
    fn output(&self, arguments: &[String]) -> Result<Output>;
}

pub struct ProcessHerdr {
    binary: String,
}

impl ProcessHerdr {
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            binary: std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into()),
        }
    }
}

impl Herdr for ProcessHerdr {
    fn output(&self, arguments: &[String]) -> Result<Output> {
        Ok(Command::new(&self.binary).args(arguments).output()?)
    }
}

pub fn pane_exists(herdr: &impl Herdr, pane_id: &str) -> Result<bool> {
    let output = herdr.output(&["pane".into(), "get".into(), pane_id.into()])?;
    if output.status.success() {
        return Ok(true);
    }
    if has_error_code(&output, "pane_not_found") {
        return Ok(false);
    }
    Err(command_error(&output))
}

pub fn pane_info(herdr: &impl Herdr, pane_id: &str) -> Result<Value> {
    checked(herdr, &["pane".into(), "get".into(), pane_id.to_owned()])
}

pub fn pane_root(herdr: &impl Herdr, pane_id: &str) -> Result<std::path::PathBuf> {
    let value = pane_info(herdr, pane_id)?;
    let pane = value
        .get("result")
        .and_then(|result| result.get("pane"))
        .ok_or_else(|| Error::Message("Herdr pane response has no pane record".into()))?;
    pane.get("foreground_cwd")
        .or_else(|| pane.get("cwd"))
        .and_then(Value::as_str)
        .map(std::path::PathBuf::from)
        .ok_or_else(|| Error::Message("Herdr pane has no working directory".into()))
}

pub fn pane_left_neighbor(herdr: &impl Herdr, pane_id: &str) -> Result<Option<String>> {
    let value = checked(
        herdr,
        &[
            "pane".into(),
            "neighbor".into(),
            "--pane".into(),
            pane_id.into(),
            "--direction".into(),
            "left".into(),
        ],
    )?;
    Ok(value
        .get("result")
        .and_then(|result| result.get("neighbor"))
        .and_then(|neighbor| neighbor.get("neighbor_pane_id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned))
}

pub fn open_or_focus(
    herdr: &impl Herdr,
    store: &StateStore,
    context: &PluginContext,
) -> Result<()> {
    open_or_focus_at(herdr, store, context, ViewerPlacement::Split)
}

pub fn open_or_focus_tab(
    herdr: &impl Herdr,
    store: &StateStore,
    context: &PluginContext,
) -> Result<()> {
    open_or_focus_at(herdr, store, context, ViewerPlacement::Tab)
}

fn open_or_focus_at(
    herdr: &impl Herdr,
    store: &StateStore,
    context: &PluginContext,
    placement: ViewerPlacement,
) -> Result<()> {
    let invoking = context
        .pane_id
        .as_deref()
        .ok_or_else(|| Error::Message("Herdr did not provide an invoking pane".into()))?;
    // Restored panes keep their IDs but lose their plugin process. Resolve the
    // original source even when the shortcut is invoked from the old viewer.
    let source = store.mapping_for_viewer(invoking)?;
    let target = source
        .as_ref()
        .map_or(invoking, |mapping| mapping.target_pane_id.as_str());
    if let Some(mapping) = store.viewer_mapping_for(target, placement)? {
        let output = herdr.output(&[
            "plugin".into(),
            "pane".into(),
            "focus".into(),
            mapping.viewer_pane_id.clone(),
        ])?;
        if output.status.success() {
            if placement == ViewerPlacement::Split {
                zoom_viewer(herdr, &mapping.viewer_pane_id)?;
            }
            return Ok(());
        }
        if !has_error_code(&output, "plugin_pane_not_found")
            && !has_error_code(&output, "pane_not_found")
        {
            return Err(command_error(&output));
        }
        store.remove_viewer_mapping_for(target, placement)?;
    }
    // The live source pane is authoritative; callback cwd can be the plugin's
    // installation directory, especially after session restoration.
    let root = pane_root(herdr, target)?;
    let mut arguments = vec![
        "plugin".into(),
        "pane".into(),
        "open".into(),
        "--plugin".into(),
        PLUGIN_ID.into(),
        "--entrypoint".into(),
        "viewer".into(),
        "--placement".into(),
        placement.as_str().into(),
    ];
    if placement == ViewerPlacement::Split {
        arguments.extend([
            "--target-pane".into(),
            target.into(),
            "--direction".into(),
            "right".into(),
            "--env".into(),
            format!("HERDR_AGENT_DIFF_TARGET_PANE={target}"),
        ]);
        add_root_environment(&mut arguments, Some(&root));
        arguments.push("--focus".into());
    } else {
        let workspace = context
            .workspace_id
            .as_deref()
            .ok_or_else(|| Error::Message("Herdr did not provide an invoking workspace".into()))?;
        arguments.extend([
            "--workspace".into(),
            workspace.into(),
            "--env".into(),
            format!("HERDR_AGENT_DIFF_TARGET_PANE={target}"),
            "--env".into(),
            format!("HERDR_AGENT_DIFF_VIEWER_PLACEMENT={}", placement.as_str()),
        ]);
        add_root_environment(&mut arguments, Some(&root));
        arguments.push("--focus".into());
    }
    let response = checked(herdr, &arguments)?;
    if placement == ViewerPlacement::Split {
        let pane_id = response
            .pointer("/result/plugin_pane/pane/pane_id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Message("Herdr did not return the viewer pane id".into()))?;
        zoom_viewer(herdr, pane_id)?;
    }
    Ok(())
}

fn zoom_viewer(herdr: &impl Herdr, pane_id: &str) -> Result<()> {
    checked(
        herdr,
        &["pane".into(), "zoom".into(), pane_id.into(), "--on".into()],
    )?;
    Ok(())
}

fn add_root_environment(arguments: &mut Vec<String>, cwd: Option<&Path>) {
    if let Some(cwd) = cwd {
        arguments.extend([
            "--env".into(),
            format!("HERDR_AGENT_DIFF_ROOT={}", cwd.to_string_lossy()),
        ]);
    }
}

pub fn register_viewer(store: &StateStore, target_pane_id: &str) -> Result<()> {
    register_viewer_for(store, target_pane_id, ViewerPlacement::Split)
}

pub fn register_viewer_for(
    store: &StateStore,
    target_pane_id: &str,
    placement: ViewerPlacement,
) -> Result<()> {
    let viewer_pane_id = std::env::var("HERDR_PANE_ID")
        .map_err(|_| Error::Message("viewer pane id is unavailable".into()))?;
    store.set_viewer_mapping_for(
        &ViewerMapping {
            target_pane_id: target_pane_id.into(),
            viewer_pane_id,
        },
        placement,
    )
}

fn checked(herdr: &impl Herdr, arguments: &[String]) -> Result<Value> {
    let output = herdr.output(arguments)?;
    if !output.status.success() {
        return Err(command_error(&output));
    }
    serde_json::from_slice(&output.stdout).map_err(Into::into)
}

fn has_error_code(output: &Output, code: &str) -> bool {
    serde_json::from_slice::<Value>(&output.stderr)
        .is_ok_and(|value| value.pointer("/error/code").and_then(Value::as_str) == Some(code))
}

fn command_error(output: &Output) -> Error {
    Error::Message(format!(
        "Herdr command failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}
