//! Undo/Redo system based on scene snapshots.
#![allow(dead_code)]
//!
//! Every mutation that should be undoable must be wrapped in
//! `push_undo()`. The system stores up to MAX_HISTORY full scene +
//! track-assignment clones. Storing the lane assignments alongside
//! the scene is what makes "Ctrl+Z restores the layer the deleted
//! element lived on" work — without them, an undone delete put the
//! element back on the default lane instead of where the user had
//! placed it.

use std::collections::HashMap;

use memstroy_core::Scene;

const MAX_HISTORY: usize = 120;

/// One snapshot in the history stack. Stores the full `Scene`
/// alongside the editor's track-assignment maps so an undo restores
/// both the scene tree AND the layer lane each clip lived on.
#[derive(Clone)]
pub struct UndoSnapshot {
    pub scene: Scene,
    pub actor_track_assignments: HashMap<usize, usize>,
    pub overlay_track_assignments: HashMap<usize, usize>,
    pub audio_track_assignments: HashMap<usize, usize>,
}

pub struct UndoStack {
    /// Past states (most recent at the end).
    undo: Vec<UndoSnapshot>,
    /// Future states (most recent at the end) — populated by undo.
    redo: Vec<UndoSnapshot>,
}

impl Default for UndoStack {
    fn default() -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }
}

impl UndoStack {
    /// Legacy single-arg snapshot push. Kept for callers that don't
    /// have track-assignment context handy; they get a snapshot with
    /// empty assignment maps. Prefer `push_full` for any path that
    /// can affect lane membership.
    pub fn push(&mut self, scene: &Scene) {
        self.push_full(UndoSnapshot {
            scene: scene.clone(),
            actor_track_assignments: HashMap::new(),
            overlay_track_assignments: HashMap::new(),
            audio_track_assignments: HashMap::new(),
        });
    }

    /// Save full editor state (scene + track-assignment maps) before
    /// a mutation. Call BEFORE applying the change.
    pub fn push_full(&mut self, snap: UndoSnapshot) {
        self.undo.push(snap);
        if self.undo.len() > MAX_HISTORY {
            self.undo.remove(0);
        }
        // Any new action clears the redo stack.
        self.redo.clear();
    }

    /// Undo: restore the previous state. Returns the restored
    /// snapshot (caller should replace its scene + assignment maps
    /// with the contents). The *current* state should be passed so
    /// we can push it to redo.
    pub fn undo_full(&mut self, current: UndoSnapshot) -> Option<UndoSnapshot> {
        if let Some(prev) = self.undo.pop() {
            self.redo.push(current);
            Some(prev)
        } else {
            None
        }
    }

    /// Redo: restore the next state. Returns the restored snapshot.
    pub fn redo_full(&mut self, current: UndoSnapshot) -> Option<UndoSnapshot> {
        if let Some(next) = self.redo.pop() {
            self.undo.push(current);
            Some(next)
        } else {
            None
        }
    }

    /// Legacy scene-only undo wrapper. Kept so old call sites compile;
    /// new ones should use `undo_full` to also recover assignment maps.
    pub fn undo(&mut self, current: &Scene) -> Option<Scene> {
        let snap = self.undo_full(UndoSnapshot {
            scene: current.clone(),
            actor_track_assignments: HashMap::new(),
            overlay_track_assignments: HashMap::new(),
            audio_track_assignments: HashMap::new(),
        })?;
        Some(snap.scene)
    }

    /// Legacy scene-only redo wrapper. See `undo` above.
    pub fn redo(&mut self, current: &Scene) -> Option<Scene> {
        let snap = self.redo_full(UndoSnapshot {
            scene: current.clone(),
            actor_track_assignments: HashMap::new(),
            overlay_track_assignments: HashMap::new(),
            audio_track_assignments: HashMap::new(),
        })?;
        Some(snap.scene)
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    pub fn undo_count(&self) -> usize {
        self.undo.len()
    }
    pub fn redo_count(&self) -> usize {
        self.redo.len()
    }
}
