//! MemStroy Scripting Language (MSL) — documented DSL for AI automation.
//!
//! # Overview
//!
//! MSL is a simple imperative scripting language designed so that an LLM
//! (GPT-4, Claude, etc.) can generate programs that manipulate all scene
//! objects. The language is deliberately minimal and JSON-friendly so that
//! AI context windows stay small.
//!
//! # Language Reference
//!
//! ## Commands (one per line, or semicolon-separated)
//!
//! ```text
//! // ─── Scene ───
//! set_duration <seconds>
//! set_fps <fps>
//! set_resolution <width> <height>
//!
//! // ─── Actors ───
//! add_actor <id> <source_path>
//! remove_actor <id>
//! set_actor_timing <id> <t_in> <t_out>
//! set_actor_pos <id> <x_norm> <y_norm>
//! set_actor_scale <id> <scale>
//! set_actor_rotation <id> <degrees>
//! set_actor_opacity <id> <0..1>
//! set_actor_visible <id> <true|false>
//! add_actor_keyframe <id> <t> <x> <y> <scale> <rotation> <opacity> [easing]
//!
//! // ─── Canvas (world-pixel) ───
//! set_actor_world_pos <id> <world_x> <world_y>
//! set_actor_world_width <id> <width_px>
//! add_canvas_keyframe <id> <t> <world_x> <world_y> <width> <scale> <rotation> <opacity> [easing]
//!
//! // ─── Overlays ───
//! add_text <id> <"text content"> <t_in> <t_out>
//! add_image <id> <source_path> <t_in> <t_out>
//! add_video <id> <source_path> <t_in> <t_out>
//! remove_overlay <id>
//! set_overlay_pos <id> <x_norm> <y_norm>
//! set_overlay_scale <id> <scale>
//! set_text_content <id> <"new text">
//! set_text_style <id> font_size=<n> color=<r,g,b> [bold] [box=<r,g,b>]
//!
//! // ─── Backgrounds ───
//! add_background <id> <source_path> <start> <duration>
//! add_solid_bg <id> <r> <g> <b> <start> <duration>
//! remove_background <id>
//! set_bg_fit <id> <cover|contain|stretch|original>
//! set_bg_transition <id> <cut|fade|snap|slide_left|slide_right|slide_up|slide_down>
//!
//! // ─── Render Frame (camera) ───
//! set_render_frame_pos <world_x> <world_y>
//! set_render_frame_zoom <zoom>
//! add_render_frame_keyframe <t> <world_x> <world_y> <zoom> <rotation> [easing]
//!
//! // ─── Audio ───
//! add_audio <id> <source_path> <t_in> [t_out] [volume]
//! remove_audio <id>
//!
//! // ─── Skeleton ───
//! attach_to_skeleton <actor_id> <skeleton_name> <point_name> [scale] [follow_rotation]
//!
//! // ─── Utility ───
//! list_actors
//! list_overlays
//! list_backgrounds
//! list_audio
//! describe_scene
//! ```
//!
//! ## Easing values: step, linear, ease_in, ease_out, ease_in_out, cubic
//!
//! ## Example program:
//! ```text
//! set_duration 8.0
//! set_resolution 1080 1920
//! add_actor mellstroy1 clips/mellstroy_001.mp4
//! set_actor_timing mellstroy1 0.0 5.0
//! set_actor_pos mellstroy1 0.5 0.7
//! add_actor_keyframe mellstroy1 0.0 0.5 0.7 1.0 0.0 1.0 linear
//! add_actor_keyframe mellstroy1 2.0 0.3 0.6 1.2 -5.0 1.0 ease_out
//! add_text title "МЕЛСТРОЙ БЬЁТ ПО СТОЛУ" 0.5 4.0
//! set_overlay_pos title 0.5 0.15
//! set_text_style title font_size=72 color=255,255,255 bold box=0,0,0
//! add_background bg1 backgrounds/city.mp4 0.0 8.0
//! set_bg_fit bg1 cover
//! ```

use std::path::PathBuf;

use crate::*;


// ─── ERROR TYPE ──────────────────────────────────────────────────────

/// Errors produced during script parsing or execution.
#[derive(Debug, Clone)]
pub struct ScriptError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ScriptError {}

/// Result of executing a script — either success with optional output messages,
/// or a list of errors.
#[derive(Debug, Clone)]
pub struct ScriptResult {
    pub messages: Vec<String>,
    pub errors: Vec<ScriptError>,
}

impl ScriptResult {
    pub fn ok() -> Self { Self { messages: Vec::new(), errors: Vec::new() } }
    pub fn is_ok(&self) -> bool { self.errors.is_empty() }
}


// ─── INTERPRETER ─────────────────────────────────────────────────────

/// Execute an MSL script against a scene. Modifies the scene in-place.
/// Returns messages (for `list_*`/`describe_scene`) and any errors.
pub fn execute_script(script: &str, scene: &mut Scene) -> ScriptResult {
    let mut result = ScriptResult::ok();

    for (line_num, raw_line) in script.lines().enumerate() {
        let line = raw_line.trim();
        // Skip empty lines and comments
        if line.is_empty() || line.starts_with("//") || line.starts_with('#') {
            continue;
        }

        if let Err(msg) = execute_line(line, scene, &mut result.messages) {
            result.errors.push(ScriptError {
                line: line_num + 1,
                message: msg,
            });
        }
    }

    result
}

fn execute_line(line: &str, scene: &mut Scene, messages: &mut Vec<String>) -> Result<(), String> {
    let tokens = tokenize(line);
    if tokens.is_empty() { return Ok(()); }

    let cmd = tokens[0].as_str();
    let args = &tokens[1..];

    match cmd {
        "set_duration" => {
            let d = parse_f32(args, 0, "seconds")?;
            scene.output.duration = d;
        }
        "set_fps" => {
            let fps = parse_u32(args, 0, "fps")?;
            scene.output.fps = fps;
        }
        "set_resolution" => {
            let w = parse_u32(args, 0, "width")?;
            let h = parse_u32(args, 1, "height")?;
            scene.output.resolution = [w, h];
            scene.render_frame.resolution = [w, h];
        }
        "add_actor" => {
            let id = get_arg(args, 0, "id")?;
            let source = get_arg(args, 1, "source_path")?;
            let actor = Actor {
                id: id.to_string(),
                source: PathBuf::from(source),
                anchors: None,
                chroma_key: ChromaKeyParams::default(),
                layout: vec![Keyframe::new(0.0, ActorState::default())],
                t_in: Some(0.0),
                t_out: Some(scene.output.duration),
                source_start: 0.0,
                loop_source: false,
                flip_horizontal: false,
                attachments: Vec::new(),
                skeleton_attachments: Vec::new(),
                modifiers: Vec::new(),
                visible: true,
                color_correction: ColorCorrection::default(),
                transition_in: Transition::Cut,
                transition_out: Transition::Cut,
                transition_duration: 0.3,
                effects: Vec::new(),
                animated_params: Default::default(),
            };
            scene.actors.push(actor);
        }
        "remove_actor" => {
            let id = get_arg(args, 0, "id")?;
            let pos = scene.actors.iter().position(|a| a.id == id)
                .ok_or_else(|| format!("actor '{}' not found", id))?;
            scene.actors.remove(pos);
        }
        "set_actor_timing" => {
            let id = get_arg(args, 0, "id")?;
            let t_in = parse_f32(args, 1, "t_in")?;
            let t_out = parse_f32(args, 2, "t_out")?;
            let actor = find_actor_mut(scene, id)?;
            actor.t_in = Some(t_in);
            actor.t_out = Some(t_out);
        }
        "set_actor_pos" => {
            let id = get_arg(args, 0, "id")?;
            let x = parse_f32(args, 1, "x")?;
            let y = parse_f32(args, 2, "y")?;
            let actor = find_actor_mut(scene, id)?;
            if let Some(kf) = actor.layout.first_mut() {
                kf.value.pos = [x, y];
            } else {
                actor.layout.push(Keyframe::new(0.0, ActorState { pos: [x, y], ..Default::default() }));
            }
        }
        "set_actor_scale" => {
            let id = get_arg(args, 0, "id")?;
            let s = parse_f32(args, 1, "scale")?;
            let actor = find_actor_mut(scene, id)?;
            if let Some(kf) = actor.layout.first_mut() { kf.value.scale = s; }
        }
        "set_actor_rotation" => {
            let id = get_arg(args, 0, "id")?;
            let r = parse_f32(args, 1, "degrees")?;
            let actor = find_actor_mut(scene, id)?;
            if let Some(kf) = actor.layout.first_mut() { kf.value.rotation_deg = r; }
        }
        "set_actor_opacity" => {
            let id = get_arg(args, 0, "id")?;
            let o = parse_f32(args, 1, "opacity")?;
            let actor = find_actor_mut(scene, id)?;
            if let Some(kf) = actor.layout.first_mut() { kf.value.opacity = o; }
        }
        "set_actor_visible" => {
            let id = get_arg(args, 0, "id")?;
            let v = get_arg(args, 1, "true|false")?;
            let actor = find_actor_mut(scene, id)?;
            actor.visible = v == "true";
        }
        "add_actor_keyframe" => cmd_add_actor_keyframe(args, scene)?,
        "set_actor_world_pos" => cmd_set_actor_world_pos(args, scene)?,
        "add_canvas_keyframe" => cmd_add_canvas_keyframe(args, scene)?,
        "add_text" => cmd_add_text(args, scene)?,
        "add_image" => cmd_add_image(args, scene)?,
        "add_video" => cmd_add_video_overlay(args, scene)?,
        "remove_overlay" => cmd_remove_overlay(args, scene)?,
        "set_overlay_pos" => cmd_set_overlay_pos(args, scene)?,
        "set_overlay_scale" => cmd_set_overlay_scale(args, scene)?,
        "set_text_content" => cmd_set_text_content(args, scene)?,
        "add_background" => cmd_add_background(args, scene)?,
        "add_solid_bg" => cmd_add_solid_bg(args, scene)?,
        "remove_background" => {
            let id = get_arg(args, 0, "id")?;
            let pos = scene.backgrounds.iter().position(|b| b.id == id)
                .ok_or_else(|| format!("background '{}' not found", id))?;
            scene.backgrounds.remove(pos);
        }
        "set_bg_fit" => cmd_set_bg_fit(args, scene)?,
        "set_bg_transition" => cmd_set_bg_transition(args, scene)?,
        "set_render_frame_pos" => {
            let x = parse_f32(args, 0, "world_x")?;
            let y = parse_f32(args, 1, "world_y")?;
            if let Some(kf) = scene.render_frame.layout.first_mut() {
                kf.value.pos = WorldPos { x, y };
            }
        }
        "set_render_frame_zoom" => {
            let z = parse_f32(args, 0, "zoom")?;
            if let Some(kf) = scene.render_frame.layout.first_mut() {
                kf.value.zoom = z;
            }
        }
        "add_render_frame_keyframe" => cmd_add_render_frame_kf(args, scene)?,
        "add_audio" => cmd_add_audio(args, scene)?,
        "remove_audio" => {
            let id = get_arg(args, 0, "id")?;
            let pos = scene.audio.iter().position(|a| a.id == id)
                .ok_or_else(|| format!("audio '{}' not found", id))?;
            scene.audio.remove(pos);
        }
        "attach_to_skeleton" => cmd_attach_skeleton(args, scene)?,
        "list_actors" => {
            let mut out = String::from("Actors:\n");
            for a in &scene.actors {
                out.push_str(&format!(
                    "  {} | src={} | t={:.2}..{:.2} | visible={}\n",
                    a.id,
                    a.source.display(),
                    a.t_in.unwrap_or(0.0),
                    a.t_out.unwrap_or(scene.output.duration),
                    a.visible,
                ));
            }
            messages.push(out);
        }
        "list_overlays" => {
            let mut out = String::from("Overlays:\n");
            for ov in &scene.overlays {
                let desc = match ov {
                    Overlay::Text(t) => format!("  TEXT '{}' | {:.2}..{:.2}", t.text, t.t_in, t.t_out),
                    Overlay::Image(i) => format!("  IMG {} | {:.2}..{:.2}", i.source.display(), i.t_in, i.t_out),
                    Overlay::Video(v) => format!("  VID {} | {:.2}..{:.2}", v.source.display(), v.t_in, v.t_out),
                };
                out.push_str(&desc);
                out.push('\n');
            }
            messages.push(out);
        }
        "list_backgrounds" => {
            let mut out = String::from("Backgrounds:\n");
            for bg in &scene.backgrounds {
                out.push_str(&format!("  {} | {:.2}..{:.2} | {:?}\n", bg.id, bg.start, bg.start + bg.duration, bg.fit));
            }
            messages.push(out);
        }
        "list_audio" => {
            let mut out = String::from("Audio:\n");
            for a in &scene.audio {
                out.push_str(&format!("  {} | src={} | {:.2}..{:.2}\n", a.id, a.source.display(), a.t_in, a.t_out.unwrap_or(scene.output.duration)));
            }
            messages.push(out);
        }
        "describe_scene" => {
            let msg = format!(
                "Scene: {}x{} @ {} fps, {:.1}s\nActors: {}\nOverlays: {}\nBackgrounds: {}\nAudio: {}\nRender frame: ({:.0}, {:.0}) zoom={:.2}\n",
                scene.output.resolution[0], scene.output.resolution[1],
                scene.output.fps, scene.output.duration,
                scene.actors.len(), scene.overlays.len(),
                scene.backgrounds.len(), scene.audio.len(),
                scene.render_frame.layout.first().map(|k| k.value.pos.x).unwrap_or(0.0),
                scene.render_frame.layout.first().map(|k| k.value.pos.y).unwrap_or(0.0),
                scene.render_frame.layout.first().map(|k| k.value.zoom).unwrap_or(1.0),
            );
            messages.push(msg);
        }
        _ => return Err(format!("unknown command: '{}'", cmd)),
    }

    Ok(())
}


// ─── COMMAND IMPLEMENTATIONS ─────────────────────────────────────────

fn cmd_add_actor_keyframe(args: &[String], scene: &mut Scene) -> Result<(), String> {
    let id = get_arg(args, 0, "id")?;
    let t = parse_f32(args, 1, "t")?;
    let x = parse_f32(args, 2, "x")?;
    let y = parse_f32(args, 3, "y")?;
    let scale = parse_f32(args, 4, "scale")?;
    let rot = parse_f32(args, 5, "rotation")?;
    let opacity = parse_f32(args, 6, "opacity")?;
    let easing = parse_easing_opt(args, 7);
    let actor = find_actor_mut(scene, id)?;
    let kf = Keyframe {
        t,
        value: ActorState { pos: [x, y], scale, scale_y: 1.0, rotation_deg: rot, opacity, flip_x_anim: 1.0, flip_y_anim: 1.0 },
        easing,
    };
    actor.layout.push(kf);
    actor.layout.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap());
    Ok(())
}

fn cmd_set_actor_world_pos(args: &[String], scene: &mut Scene) -> Result<(), String> {
    let id = get_arg(args, 0, "id")?;
    let wx = parse_f32(args, 1, "world_x")?;
    let wy = parse_f32(args, 2, "world_y")?;
    // Find or create canvas_layout entry
    if let Some(cl) = scene.canvas_layouts.iter_mut().find(|cl| cl.element_id == id) {
        if let Some(kf) = cl.keyframes.first_mut() {
            kf.value.pos = WorldPos { x: wx, y: wy };
        }
    } else {
        scene.canvas_layouts.push(CanvasLayout {
            element_id: id.to_string(),
            keyframes: vec![Keyframe::new(0.0, CanvasTransform {
                pos: WorldPos { x: wx, y: wy },
                ..Default::default()
            })],
        });
    }
    Ok(())
}

fn cmd_add_canvas_keyframe(args: &[String], scene: &mut Scene) -> Result<(), String> {
    let id = get_arg(args, 0, "id")?;
    let t = parse_f32(args, 1, "t")?;
    let wx = parse_f32(args, 2, "world_x")?;
    let wy = parse_f32(args, 3, "world_y")?;
    let width = parse_f32(args, 4, "width")?;
    let scale = parse_f32(args, 5, "scale")?;
    let rot = parse_f32(args, 6, "rotation")?;
    let opacity = parse_f32(args, 7, "opacity")?;
    let easing = parse_easing_opt(args, 8);
    let transform = CanvasTransform {
        pos: WorldPos { x: wx, y: wy },
        width,
        scale,
        rotation_deg: rot,
        opacity,
    };
    let kf = Keyframe { t, value: transform, easing };
    if let Some(cl) = scene.canvas_layouts.iter_mut().find(|cl| cl.element_id == id) {
        cl.keyframes.push(kf);
        cl.keyframes.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap());
    } else {
        scene.canvas_layouts.push(CanvasLayout {
            element_id: id.to_string(),
            keyframes: vec![kf],
        });
    }
    Ok(())
}

fn cmd_add_text(args: &[String], scene: &mut Scene) -> Result<(), String> {
    let id = get_arg(args, 0, "id")?;
    let text = get_arg(args, 1, "text")?;
    let t_in = parse_f32(args, 2, "t_in")?;
    let t_out = parse_f32(args, 3, "t_out")?;
    scene.overlays.push(Overlay::Text(TextOverlay {
        id: id.to_string(),
        text: text.to_string(),
        t_in,
        t_out,
        style: TextStyle::default(),
        layout: vec![Keyframe::new(0.0, OverlayState::default())],
        modifiers: Vec::new(),
        skeleton_attachment: None,
        z_index: 100,
        behind_actors: false,
        effects: Vec::new(),
        animated_params: Default::default(),
    }));
    Ok(())
}

fn cmd_add_image(args: &[String], scene: &mut Scene) -> Result<(), String> {
    let id = get_arg(args, 0, "id")?;
    let src = get_arg(args, 1, "source_path")?;
    let t_in = parse_f32(args, 2, "t_in")?;
    let t_out = parse_f32(args, 3, "t_out")?;
    scene.overlays.push(Overlay::Image(ImageOverlay {
        id: id.to_string(),
        source: PathBuf::from(src),
        t_in,
        t_out,
        layout: vec![Keyframe::new(0.0, OverlayState::default())],
        modifiers: Vec::new(),
        skeleton_attachment: None,
        effects: Vec::new(),
        animated_params: Default::default(),
    }));
    Ok(())
}

fn cmd_add_video_overlay(args: &[String], scene: &mut Scene) -> Result<(), String> {
    let id = get_arg(args, 0, "id")?;
    let src = get_arg(args, 1, "source_path")?;
    let t_in = parse_f32(args, 2, "t_in")?;
    let t_out = parse_f32(args, 3, "t_out")?;
    scene.overlays.push(Overlay::Video(VideoOverlay {
        id: id.to_string(),
        source: PathBuf::from(src),
        t_in,
        t_out,
        source_start: 0.0,
        loop_source: false,
        chroma_key: None,
        layout: vec![Keyframe::new(0.0, OverlayState::default())],
        modifiers: Vec::new(),
        skeleton_attachment: None,
        effects: Vec::new(),
        animated_params: Default::default(),
    }));
    Ok(())
}

fn cmd_remove_overlay(args: &[String], scene: &mut Scene) -> Result<(), String> {
    let id = get_arg(args, 0, "id")?;
    let pos = scene.overlays.iter().position(|ov| match ov {
        Overlay::Text(t) => t.id == id,
        Overlay::Image(i) => i.id == id,
        Overlay::Video(v) => v.id == id,
    }).ok_or_else(|| format!("overlay '{}' not found", id))?;
    scene.overlays.remove(pos);
    Ok(())
}


fn cmd_set_overlay_pos(args: &[String], scene: &mut Scene) -> Result<(), String> {
    let id = get_arg(args, 0, "id")?;
    let x = parse_f32(args, 1, "x")?;
    let y = parse_f32(args, 2, "y")?;
    let ov = find_overlay_mut(scene, id)?;
    match ov {
        Overlay::Text(t) => { if let Some(kf) = t.layout.first_mut() { kf.value.pos = [x, y]; } }
        Overlay::Image(i) => { if let Some(kf) = i.layout.first_mut() { kf.value.pos = [x, y]; } }
        Overlay::Video(v) => { if let Some(kf) = v.layout.first_mut() { kf.value.pos = [x, y]; } }
    }
    Ok(())
}

fn cmd_set_overlay_scale(args: &[String], scene: &mut Scene) -> Result<(), String> {
    let id = get_arg(args, 0, "id")?;
    let s = parse_f32(args, 1, "scale")?;
    let ov = find_overlay_mut(scene, id)?;
    match ov {
        Overlay::Text(t) => { if let Some(kf) = t.layout.first_mut() { kf.value.scale = s; } }
        Overlay::Image(i) => { if let Some(kf) = i.layout.first_mut() { kf.value.scale = s; } }
        Overlay::Video(v) => { if let Some(kf) = v.layout.first_mut() { kf.value.scale = s; } }
    }
    Ok(())
}

fn cmd_set_text_content(args: &[String], scene: &mut Scene) -> Result<(), String> {
    let id = get_arg(args, 0, "id")?;
    let text = get_arg(args, 1, "text")?;
    let ov = find_overlay_mut(scene, id)?;
    match ov {
        Overlay::Text(t) => { t.text = text.to_string(); }
        _ => return Err(format!("'{}' is not a text overlay", id)),
    }
    Ok(())
}

fn cmd_add_background(args: &[String], scene: &mut Scene) -> Result<(), String> {
    let id = get_arg(args, 0, "id")?;
    let src = get_arg(args, 1, "source_path")?;
    let start = parse_f32(args, 2, "start")?;
    let duration = parse_f32(args, 3, "duration")?;
    let ext = PathBuf::from(src).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let source = if ["jpg", "jpeg", "png", "webp"].contains(&ext.as_str()) {
        MediaSource::Image { path: PathBuf::from(src) }
    } else {
        MediaSource::Video { path: PathBuf::from(src), r#loop: true, start_at: 0.0 }
    };
    scene.backgrounds.push(Background {
        id: id.to_string(),
        source,
        start,
        duration,
        fit: Fit::Cover,
        transition: Transition::Cut,
    });
    Ok(())
}

fn cmd_add_solid_bg(args: &[String], scene: &mut Scene) -> Result<(), String> {
    let id = get_arg(args, 0, "id")?;
    let r = parse_u32(args, 1, "r")? as u8;
    let g = parse_u32(args, 2, "g")? as u8;
    let b = parse_u32(args, 3, "b")? as u8;
    let start = parse_f32(args, 4, "start")?;
    let duration = parse_f32(args, 5, "duration")?;
    scene.backgrounds.push(Background {
        id: id.to_string(),
        source: MediaSource::SolidColor { color: [r, g, b] },
        start,
        duration,
        fit: Fit::Cover,
        transition: Transition::Cut,
    });
    Ok(())
}

fn cmd_set_bg_fit(args: &[String], scene: &mut Scene) -> Result<(), String> {
    let id = get_arg(args, 0, "id")?;
    let fit_str = get_arg(args, 1, "fit")?;
    let fit = match fit_str {
        "cover" => Fit::Cover,
        "contain" => Fit::Contain,
        "stretch" => Fit::Stretch,
        "original" => Fit::Original,
        _ => return Err(format!("unknown fit: '{}'", fit_str)),
    };
    let bg = scene.backgrounds.iter_mut().find(|b| b.id == id)
        .ok_or_else(|| format!("background '{}' not found", id))?;
    bg.fit = fit;
    Ok(())
}

fn cmd_set_bg_transition(args: &[String], scene: &mut Scene) -> Result<(), String> {
    let id = get_arg(args, 0, "id")?;
    let tr_str = get_arg(args, 1, "transition")?;
    let tr = parse_transition(tr_str)?;
    let bg = scene.backgrounds.iter_mut().find(|b| b.id == id)
        .ok_or_else(|| format!("background '{}' not found", id))?;
    bg.transition = tr;
    Ok(())
}

fn cmd_add_render_frame_kf(args: &[String], scene: &mut Scene) -> Result<(), String> {
    let t = parse_f32(args, 0, "t")?;
    let wx = parse_f32(args, 1, "world_x")?;
    let wy = parse_f32(args, 2, "world_y")?;
    let zoom = parse_f32(args, 3, "zoom")?;
    let rot = parse_f32(args, 4, "rotation")?;
    let easing = parse_easing_opt(args, 5);
    let kf = Keyframe {
        t,
        value: RenderFrameState { pos: WorldPos { x: wx, y: wy }, zoom, rotation_deg: rot },
        easing,
    };
    scene.render_frame.layout.push(kf);
    scene.render_frame.layout.sort_by(|a, b| a.t.partial_cmp(&b.t).unwrap());
    Ok(())
}

fn cmd_add_audio(args: &[String], scene: &mut Scene) -> Result<(), String> {
    let id = get_arg(args, 0, "id")?;
    let src = get_arg(args, 1, "source_path")?;
    let t_in = parse_f32(args, 2, "t_in")?;
    let t_out = if args.len() > 3 { Some(parse_f32(args, 3, "t_out")?) } else { None };
    let volume = if args.len() > 4 { parse_f32(args, 4, "volume")? } else { 1.0 };
    scene.audio.push(AudioTrack {
        id: id.to_string(),
        source: PathBuf::from(src),
        t_in,
        t_out,
        source_start: 0.0,
        volume,
        speed: 1.0,
        parent_actor: None,
        volume_kfs: Vec::new(),
        speed_kfs: Vec::new(),
        animated_params: Default::default(),
    });
    Ok(())
}

fn cmd_attach_skeleton(args: &[String], scene: &mut Scene) -> Result<(), String> {
    let actor_id = get_arg(args, 0, "actor_id")?;
    let skel_name = get_arg(args, 1, "skeleton_name")?;
    let point_name = get_arg(args, 2, "point_name")?;
    let scale = if args.len() > 3 { parse_f32(args, 3, "scale")? } else { 1.0 };
    let follow = if args.len() > 4 { args[4] == "true" } else { false };
    let actor = find_actor_mut(scene, actor_id)?;
    actor.skeleton_attachments.push(SkeletonAttachment {
        skeleton_id: skel_name.to_string(),
        point_name: point_name.to_string(),
        offset: [0.0, 0.0],
        scale,
        follow_rotation: follow,
    });
    Ok(())
}


// ─── HELPERS ─────────────────────────────────────────────────────────

/// Tokenize a line, respecting quoted strings.
fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;

    for ch in line.chars() {
        if ch == '"' {
            in_quote = !in_quote;
            // Don't include the quote chars in the token
        } else if ch.is_whitespace() && !in_quote {
            if !current.is_empty() {
                tokens.push(current.clone());
                current.clear();
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn get_arg<'a>(args: &'a [String], idx: usize, name: &str) -> Result<&'a str, String> {
    args.get(idx)
        .map(|s| s.as_str())
        .ok_or_else(|| format!("missing argument: {}", name))
}

fn parse_f32(args: &[String], idx: usize, name: &str) -> Result<f32, String> {
    let s = get_arg(args, idx, name)?;
    s.parse::<f32>().map_err(|_| format!("'{}' is not a valid number for {}", s, name))
}

fn parse_u32(args: &[String], idx: usize, name: &str) -> Result<u32, String> {
    let s = get_arg(args, idx, name)?;
    s.parse::<u32>().map_err(|_| format!("'{}' is not a valid integer for {}", s, name))
}

fn parse_easing_opt(args: &[String], idx: usize) -> Easing {
    args.get(idx).map(|s| parse_easing(s)).unwrap_or(Easing::Linear)
}

fn parse_easing(s: &str) -> Easing {
    match s {
        "step" => Easing::Step,
        "linear" => Easing::Linear,
        "ease_in" => Easing::EaseIn,
        "ease_out" => Easing::EaseOut,
        "ease_in_out" => Easing::EaseInOut,
        "cubic" => Easing::Cubic,
        _ => Easing::Linear,
    }
}

fn parse_transition(s: &str) -> Result<Transition, String> {
    match s {
        "cut" => Ok(Transition::Cut),
        "fade" => Ok(Transition::Fade),
        "snap" => Ok(Transition::Snap),
        "slide_left" => Ok(Transition::SlideLeft),
        "slide_right" => Ok(Transition::SlideRight),
        "slide_up" => Ok(Transition::SlideUp),
        "slide_down" => Ok(Transition::SlideDown),
        _ => Err(format!("unknown transition: '{}'", s)),
    }
}

fn find_actor_mut<'a>(scene: &'a mut Scene, id: &str) -> Result<&'a mut Actor, String> {
    scene.actors.iter_mut().find(|a| a.id == id)
        .ok_or_else(|| format!("actor '{}' not found", id))
}

fn find_overlay_mut<'a>(scene: &'a mut Scene, id: &str) -> Result<&'a mut Overlay, String> {
    scene.overlays.iter_mut().find(|ov| match ov {
        Overlay::Text(t) => t.id == id,
        Overlay::Image(i) => i.id == id,
        Overlay::Video(v) => v.id == id,
    }).ok_or_else(|| format!("overlay '{}' not found", id))
}

// ─── CONTEXT EXPORT FOR AI ───────────────────────────────────────────

/// Generate a compact context description of the scene for AI consumption.
/// This string can be sent to an LLM alongside the MSL documentation so it
/// knows what resources and state are available.
pub fn export_scene_context(scene: &Scene) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# Scene Context\nresolution: {}x{}\nfps: {}\nduration: {:.1}s\n\n",
        scene.output.resolution[0], scene.output.resolution[1],
        scene.output.fps, scene.output.duration
    ));
    if !scene.actors.is_empty() {
        out.push_str("## Actors\n");
        for a in &scene.actors {
            out.push_str(&format!(
                "- id={} src={} t={:.2}..{:.2} visible={}\n",
                a.id, a.source.display(),
                a.t_in.unwrap_or(0.0), a.t_out.unwrap_or(scene.output.duration),
                a.visible
            ));
        }
        out.push('\n');
    }
    if !scene.overlays.is_empty() {
        out.push_str("## Overlays\n");
        for ov in &scene.overlays {
            let line = match ov {
                Overlay::Text(t) => format!("- TEXT id={} \"{}\" t={:.2}..{:.2}\n", t.id, t.text, t.t_in, t.t_out),
                Overlay::Image(i) => format!("- IMG id={} src={} t={:.2}..{:.2}\n", i.id, i.source.display(), i.t_in, i.t_out),
                Overlay::Video(v) => format!("- VID id={} src={} t={:.2}..{:.2}\n", v.id, v.source.display(), v.t_in, v.t_out),
            };
            out.push_str(&line);
        }
        out.push('\n');
    }
    if !scene.backgrounds.is_empty() {
        out.push_str("## Backgrounds\n");
        for bg in &scene.backgrounds {
            out.push_str(&format!("- id={} t={:.2}..{:.2} fit={:?}\n", bg.id, bg.start, bg.start + bg.duration, bg.fit));
        }
        out.push('\n');
    }
    if !scene.audio.is_empty() {
        out.push_str("## Audio\n");
        for a in &scene.audio {
            out.push_str(&format!("- id={} src={} t={:.2}..{:.2}\n", a.id, a.source.display(), a.t_in, a.t_out.unwrap_or(scene.output.duration)));
        }
        out.push('\n');
    }
    if !scene.skeleton_templates.is_empty() {
        out.push_str("## Skeleton Templates\n");
        for tmpl in &scene.skeleton_templates {
            let points: Vec<&str> = tmpl.points.keys().map(|s| s.as_str()).collect();
            out.push_str(&format!("- name={} clip={} points=[{}]\n", tmpl.name, tmpl.source_clip.display(), points.join(", ")));
        }
        out.push('\n');
    }
    out
}

/// Generate the MSL language documentation as a string (for sending to AI).
pub fn msl_documentation() -> &'static str {
    include_str!("scripting.rs")
        .split("//! # Overview")
        .nth(1)
        .and_then(|s| s.split("use std::path").next())
        .unwrap_or("See scripting.rs module documentation for MSL reference.")
}
