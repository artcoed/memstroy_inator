//! Undo/Redo system based on scene snapshots.
#![allow(dead_code)]
//!
//! Every mutation that should be undoable must be wrapped in
//! `push_undo()`. The system stores up to MAX_HISTORY full scene
//! clones. This is simple but effective for scenes of this size
//! (typically a few KB of YAML).

use memstroy_core::Scene;

const MAX_HISTORY: usize = 50;

pub struct UndoStack {
    /// Past states (most recent at the end).
    undo: Vec<Scene>,
    /// Future states (most recent at the end) — populated by undo.
    redo: Vec<Scene>,
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
    /// Save current scene state before a mutation.
    /// Call this BEFORE applying the change.
    pub fn push(&mut self, scene: &Scene) {
        self.undo.push(scene.clone());
        if self.undo.len() > MAX_HISTORY {
            self.undo.remove(0);
        }
        // Any new action clears the redo stack.
        self.redo.clear();
    }

    /// Undo: restore the previous state. Returns the restored scene
    /// (caller should replace their current scene with it).
    /// The *current* scene should be passed so we can push it to redo.
    pub fn undo(&mut self, current: &Scene) -> Option<Scene> {
        if let Some(prev) = self.undo.pop() {
            self.redo.push(current.clone());
            Some(prev)
        } else {
            None
        }
    }

    /// Redo: restore the next state. Returns the restored scene.
    pub fn redo(&mut self, current: &Scene) -> Option<Scene> {
        if let Some(next) = self.redo.pop() {
            self.undo.push(current.clone());
            Some(next)
        } else {
            None
        }
    }

    pub fn can_undo(&self) -> bool { !self.undo.is_empty() }
    pub fn can_redo(&self) -> bool { !self.redo.is_empty() }
    pub fn undo_count(&self) -> usize { self.undo.len() }
    pub fn redo_count(&self) -> usize { self.redo.len() }
}
