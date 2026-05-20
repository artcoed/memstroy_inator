//! Node editor — *scaffold*.
//!
//! This is a deliberately minimal foundation for the planned visual graph
//! editor (filter graphs, color grading, generators, parameter routing).
//! The window is wireframe-grade: draggable rectangular nodes on a grid,
//! placeholder connection lines drawn in chain order. Real graph
//! evaluation, port routing, parameter binding to the scene model, and
//! hot-reload onto the render pipeline are TODO.
//!
//! The intent of merging this now is to claim the surface area in the
//! UI (a window the user can summon, an extension point in the codebase)
//! so subsequent work can iterate without touching the app shell.

use egui::{Color32, Pos2, Rect, Rounding, Sense, Stroke, Vec2};

#[derive(Copy, Clone, PartialEq, Eq)]
enum NodeKind {
    Source,
    ChromaKey,
    ColorGrade,
    Output,
}

impl NodeKind {
    fn color(self) -> Color32 {
        match self {
            NodeKind::Source => Color32::from_rgb(80, 160, 230),
            NodeKind::ChromaKey => Color32::from_rgb(80, 200, 90),
            NodeKind::ColorGrade => Color32::from_rgb(220, 160, 60),
            NodeKind::Output => Color32::from_rgb(220, 80, 100),
        }
    }
    fn label(self) -> &'static str {
        match self {
            NodeKind::Source => "Source",
            NodeKind::ChromaKey => "Chroma Key",
            NodeKind::ColorGrade => "Color Grade",
            NodeKind::Output => "Output",
        }
    }
}

struct EditorNode {
    id: usize,
    title: String,
    /// Position relative to the canvas top-left.
    pos: Pos2,
    kind: NodeKind,
}

pub struct NodeEditor {
    nodes: Vec<EditorNode>,
    next_id: usize,
    pan: Vec2,
}

impl Default for NodeEditor {
    fn default() -> Self {
        Self {
            nodes: vec![
                EditorNode {
                    id: 0,
                    title: "actor.source".into(),
                    pos: Pos2::new(40.0, 60.0),
                    kind: NodeKind::Source,
                },
                EditorNode {
                    id: 1,
                    title: "chroma_key".into(),
                    pos: Pos2::new(260.0, 60.0),
                    kind: NodeKind::ChromaKey,
                },
                EditorNode {
                    id: 2,
                    title: "scene.output".into(),
                    pos: Pos2::new(480.0, 60.0),
                    kind: NodeKind::Output,
                },
            ],
            next_id: 3,
            pan: Vec2::ZERO,
        }
    }
}

impl NodeEditor {
    /// Show the node editor as a floating window. `open` is two-way: the user
    /// can close it via the window's [x] button.
    pub fn show(&mut self, ctx: &egui::Context, open: &mut bool) {
        if !*open {
            return;
        }
        egui::Window::new("\u{1F9E9} Node Editor [scaffold]")
            .open(open)
            .default_size([760.0, 500.0])
            .min_width(480.0)
            .min_height(320.0)
            .resizable(true)
            .show(ctx, |ui| {
                self.canvas(ui);
            });
    }

    fn canvas(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("\u{1F6A7} Scaffold \u{2014} graph evaluation TBD")
                    .color(Color32::from_rgb(220, 180, 100))
                    .size(12.0),
            );
            ui.separator();
            if ui.small_button("\u{2795} Source").clicked() {
                self.add_node(NodeKind::Source);
            }
            if ui.small_button("\u{2795} ChromaKey").clicked() {
                self.add_node(NodeKind::ChromaKey);
            }
            if ui.small_button("\u{2795} ColorGrade").clicked() {
                self.add_node(NodeKind::ColorGrade);
            }
            if ui.small_button("\u{2795} Output").clicked() {
                self.add_node(NodeKind::Output);
            }
            ui.separator();
            if ui.small_button("\u{1F504} Reset view").clicked() {
                self.pan = Vec2::ZERO;
            }
        });
        ui.separator();

        let avail = ui.available_size();
        let (rect, canvas_resp) =
            ui.allocate_exact_size(avail, Sense::click_and_drag());
        let painter = ui.painter_at(rect);

        // Background
        painter.rect_filled(rect, Rounding::same(6.0), Color32::from_rgb(14, 14, 22));

        // Grid (offset by pan)
        let grid = 24.0_f32;
        let grid_color = Color32::from_rgba_premultiplied(255, 255, 255, 8);
        let ox = self.pan.x.rem_euclid(grid);
        let oy = self.pan.y.rem_euclid(grid);
        let mut x = rect.min.x + ox;
        while x < rect.max.x {
            painter.line_segment(
                [egui::pos2(x, rect.min.y), egui::pos2(x, rect.max.y)],
                Stroke::new(1.0, grid_color),
            );
            x += grid;
        }
        let mut y = rect.min.y + oy;
        while y < rect.max.y {
            painter.line_segment(
                [egui::pos2(rect.min.x, y), egui::pos2(rect.max.x, y)],
                Stroke::new(1.0, grid_color),
            );
            y += grid;
        }

        // Pan canvas with middle-click drag (or right-click drag).
        if canvas_resp.dragged_by(egui::PointerButton::Middle)
            || canvas_resp.dragged_by(egui::PointerButton::Secondary)
        {
            self.pan += canvas_resp.drag_delta();
        }

        let node_size = Vec2::new(160.0, 48.0);
        let node_origin = |pos: Pos2| -> Pos2 { rect.min + self.pan + pos.to_vec2() };

        // Connections (placeholder: chain in declaration order).
        if self.nodes.len() >= 2 {
            for w in self.nodes.windows(2) {
                let a_top = node_origin(w[0].pos);
                let b_top = node_origin(w[1].pos);
                let a = a_top + Vec2::new(node_size.x, node_size.y * 0.5);
                let b = b_top + Vec2::new(0.0, node_size.y * 0.5);
                draw_connection(&painter, a, b);
            }
        }

        // Nodes (interaction + paint).
        for node in &mut self.nodes {
            let topleft = node_origin(node.pos);
            let node_rect = Rect::from_min_size(topleft, node_size);
            let id = ui.make_persistent_id(("node-editor-node", node.id));
            let n_resp = ui.interact(node_rect, id, Sense::click_and_drag());
            if n_resp.dragged() {
                node.pos += n_resp.drag_delta();
            }

            let bg = Color32::from_rgb(36, 36, 50);
            let stroke = Stroke::new(if n_resp.hovered() { 2.5 } else { 1.5 }, node.kind.color());
            painter.rect_filled(node_rect, Rounding::same(6.0), bg);
            painter.rect_stroke(node_rect, Rounding::same(6.0), stroke);

            // Header strip
            let header_rect = Rect::from_min_size(
                node_rect.min,
                Vec2::new(node_size.x, 18.0),
            );
            painter.rect_filled(
                header_rect,
                Rounding {
                    nw: 6.0,
                    ne: 6.0,
                    sw: 0.0,
                    se: 0.0,
                },
                node.kind.color().gamma_multiply(0.5),
            );
            painter.text(
                header_rect.left_center() + Vec2::new(8.0, 0.0),
                egui::Align2::LEFT_CENTER,
                node.kind.label(),
                egui::FontId::proportional(11.0),
                Color32::from_rgb(240, 240, 250),
            );

            // Title
            painter.text(
                node_rect.left_top() + Vec2::new(8.0, 26.0),
                egui::Align2::LEFT_TOP,
                &node.title,
                egui::FontId::proportional(12.0),
                Color32::from_rgb(220, 220, 240),
            );

            // Port dots (visual only).
            let in_port = node_rect.left_center() + Vec2::new(0.0, 6.0);
            let out_port = node_rect.right_center() + Vec2::new(0.0, 6.0);
            painter.circle_filled(in_port, 4.0, Color32::from_rgb(180, 180, 200));
            painter.circle_filled(out_port, 4.0, Color32::from_rgb(180, 180, 200));
        }
    }

    fn add_node(&mut self, kind: NodeKind) {
        let n = self.next_id;
        let title = format!(
            "{}_{}",
            kind.label().to_lowercase().replace(' ', "_"),
            n
        );
        self.nodes.push(EditorNode {
            id: n,
            title,
            pos: Pos2::new(80.0 + 28.0 * n as f32, 200.0 + 8.0 * n as f32),
            kind,
        });
        self.next_id += 1;
    }
}

/// Draw a smooth-ish bezier-style connection between two ports.
fn draw_connection(painter: &egui::Painter, a: Pos2, b: Pos2) {
    let stroke = Stroke::new(2.0, Color32::from_rgb(140, 100, 255));
    let dx = (b.x - a.x).abs().max(40.0) * 0.5;
    let c1 = Pos2::new(a.x + dx, a.y);
    let c2 = Pos2::new(b.x - dx, b.y);
    // Approximate a cubic bezier with line segments.
    let steps = 24;
    let mut prev = a;
    for i in 1..=steps {
        let t = i as f32 / steps as f32;
        let p = cubic_bezier(a, c1, c2, b, t);
        painter.line_segment([prev, p], stroke);
        prev = p;
    }
}

fn cubic_bezier(p0: Pos2, p1: Pos2, p2: Pos2, p3: Pos2, t: f32) -> Pos2 {
    let one_t = 1.0 - t;
    let b0 = one_t * one_t * one_t;
    let b1 = 3.0 * one_t * one_t * t;
    let b2 = 3.0 * one_t * t * t;
    let b3 = t * t * t;
    Pos2::new(
        b0 * p0.x + b1 * p1.x + b2 * p2.x + b3 * p3.x,
        b0 * p0.y + b1 * p1.y + b2 * p2.y + b3 * p3.y,
    )
}
