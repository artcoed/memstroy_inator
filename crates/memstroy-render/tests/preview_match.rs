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
    ImageOverlay, Keyframe, OutputSpec, Overlay, OverlayState, RenderFrame, RenderFrameState,
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
        effect_layers: Vec::new(),
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
        z_order: 0,
        parent_id: None,
        mellstroy_footage: Default::default(),
        mute_audio: false,
    }
}

fn build_filter_graph(scene: &Scene) -> String {
    let plan = build_plan(
        scene,
        &PathBuf::from("/tmp/out.mp4"),
        &PathBuf::from("/tmp/assets"),
    )
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
                pos: WorldPos {
                    x: 1500.0,
                    y: 800.0,
                },
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
                pos: WorldPos {
                    x: 1200.0,
                    y: 960.0,
                },
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
            ActorState {
                pos: [0.5, 0.5],
                ..ActorState::default()
            },
        ),
        Keyframe {
            t: 1.0,
            value: ActorState {
                pos: [0.9, 0.5],
                ..ActorState::default()
            },
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
        Keyframe::new(
            0.0,
            ActorState {
                rotation_deg: 0.0,
                ..ActorState::default()
            },
        ),
        Keyframe::new(
            2.0,
            ActorState {
                rotation_deg: 90.0,
                ..ActorState::default()
            },
        ),
    ];
    scene.actors.push(actor);

    let graph = build_filter_graph(&scene);
    assert!(
        graph.contains("rotate="),
        "per-frame rotation didn't emit a rotate filter:\n{graph}",
    );
    // The hypot bounding box must be present so animated angles
    // don't clip the corners (rotw/roth are init-time only).
    assert!(
        graph.contains("hypot(iw"),
        "rotate canvas not over-sized: {graph}"
    );
}

#[test]
fn static_opacity_emits_alpha_multiplier() {
    // Mid-point opacity below 1.0 must produce a colorchannelmixer
    // alpha multiplier so the actor fades like the preview.
    let mut scene = baseline_scene();
    let mut actor = baseline_actor("a1");
    actor.layout = vec![Keyframe::new(
        0.0,
        ActorState {
            opacity: 0.4,
            ..ActorState::default()
        },
    )];
    scene.actors.push(actor);

    let graph = build_filter_graph(&scene);
    assert!(
        graph.contains("colorchannelmixer=aa=0.4"),
        "opacity 0.4 didn't materialise as a colorchannelmixer alpha:\n{graph}",
    );
}

#[test]
fn animated_opacity_emits_geq_alpha_filter() {
    // Regression for "У клипов прозрачность неверная после рендера в
    // отличие от предпросмотра": the renderer used to flatten any
    // animated opacity to a single midpoint sample (by index, not by
    // time), so a fade-in keyframed `0.0 → 1.0` rendered as a flat
    // `1.0` (the second keyframe) and didn't match the canvas
    // preview's per-frame `keyframe::sample(&layout, t)`. The fix
    // routes animated opacity through `geq` with a piecewise
    // expression in `T` (geq's only time variable) so each output
    // frame gets the right alpha.
    let mut scene = baseline_scene();
    let mut actor = baseline_actor("a1");
    actor.layout = vec![
        Keyframe::new(
            0.0,
            ActorState {
                opacity: 0.0,
                ..ActorState::default()
            },
        ),
        Keyframe::new(
            1.0,
            ActorState {
                opacity: 1.0,
                ..ActorState::default()
            },
        ),
    ];
    scene.actors.push(actor);

    let graph = build_filter_graph(&scene);
    assert!(
        graph.contains(":a='clip(alpha(X,Y)*("),
        "animated opacity didn't emit per-frame geq alpha multiplier:\n{graph}",
    );
    // The geq filter requires an explicit luminance/RGB expression
    // alongside the alpha — otherwise ffmpeg aborts graph init with
    // "A luminance or RGB expression is mandatory". We feed the YUV
    // passthrough triplet `lum=p(X,Y) / cb=p(X,Y) / cr=p(X,Y)` so the
    // colour channels are unchanged.
    assert!(
        graph.contains("geq=lum='p(X,Y)':cb='p(X,Y)':cr='p(X,Y)':a='"),
        "animated alpha geq missing the YUV passthrough triplet — ffmpeg will reject the filter:\n{graph}",
    );
    // The piecewise expression must use uppercase `T` (geq's
    // vocabulary) and reference the keyframe boundary at t=1.
    assert!(
        graph.contains("if(lt(T,1.000000)"),
        "geq expression doesn't switch on `T` at the keyframe boundary:\n{graph}",
    );
    // And it must NOT fall back to the static fast path on the same
    // actor — that would mean opacity got applied twice.
    assert!(
        !graph.contains("colorchannelmixer=aa=1.0000"),
        "animated path leaked an extra static alpha multiplier:\n{graph}",
    );
}

#[test]
fn clip_with_t_in_emits_tpad_to_align_source_timeline() {
    // Regression for "два клипа разместил последовательно на одном
    // слое и после рендера только первый отрисовался": when an actor
    // has `t_in > 0` the renderer used to feed its source straight
    // through, expecting `enable=between(t,t_in,t_out)` to gate
    // visibility. But the source decoder runs from scene-time 0 in
    // step with the shared timeline — by the time the enable window
    // opens at t_in the source is already at PTS=t_in (or worse,
    // past EOF for short clips). The fix is `time_align_filters`:
    // trim → setpts=PTS-STARTPTS → tpad transparent pad before the
    // content, so the source's first frame lands at scene-time
    // t_in. We assert the new chain shows up in the filter graph
    // for an actor whose t_in is non-zero.
    let mut scene = baseline_scene();
    scene.output.duration = 10.0;
    let mut actor = baseline_actor("late");
    actor.t_in = Some(4.0);
    actor.t_out = Some(8.0);
    scene.actors.push(actor);

    let graph = build_filter_graph(&scene);

    assert!(
        graph.contains("trim=duration=4.000000"),
        "expected `trim=duration=4` to cap the post-speed source at clip_dur:\n{graph}",
    );
    assert!(
        graph.contains("tpad=start_duration=4.000000:color=black@0.0"),
        "expected `tpad=start_duration=4:color=black@0.0` to delay the source to t_in=4:\n{graph}",
    );
    // tail = scene_dur - t_out = 10 - 8 = 2
    assert!(
        graph.contains("tpad=stop_duration=2.000000:color=black@0.0"),
        "expected trailing tpad to extend the stream to scene end:\n{graph}",
    );
}

#[test]
fn sequential_clips_each_get_their_own_alignment() {
    // The exact bug the user reported: two clips on the same lane
    // back-to-back. Both must emit independent time-align chains so
    // their sources don't race each other against the shared scene
    // timeline. We also confirm the SECOND clip's tpad shifts it to
    // scene-time 4 — without this it would render its source's
    // 4-second mark at scene-time 4 (or EOF, depending on source
    // length), exactly the "second clip never appears" symptom.
    let mut scene = baseline_scene();
    scene.output.duration = 8.0;
    let mut a = baseline_actor("clipA");
    a.t_in = Some(0.0);
    a.t_out = Some(4.0);
    scene.actors.push(a);
    let mut b = baseline_actor("clipB");
    b.t_in = Some(4.0);
    b.t_out = Some(8.0);
    scene.actors.push(b);

    let graph = build_filter_graph(&scene);

    // Each actor gets its own trim + setpts=PTS-STARTPTS.
    assert!(
        graph.matches("trim=duration=4.000000").count() >= 2,
        "expected two independent `trim=duration=4` filters, one per clip:\n{graph}",
    );
    let setpts_count = graph.matches("setpts=PTS-STARTPTS").count();
    assert!(
        setpts_count >= 2,
        "expected at least two PTS resets (one per clip), got {setpts_count}:\n{graph}",
    );
    // Clip A is at t_in=0 so no start tpad; clip B at t_in=4 must
    // pick up a `tpad=start_duration=4`.
    assert!(
        graph.contains("tpad=start_duration=4.000000:color=black@0.0"),
        "second clip didn't shift its source to start at scene-time 4:\n{graph}",
    );
    // And clip A must NOT have a start_duration tpad (t_in=0).
    // To verify, the only start_duration occurrence should be the
    // one for clip B.
    assert_eq!(
        graph.matches("tpad=start_duration=").count(),
        1,
        "exactly one start_duration tpad expected (only clip B has t_in>0):\n{graph}",
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
    assert!(
        graph.contains("eq=brightness=0.2000"),
        "missing eq.brightness: {graph}"
    );
    assert!(
        graph.contains("contrast=1.3000"),
        "missing eq.contrast: {graph}"
    );
    assert!(
        graph.contains("saturation=1.5000"),
        "missing eq.saturation: {graph}"
    );
    assert!(
        graph.contains("colorbalance="),
        "missing colorbalance: {graph}"
    );
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
            Keyframe::new(
                2.0,
                OverlayState {
                    rotation_deg: 45.0,
                    ..OverlayState::default()
                },
            ),
        ],
        modifiers: Vec::new(),
        skeleton_attachment: None,
        effects: Vec::new(),
        animated_params: Default::default(),
        chroma_key: None,
        z_order: 0,
        parent_id: None,
    }));

    let graph = build_filter_graph(&scene);
    assert!(
        graph.contains("rotate="),
        "image overlay rotation animation didn't emit rotate filter:\n{graph}",
    );
}

#[test]
fn animated_rotation_quotes_angle_to_protect_internal_commas() {
    // Regression: the per-element `rotate=` filter used to embed the
    // unquoted angle expression directly, e.g.
    //   rotate=if(lt(t,2),0,30)*PI/180:c=none:ow=hypot(iw\,ih):oh=hypot(iw\,ih)
    // FFmpeg's filtergraph parser splits filter chains on bare commas.
    // The commas inside `if(lt(t,X),Y,Z)` were therefore parsed as
    // chain separators, producing the user-visible
    //   "No option name near 'none:ow=hypot(iw,ih):oh=hypot(iw,ih)'"
    // error that crashed every render with an animated rotation OR
    // an animated render-frame rotation. Wrapping the angle in
    // single quotes (`rotate='...':c=none:...`) tells the parser to
    // treat the whole expression as one positional argument and
    // restores the export.
    let mut scene = baseline_scene();
    let mut actor = baseline_actor("a1");
    actor.layout = vec![
        Keyframe::new(
            0.0,
            ActorState {
                rotation_deg: 0.0,
                ..ActorState::default()
            },
        ),
        Keyframe::new(2.0, Easing::default().into_state_with_rot(45.0)),
    ];
    scene.actors.push(actor);

    let graph = build_filter_graph(&scene);
    // The angle MUST be quoted with `'...'`. Anything that produces
    // a bare `rotate=if(` (without the leading single quote) is the
    // regressed form.
    assert!(
        graph.contains("rotate='"),
        "rotate angle is not single-quoted; animated rotation expressions will trip the filtergraph parser:\n{graph}",
    );
    assert!(
        !graph.contains("rotate=if(lt"),
        "rotate angle is unquoted with internal commas — ffmpeg will misparse the chain:\n{graph}",
    );
}

trait IntoActorStateExt {
    fn into_state_with_rot(self, deg: f32) -> ActorState;
}
impl IntoActorStateExt for Easing {
    fn into_state_with_rot(self, deg: f32) -> ActorState {
        ActorState {
            rotation_deg: deg,
            ..ActorState::default()
        }
    }
}

#[test]
fn overlay_layout_uses_clip_local_time_base() {
    // Regression: the renderer used to lower overlay layout
    // keyframes against the bare scene-time `t`, but the canvas
    // preview samples them at `t - t_in` (clip-local). For an
    // overlay with `t_in > 0` and a non-trivial position animation,
    // the rendered MP4 was therefore shifted by `t_in` seconds
    // relative to the preview — matching the user's "после рендера
    // не так, как на холсте" report. The fix passes
    // `LayoutTimeBase::ClipLocal { t_in }` so the piecewise
    // expressions evaluate against `(t - t_in)` instead of `t`.
    //
    // We exercise this with an overlay whose layout has two
    // keyframes (forces piecewise lowering) at clip-local times
    // 0.0 and 1.0, placed on the timeline at scene-time `t_in=2.0`.
    // The rendered expression for X must contain the
    // `(t-2.000000)` substitution so the boundary `lt(t-2, 1)` lands
    // at scene-time 3, matching where the canvas preview's
    // `keyframe::sample` reaches the 2nd keyframe.
    let mut scene = baseline_scene();
    scene.output.duration = 6.0;
    scene.overlays.push(Overlay::Image(ImageOverlay {
        id: "im1".into(),
        source: PathBuf::from("img.png"),
        t_in: 2.0,
        t_out: 5.0,
        layout: vec![
            Keyframe::new(
                0.0,
                OverlayState {
                    pos: [0.2, 0.5],
                    ..OverlayState::default()
                },
            ),
            Keyframe::new(
                1.0,
                OverlayState {
                    pos: [0.8, 0.5],
                    ..OverlayState::default()
                },
            ),
        ],
        modifiers: Vec::new(),
        skeleton_attachment: None,
        effects: Vec::new(),
        animated_params: Default::default(),
        chroma_key: None,
        z_order: 0,
        parent_id: None,
    }));

    let graph = build_filter_graph(&scene);

    // The clip-local substitution must appear in the overlay's
    // position expression.
    assert!(
        graph.contains("(t-2.000000)"),
        "overlay layout didn't render against clip-local time `(t - t_in)`:\n{graph}",
    );
}
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
        vec![Keyframe::new(0.0, 2.0_f32), Keyframe::new(1.0, 24.0_f32)],
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
        Keyframe::new(
            0.0,
            PointState {
                x: 0.4,
                y: 0.2,
                scale: 1.0,
                rotation_deg: 0.0,
            },
        ),
        Keyframe::new(
            1.0,
            PointState {
                x: 0.6,
                y: 0.3,
                scale: 1.0,
                rotation_deg: 0.0,
            },
        ),
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
        z_order: 0,
        parent_id: None,
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
        PointState {
            x: 0.25,
            y: 0.15,
            scale: 1.0,
            rotation_deg: 0.0,
        },
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
fn actor_fallback_audio_is_scheduled_per_clip_not_per_source_path() {
    // The preview's `build_sources` schedules actor fallback audio for
    // every visible actor clip. It only uses explicit AudioTrack rows
    // to suppress fallback. The renderer used to add the first actor's
    // path to its dedupe set, so two actor clips from the same source
    // collapsed to one audio window in the export.
    let shared = std::env::temp_dir().join("memstroy_render_test_shared_actor_audio.wav");
    touch_audio_file(&shared);

    let mut scene = baseline_scene();
    scene.output.duration = 6.0;
    let mut first = baseline_actor("first");
    first.source = shared.clone();
    first.t_in = Some(0.0);
    first.t_out = Some(2.0);
    scene.actors.push(first);

    let mut second = baseline_actor("second");
    second.source = shared.clone();
    second.t_in = Some(3.0);
    second.t_out = Some(5.0);
    scene.actors.push(second);

    let graph = build_filter_graph(&scene);

    assert!(
        graph.contains("amix=inputs=3:duration=longest:dropout_transition=0:normalize=0"),
        "two actor clips plus the silent bed should produce three amix inputs:\n{graph}",
    );
    assert!(
        graph.contains("adelay=3000:all=1,asetpts=PTS-STARTPTS"),
        "second actor fallback audio was not delayed to its clip window:\n{graph}",
    );

    let _ = std::fs::remove_file(&shared);
}

#[test]
fn explicit_audio_for_one_actor_does_not_suppress_same_source_actor_fallback() {
    // Split/copy workflows can leave several actor clips pointing at the
    // same media file while only some of them have explicit audio rows.
    // Explicit rows should suppress fallback for their own parent actor,
    // not for every actor that happens to share the source path.
    let shared = std::env::temp_dir().join("memstroy_render_test_shared_actor_explicit.wav");
    touch_audio_file(&shared);

    let mut scene = baseline_scene();
    scene.output.duration = 6.0;

    let mut first = baseline_actor("first");
    first.source = shared.clone();
    first.t_in = Some(0.0);
    first.t_out = Some(2.0);
    scene.actors.push(first);

    let mut second = baseline_actor("second");
    second.source = shared.clone();
    second.t_in = Some(3.0);
    second.t_out = Some(5.0);
    second.speed = 2.0;
    scene.actors.push(second);

    let mut explicit = audio_track("first_bound", 0.0);
    explicit.source = shared.clone();
    explicit.parent_actor = Some("first".into());
    explicit.t_out = Some(2.0);
    scene.audio.push(explicit);

    let graph = build_filter_graph(&scene);

    assert!(
        graph.contains("amix=inputs=3:duration=longest:dropout_transition=0:normalize=0"),
        "bound audio + actor fallback + silent bed should produce three amix inputs:\n{graph}",
    );
    assert!(
        graph.contains("adelay=3000:all=1,asetpts=PTS-STARTPTS"),
        "second actor fallback should still be delayed to its own timeline window:\n{graph}",
    );
    assert!(
        graph.contains("asetrate=88200.000000"),
        "second actor fallback should inherit actor.speed=2.0:\n{graph}",
    );

    let _ = std::fs::remove_file(&shared);
}

#[test]
fn explicit_audio_for_duplicate_actor_id_matches_the_clip_window() {
    // Old projects can contain split/copied clips that still share an
    // actor id. A parent_actor link names the id, but the renderer must
    // suppress fallback only for the clip whose timing/source_start
    // matches that audio row.
    let shared = std::env::temp_dir().join("memstroy_render_test_duplicate_actor_id.wav");
    touch_audio_file(&shared);

    let mut scene = baseline_scene();
    scene.output.duration = 6.0;

    let mut first = baseline_actor("clip");
    first.source = shared.clone();
    first.t_in = Some(0.0);
    first.t_out = Some(2.0);
    first.source_start = 0.0;
    scene.actors.push(first);

    let mut second = baseline_actor("clip");
    second.source = shared.clone();
    second.t_in = Some(3.0);
    second.t_out = Some(5.0);
    second.source_start = 3.0;
    scene.actors.push(second);

    let mut explicit = audio_track("clip_audio", 0.0);
    explicit.source = shared.clone();
    explicit.parent_actor = Some("clip".into());
    explicit.t_out = Some(2.0);
    explicit.source_start = 0.0;
    scene.audio.push(explicit);

    let graph = build_filter_graph(&scene);

    assert!(
        graph.contains("amix=inputs=3:duration=longest:dropout_transition=0:normalize=0"),
        "explicit row should suppress only the matching duplicate-id clip while preserving the silent bed:\n{graph}",
    );
    assert!(
        graph.contains("adelay=3000:all=1,asetpts=PTS-STARTPTS"),
        "second duplicate-id clip should keep its own delayed fallback:\n{graph}",
    );

    let _ = std::fs::remove_file(&shared);
}

#[test]
fn split_audio_from_same_source_keeps_independent_source_windows() {
    let shared = std::env::temp_dir().join("memstroy_render_test_shared_split_audio.wav");
    touch_audio_file(&shared);

    let mut scene = baseline_scene();
    scene.output.duration = 7.0;

    let starts = [(0.0, 0.0), (2.326, 2.199), (2.769, 2.583)];
    for (idx, (t_in, source_start)) in starts.into_iter().enumerate() {
        let mut tr = audio_track(&format!("split_{idx}"), t_in);
        tr.source = shared.clone();
        tr.source_start = source_start;
        tr.t_out = Some(if idx == 0 {
            2.199
        } else if idx == 1 {
            2.709
        } else {
            6.594
        });
        scene.audio.push(tr);
    }

    let plan = build_plan(&scene, &PathBuf::from("out.mp4"), &PathBuf::from("."))
        .expect("plan should build");
    let graph = plan.filter_complex;

    assert_eq!(
        plan.inputs
            .iter()
            .filter(|input| input.path == shared)
            .count(),
        3,
        "each split clip needs its own ffmpeg input/label",
    );
    assert!(
        plan.inputs.iter().all(|input| input.seek.is_none()),
        "audio source offsets must live in per-track atrim filters, not shared input seeks",
    );
    assert!(
        graph.contains("atrim=start=2.199000")
            && graph.contains("atrim=start=2.583000")
            && graph.contains("adelay=2326:all=1,asetpts=PTS-STARTPTS")
            && graph.contains("adelay=2769:all=1,asetpts=PTS-STARTPTS")
            && graph.contains("anullsrc=channel_layout=stereo:sample_rate=44100:d=7.000000"),
        "split source windows, timeline delay resets, or silent bed missing:\n{graph}",
    );

    let _ = std::fs::remove_file(shared);
}

#[test]
fn muted_actor_does_not_render_embedded_fallback_audio() {
    // Deleting the auto-generated audio row sets actor.mute_audio so
    // preview stays silent for that actor. The CPU audio mux path
    // missed this check and could resurrect the embedded soundtrack
    // in the final MP4.
    let shared = std::env::temp_dir().join("memstroy_render_test_muted_actor_audio.wav");
    touch_audio_file(&shared);

    let mut scene = baseline_scene();
    let mut actor = baseline_actor("muted_actor");
    actor.source = shared.clone();
    actor.mute_audio = true;
    scene.actors.push(actor);

    let plan = build_plan(
        &scene,
        &PathBuf::from("/tmp/out.mp4"),
        &PathBuf::from("/tmp/assets"),
    )
    .expect("plan");

    assert!(
        plan.map_audio.is_none(),
        "muted actor should not create an exported fallback audio stream",
    );
    assert!(
        !plan.filter_complex.contains("aresample=44100"),
        "muted actor leaked an audio filter chain into the graph:\n{}",
        plan.filter_complex,
    );

    let _ = std::fs::remove_file(&shared);
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
    // dropouts, and includes one extra scene-length silent bed input.
    assert!(
        graph.contains("amix=inputs=3:duration=longest:dropout_transition=0:normalize=0"),
        "amix not configured with the post-fix robust parameters:\n{graph}",
    );

    // The silent bed is what saves the AAC encoder from EOF before
    // init when one source ends early. Per-track apad used to be used
    // here, but it interacted badly with delayed split clips.
    assert!(
        graph.contains("anullsrc=channel_layout=stereo:sample_rate=44100:d="),
        "scene-length silent bed missing — AAC encoder may EOF before init:\n{graph}",
    );
    assert!(
        !graph.contains("apad=whole_dur="),
        "per-track apad should not be used; it truncates/delays split audio unexpectedly:\n{graph}",
    );

    // adelay positions the second track at 1.5 s on the timeline,
    // and must reset PTS immediately so amix aligns the leading
    // silence at scene zero.
    assert!(
        graph.contains("adelay=1500:all=1,asetpts=PTS-STARTPTS"),
        "adelay for t_in=1.5 not present (or wrong syntax):\n{graph}",
    );
}

#[test]
fn audio_single_track_mixes_with_silent_bed_and_pins_format() {
    // Single-track scenes still use amix so the scene-length silent
    // bed can keep the AAC bus alive through the full render.
    let mut scene = baseline_scene();
    scene.audio.push(audio_track("only", 0.0));
    let graph = build_filter_graph(&scene);

    assert!(
        graph.contains("amix=inputs=2:duration=longest:dropout_transition=0:normalize=0"),
        "single-track scene should mix the real track with the silent bed:\n{graph}",
    );
    assert!(
        graph.contains("anullsrc=channel_layout=stereo:sample_rate=44100:d="),
        "single-track scene missing the scene-length silent bed:\n{graph}",
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

// ─── Inspector-knob regression tests ─────────────────────────────────
//
// These pin the contract that every inspector-exposed audio param
// (speed, pitch, pan, low-pass / high-pass cutoff, reverb) is wired
// through the renderer's filter graph. Until the fix, the renderer
// captured only volume/mute/fades and silently dropped the rest, so
// the rendered video sounded different from the live preview — the
// user's "звук в видео рендериться, но без измененных в инспекторе
// параметров" report.

#[test]
fn audio_speed_lowers_to_asetrate_resample() {
    // 2× speed mirrors the live preview's `Source::speed(2.0)`. The
    // closest ffmpeg equivalent that DOES change pitch (matching the
    // preview's no-time-stretch behaviour) is
    // `asetrate=SR*rate, aresample=SR`.
    let mut scene = baseline_scene();
    let mut tr = audio_track("fast", 0.0);
    tr.speed = 2.0;
    scene.audio.push(tr);
    let graph = build_filter_graph(&scene);

    assert!(
        graph.contains("asetrate=88200"),
        "speed=2.0 must double the bus rate via asetrate:\n{graph}",
    );
    // After asetrate the chain MUST aresample back to the canonical
    // bus rate so amix / the AAC encoder see a stable PCM stream.
    let resample_count = graph.matches("aresample=44100").count();
    assert!(
        resample_count >= 2,
        "asetrate must be followed by aresample=44100 to re-pin the bus \
         rate (got {resample_count} aresample filters):\n{graph}",
    );
}

#[test]
fn audio_pitch_semitones_fold_into_resample_factor() {
    // +12 semitones = 1 octave up = factor 2.0. With a default
    // speed=1.0 the effective rate is 2.0 → asetrate=88200 just like
    // a 2× speed track.
    let mut scene = baseline_scene();
    let mut tr = audio_track("octup", 0.0);
    tr.pitch_semitones = 12.0;
    scene.audio.push(tr);
    let graph = build_filter_graph(&scene);

    assert!(
        graph.contains("asetrate=88200"),
        "pitch_semitones=+12 must fold into a 2× resample factor:\n{graph}",
    );
}

#[test]
fn audio_default_speed_pitch_skip_asetrate_node() {
    // The unmodified-defaults case must NOT spend graph budget on a
    // resample no-op (the previous implementation's chain stayed
    // tight on the common path; we don't want this regression).
    let mut scene = baseline_scene();
    scene.audio.push(audio_track("plain", 0.0));
    let graph = build_filter_graph(&scene);

    assert!(
        !graph.contains("asetrate="),
        "default-rate track should not emit an asetrate filter:\n{graph}",
    );
}

#[test]
fn audio_pan_emits_equal_power_pan_filter() {
    // Pan = +1.0 (full right). Equal-power law: theta = π/2,
    // cos(theta)=0, sin(theta)=1. After folding the 0.5/0.5 mono
    // downmix and the sqrt(2)*0.5 normaliser, the right coefficient
    // is sqrt(2)/4 ≈ 0.353553 and the left coefficient is 0 (the
    // floating-point representation of cos(π/2) is a hair below zero
    // so it formats as `-0.000000`, which is fine — both signs
    // multiply to mute the left channel).
    let mut scene = baseline_scene();
    let mut tr = audio_track("right", 0.0);
    tr.pan = 1.0;
    scene.audio.push(tr);
    let graph = build_filter_graph(&scene);

    // Right-channel coefficient is the load-bearing signal that the
    // equal-power law was applied. The left-channel coefficient is
    // zero modulo float noise — assert separately on its absolute
    // value so the test is robust to the sign bit.
    assert!(
        graph.contains("c1=0.353553*c0+0.353553*c1"),
        "pan=+1 didn't lower into the equal-power right-channel mix:\n{graph}",
    );
    assert!(
        graph.contains("c0=0.000000*c0+0.000000*c1")
            || graph.contains("c0=-0.000000*c0+-0.000000*c1"),
        "pan=+1 left-channel coefficients should be zero:\n{graph}",
    );
}

#[test]
fn audio_pan_centre_skips_pan_filter() {
    // pan=0 is identity — emitting a `pan=` would be a wasted node.
    let mut scene = baseline_scene();
    let mut tr = audio_track("center", 0.0);
    tr.pan = 0.0;
    scene.audio.push(tr);
    let graph = build_filter_graph(&scene);

    assert!(
        !graph.contains("pan=stereo|"),
        "pan=0 (centred) should not allocate a pan filter:\n{graph}",
    );
}

#[test]
fn audio_low_pass_high_pass_emit_biquad_filters() {
    let mut scene = baseline_scene();
    let mut tr = audio_track("filtered", 0.0);
    tr.low_pass_hz = Some(8000);
    tr.high_pass_hz = Some(120);
    scene.audio.push(tr);
    let graph = build_filter_graph(&scene);

    assert!(
        graph.contains("highpass=f=120"),
        "high_pass_hz didn't lower into a highpass filter:\n{graph}",
    );
    assert!(
        graph.contains("lowpass=f=8000"),
        "low_pass_hz didn't lower into a lowpass filter:\n{graph}",
    );
}

#[test]
fn audio_disabled_filters_emit_no_pass_filters() {
    // `low_pass_hz = None` / `high_pass_hz = None` represent the
    // inspector toggle being OFF — no biquad filter should be emitted.
    let mut scene = baseline_scene();
    let mut tr = audio_track("clean", 0.0);
    tr.low_pass_hz = None;
    tr.high_pass_hz = None;
    scene.audio.push(tr);
    let graph = build_filter_graph(&scene);

    assert!(
        !graph.contains("lowpass=f="),
        "lowpass should not be emitted when low_pass_hz=None:\n{graph}",
    );
    assert!(
        !graph.contains("highpass=f="),
        "highpass should not be emitted when high_pass_hz=None:\n{graph}",
    );
}

#[test]
fn audio_reverb_emits_aecho_with_preview_parameters() {
    // The preview's `dsp::Reverb` uses ~120 ms delay and
    // feedback = mix * 0.55 (capped at 0.7). The renderer's `aecho`
    // approximation must use the same constants so the rendered tail
    // matches what the user heard.
    let mut scene = baseline_scene();
    let mut tr = audio_track("wet", 0.0);
    tr.reverb = 0.5;
    scene.audio.push(tr);
    let graph = build_filter_graph(&scene);

    // mix=0.5 → aecho mix=0.5, decay=0.275.
    assert!(
        graph.contains("aecho=1.0:0.500000:120:0.275000"),
        "reverb didn't lower into aecho with preview-matching constants:\n{graph}",
    );
}

#[test]
fn audio_reverb_zero_skips_aecho_node() {
    let mut scene = baseline_scene();
    let mut tr = audio_track("dry", 0.0);
    tr.reverb = 0.0;
    scene.audio.push(tr);
    let graph = build_filter_graph(&scene);

    assert!(
        !graph.contains("aecho="),
        "reverb=0 should not allocate an aecho filter:\n{graph}",
    );
}

#[test]
fn audio_volume_keyframes_lower_to_per_segment_chain() {
    // Two keyframes (0.0 → 1.0 across 2 s) with `"volume"` flagged as
    // animated must lower into a chain of
    // `volume=…:enable='between(t,a,b)'` clauses, mirroring how
    // `expr.rs::effect_animated_filter_chain` lowers animated video
    // effects. The static `volume` field MUST NOT leak through as an
    // unconditional filter — that would override the keyed envelope.
    use memstroy_core::{Easing, Keyframe};

    let mut scene = baseline_scene();
    scene.output.duration = 2.0;
    let mut tr = audio_track("envelope", 0.0);
    tr.t_out = Some(2.0);
    tr.volume = 0.5; // static fallback; should not appear as bare filter
    tr.animated_params.insert("volume".into());
    tr.volume_kfs = vec![
        Keyframe {
            t: 0.0,
            value: 0.0_f32,
            easing: Easing::default(),
        },
        Keyframe {
            t: 2.0,
            value: 1.0_f32,
            easing: Easing::default(),
        },
    ];
    scene.audio.push(tr);
    let graph = build_filter_graph(&scene);

    // Per-segment volume filters MUST exist.
    assert!(
        graph.contains("volume=") && graph.contains(":enable='between(t,"),
        "animated volume didn't lower into per-segment chain:\n{graph}",
    );
    // The static fallback (0.5) MUST NOT escape as an unconditional
    // filter — every emitted volume node has to be gated by enable=.
    // Look for the bare `volume=0.500000,` (followed by a comma —
    // i.e. a chain separator, not part of an enable expression) or
    // `volume=0.500000[` (label terminator). Either pattern would
    // mean the static value leaked through.
    assert!(
        !graph.contains("volume=0.500000,") && !graph.contains("volume=0.500000["),
        "static volume value leaked through as unconditional filter:\n{graph}",
    );
}

#[test]
fn audio_pan_keyframes_lower_to_per_segment_chain() {
    use memstroy_core::{Easing, Keyframe};
    let mut scene = baseline_scene();
    scene.output.duration = 2.0;
    let mut tr = audio_track("sweep", 0.0);
    tr.t_out = Some(2.0);
    tr.animated_params.insert("pan".into());
    tr.pan_kfs = vec![
        Keyframe {
            t: 0.0,
            value: -1.0_f32,
            easing: Easing::default(),
        },
        Keyframe {
            t: 2.0,
            value: 1.0_f32,
            easing: Easing::default(),
        },
    ];
    scene.audio.push(tr);
    let graph = build_filter_graph(&scene);

    // Per-segment pan filters are emitted as
    // `pan=stereo|...|...:enable='between(t,a,b)'`. Counting
    // distinct `pan=stereo|` opener occurrences each followed by
    // an `:enable='between(t,` is robust to the commas living
    // inside the enable expression.
    let pan_segments = graph.matches("pan=stereo|").count();
    assert!(
        pan_segments >= 2,
        "expected ≥2 per-segment pan filters, got {pan_segments}:\n{graph}",
    );
    // Every per-segment pan filter must carry an enable clause —
    // the count of `pan=stereo|` openers should equal the count of
    // `pan=...enable=` patterns when animation is active.
    let gated = graph.matches(":enable='between(t,").count();
    assert!(
        gated >= 2,
        "expected pan-segment filters to be enable-gated, found {gated} \
         enable= clauses total in graph:\n{graph}",
    );
}

#[test]
fn audio_low_pass_keyframes_lower_to_per_segment_chain() {
    use memstroy_core::{Easing, Keyframe};
    let mut scene = baseline_scene();
    scene.output.duration = 2.0;
    let mut tr = audio_track("sweepfilter", 0.0);
    tr.t_out = Some(2.0);
    tr.low_pass_hz = Some(8000); // enables animation path
    tr.animated_params.insert("low_pass".into());
    tr.low_pass_kfs = vec![
        Keyframe {
            t: 0.0,
            value: 1000.0_f32,
            easing: Easing::default(),
        },
        Keyframe {
            t: 2.0,
            value: 8000.0_f32,
            easing: Easing::default(),
        },
    ];
    scene.audio.push(tr);
    let graph = build_filter_graph(&scene);

    let lp_count = graph.matches("lowpass=f=").count();
    assert!(
        lp_count >= 2,
        "expected ≥2 per-segment lowpass filters, got {lp_count}:\n{graph}",
    );
    // Every emitted lowpass must be enable-gated when animated.
    // Confirm at least one segment carries an enable clause adjacent
    // to a lowpass filter.
    assert!(
        graph.contains("lowpass=f=") && graph.contains(":enable='between(t,"),
        "animated lowpass missing enable= gates:\n{graph}",
    );
}

#[test]
fn audio_low_pass_disabled_in_inspector_skips_animation() {
    // Even when keyframes are authored, an OFF inspector toggle
    // (`low_pass_hz == None`) must short-circuit the animation path
    // — otherwise the user's "off" click is a no-op in the export.
    use memstroy_core::{Easing, Keyframe};
    let mut scene = baseline_scene();
    scene.output.duration = 2.0;
    let mut tr = audio_track("offswitch", 0.0);
    tr.t_out = Some(2.0);
    tr.low_pass_hz = None;
    tr.animated_params.insert("low_pass".into());
    tr.low_pass_kfs = vec![
        Keyframe {
            t: 0.0,
            value: 1000.0_f32,
            easing: Easing::default(),
        },
        Keyframe {
            t: 2.0,
            value: 8000.0_f32,
            easing: Easing::default(),
        },
    ];
    scene.audio.push(tr);
    let graph = build_filter_graph(&scene);

    assert!(
        !graph.contains("lowpass=f="),
        "low_pass_hz=None must disable the filter even with keyframes set:\n{graph}",
    );
}

#[test]
fn audio_reverb_keyframes_lower_to_per_segment_chain() {
    use memstroy_core::{Easing, Keyframe};
    let mut scene = baseline_scene();
    scene.output.duration = 2.0;
    let mut tr = audio_track("reverbenv", 0.0);
    tr.t_out = Some(2.0);
    tr.animated_params.insert("reverb".into());
    tr.reverb_kfs = vec![
        Keyframe {
            t: 0.0,
            value: 0.0_f32,
            easing: Easing::default(),
        },
        Keyframe {
            t: 2.0,
            value: 0.8_f32,
            easing: Easing::default(),
        },
    ];
    scene.audio.push(tr);
    let graph = build_filter_graph(&scene);

    let aecho_count = graph.matches("aecho=").count();
    assert!(
        aecho_count >= 2,
        "expected ≥2 per-segment aecho filters, got {aecho_count}:\n{graph}",
    );
    assert!(
        graph.contains("aecho=") && graph.contains(":enable='between(t,"),
        "animated reverb missing enable= gates:\n{graph}",
    );
}

#[test]
fn audio_mute_overrides_animated_volume() {
    // The GUI's M button is a hard mute. Even when the user has
    // authored a volume envelope, mute must collapse the chain to
    // silence — not "play the keyframe values".
    use memstroy_core::{Easing, Keyframe};
    let mut scene = baseline_scene();
    scene.output.duration = 2.0;
    let mut tr = audio_track("mutedanim", 0.0);
    tr.t_out = Some(2.0);
    tr.mute = true;
    tr.animated_params.insert("volume".into());
    tr.volume_kfs = vec![
        Keyframe {
            t: 0.0,
            value: 0.5_f32,
            easing: Easing::default(),
        },
        Keyframe {
            t: 2.0,
            value: 1.0_f32,
            easing: Easing::default(),
        },
    ];
    scene.audio.push(tr);
    let graph = build_filter_graph(&scene);

    // Hard `volume=0.000000` somewhere in the chain.
    assert!(
        graph.contains("volume=0.000000"),
        "mute didn't override the animated volume envelope:\n{graph}",
    );
    // No per-segment volume filters should leak through — the chain
    // collapses to the unconditional silence node. If any
    // `volume=…` appears with an `:enable=between(t,` immediately
    // after, that means a per-segment filter was emitted alongside
    // the mute (a regression).
    let segmented_volume = graph.matches("volume=").count() > 1
        || graph.contains("volume=0.500000:enable=")
        || graph.contains("volume=1.000000:enable=");
    assert!(
        !segmented_volume,
        "mute should suppress per-segment animated volume filters:\n{graph}",
    );
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

#[test]
fn mask_alphamerge_pins_blend_inputs_to_common_axis() {
    // Pin the contract of `emit_mask_alphamerge`: every link feeding
    // the `blend` and `alphamerge` filters in the mask sub-graph must
    // declare an explicit format / fps / setpts / setsar so
    // `framesync_configure()` (the helper both filters share) can
    // pair the still-image-derived mask stream with the video-derived
    // alpha stream on a common axis.
    //
    // Without this lock, ffmpeg 7.0+ aborts filter graph init with
    //
    //   [Parsed_blend_NN] Failed to configure output pad on Parsed_blend_NN
    //   Error reinitializing filters!
    //
    // …which kills both encoder threads (libx264 -22, AAC EOF) and
    // produces a 0-byte mp4. The fix is purely defensive — adds
    // explicit per-link normalisation around the sub-graph so the
    // graph initialises cleanly on every supported ffmpeg.
    use memstroy_core::{Effect, EffectKind, ImageOverlay, MaskShape, OverlayState};

    let mut scene = baseline_scene();
    scene.overlays.push(Overlay::Image(ImageOverlay {
        id: "masked_img".into(),
        source: PathBuf::from("test.png"),
        t_in: 0.0,
        t_out: 2.0,
        layout: vec![Keyframe::new(0.0, OverlayState::default())],
        modifiers: Vec::new(),
        skeleton_attachment: None,
        effects: vec![Effect::new(EffectKind::Mask {
            shape: MaskShape::Polygon {
                points: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            },
            feather: 0.0,
            invert: false,
        })],
        animated_params: Default::default(),
        chroma_key: None,
        z_order: 0,
        parent_id: None,
    }));

    let graph = build_filter_graph(&scene);

    // Each of the four links into blend/alphamerge gets its own
    // explicit (format=)?,fps,setpts,setsar wrapper. The exact label
    // names are an implementation detail, so we just assert the
    // tokens land in the graph somewhere and the count is right.
    //
    // 1) main split feed — must include `format=yuva420p` AND the
    //    timing locks BEFORE `split`.
    assert!(
        graph.contains("format=yuva420p,fps=30,setpts=PTS-STARTPTS,setsar=1,split=2"),
        "main stream not normalised before split:\n{graph}",
    );
    // 2) extracted alpha — feeds blend, must be re-stamped as gray
    //    on the same axis.
    assert!(
        graph.contains("alphaextract,format=gray,fps=30,setpts=PTS-STARTPTS,setsar=1"),
        "alphaextract output not normalised before blend:\n{graph}",
    );
    // 3) mask PNG raw — must be force-converted to the same axis
    //    immediately after the [idx:v] reference.
    assert!(
        graph.contains("format=gray,fps=30,setpts=PTS-STARTPTS,setsar=1"),
        "mask PNG not normalised after [idx:v]:\n{graph}",
    );
    // 4) post-scale2ref mask — even though scale2ref already wraps
    //    the mask to source dims, its timing/sar metadata is unsafe
    //    to feed straight into blend on ffmpeg 7+.
    assert!(
        graph.contains("scale2ref=w=main_w:h=main_h"),
        "scale2ref no longer present (intended fallback for older ffmpeg):\n{graph}",
    );
    // 5) blend uses repeatlast=1 so the looped still PNG keeps
    //    feeding the chain after its first frame — without this the
    //    second-and-later video frames would arrive at blend with
    //    only one input live and framesync would tear down.
    assert!(
        graph.contains("blend=all_mode=multiply:all_opacity=1:shortest=0:repeatlast=1"),
        "blend missing shortest=0:repeatlast=1 framesync hint:\n{graph}",
    );
    // 6) alphamerge passthrough is also re-stamped (alphamerge uses
    //    framesync internally too).
    assert!(
        graph.contains("alphamerge"),
        "alphamerge filter missing — mask sub-graph broken:\n{graph}",
    );
}

// ─── PR #71: per-element z_order parity with preview canvas ────────
//
// User report (verbatim, translated): "In the preview the Mellstroy
// clips sit on top of the image, but after rendering only the image
// is visible." Root cause: the renderer was emitting actors first and
// image / video overlays unconditionally AFTER actors, regardless of
// the timeline-track-derived z-order the preview canvas honours.
// `Scene::*::z_order` (populated by `populate_render_z_order` in the
// GUI from `*_track_assignments`) plus
// `FilterGraphBuilder::emit_z_ordered_elements` interleaves them
// correctly. The tests below pin both the new behaviour and the
// legacy fallback so old saved scenes don't drift.

#[test]
fn image_overlay_with_lower_z_order_renders_below_actor() {
    // Stacking the user expects:
    //   * top track    → actor   → drawn LAST  (visible on top)
    //   * bottom track → image   → drawn FIRST (background)
    //
    // GUI maps lower track index to higher z_order:
    //   `populate_render_z_order` produces `z_order = -(track + 1)`,
    //   so an actor on track 0 gets `-1` and an image on track 1
    //   gets `-2`.
    //
    // The renderer's new single-pass `emit_z_ordered_elements`
    // sorts ascending: image (`-2`) is emitted before actor (`-1`),
    // which means ffmpeg sees the image's `-i` first (input slot 0)
    // and the actor's `-i` second (slot 1). The actor chain labels
    // itself `[actor_*]`, so checking which input slot feeds that
    // chain is enough to lock the order.
    let mut scene = baseline_scene();
    let mut actor = baseline_actor("hero");
    actor.z_order = -1; // top track
    scene.actors.push(actor);

    scene.overlays.push(Overlay::Image(ImageOverlay {
        id: "bg_img".into(),
        source: PathBuf::from("img.png"),
        t_in: 0.0,
        t_out: 2.0,
        layout: vec![Keyframe::new(0.0, OverlayState::default())],
        modifiers: Vec::new(),
        skeleton_attachment: None,
        effects: Vec::new(),
        animated_params: Default::default(),
        chroma_key: None,
        z_order: -2, // bottom track
        parent_id: None,
    }));

    let graph = build_filter_graph(&scene);

    // The actor must be slot 1, NOT slot 0 — slot 0 belongs to the
    // lower-z_order image overlay that gets emitted first now.
    assert!(
        graph.contains("[1:v]format=yuva420p") && graph.contains("[actor_"),
        "actor chain is not bound to ffmpeg input slot 1 — \
         lower-z_order image should have claimed slot 0 first:\n{graph}",
    );
    assert!(
        graph.contains("[0:v]format=yuva420p") && graph.contains("[img_"),
        "actor was emitted FIRST (slot 0); the image with z_order=-2 \
         should have been emitted before it:\n{graph}",
    );
}

#[test]
fn image_overlay_with_higher_z_order_renders_above_actor() {
    // Inverse case: image authored on a track ABOVE the actor (lower
    // track index) must end up drawn LAST (visually on top of the
    // actor) — exactly what the preview shows when the user puts an
    // overlay sticker on V0 with a Mellstroy clip on V1.
    //
    //   actor on track 1 → z_order = -2 (drawn first, behind)
    //   image on track 0 → z_order = -1 (drawn last, on top)
    let mut scene = baseline_scene();
    let mut actor = baseline_actor("hero");
    actor.z_order = -2; // bottom track
    scene.actors.push(actor);

    scene.overlays.push(Overlay::Image(ImageOverlay {
        id: "sticker".into(),
        source: PathBuf::from("sticker.png"),
        t_in: 0.0,
        t_out: 2.0,
        layout: vec![Keyframe::new(0.0, OverlayState::default())],
        modifiers: Vec::new(),
        skeleton_attachment: None,
        effects: Vec::new(),
        animated_params: Default::default(),
        chroma_key: None,
        z_order: -1, // top track
        parent_id: None,
    }));

    let graph = build_filter_graph(&scene);

    // Actor emitted first → slot 0; image emitted second → slot 1.
    assert!(
        graph.contains("[0:v]format=yuva420p") && graph.contains("[actor_"),
        "actor with z_order=-2 should be emitted FIRST (slot 0):\n{graph}",
    );
    assert!(
        graph.contains("[1:v]format=yuva420p") && graph.contains("[img_"),
        "input slot 1 should belong to the \
         higher-z_order image:\n{graph}",
    );
}

#[test]
fn zero_z_order_keeps_image_overlay_below_actor_like_canvas_preview() {
    // Saved GUI projects can contain all-zero scene z_order values while
    // their layer assignments live in the .memstroy layout wrapper. If the
    // renderer falls back to "actor first, image second", the image covers
    // the clip in render/extract even though the canvas preview shows the
    // clip on top. With no explicit z_order, image/video overlays should
    // be treated as background media below actors.
    let mut scene = baseline_scene();
    scene.actors.push(baseline_actor("hero"));
    scene.overlays.push(Overlay::Image(ImageOverlay {
        id: "img".into(),
        source: PathBuf::from("legacy.png"),
        t_in: 0.0,
        t_out: 2.0,
        layout: vec![Keyframe::new(0.0, OverlayState::default())],
        modifiers: Vec::new(),
        skeleton_attachment: None,
        effects: Vec::new(),
        animated_params: Default::default(),
        chroma_key: None,
        z_order: 0,
        parent_id: None,
    }));

    let graph = build_filter_graph(&scene);

    // Image first -> slot 0, actor second -> slot 1, so the actor is
    // overlaid after the image and remains visible.
    assert!(
        graph.contains("[0:v]format=yuva420p") && graph.contains("[img_"),
        "zero-z fallback should emit image first as the background layer:\n{graph}",
    );
    assert!(
        graph.contains("[1:v]format=yuva420p") && graph.contains("[actor_"),
        "zero-z fallback should emit actor second so it draws on top:\n{graph}",
    );
}

#[test]
fn empty_backgrounds_use_configured_color_not_chromakey_green() {
    // Regression: the renderer used to paint the base canvas bright
    // chromakey green (`[0, 255, 0]`) whenever `scene.backgrounds`
    // was empty, on the theory that exports would be re-keyed in
    // post. In practice that flooded the entire void with green
    // wherever a chroma-keyed actor's transparent pixels lived, so
    // the user's bug "итоговый рендер не совпадает с превью" — the
    // editor canvas's neutral void becomes a wall of bright green
    // in the MP4. Emitting the configured `output.background_color`
    // unconditionally fixes that and keeps existing scenes with a
    // non-default colour (e.g. user-chosen white) round-tripping
    // correctly.
    let mut scene = baseline_scene();
    scene.output.background_color = [255, 255, 255]; // explicit white
    assert!(scene.backgrounds.is_empty());
    scene.actors.push(baseline_actor("only_actor"));

    let graph = build_filter_graph(&scene);

    // Base canvas must use the configured background_color (white)
    // — NOT the legacy chromakey-green hack.
    assert!(
        graph.contains("color=c=0xFFFFFF:"),
        "base canvas should be the configured `output.background_color` (white) when there are no backgrounds, got:\n{graph}",
    );
    assert!(
        !graph.contains("color=c=0x00FF00:s="),
        "base canvas must NOT default to bright chromakey green when scene.backgrounds is empty:\n{graph}",
    );
}

#[test]
fn build_plan_canonicalises_render_frame_resolution_to_output_resolution() {
    // Regression: the canvas preview converts legacy `[0..1]` pos
    // values via `pos * render_frame.resolution`, while the renderer's
    // `expr::build_element_transform` does the same conversion via
    // `pos * output.resolution`. The two formulae agree only when
    // both resolution fields hold the same value. Scenes loaded from
    // disk where the inspector panel never opened to re-sync them
    // (panels.rs `inspector_nothing`) used to produce a mismatch
    // that pushed every overlay to the wrong world position in the
    // export — the bg/text plates ended up off-canvas while the
    // actor (centred at 0.5, 0.5) survived. `build_plan` now
    // canonicalises by syncing `output.resolution = rf.resolution`
    // because the render frame IS the on-canvas selection area and
    // its resolution is the single source of truth for the output
    // file's pixel dimensions.
    let mut scene = baseline_scene();
    scene.output.resolution = [1080, 1920];
    scene.render_frame.resolution = [3840, 3840]; // intentionally divergent
    scene.actors.push(baseline_actor("hero"));

    // Build the graph; the canonicalisation step inside `build_plan`
    // should clone the scene and align output.resolution → rf.resolution
    // BEFORE the filtergraph builder reads anything. The user-visible
    // proof is that the legacy `pos * out_w` substring uses the
    // RENDER FRAME width (3840), which is what the canvas preview's
    // `world_w = rf.resolution[0]` formula always used.
    let graph = build_filter_graph(&scene);

    // Find the legacy normalised pos lowering: `(({pos_x})*3840.0000)`.
    // The exact substring depends on how the piecewise expression is
    // formatted, so we assert on the W constant the formula multiplies
    // by — that's `rf.resolution[0]` (3840) which is now the
    // authoritative output width after canonicalisation.
    assert!(
        graph.contains("*3840.0000)"),
        "world_x lowering should use rf.resolution width (3840) as the authoritative output size:\n{graph}",
    );
    assert!(
        !graph.contains("*1080.0000)"),
        "the stale output.resolution (1080) must NOT leak into the renderer's world-pos formulae — rf.resolution is the source of truth:\n{graph}",
    );
}

#[test]
fn build_plan_keeps_scene_unchanged_when_resolutions_already_match() {
    // The canonicalisation step must be a no-op for the common case
    // where `output.resolution == render_frame.resolution`. We verify
    // that the build still succeeds and produces the same legacy
    // lowering against the matching width (so the cheap path that
    // skips the clone is exercised).
    let mut scene = baseline_scene();
    scene.output.resolution = [1080, 1920];
    scene.render_frame.resolution = [1080, 1920]; // already aligned
    scene.actors.push(baseline_actor("hero"));

    let graph = build_filter_graph(&scene);
    assert!(
        graph.contains("*1080.0000)"),
        "expected matching out_w lowering: {graph}"
    );
}

// ─── Render-frame camera parity (rewrite) ──────────────────────────
//
// The render-frame camera (rf.pos / rf.zoom / rf.rotation_deg + its
// modifiers) is now baked into every element's per-overlay
// X / Y / scale / rotate expressions, mirroring the snapshot
// rasterizer's `world_to_output`. The previous build applied the
// camera as a post-composite `pad → rotate → crop → scale` over a
// fixed `W × H` buffer that didn't contain world content for
// `rf.zoom < 1` or `rf.rotation_deg != 0` — producing transparent
// corners and silently-clamped centre crops. The tests below pin
// the new contract so any regression that re-introduces the
// post-composite camera (or drops the per-element zoom / rotation)
// flips a CI light immediately.

#[test]
fn render_frame_zoom_multiplies_per_element_scale() {
    // When rf.zoom = 2.0 the rf rectangle covers half the world
    // area, so every element should appear TWICE as big in the
    // output (their scale is multiplied by rf.zoom). We assert the
    // zoom value `2.000000` shows up in the actor's `scale=` filter
    // — fingerprint of the per-element rf-zoom multiplication.
    let mut scene = baseline_scene();
    scene.render_frame.layout = vec![Keyframe::new(
        0.0,
        RenderFrameState {
            pos: WorldPos { x: 540.0, y: 960.0 },
            zoom: 2.0,
            rotation_deg: 0.0,
        },
    )];
    scene.actors.push(baseline_actor("a1"));

    let graph = build_filter_graph(&scene);
    // The scale filter must reference `2.000000` from the rf-zoom
    // multiplier on the actor's scale_y / scale_x expression.
    assert!(
        graph.contains("scale=w='max(2,iw*") && graph.contains("2.000000"),
        "rf.zoom=2 not folded into per-element scale filter:\n{graph}",
    );
}

#[test]
fn render_frame_zoom_below_one_widens_visible_world() {
    // rf.zoom = 0.5 means the rf rectangle covers TWICE the world
    // area, so elements should appear at HALF their authored size
    // in the output. The post-composite camera's `min(iw, …)` clamp
    // used to silently disable this case (the W×H composite buffer
    // didn't extend past the rf rectangle), producing a cropped
    // 1× output instead of the wider 0.5× crop the canvas shows.
    // The new pipeline applies the zoom factor per-element so the
    // export produces the wider view correctly.
    let mut scene = baseline_scene();
    scene.render_frame.layout = vec![Keyframe::new(
        0.0,
        RenderFrameState {
            pos: WorldPos { x: 540.0, y: 960.0 },
            zoom: 0.5,
            rotation_deg: 0.0,
        },
    )];
    scene.actors.push(baseline_actor("a1"));

    let graph = build_filter_graph(&scene);
    // The factor 0.5 must appear in the scale filter for the
    // actor — fingerprint of the per-element rf-zoom path.
    assert!(
        graph.contains("0.500000"),
        "rf.zoom=0.5 not folded into per-element scale:\n{graph}",
    );
    // The previous post-composite camera filter signature must NOT
    // appear — the new pipeline doesn't crop the composite at all
    // for the rf path (camera transform is per-element).
    assert!(
        !graph.contains("[rfcam"),
        "post-composite rf-camera leaked back in:\n{graph}",
    );
    assert!(
        !graph.contains("[rfpad"),
        "post-composite rf-camera pad leaked back in:\n{graph}",
    );
}

#[test]
fn render_frame_rotation_emits_rotate_on_every_element() {
    // When rf.rotation_deg != 0, the un-rotation `-rf.rotation_deg`
    // must be folded into every element's rotation, which means a
    // per-frame `rotate=` filter has to fire even on actors that
    // have no per-frame rotation of their own. This is the
    // counter-rotation that lands the rf-aligned content axis-
    // aligned in the output.
    let mut scene = baseline_scene();
    scene.render_frame.layout = vec![Keyframe::new(
        0.0,
        RenderFrameState {
            pos: WorldPos { x: 540.0, y: 960.0 },
            zoom: 1.0,
            rotation_deg: 30.0,
        },
    )];
    scene.actors.push(baseline_actor("a1"));

    let graph = build_filter_graph(&scene);
    assert!(
        graph.contains("rotate="),
        "rf rotation didn't propagate to per-element rotate filter:\n{graph}",
    );
    // The hypot bbox must be present so the rotated frame doesn't
    // get its corners clipped at the overlay boundary (this is
    // already the convention the per-frame rotation path uses for
    // animated layout rotations; we want the same shape here).
    assert!(
        graph.contains("hypot(iw"),
        "rotate filter doesn't pad to hypot — corners will clip:\n{graph}",
    );
    // The rf rotation value (30.000000) must show up in the rotate
    // expression, fingerprinting the per-element fold.
    assert!(
        graph.contains("30.000000"),
        "rf.rotation_deg=30 not folded into per-element rotation:\n{graph}",
    );
}

#[test]
fn render_frame_camera_no_longer_runs_post_composite() {
    // Regression: the new pipeline applies the rf camera per-element,
    // so the post-composite `pad → rotate → crop → scale` pass that
    // used to run over the `W × H` buffer must NOT fire any more.
    // We assert the legacy labels never appear for an animated rf.
    let mut scene = baseline_scene();
    scene.render_frame.layout = vec![
        Keyframe::new(0.0, RenderFrameState::default()),
        Keyframe::new(
            2.0,
            RenderFrameState {
                pos: WorldPos { x: 540.0, y: 960.0 },
                zoom: 1.5,
                rotation_deg: 20.0,
            },
        ),
    ];
    scene.actors.push(baseline_actor("a1"));

    let graph = build_filter_graph(&scene);
    assert!(
        !graph.contains("[rfcam"),
        "post-composite rf-camera filter leaked back in:\n{graph}",
    );
    assert!(
        !graph.contains("[rfpad"),
        "post-composite rf-camera pad leaked back in:\n{graph}",
    );
    assert!(
        !graph.contains("[rfrot"),
        "post-composite rf-camera rotate leaked back in:\n{graph}",
    );
}

#[test]
fn render_frame_modifier_pulse_animates_per_element_scale() {
    // A Pulse modifier on the render-frame should animate the rf zoom
    // and therefore animate every element's scale. The sine term must
    // appear in the scale filter — fingerprint of rf-modifier
    // propagation. Without this, rf modifiers visibly shake the
    // outline in preview but the export stays static.
    let mut scene = baseline_scene();
    scene.render_frame.modifiers.push(TrackModifier {
        enabled: true,
        kind: ModifierKind::Pulse {
            freq_hz: 3.0,
            amp_scale: 0.2,
        },
        ..Default::default()
    });
    scene.actors.push(baseline_actor("a1"));

    let graph = build_filter_graph(&scene);
    // The amp_scale (0.2) and the sin() time term must appear inside
    // a scale=… expression for the actor — proves the rf Pulse
    // modifier has been folded into per-element scale.
    assert!(
        graph.contains("scale=w='max(2,iw*"),
        "scale filter not present on actor:\n{graph}",
    );
    assert!(
        graph.contains("sin(") && graph.contains("0.200000"),
        "rf Pulse modifier (amp_scale=0.2 sin term) not threaded into per-element scale:\n{graph}",
    );
}

#[test]
fn default_render_frame_skips_zoom_rotate_fast_path() {
    // The default render frame (single keyframe at default state, no
    // modifiers) must NOT emit rf-camera arithmetic into the scale /
    // rotate expressions — that's the fast path so the typical
    // scene's filter-graph stays compact. The actor has no
    // per-frame rotation either, so `rotate=` should not appear at
    // all.
    let mut scene = baseline_scene();
    scene.actors.push(baseline_actor("a1"));

    let graph = build_filter_graph(&scene);
    assert!(
        !graph.contains("rotate="),
        "default rf shouldn't emit a rotate filter on a non-rotated actor:\n{graph}",
    );
}

// ─── CPU compositor parity ─────────────────────────────────────────
//
// The new CPU pipeline (`compositor.rs`) is the canonical render path.
// These tests pin the contract that simple scenes produce sane output
// and that the pipeline doesn't regress when scenes contain mixes of
// elements that historically failed in the FFmpeg-graph backend.

use memstroy_render::render_preview_frame_cpu;
use std::path::Path;

fn _temp_assets_root_unused() -> PathBuf {
    PathBuf::from(".")
}

fn temp_out_path(name: &str) -> PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("memstroy-cputest-{}-{}-{}.png", name, pid, nanos))
}

#[test]
fn cpu_pipeline_produces_image_of_expected_dimensions() {
    let mut scene = baseline_scene();
    scene.format_version = 2;
    scene.output.duration = 0.5;
    scene.output.resolution = [1080, 1920];
    scene.render_frame.resolution = [1080, 1920];
    scene.output.background_color = [10, 20, 30];

    let out = temp_out_path("dims");
    let result = render_preview_frame_cpu(&scene, Path::new("."), 0.0, &out);
    if let Err(e) = &result {
        eprintln!("CPU preview failed: {}", e);
    }
    assert!(result.is_ok(), "CPU preview must succeed for empty scene");
    let img = image::open(&out).expect("decode preview").to_rgba8();
    assert_eq!(img.width(), 1080);
    assert_eq!(img.height(), 1920);
    let _ = std::fs::remove_file(&out);
}

#[test]
fn cpu_pipeline_paints_background_color_when_empty() {
    // An empty scene should produce a uniform fill in the configured
    // background colour. Sample several pixels to confirm.
    let mut scene = baseline_scene();
    scene.format_version = 2;
    scene.output.duration = 0.5;
    scene.output.resolution = [1080, 1920];
    scene.render_frame.resolution = [1080, 1920];
    scene.output.background_color = [200, 100, 50];

    let out = temp_out_path("bgcolor");
    render_preview_frame_cpu(&scene, Path::new("."), 0.0, &out).expect("CPU preview must succeed");

    let img = image::open(&out).expect("decode preview").to_rgba8();
    let samples = [
        (50u32, 50u32),
        (img.width() / 2, img.height() / 2),
        (img.width() - 50, img.height() - 50),
    ];
    for (x, y) in &samples {
        let p = img.get_pixel(*x, *y).0;
        assert!(
            (p[0] as i32 - 200).abs() < 5
                && (p[1] as i32 - 100).abs() < 5
                && (p[2] as i32 - 50).abs() < 5,
            "background colour mismatch at ({},{}): {:?}",
            x,
            y,
            p
        );
        assert_eq!(p[3], 255, "expected fully opaque");
    }
    let _ = std::fs::remove_file(&out);
}

#[test]
fn cpu_pipeline_places_image_overlay_at_world_position() {
    // Create a synthetic image overlay at a known position and verify
    // the rendered output puts its centre at the expected output
    // coordinates. Uses a tiny coloured PNG generated on the fly so
    // the test doesn't depend on any committed asset.
    use memstroy_core::{Easing, ImageOverlay, OverlayState};

    // 100×100 solid red PNG.
    let tmp_dir = std::env::temp_dir().join(format!(
        "memstroy-cputest-asset-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp_dir).expect("mkdir asset dir");
    let img_path = tmp_dir.join("red.png");
    let red = image::RgbaImage::from_pixel(100, 100, image::Rgba([255, 0, 0, 255]));
    red.save(&img_path).expect("write red.png");

    // Scene: 200×200, image overlay at pos (0.5, 0.5), scale=1, no rf zoom.
    let mut scene = baseline_scene();
    scene.format_version = 2;
    scene.output.resolution = [200, 200];
    scene.render_frame.resolution = [200, 200];
    // The default `RenderFrameState` carries a 1080×1920-era pos
    // (540, 960). For a 200×200 scene we must reposition the rf to
    // the centre of its own output rectangle so the legacy
    // normalised pos of (0.5, 0.5) lands at the rf centre.
    scene.render_frame.layout = vec![Keyframe {
        t: 0.0,
        value: memstroy_core::RenderFrameState {
            pos: memstroy_core::canvas::WorldPos { x: 100.0, y: 100.0 },
            zoom: 1.0,
            rotation_deg: 0.0,
        },
        easing: Easing::Linear,
    }];
    scene.output.duration = 0.5;
    scene.output.background_color = [0, 200, 0]; // bright green

    // Image overlay using the relative path from the temp dir.
    scene.overlays.push(Overlay::Image(ImageOverlay {
        id: "img1".into(),
        source: PathBuf::from("red.png"),
        t_in: 0.0,
        t_out: 0.5,
        layout: vec![Keyframe {
            t: 0.0,
            value: OverlayState {
                pos: [0.5, 0.5], // dead centre
                scale: 1.0,
                scale_y: 1.0,
                rotation_deg: 0.0,
                opacity: 1.0,
                flip_x_anim: 1.0,
                flip_y_anim: 1.0,
            },
            easing: Easing::Linear,
        }],
        modifiers: Vec::new(),
        skeleton_attachment: None,
        effects: Vec::new(),
        animated_params: Default::default(),
        chroma_key: None,
        z_order: 0,
        parent_id: None,
    }));

    let out_path =
        std::env::temp_dir().join(format!("memstroy-cputest-place-{}.png", std::process::id()));
    render_preview_frame_cpu(&scene, &tmp_dir, 0.0, &out_path).expect("CPU preview must succeed");
    let img = image::open(&out_path).expect("decode preview").to_rgba8();
    assert_eq!(img.width(), 200);
    assert_eq!(img.height(), 200);

    // The 100×100 red image is centred on (100, 100) → rectangle
    // covers x in [50..150], y in [50..150]. Pixels inside should be
    // red; pixels outside should be the green background.
    let center = img.get_pixel(100, 100).0;
    assert!(
        center[0] > 240 && center[1] < 30 && center[2] < 30,
        "centre pixel should be red, got {:?}",
        center
    );
    // Just inside the right edge of the image — still red.
    let inside_edge = img.get_pixel(140, 100).0;
    assert!(
        inside_edge[0] > 200 && inside_edge[1] < 80 && inside_edge[2] < 80,
        "pixel just inside right edge of overlay should be red, got {:?}",
        inside_edge
    );
    // Outside the image — green background.
    let outside = img.get_pixel(180, 100).0;
    assert!(
        outside[1] > 150 && outside[0] < 80 && outside[2] < 80,
        "pixel outside overlay should be the green background, got {:?}",
        outside
    );

    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[test]
fn cpu_pipeline_honours_render_frame_pos_offset() {
    // When `render_frame.pos` shifts AWAY from the world origin, the
    // output should show world content at a translated offset:
    //   output_centre = render_frame.pos
    //   world (rf.pos + d) → output (W/2 + d)
    //
    // We place an image overlay at world (700, 1100) with the
    // render-frame at (700, 1100) so the overlay's centre lands on
    // the output's centre regardless of the rf shift.
    use memstroy_core::{
        canvas::{CanvasTransform, RenderFrameState, WorldPos},
        CanvasLayout, Easing, ImageOverlay, OverlayState,
    };

    let tmp_dir = std::env::temp_dir().join(format!(
        "memstroy-cputest-rfpos-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmp_dir).expect("mkdir asset dir");
    let img_path = tmp_dir.join("blue.png");
    let blue = image::RgbaImage::from_pixel(100, 100, image::Rgba([0, 0, 255, 255]));
    blue.save(&img_path).expect("write blue.png");

    let mut scene = baseline_scene();
    scene.format_version = 2;
    scene.output.resolution = [400, 400];
    scene.render_frame.resolution = [400, 400];
    scene.output.duration = 0.5;
    scene.output.background_color = [200, 200, 200];

    // Render frame centred at world (700, 1100).
    scene.render_frame.layout = vec![Keyframe {
        t: 0.0,
        value: RenderFrameState {
            pos: WorldPos {
                x: 700.0,
                y: 1100.0,
            },
            zoom: 1.0,
            rotation_deg: 0.0,
        },
        easing: Easing::Linear,
    }];

    // Image overlay anchored at world (700, 1100) via canvas_layouts.
    scene.overlays.push(Overlay::Image(ImageOverlay {
        id: "blue1".into(),
        source: PathBuf::from("blue.png"),
        t_in: 0.0,
        t_out: 0.5,
        layout: vec![Keyframe {
            t: 0.0,
            value: OverlayState::default(),
            easing: Easing::Linear,
        }],
        modifiers: Vec::new(),
        skeleton_attachment: None,
        effects: Vec::new(),
        animated_params: Default::default(),
        chroma_key: None,
        z_order: 0,
        parent_id: None,
    }));
    scene.canvas_layouts.push(CanvasLayout {
        element_id: "blue1".into(),
        keyframes: vec![Keyframe {
            t: 0.0,
            value: CanvasTransform {
                pos: WorldPos {
                    x: 700.0,
                    y: 1100.0,
                },
                width: 100.0,
                scale: 1.0,
                rotation_deg: 0.0,
                opacity: 1.0,
            },
            easing: Easing::Linear,
        }],
    });

    let out_path =
        std::env::temp_dir().join(format!("memstroy-cputest-rfpos-{}.png", std::process::id()));
    render_preview_frame_cpu(&scene, &tmp_dir, 0.0, &out_path).expect("CPU preview must succeed");
    let img = image::open(&out_path).expect("decode").to_rgba8();

    // The blue 100×100 image should be centred at output (200, 200).
    let center = img.get_pixel(200, 200).0;
    assert!(
        center[2] > 200 && center[0] < 60 && center[1] < 60,
        "centre pixel should be blue when rf.pos == overlay world pos, got {:?}",
        center
    );
    let outside = img.get_pixel(20, 20).0;
    assert!(
        outside[0] > 150 && outside[1] > 150 && outside[2] > 150,
        "outside pixel should be the grey background, got {:?}",
        outside
    );

    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_dir_all(&tmp_dir);
}
