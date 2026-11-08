//! Windows as taskbars see them, through wlr-foreign-toplevel-management:
//! what they are told, and what they may ask for.

use std::collections::HashSet;

use smithay::output::Output;
use smithay::utils::{Logical, Rectangle};

use super::protocols::foreign_toplevel::{
    delegate_foreign_toplevel_manager, ForeignToplevelManagerHandler, ForeignToplevelManagerState,
    ToplevelInfo, ToplevelRequest, ToplevelStates, WlrToplevel,
};
use super::{ManagedWindow, State};
use crate::canoe;

impl State {
    /// Announce newly mapped windows to taskbars and tell them what changed
    /// about the others.
    pub fn update_wlr_toplevels(&mut self) {
        let outputs: Vec<(Output, Rectangle<i32, Logical>)> = self
            .active_outputs()
            .filter_map(|o| Some((o.output.clone(), self.output_geometry(&o.output)?)))
            .collect();
        // Windows taskbars may know about: those announced on the
        // ext-foreign-toplevel list, i.e. mapped.
        let mut pending: Vec<usize> = (0..self.windows.len())
            .filter(|&i| self.windows[i].foreign.is_some())
            .collect();
        let mapped: HashSet<canoe::WindowId> =
            pending.iter().map(|&i| self.windows[i].id).collect();

        // Parents first, so a new child can name its parent.
        while !pending.is_empty() {
            let mut deferred = Vec::new();
            for &i in &pending {
                let parent_id = self.wm_parent(self.windows[i].id);
                let parent = parent_id.and_then(|id| self.wlr_toplevel_of(id));
                if parent.is_none() && parent_id.is_some_and(|id| mapped.contains(&id)) {
                    deferred.push(i);
                    continue;
                }
                self.send_wlr_toplevel(i, &outputs, parent);
            }
            if deferred.len() == pending.len() {
                // Parents that wait for each other: announce them without.
                for &i in &deferred {
                    self.send_wlr_toplevel(i, &outputs, None);
                }
                break;
            }
            pending = deferred;
        }
    }

    /// Announce the window at `index`, or update what taskbars know of it.
    fn send_wlr_toplevel(
        &mut self,
        index: usize,
        outputs: &[(Output, Rectangle<i32, Logical>)],
        parent: Option<WlrToplevel>,
    ) {
        let info = self.wlr_toplevel_info(&self.windows[index], outputs, parent);
        match &self.windows[index].wlr_toplevel {
            Some(toplevel) => toplevel.update(info),
            None => {
                let toplevel = self
                    .foreign_toplevel_manager_state
                    .new_toplevel::<State>(info);
                self.windows[index].wlr_toplevel = Some(toplevel);
            }
        }
    }

    fn wm_parent(&self, id: canoe::WindowId) -> Option<canoe::WindowId> {
        self.context
            .borrow()
            .windows
            .get(&id)
            .and_then(|w| w.borrow().parent)
    }

    fn wlr_toplevel_of(&self, id: canoe::WindowId) -> Option<WlrToplevel> {
        self.windows
            .iter()
            .find(|w| w.id == id)
            .and_then(|w| w.wlr_toplevel.clone())
    }

    fn wlr_toplevel_info(
        &self,
        window: &ManagedWindow,
        outputs: &[(Output, Rectangle<i32, Logical>)],
        parent: Option<WlrToplevel>,
    ) -> ToplevelInfo {
        let (app_id, title) = super::wm_events::shell_properties(&window.shell);
        let states = ToplevelStates {
            maximized: window.maximized,
            minimized: window.hidden,
            activated: self.wm_focus == Some(window.key),
            fullscreen: window.fullscreen.is_some(),
        };
        // The outputs the window overlaps, wherever it was last shown if it
        // is minimized; taskbars that list only their own output's windows
        // need one to show it at all, so a window off every output counts
        // as on the output the window manager assigned it. A fullscreen
        // window covers its output wherever its position is, and a window
        // waiting for its client to take a new size is already reported
        // where it goes.
        let fullscreen_area = window
            .fullscreen
            .and_then(|key| self.output_by_key(key))
            .and_then(|o| self.output_geometry(&o.output));
        let rect = match (fullscreen_area, window.pending_position) {
            (Some(area), _) => area,
            (None, Some((pos, size, _))) => Rectangle::new(pos, size),
            (None, None) => Rectangle::new(window.pos, window.window.geometry().size),
        };
        let mut on: Vec<Output> = outputs
            .iter()
            .filter(|(_, area)| rect.overlaps(*area))
            .map(|(output, _)| output.clone())
            .collect();
        if on.is_empty() {
            let output_id = self
                .context
                .borrow()
                .windows
                .get(&window.id)
                .and_then(|w| w.borrow().output.as_ref().and_then(|o| o.upgrade()))
                .map(|o| o.borrow().id);
            if let Some(output) = self
                .active_outputs()
                .find(|o| Some(o.id) == output_id)
                .or_else(|| self.active_outputs().next())
            {
                on.push(output.output.clone());
            }
        }
        ToplevelInfo {
            title: title.unwrap_or_default(),
            app_id: app_id.unwrap_or_default(),
            states,
            outputs: on,
            parent,
        }
    }
}

impl ForeignToplevelManagerHandler for State {
    fn foreign_toplevel_manager_state(&mut self) -> &mut ForeignToplevelManagerState {
        &mut self.foreign_toplevel_manager_state
    }

    fn toplevel_request(&mut self, toplevel: &WlrToplevel, request: ToplevelRequest) {
        let Some(window) = self
            .windows
            .iter()
            .find(|w| w.wlr_toplevel.as_ref() == Some(toplevel))
        else {
            return;
        };
        let id = window.id;
        let hidden = window.hidden;
        let surface = window.wl_surface().clone();
        let event = match request {
            ToplevelRequest::Activate | ToplevelRequest::Unminimize => {
                // Nothing comes up in front of the lock screen, and
                // unminimizing a shown window does nothing.
                if self.lock_state != super::LockState::Unlocked
                    || (request == ToplevelRequest::Unminimize && !hidden)
                {
                    return;
                }
                self.context.borrow_mut().activate_window(id);
                self.schedule_wm();
                return;
            }
            ToplevelRequest::Maximize => canoe::WindowEvent::Maximize,
            ToplevelRequest::Unmaximize => canoe::WindowEvent::Unmaximize,
            ToplevelRequest::Minimize => {
                if hidden {
                    return;
                }
                canoe::WindowEvent::Minimize
            }
            ToplevelRequest::Fullscreen(output) => {
                canoe::WindowEvent::Fullscreen(self.wm_output_weak(output.as_ref()))
            }
            ToplevelRequest::Unfullscreen => canoe::WindowEvent::Unfullscreen,
            ToplevelRequest::Close => canoe::WindowEvent::Close,
        };
        self.window_request(&surface, event);
    }
}

delegate_foreign_toplevel_manager!(State);
