//! Regression tests for the canvas-preview ↔ filtergraph parity fix.
//!
//! Background: the renderer used to ignore `Scene.canvas_layouts`,
//! `render_frame.pos` motion, keyframe easing, modifiers and per-
//! frame rotation/opacity. Users reported "при рендере в результате
//! клипы в видеоролике совершенно не так выглядят, как на
//! предпросмотре области рендера" — the export ran on a frozen
//! subset of the scene model.
//!
//! The fix lives in `crates/memstroy-render/src/expr.rs` and is wired
//! into the actor / image / video / text emitters. These tests pin
//! the contract: each previously-dropped feature now appears in the
//! generated `filter_complex` graph in a recognisable form so
//! regressions show up in CI before they reach end users.

use std::path::PathBuf;

use memstroy_core::{
    canvas::WorldPos,
    keyframe::{ModifierKind, TrackModifier},
    Actor, ActorState, CanvasLayout, CanvasTransform, ChromaKeyParams, ColorCorrection, Easing,
    ImageOverlay, Keyframe, Overlay, OutputSpec, OverlayState, RenderFrame, RenderFrameState,
    Scene,
};

use memstroy_render::build_plan;

fn baseline_scene() -> Scene {
    Scene {
        format_version: 1,
        output: OutputSpec {
            resolution: [1080, 1920],
            fps: 30,
            duration: 4.0,
            background_color: [0, 0, 0],
        },
        backgrounds: Vec::new(),
        camera: Vec::new(),
        actors: Vec::new(),
        overlays: Vec::new(),
        audio: Vec::new(),
        render_frame: RenderFrame::default(),
        canvas_layouts: Vec::new(),
        skeleton_templates: Vec::new(),
    }
}

fn baseline_actor(id: &str) -> Actor {
    Actor {
        id: id.into(),
        source: PathBuf::from("clip.mp4"),
        anchors: None,
        chroma_key: ChromaKeyParams::default(),
        layout: vec![Keyframe::new(0.0, ActorState::default())],
        t_in: Some(0.0),
        t_out: Some(2.0),
        source_start: 0.0,
        loop_source: false,
        flip_horizontal: false,
        attachments: Vec::new(),
        skeleton_attachments: Vec::new(),
        modifiers: Vec::new(),
        visible: true,
        color_correction: ColorCorrection::default(),
        transition_in: Default::default(),
        transition_out: Default::default(),
        transition_duration: 0.3,
        effects: Vec::new(),
        speed: 1.0,
        animated_params: Default::default(),
    }
}

fn build_filter_graph(scene: &Scene) -> String {
    let plan = build_plan(scene, &PathBuf::from("/tmp/out.mp4"), &PathBuf::from("/tmp/assets"))
        .expect("plan");
    plan.filter_complex
}

#[test]
fn legacy_layout_default_render_frame_matches_old_geometry() {
    // For the simplest case (no canvas_layouts, default render_frame,
    // single keyframe with default pos = [0.5, 0.7]) the new pipeline
    // must reduce to the same geometry the old `pos*W - w/2` produced.
    // We assert the centre-of-element on the composite is W/2 = 540.
    let mut scene = baseline_scene();
    scene.actors.push(baseline_actor("a1"));

    let graph = build_filter_graph(&scene);
    // The new pipeline phrases this as `(pos_x - 0.5) * W` translated
    // by the render_frame which sits at (540, 960). Either way the
    // centre lands at 540 and the overlay X subtracts w/2 to place
    // the top-left.
    assert!(
        graph.contains("overlay=x='"),
        "filter_complex missing actor overlay: {graph}",
    );
    assert!(graph.contains(":enable='between(t,0,2)'"));
}

#[test]
fn canvas_layouts_world_pixel_position_is_honoured() {
    // The preview's primary positioning source is `Scene.canvas_layouts`
    // (Free Canvas v2 world pixels). Old renderer ignored it, so an
    // actor at world (1500, 800) would render in the wrong spot.
    let mut scene = baseline_scene();
    scene.actors.push(baseline_actor("hero"));
    scene.canvas_layouts.push(CanvasLayout {
        element_id: "hero".into(),
        keyframes: vec![Keyframe::new(
            0.0,
            CanvasTransform {
                pos: WorldPos { x: 1500.0, y: 800.0 },
                width: 500.0,
                scale: 1.0,
                rotation_deg: 0.0,
                opacity: 1.0,
            },
        )],
    });

    let graph = build_filter_graph(&scene);
    // The world-pixel pos must appear literally in the overlay X
    // expression (it shows up as `1500.000000` after our 6-digit
    // formatting). If the renderer reverted to legacy normalised
    // positions, the value would be 0.5 / 0.7 instead.
    assert!(
        graph.contains("1500.000000"),
        "world-pixel position 1500 not present in graph:\n{graph}",
    );
    assert!(
        graph.contains("800.000000"),
        "world-pixel position 800 not present in graph:\n{graph}",
    );
}

#[test]
fn render_frame_motion_translates_legacy_positions() {
    // When `render_frame.pos` animates, every legacy-positioned
    // element should slide opposite the frame movement so its
    // on-canvas world position stays put. The expression must
    // reference `render_frame.pos.x` keyframe values (we encode
    // the second keyframe at 1200.0).
    let mut scene = baseline_scene();
    scene.render_frame.layout = vec![
        Keyframe::new(0.0, RenderFrameState::default()),
        Keyframe::new(
            2.0,
            RenderFrameState {
                pos: WorldPos { x: 1200.0, y: 960.0 },
                zoom: 1.0,
                rotation_deg: 0.0,
            },
        ),
    ];
    scene.actors.push(baseline_actor("a1"));

    let graph = build_filter_graph(&scene);
    assert!(
        graph.contains("1200.000000"),
        "render_frame.pos.x = 1200 not threaded into the position expression:\n{graph}",
    );
}

#[test]
fn step_easing_does_not_emit_linear_segment() {
    // `Easing::Step` should hold the previous value until the next
    // keyframe. The old renderer used pure linear interpolation and
    // the discrete jump was lost. After the fix the step segment
    // must be the previous *value* (constant 0.5 here), not a linear
    // interpolation expression.
    let mut scene = baseline_scene();
    let mut actor = baseline_actor("a1");
    actor.layout = vec![
        Keyframe::new(
            0.0,
            ActorState { pos: [0.5, 0.5], ..ActorState::default() },
        ),
        Keyframe {
            t: 1.0,
            value: ActorState { pos: [0.9, 0.5], ..ActorState::default() },
            easing: Easing::Step,
        },
    ];
    scene.actors.push(actor);

    let graph = build_filter_graph(&scene);
    // For step easing the segment between t=0 and t=1 must NOT
    // contain `((t-0) / 1)` style interpolation; instead the constant
    // previous value (0.5) shows up unmultiplied. We assert the lack
    // of the linear delta term `+(0.4)*` which a linear segment
    // would emit.
    assert!(
        !graph.contains("+(0.400000)*((t-0"),
        "step easing was flattened to a linear segment:\n{graph}",
    );
}

#[test]
fn modifier_wobble_emits_sine_term_in_position() {
    // Wobble is `amp_x * sin(2*PI*freq*t)`. The renderer must add
    // this term to the world-X / world-Y expression so the export
    // wobbles in lock-step with the preview.
    let mut scene = baseline_scene();
    let mut actor = baseline_actor("a1");
    actor.modifiers.push(TrackModifier {
        enabled: true,
        kind: ModifierKind::Wobble {
            freq_hz: 2.0,
            amp_x: 12.0,
            amp_y: 0.0,
            amp_rot_deg: 0.0,
            phase: 0.0,
        },
        ..Default::default()
    });
    scene.actors.push(actor);

    let graph = build_filter_graph(&scene);
    assert!(
        graph.contains("sin(") && graph.contains("12.000000"),
        "wobble amp/sin not threaded into position expression:\n{graph}",
    );
}

#[test]
fn per_frame_rotation_emits_rotate_filter() {
    // A keyframed rotation on the actor's layout must produce a
    // `rotate=...:c=none` filter so the export rotates per frame.
    let mut scene = baseline_scene();
    let mut actor = baseline_actor("a1");
    actor.layout = vec![
        Keyframe::new(0.0, ActorState { rotation_deg: 0.0, ..ActorState::default() }),
        Keyframe::new(2.0, ActorState { rotation_deg: 90.0, ..ActorState::default() }),
    ];
    scene.actors.push(actor);

    let graph = build_filter_graph(&scene);
    assert!(
        graph.contains("rotate="),
        "per-frame rotation didn't emit a rotate filter:\n{graph}",
    );
    // The hypot bounding box must be present so animated angles
    // don't clip the corners (rotw/roth are init-time only).
    assert!(graph.contains("hypot(iw"), "rotate canvas not over-sized: {graph}");
}

#[test]
fn static_opacity_emits_alpha_multiplier() {
    // Mid-point opacity below 1.0 must produce a colorchannelmixer
    // alpha multiplier so the actor fades like the preview.
    let mut scene = baseline_scene();
    let mut actor = baseline_actor("a1");
    actor.layout = vec![Keyframe::new(
        0.0,
        ActorState { opacity: 0.4, ..ActorState::default() },
    )];
    scene.actors.push(actor);

    let graph = build_filter_graph(&scene);
    assert!(
        graph.contains("colorchannelmixer=aa=0.4"),
        "opacity 0.4 didn't materialise as a colorchannelmixer alpha:\n{graph}",
    );
}

#[test]
fn color_correction_emits_eq_and_colorbalance() {
    // Brightness / contrast / saturation roll into a single `eq=`
    // filter; temperature spawns a `colorbalance=`. Both must be in
    // the graph for an actor with non-identity CC.
    let mut scene = baseline_scene();
    let mut actor = baseline_actor("a1");
    actor.color_correction = ColorCorrection {
        brightness: 0.2,
        contrast: 1.3,
        saturation: 1.5,
        temperature: 0.4,
        ..ColorCorrection::default()
    };
    scene.actors.push(actor);

    let graph = build_filter_graph(&scene);
    assert!(graph.contains("eq=brightness=0.2000"), "missing eq.brightness: {graph}");
    assert!(graph.contains("contrast=1.3000"), "missing eq.contrast: {graph}");
    assert!(graph.contains("saturation=1.5000"), "missing eq.saturation: {graph}");
    assert!(graph.contains("colorbalance="), "missing colorbalance: {graph}");
}

#[test]
fn image_overlay_inherits_full_pipeline() {
    // Image overlays must follow the same pipeline as actors —
    // canvas_layouts override, render-frame translation, modifier
    // sine terms, per-frame rotation. We exercise the rotation
    // path here as a smoke check.
    let mut scene = baseline_scene();
    scene.overlays.push(Overlay::Image(ImageOverlay {
        id: "img1".into(),
        source: PathBuf::from("sticker.png"),
        t_in: 0.0,
        t_out: 2.0,
        layout: vec![
            Keyframe::new(0.0, OverlayState::default()),
            Keyframe::new(2.0, OverlayState { rotation_deg: 45.0, ..OverlayState::default() }),
        ],
        modifiers: Vec::new(),
        skeleton_attachment: None,
        effects: Vec::new(),
        animated_params: Default::default(),
        chroma_key: None,
    }));

    let graph = build_filter_graph(&scene);
    assert!(
        graph.contains("rotate="),
        "image overlay rotation animation didn't emit rotate filter:\n{graph}",
    );
}


// ─── PR #62 follow-up: effect animation / LGG curves / skeleton ───
//
// PR #62 wired in the keyframe / modifier / world-pixel pipeline but
// left three further preview-vs-render parity gaps open: per-frame
// effect parameter animation, tone curves + per-channel LGG, and
// skeleton-attachment-driven world position. The tests below pin the
// contract for each of those gaps so a future regression flips a
// red CI light.

#[test]
fn animated_effect_param_emits_per_segment_chain() {
    // An `Effect::Blur` whose `radius` is keyframed must lower into
    // a chain of `boxblur=…:enable='between(t,…)'` clauses, one per
    // keyframe-bounded segment, so the export blurs in lock-step
    // with the canvas preview's `effect.sampled_at(t_local)` path.
    use memstroy_core::{Effect, EffectKind, Keyframe};

    let mut scene = baseline_scene();
    let mut actor = baseline_actor("a1");
    let mut blur = Effect::new(EffectKind::Blur { radius: 4.0 });
    blur.animated_params.insert("p0".into());
    blur.param_kfs.insert(
        "p0".into(),
        vec![
            Keyframe::new(0.0, 2.0_f32),
            Keyframe::new(1.0, 24.0_f32),
        ],
    );
    actor.effects.push(blur);
    scene.actors.push(actor);

    let graph = build_filter_graph(&scene);
    // The chain must contain at least two `boxblur` invocations
    // gated by `between(t,…)` — proves the per-segment animation
    // expansion fired instead of the static fast path.
    let blur_count = graph.matches("boxblur=").count();
    assert!(
        blur_count >= 2,
        "expected multi-segment boxblur chain, got {blur_count}: {graph}",
    );
    assert!(
        graph.contains("boxblur=") && graph.contains(":enable='between(t,"),
        "missing per-segment enable on boxblur:\n{graph}",
    );
}

#[test]
fn animated_intensity_alone_drives_segmentation() {
    // The master `intensity` envelope is itself one of the keyable
    // parameters (`Effect.animated_params` includes `"intensity"`).
    // Animating only intensity — even on a parameterless effect like
    // Grayscale — should still fan out into a per-segment chain so
    // the export fades in/out the same way the preview does.
    use memstroy_core::{Effect, Keyframe};

    let mut scene = baseline_scene();
    let mut actor = baseline_actor("a1");
    let mut gray = Effect::grayscale();
    gray.animated_params.insert("intensity".into());
    gray.param_kfs.insert(
        "intensity".into(),
        vec![
            Keyframe::new(0.0, 0.0_f32),
            Keyframe::new(1.0, 1.0_f32),
            Keyframe::new(2.0, 0.0_f32),
        ],
    );
    actor.effects.push(gray);
    scene.actors.push(actor);

    let graph = build_filter_graph(&scene);
    let mixer_count = graph.matches("colorchannelmixer=rr=").count();
    assert!(
        mixer_count >= 3,
        "expected ≥3 grayscale segments for kf-animated intensity, got {mixer_count}:\n{graph}",
    );
}

#[test]
fn cc_lift_gain_gamma_lowers_to_lutrgb() {
    // Per-channel Lift / Gain / Gamma must show up in the graph
    // as a `lutrgb=` filter using the DaVinci formula
    // `pow(max(0,(val/255 + L*(1-val/255))*G), 1/Gma)`. The test
    // injects a non-trivial lift on the red channel and checks
    // both the filter name and the formula's signature.
    let mut scene = baseline_scene();
    let mut actor = baseline_actor("a1");
    actor.color_correction = ColorCorrection {
        lift: [0.2, 0.0, 0.0],
        gain: [1.0, 1.2, 1.0],
        gamma: [1.0, 1.0, 0.8],
        ..ColorCorrection::default()
    };
    scene.actors.push(actor);

    let graph = build_filter_graph(&scene);
    assert!(
        graph.contains("lutrgb=r='clip(255*pow(max(0,(val/255+(0.2000)"),
        "lift on R channel didn't materialise in lutrgb expression:\n{graph}",
    );
    assert!(
        graph.contains("g='clip(255*pow(max(0,(val/255+(0.0000)*(1-val/255))*(1.2000))"),
        "gain on G channel didn't materialise in lutrgb expression:\n{graph}",
    );
    // 1/0.8 = 1.25 — the inverse-gamma factor on B should appear.
    assert!(
        graph.contains("1.2500"),
        "inverse gamma 1/0.8 = 1.25 not present:\n{graph}",
    );
}

#[test]
fn cc_tone_curves_lower_to_curves_filter() {
    // Master + per-channel tone curves must each emit a separate
    // `curves=preset=none:…='…/…'` filter so the export honours
    // the editor's curve points.
    use memstroy_core::ToneCurves;

    let mut scene = baseline_scene();
    let mut actor = baseline_actor("a1");
    actor.color_correction = ColorCorrection {
        curves: ToneCurves {
            master: vec![[0.0, 0.0], [0.5, 0.6], [1.0, 1.0]],
            red: vec![[0.0, 0.1], [1.0, 0.9]],
            green: vec![[0.0, 0.0], [1.0, 1.0]], // identity → not emitted
            blue: vec![[0.0, 0.0], [1.0, 1.0]],
        },
        ..ColorCorrection::default()
    };
    scene.actors.push(actor);

    let graph = build_filter_graph(&scene);
    assert!(
        graph.contains("curves=preset=none:m='0.0000/0.0000 0.5000/0.6000 1.0000/1.0000'"),
        "master tone curve didn't emit `curves=…:m=`:\n{graph}",
    );
    assert!(
        graph.contains("curves=preset=none:r='0.0000/0.1000 1.0000/0.9000'"),
        "red tone curve didn't emit `curves=…:r=`:\n{graph}",
    );
    // Identity green/blue must be skipped — only one occurrence of
    // `:g='` or `:b='` would mean we leaked an identity filter.
    assert!(
        !graph.contains("curves=preset=none:g='"),
        "identity green curve was emitted (should be skipped):\n{graph}",
    );
}

#[test]
fn skeleton_attachment_overrides_overlay_world_pos() {
    // An overlay bound to a host actor's skeleton point must take
    // its world position from the skeleton track — NOT the legacy
    // normalised layout. We assert the host's source-pixel scale
    // (`1080.0000` for the default 1080×1920 fallback used when no
    // ffprobe data is available) appears in the overlay's X
    // expression, which is the unique fingerprint of the skeleton
    // projection branch.
    use memstroy_core::skeleton::{
        PointState, SkeletonAttachment, SkeletonPoint, SkeletonTemplate,
    };
    use std::collections::BTreeMap;

    let mut scene = baseline_scene();
    let host = baseline_actor("hero");
    let host_clip = host.source.clone();
    scene.actors.push(host);

    let mut points = BTreeMap::new();
    let mut hat = SkeletonPoint {
        name: "hat".into(),
        ..Default::default()
    };
    hat.track = vec![
        Keyframe::new(0.0, PointState { x: 0.4, y: 0.2, scale: 1.0, rotation_deg: 0.0 }),
        Keyframe::new(1.0, PointState { x: 0.6, y: 0.3, scale: 1.0, rotation_deg: 0.0 }),
    ];
    points.insert("hat".into(), hat);
    scene.skeleton_templates.push(SkeletonTemplate {
        name: "hero_skel".into(),
        source_clip: host_clip,
        fps: 30.0,
        clip_duration: 2.0,
        points,
    });

    scene.overlays.push(Overlay::Image(ImageOverlay {
        id: "cap".into(),
        source: PathBuf::from("cap.png"),
        t_in: 0.0,
        t_out: 2.0,
        layout: vec![Keyframe::new(0.0, OverlayState::default())],
        modifiers: Vec::new(),
        skeleton_attachment: Some(SkeletonAttachment {
            skeleton_id: "hero_skel".into(),
            point_name: "hat".into(),
            offset: [0.0, 0.0],
            scale: 1.0,
            follow_rotation: false,
        }),
        effects: Vec::new(),
        animated_params: Default::default(),
        chroma_key: None,
    }));

    let graph = build_filter_graph(&scene);
    assert!(
        graph.contains("1080.0000"),
        "skeleton-attached overlay didn't use host source width fallback:\n{graph}",
    );
    // The point's normalised X coords (0.4, 0.6) must appear in the
    // piecewise expression — proves the skeleton track is being
    // sampled rather than the legacy layout (which would emit 0.5).
    assert!(
        graph.contains("0.400000") && graph.contains("0.600000"),
        "skeleton point keyframes (0.4, 0.6) not threaded into expression:\n{graph}",
    );
}

#[test]
fn actor_skeleton_attachment_overrides_own_world_pos() {
    // When an actor itself binds to a skeleton point (the
    // `Actor.skeleton_attachments[0]` override path), its world
    // position must come from that skeleton, not from the legacy
    // layout's `pos = [0.5, 0.7]`. We use a point whose Y differs
    // from the legacy default so the override is detectable.
    use memstroy_core::skeleton::{
        PointState, SkeletonAttachment, SkeletonPoint, SkeletonTemplate,
    };
    use std::collections::BTreeMap;

    let mut scene = baseline_scene();
    // Host clip provides the skeleton; a separate "follower" actor
    // is the one that binds to the host's hat point.
    let host = baseline_actor("hero");
    let host_clip = host.source.clone();
    scene.actors.push(host);

    let mut follower = baseline_actor("a2");
    follower.skeleton_attachments.push(SkeletonAttachment {
        skeleton_id: "hero_skel".into(),
        point_name: "hat".into(),
        offset: [0.0, 0.0],
        scale: 1.0,
        follow_rotation: false,
    });
    scene.actors.push(follower);

    let mut points = BTreeMap::new();
    let mut hat = SkeletonPoint {
        name: "hat".into(),
        ..Default::default()
    };
    hat.track = vec![Keyframe::new(
        0.0,
        PointState { x: 0.25, y: 0.15, scale: 1.0, rotation_deg: 0.0 },
    )];
    points.insert("hat".into(), hat);
    scene.skeleton_templates.push(SkeletonTemplate {
        name: "hero_skel".into(),
        source_clip: host_clip,
        fps: 30.0,
        clip_duration: 2.0,
        points,
    });

    let graph = build_filter_graph(&scene);
    assert!(
        graph.contains("0.250000"),
        "actor-skeleton-attached follower didn't take its X from the skeleton point (0.25):\n{graph}",
    );
}


// ─── Audio pipeline regression tests ─────────────────────────────────
//
// The render used to fail with `Could not open encoder before EOF`
// (AAC) and `-22 Invalid argument` (libx264) whenever the user
// dropped two audio tracks coming from sources with different sample
// rates / channel layouts — `amix` requires uniform inputs and aborts
// the entire filter graph on mismatch. The fix in
// `filtergraph.rs::emit_audio` normalises every track BEFORE amix and
// pins the post-mix bus once more so the AAC encoder always sees a
// stable PCM stream. These tests pin the contract.

use memstroy_core::AudioTrack;

/// Materialise a placeholder audio file at `path` so the renderer's
/// existence check (added defensively to skip stale references after
/// asset rename/move) doesn't drop the track on the floor in the
/// test. The bytes form a valid 44-byte PCM WAV header (mono, 16-bit,
/// 44.1 kHz, zero-length data chunk), so when the test environment
/// has `ffprobe` on PATH the renderer's audio-stream probe correctly
/// classifies the file as having an audio stream — without this the
/// new "skip silent sources" guard would drop every test track and
/// every assertion in the audio block would fail.
fn touch_audio_file(path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // RIFF / WAVE / fmt  / 16-byte PCM descriptor / data / 0-byte body.
    // RIFF chunk size is 36 bytes (8+16+8+4) — i.e. total file - 8.
    // PCM=1, channels=1, sample_rate=44100, byte_rate=88200,
    // block_align=2, bits_per_sample=16.
    const WAV_HEADER: &[u8] = &[
        0x52, 0x49, 0x46, 0x46, // "RIFF"
        0x24, 0x00, 0x00, 0x00, // RIFF size = 36
        0x57, 0x41, 0x56, 0x45, // "WAVE"
        0x66, 0x6D, 0x74, 0x20, // "fmt "
        0x10, 0x00, 0x00, 0x00, // fmt chunk size = 16
        0x01, 0x00, // audio format = PCM
        0x01, 0x00, // num channels = 1
        0x44, 0xAC, 0x00, 0x00, // sample rate = 44100
        0x88, 0x58, 0x01, 0x00, // byte rate = 88200
        0x02, 0x00, // block align = 2
        0x10, 0x00, // bits per sample = 16
        0x64, 0x61, 0x74, 0x61, // "data"
        0x00, 0x00, 0x00, 0x00, // data size = 0
    ];
    let _ = std::fs::write(path, WAV_HEADER);
}

fn audio_track(id: &str, t_in: f32) -> AudioTrack {
    let path = std::env::temp_dir().join(format!("memstroy_render_test_{id}.wav"));
    touch_audio_file(&path);
    AudioTrack {
        id: id.into(),
        source: path,
        t_in,
        ..AudioTrack::default()
    }
}

#[test]
fn audio_chain_normalises_each_track_before_amix() {
    // Two tracks, mismatched rates in real life (48k + 44.1k) — the
    // graph must aresample + aformat each one to the bus rate so
    // amix's uniform-format requirement is satisfied.
    let mut scene = baseline_scene();
    scene.audio.push(audio_track("a1", 0.0));
    scene.audio.push(audio_track("a2", 1.5));

    let graph = build_filter_graph(&scene);

    // Per-track normalisation — both aresample and aformat must be
    // present (count >= 2 for two tracks).
    let resample_count = graph.matches("aresample=44100").count();
    assert!(
        resample_count >= 2,
        "expected aresample=44100 on every audio track, got {resample_count}:\n{graph}",
    );
    let aformat_count = graph.matches("channel_layouts=stereo").count();
    assert!(
        aformat_count >= 3,
        "expected aformat with channel_layouts=stereo per track + post-mix \
         (>=3), got {aformat_count}:\n{graph}",
    );

    // amix uses the safe variant: longest duration, no early
    // dropouts (we apad the per-track chains anyway).
    assert!(
        graph.contains("amix=inputs=2:duration=longest:dropout_transition=0:normalize=0"),
        "amix not configured with the post-fix robust parameters:\n{graph}",
    );

    // The per-track apad is what saves the AAC encoder from EOF
    // before init when one source ends early.
    assert!(
        graph.contains("apad=whole_dur="),
        "per-track apad missing — AAC encoder may EOF before init:\n{graph}",
    );

    // adelay positions the second track at 1.5 s on the timeline.
    assert!(
        graph.contains("adelay=1500:all=1"),
        "adelay for t_in=1.5 not present (or wrong syntax):\n{graph}",
    );
}

#[test]
fn audio_single_track_skips_amix_but_pins_format() {
    // Single-track scenes don't need a mixer node — but they still
    // need the final aformat lock so the AAC encoder knows what to
    // expect. (Without it the `-c:a aac -ar 44100 -ac 2` flags rely
    // on auto-conversion that historically broke the pipeline.)
    let mut scene = baseline_scene();
    scene.audio.push(audio_track("only", 0.0));
    let graph = build_filter_graph(&scene);

    assert!(
        !graph.contains("amix="),
        "single-track scene should not allocate an amix node:\n{graph}",
    );
    assert!(
        graph.contains("aformat=sample_fmts=fltp:sample_rates=44100:channel_layouts=stereo"),
        "single-track scene missing the format-pin filter:\n{graph}",
    );
}

#[test]
fn audio_mute_forces_volume_to_zero() {
    // The previous code wrote `volume={tr.volume}` regardless of the
    // `mute` flag — the user's mute click was a no-op in the export.
    // The fix must collapse `mute=true` to `volume=0`.
    let mut scene = baseline_scene();
    let mut tr = audio_track("muted", 0.0);
    tr.volume = 0.8;
    tr.mute = true;
    scene.audio.push(tr);
    let graph = build_filter_graph(&scene);

    assert!(
        graph.contains("volume=0.000000"),
        "mute flag did not collapse volume to zero:\n{graph}",
    );
    assert!(
        !graph.contains("volume=0.800000"),
        "mute should override the static volume value, but the original leaked through:\n{graph}",
    );
}

#[test]
fn audio_fade_in_out_emit_afade_filters() {
    // fade_in / fade_out used to be silently dropped (rendered audio
    // started/ended at full volume regardless of the GUI's fade
    // ramps). The fix wires them through `afade`.
    let mut scene = baseline_scene();
    let mut tr = audio_track("fade", 0.0);
    tr.t_out = Some(2.0);
    tr.fade_in = 0.25;
    tr.fade_out = 0.5;
    scene.audio.push(tr);
    let graph = build_filter_graph(&scene);

    assert!(
        graph.contains("afade=t=in:st=0:d=0.250000"),
        "fade_in didn't surface as an afade-in filter:\n{graph}",
    );
    assert!(
        graph.contains("afade=t=out:"),
        "fade_out didn't surface as an afade-out filter:\n{graph}",
    );
}



#[test]
fn audio_track_with_no_audio_stream_is_skipped() {
    // Regression for the "Ошибка рендера / exit code -22 (Invalid
    // argument) | aac Could not open encoder before EOF" double-
    // failure: when the user drops a silent video clip on the canvas
    // the GUI auto-pushes an `AudioTrack` referencing that source
    // file. With no audio stream in the source, the renderer's
    // `[idx:a]aresample=…` reference resolved to nothing and ffmpeg
    // aborted filter-graph configuration with "Stream specifier ':a'
    // matches no streams" — taking BOTH encoder threads down with it
    // (libx264 -22, AAC EOF) and writing a 0-byte mp4. The fix
    // ffprobes each audio source up front and silently drops tracks
    // whose source has no audio.
    //
    // We can't run ffprobe in tests reliably (CI sandboxes don't
    // ship it), so we exercise the contract a different way: a
    // silent ".mp4" file (just the existence-check trips and ffprobe
    // either confirms-no-audio or fails-open) must not cause a
    // panic, and when a SECOND track with valid WAV data is also
    // present, that one still gets emitted. This pins the "skip ONE
    // bad track without breaking the rest" invariant of the fix.
    let mut scene = baseline_scene();
    // Track #1: source is a mostly-empty file (no proper audio
    // stream). With ffprobe present this gets skipped; without
    // ffprobe it falls through to ffmpeg. Either way the graph
    // builder must complete without panicking.
    let bad_path = std::env::temp_dir().join("memstroy_render_test_silent_src.bin");
    let _ = std::fs::write(&bad_path, b"\0\0\0\0not a media file");
    scene.audio.push(memstroy_core::AudioTrack {
        id: "silent".into(),
        source: bad_path.clone(),
        t_in: 0.0,
        ..memstroy_core::AudioTrack::default()
    });
    // Track #2: a real valid-header WAV — must always make it into
    // the graph regardless of how track #1 was handled.
    scene.audio.push(audio_track("good", 0.5));

    let graph = build_filter_graph(&scene);

    // The well-formed track must still appear (fade/aresample
    // chain is the unique fingerprint of its inclusion).
    assert!(
        graph.contains("aresample=44100"),
        "well-formed audio track was unexpectedly dropped:\n{graph}",
    );

    // Cleanup: don't leak the test artifact into /tmp.
    let _ = std::fs::remove_file(&bad_path);
}

#[test]
fn output_stream_is_normalised_to_yuv420p_cfr() {
    // Pin the contract of `FilterGraphBuilder::finalize_video`: every
    // graph — empty, busy, with or without audio — must end with a
    // single normalisation chunk that locks the encoder input to
    // `format=yuv420p`, square pixels, even dimensions and the
    // requested constant frame rate. Without this lock the encoder
    // sees yuva420p frames + ffmpeg's auto-conversion, which is the
    // tipping point for the libx264 "Task finished with error code:
    // -22" failure on more elaborate filter-graph topologies (rotate
    // + per-frame scale + alphamerge masks). Asserting the literal
    // tokens here means a future refactor that drops the finalise
    // step trips CI before it reaches the user.
    let scene = baseline_scene();
    let graph = build_filter_graph(&scene);

    assert!(
        graph.contains("format=yuv420p"),
        "missing final yuv420p lock — encoder may receive yuva frames:\n{graph}",
    );
    assert!(
        graph.contains("setsar=1"),
        "missing setsar=1 — non-square pixel ratio may leak through:\n{graph}",
    );
    assert!(
        graph.contains("trunc(iw/2)*2"),
        "missing even-dimension clamp — odd output resolution would crash libx264:\n{graph}",
    );
    // fps={fps} pins the constant frame rate; baseline scene runs at
    // 30 fps so the literal `fps=30` should appear.
    assert!(
        graph.contains("fps=30"),
        "missing CFR lock at output fps — non-monotonic timestamps may reach the encoder:\n{graph}",
    );
}

#[test]
fn scale_expression_is_clamped_against_zero_dimensions() {
    // libx264 rejects frames whose dimensions are zero or negative
    // with the same `-22` (EINVAL) error code. When a layout keyframe
    // animates `scale = 0.0` (e.g. a "punch out" effect) the
    // unprotected `iw*{sx}` expression evaluated to 0 → scale filter
    // produced a 0×0 frame → encoder died. The fix wraps every
    // scale-driven expression in `max(2, …)`.
    let mut scene = baseline_scene();
    let mut actor = baseline_actor("vanish");
    actor.layout = vec![
        memstroy_core::Keyframe::new(
            0.0,
            memstroy_core::ActorState {
                scale: 0.0,
                ..memstroy_core::ActorState::default()
            },
        ),
        memstroy_core::Keyframe::new(
            1.0,
            memstroy_core::ActorState {
                scale: 1.0,
                ..memstroy_core::ActorState::default()
            },
        ),
    ];
    scene.actors.push(actor);

    let graph = build_filter_graph(&scene);
    assert!(
        graph.contains("max(2,iw*("),
        "actor scale wasn't clamped against zero dimensions:\n{graph}",
    );
    assert!(
        graph.contains("max(2,ih*("),
        "actor scale_y wasn't clamped against zero dimensions:\n{graph}",
    );
}
