//! Parent hierarchy: position, rotation, and scale inheritance for
//! actors and overlays (Unity-style local-to-world composition).

use crate::canvas::WorldPos;
use crate::keyframe::{self, TrackModifier};
use crate::layout_sample::{sample_actor_layout, sample_overlay_layout, sample_render_frame_layout};
use crate::{Overlay, OverlayState, Scene};

#[inline]
fn safe_div(num: f32, den: f32) -> f32 {
    if den.abs() > 1.0e-6 {
        num / den
    } else {
        1.0
    }
}

/// Resolved parent transform accumulated along the parent chain.
#[derive(Clone, Copy, Debug, Default)]
pub struct ParentTransform {
    pub pos: WorldPos,
    pub rotation_deg: f32,
    pub scale_x: f32,
    pub scale_y: f32,
}

/// Apply a parent transform to a child's local offset (world pixels).
pub fn apply_parent_transform(child_pos: WorldPos, parent: &ParentTransform) -> WorldPos {
    let rad = parent.rotation_deg.to_radians();
    let cos_r = rad.cos();
    let sin_r = rad.sin();
    let dx = child_pos.x * parent.scale_x;
    let dy = child_pos.y * parent.scale_y;
    WorldPos {
        x: parent.pos.x + dx * cos_r - dy * sin_r,
        y: parent.pos.y + dx * sin_r + dy * cos_r,
    }
}

/// Inverse of [`apply_parent_transform`].
pub fn inverse_parent_transform(world: WorldPos, parent: &ParentTransform) -> WorldPos {
    let rad = parent.rotation_deg.to_radians();
    let cos_r = rad.cos();
    let sin_r = rad.sin();
    let dx = world.x - parent.pos.x;
    let dy = world.y - parent.pos.y;
    WorldPos {
        x: safe_div(dx * cos_r + dy * sin_r, parent.scale_x),
        y: safe_div(-dx * sin_r + dy * cos_r, parent.scale_y),
    }
}

/// Multiply child overlay scale / rotation by the resolved parent chain.
pub fn apply_parent_inheritance_overlay(ov_state: &mut OverlayState, parent: &ParentTransform) {
    ov_state.rotation_deg += parent.rotation_deg;
    ov_state.scale *= parent.scale_x;
    ov_state.scale_y *= safe_div(parent.scale_y, parent.scale_x);
}

/// Same inheritance for actor draw / hit-test paths that keep scale split.
pub fn apply_parent_inheritance_actor(
    rotation_deg: &mut f32,
    scale: &mut f32,
    scale_y: &mut f32,
    parent: &ParentTransform,
) {
    *rotation_deg += parent.rotation_deg;
    *scale *= parent.scale_x;
    *scale_y *= safe_div(parent.scale_y, parent.scale_x);
}

pub fn resolve_parent_transform(
    scene: &Scene,
    parent_id: &str,
    t: f32,
    visited: &mut Vec<String>,
) -> Option<ParentTransform> {
    if visited.contains(&parent_id.to_string()) {
        return None;
    }
    visited.push(parent_id.to_string());

    if parent_id == "__render_frame__" {
        let rf = &scene.render_frame;
        let rf_state = sample_render_frame_layout(&rf.layout, &rf.animated_params, t);
        let mod_delta = keyframe::evaluate_modifiers(&rf.modifiers, t);
        // Inspector exposes scale as 1/zoom; children inherit that display
        // scale (plus modifier pulse/shake), matching canvas preview.
        let display_scale = 1.0 / rf_state.zoom.max(1.0e-6);
        let scale = (display_scale + mod_delta.d_scale).max(0.001);
        return Some(ParentTransform {
            pos: WorldPos {
                x: rf_state.pos.x + mod_delta.dx,
                y: rf_state.pos.y + mod_delta.dy,
            },
            rotation_deg: rf_state.rotation_deg + mod_delta.d_rotation_deg,
            scale_x: scale,
            scale_y: scale,
        });
    }

    if let Some(actor) = scene.actors.iter().find(|a| a.id == parent_id) {
        let actor_t_in = actor.t_in.unwrap_or(0.0);
        let actor_state = sample_actor_layout(&actor.layout, &actor.animated_params, t);
        let mod_delta = keyframe::evaluate_modifiers(&actor.modifiers, (t - actor_t_in).max(0.0));

        let [rw, rh] = scene.render_frame.resolution;
        let world_w = rw as f32;
        let world_h = rh as f32;

        let canvas_transform = scene
            .canvas_layouts
            .iter()
            .find(|cl| cl.element_id == actor.id)
            .and_then(|cl| keyframe::sample(&cl.keyframes, t));

        let mut pos = if let Some(transform) = canvas_transform {
            WorldPos {
                x: transform.pos.x + mod_delta.dx,
                y: transform.pos.y + mod_delta.dy,
            }
        } else {
            WorldPos {
                x: actor_state.pos[0] * world_w + mod_delta.dx,
                y: actor_state.pos[1] * world_h + mod_delta.dy,
            }
        };
        let mut rotation = canvas_transform
            .map(|transform| transform.rotation_deg)
            .unwrap_or(actor_state.rotation_deg)
            + mod_delta.d_rotation_deg;
        let mut scale_x = (canvas_transform
            .map(|transform| transform.scale)
            .unwrap_or(actor_state.scale)
            + mod_delta.d_scale)
            .max(0.001);
        let mut scale_y = scale_x * actor_state.scale_y;

        if let Some(grandparent_id) = actor.parent_id.as_deref() {
            if let Some(gp) = resolve_parent_transform(scene, grandparent_id, t, visited) {
                pos = apply_parent_transform(pos, &gp);
                rotation += gp.rotation_deg;
                scale_x *= gp.scale_x;
                scale_y *= gp.scale_y;
            }
        }

        return Some(ParentTransform {
            pos,
            rotation_deg: rotation,
            scale_x,
            scale_y,
        });
    }

    if let Some(ov) = scene.overlays.iter().find(|ov| match ov {
        Overlay::Text(o) => o.id == parent_id,
        Overlay::Image(o) => o.id == parent_id,
        Overlay::Video(o) => o.id == parent_id,
    }) {
        let (t_in, parent_pid, modifiers) = match ov {
            Overlay::Text(o) => (o.t_in, o.parent_id.as_ref(), &o.modifiers),
            Overlay::Image(o) => (o.t_in, o.parent_id.as_ref(), &o.modifiers),
            Overlay::Video(o) => (o.t_in, o.parent_id.as_ref(), &o.modifiers),
        };
        let local_t = (t - t_in).max(0.0);
        let ov_state = overlay_state_at_local(ov, local_t);
        let mod_delta = keyframe::evaluate_modifiers(modifiers, local_t);

        let [rw, rh] = scene.render_frame.resolution;
        let world_w = rw as f32;
        let world_h = rh as f32;

        let overlay_id = match ov {
            Overlay::Text(o) => &o.id,
            Overlay::Image(o) => &o.id,
            Overlay::Video(o) => &o.id,
        };
        let canvas_transform = scene
            .canvas_layouts
            .iter()
            .find(|cl| cl.element_id == *overlay_id)
            .and_then(|cl| keyframe::sample(&cl.keyframes, t));

        let mut pos = if let Some(transform) = canvas_transform {
            WorldPos {
                x: transform.pos.x + mod_delta.dx,
                y: transform.pos.y + mod_delta.dy,
            }
        } else {
            WorldPos {
                x: ov_state.pos[0] * world_w + mod_delta.dx,
                y: ov_state.pos[1] * world_h + mod_delta.dy,
            }
        };
        let mut rotation = canvas_transform
            .map(|transform| transform.rotation_deg)
            .unwrap_or(ov_state.rotation_deg)
            + mod_delta.d_rotation_deg;
        let mut scale_x = (canvas_transform
            .map(|transform| transform.scale)
            .unwrap_or(ov_state.scale)
            + mod_delta.d_scale)
            .max(0.001);
        let mut scale_y = scale_x * ov_state.scale_y;

        if let Some(grandparent_id) = parent_pid {
            if let Some(gp) = resolve_parent_transform(scene, grandparent_id, t, visited) {
                pos = apply_parent_transform(pos, &gp);
                rotation += gp.rotation_deg;
                scale_x *= gp.scale_x;
                scale_y *= gp.scale_y;
            }
        }

        return Some(ParentTransform {
            pos,
            rotation_deg: rotation,
            scale_x,
            scale_y,
        });
    }

    None
}

fn overlay_state_at_local(overlay: &Overlay, local_t: f32) -> OverlayState {
    match overlay {
        Overlay::Text(t) => sample_overlay_layout(&t.layout, &t.animated_params, local_t),
        Overlay::Image(im) => sample_overlay_layout(&im.layout, &im.animated_params, local_t),
        Overlay::Video(v) => sample_overlay_layout(&v.layout, &v.animated_params, local_t),
    }
}

/// World-pixel centre of an element, including parent position inheritance.
pub fn element_world_pos(
    scene: &Scene,
    element_id: &str,
    scene_t: f32,
) -> WorldPos {
    let [rw, rh] = scene.render_frame.resolution;
    let world_w = rw as f32;
    let world_h = rh as f32;

    let base_pos = if let Some(cl) = scene
        .canvas_layouts
        .iter()
        .find(|cl| cl.element_id == element_id)
    {
        keyframe::sample(&cl.keyframes, scene_t)
            .map(|transform| transform.pos)
    } else {
        None
    };

    let base_pos = base_pos.unwrap_or_else(|| {
        if let Some(actor) = scene.actors.iter().find(|a| a.id == element_id) {
            if let Some(actor_state) = keyframe::sample(&actor.layout, scene_t) {
                return WorldPos {
                    x: actor_state.pos[0] * world_w,
                    y: actor_state.pos[1] * world_h,
                };
            }
        }
        if let Some(ov) = scene.overlays.iter().find(|ov| match ov {
            Overlay::Text(o) => o.id == element_id,
            Overlay::Image(o) => o.id == element_id,
            Overlay::Video(o) => o.id == element_id,
        }) {
            let (t_in, t_out) = match ov {
                Overlay::Text(o) => (o.t_in, o.t_out),
                Overlay::Image(o) => (o.t_in, o.t_out),
                Overlay::Video(o) => (o.t_in, o.t_out),
            };
            let local_t = if scene_t >= t_in && scene_t <= t_out {
                scene_t - t_in
            } else if scene_t < t_in {
                0.0
            } else {
                (t_out - t_in).max(0.0)
            };
            let ov_state = overlay_state_at_local(ov, local_t);
            return WorldPos {
                x: ov_state.pos[0] * world_w,
                y: ov_state.pos[1] * world_h,
            };
        }
        WorldPos {
            x: world_w * 0.5,
            y: world_h * 0.5,
        }
    });

    let parent_id = scene
        .actors
        .iter()
        .find(|a| a.id == element_id)
        .and_then(|a| a.parent_id.clone())
        .or_else(|| {
            scene.overlays.iter().find_map(|ov| match ov {
                Overlay::Text(o) if o.id == element_id => o.parent_id.clone(),
                Overlay::Image(o) if o.id == element_id => o.parent_id.clone(),
                Overlay::Video(o) if o.id == element_id => o.parent_id.clone(),
                _ => None,
            })
        });

    if let Some(pid) = parent_id {
        let mut visited = vec![element_id.to_string()];
        if let Some(parent_xform) = resolve_parent_transform(scene, &pid, scene_t, &mut visited) {
            return apply_parent_transform(base_pos, &parent_xform);
        }
    }

    base_pos
}

/// Sample overlay transform at clip-local `local_t`, apply modifiers,
/// then inherit parent rotation / scale.
pub fn overlay_visual_state(
    scene: &Scene,
    overlay: &Overlay,
    element_id: &str,
    parent_id: Option<&String>,
    scene_t: f32,
    local_t: f32,
    modifiers: &[TrackModifier],
) -> OverlayState {
    let mut ov_state = overlay_state_at_local(overlay, local_t);
    let mod_delta = keyframe::evaluate_modifiers(modifiers, local_t);
    ov_state.scale = (ov_state.scale + mod_delta.d_scale).max(0.001);
    ov_state.rotation_deg += mod_delta.d_rotation_deg;

    if let Some(pid) = parent_id {
        let mut visited = vec![element_id.to_string()];
        if let Some(pxf) = resolve_parent_transform(scene, pid, scene_t, &mut visited) {
            apply_parent_inheritance_overlay(&mut ov_state, &pxf);
        }
    }

    ov_state
}
