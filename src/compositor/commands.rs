//! Commands for the running canoe from the command line (`canoe reload`,
//! ...), which arrive over the socket in [`crate::ipc`]. Each one is answered with what to print, or with what
//! went wrong.

use super::protocols::primary_output::PrimaryOutputHandler;
use super::{LockState, State};

/// Run the command `args` (e.g. `["outputs", "primary", "eDP-1"]`).
pub fn run(state: &mut State, args: &[String]) -> Result<String, String> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["version"] => Ok(format!("canoe {}", env!("CARGO_PKG_VERSION"))),
        ["reload"] => reload(state),
        ["lock"] => lock(state),
        ["quit"] => {
            // The answer goes out before the loop gets to stop.
            state.loop_signal.stop();
            Ok(String::new())
        }
        ["outputs"] | ["outputs", "list"] => Ok(state.describe_outputs()),
        ["outputs", "primary"] => state
            .primary_output_state
            .primary()
            .map(|name| format!("{name}\n"))
            .ok_or_else(|| "no output is primary".to_string()),
        ["outputs", "primary", name] => set_primary_output(state, name),
        [] => Err("no command given; see `canoe --help`".to_string()),
        _ => Err(format!(
            "unknown command `{}`; see `canoe --help`",
            args.join(" ")
        )),
    }
}

/// Re-read the configuration, unless it has errors: then say what they are
/// and keep the one in use (SIGHUP would fall back to the defaults).
fn reload(state: &mut State) -> Result<String, String> {
    state
        .context
        .borrow()
        .check_config_file()
        .map_err(|e| format!("{e}\nThe configuration in use stays as it is."))?;
    state.reload_config();
    Ok(String::new())
}

fn lock(state: &mut State) -> Result<String, String> {
    if !matches!(state.lock_state, LockState::Unlocked) {
        return Err("the screen is already locked".to_string());
    }
    let context = state.context.borrow();
    context.spawn(&context.config.lock_cmd);
    Ok(String::new())
}

fn set_primary_output(state: &mut State, name: &str) -> Result<String, String> {
    let Some(output) = state.outputs.iter().find(|o| o.output.name() == name) else {
        let names: Vec<String> = state.outputs.iter().map(|o| o.output.name()).collect();
        return Err(format!(
            "there is no output {name}; there are: {}",
            names.join(", ")
        ));
    };
    if !output.enabled {
        return Err(format!("{name} is disabled"));
    }
    state.choose_primary_output(Some(name.to_string()));
    Ok(String::new())
}

impl State {
    /// Re-read the configuration file and apply it (SIGHUP, `canoe reload`).
    pub fn reload_config(&mut self) {
        self.context.borrow_mut().reload_config();
        self.apply_keyboard_config();
        self.apply_sway_emulation();
        self.apply_kwin_emulation();
        self.update_x11_settings();
        self.schedule_wm();
    }
}
