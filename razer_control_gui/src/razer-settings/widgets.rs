use std::cell::Cell;
use std::rc::Rc;

use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::gtk::cairo;

/// HSV Color Wheel widget for color selection.
pub struct ColorWheel {
    pub widget: gtk::DrawingArea,
    hue: Rc<Cell<f64>>,
    saturation: Rc<Cell<f64>>,
}

impl ColorWheel {
    pub fn new(size: i32) -> Self {
        let drawing_area = gtk::DrawingArea::new();
        drawing_area.set_size_request(size, size);
        drawing_area.set_halign(gtk::Align::Center);

        let hue = Rc::new(Cell::new(0.0));
        let saturation = Rc::new(Cell::new(1.0));

        let hue_d = hue.clone();
        let sat_d = saturation.clone();
        drawing_area.set_draw_func(move |_da, cr, w, h| {
            let cx = w as f64 / 2.0;
            let cy = h as f64 / 2.0;
            let radius = cx.min(cy) - 6.0;

            // Hue ring
            let segments = 360;
            for i in 0..segments {
                let a1 = (i as f64) * std::f64::consts::TAU / segments as f64;
                let a2 = ((i + 1) as f64) * std::f64::consts::TAU / segments as f64;
                let (r, g, b) = hsv_to_rgb(i as f64, 1.0, 1.0);
                cr.set_source_rgb(r, g, b);
                cr.move_to(cx, cy);
                cr.arc(cx, cy, radius, a1 - std::f64::consts::FRAC_PI_2, a2 - std::f64::consts::FRAC_PI_2);
                cr.close_path();
                let _ = cr.fill();
            }

            // White-center radial gradient (saturation)
            let pattern = cairo::RadialGradient::new(cx, cy, 0.0, cx, cy, radius);
            pattern.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 1.0);
            pattern.add_color_stop_rgba(1.0, 1.0, 1.0, 1.0, 0.0);
            let _ = cr.set_source(&pattern);
            cr.arc(cx, cy, radius, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();

            // Selection indicator
            let sel_angle = (hue_d.get() as f64).to_radians() - std::f64::consts::FRAC_PI_2;
            let sel_r = radius * sat_d.get();
            let sel_x = cx + sel_angle.cos() * sel_r;
            let sel_y = cy + sel_angle.sin() * sel_r;

            cr.set_source_rgb(0.1, 0.1, 0.15);
            cr.arc(sel_x, sel_y, 8.0, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
            cr.set_source_rgb(1.0, 1.0, 1.0);
            cr.arc(sel_x, sel_y, 6.0, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
            let (r, g, b) = hsv_to_rgb(hue_d.get(), sat_d.get(), 1.0);
            cr.set_source_rgb(r, g, b);
            cr.arc(sel_x, sel_y, 4.5, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        });

        // Drag gesture for selecting color
        let drag = gtk::GestureDrag::new();
        let hue_c = hue.clone();
        let sat_c = saturation.clone();
        let da_ref = drawing_area.clone();

        let hue_begin = hue_c.clone();
        let sat_begin = sat_c.clone();
        let da_begin = da_ref.clone();
        drag.connect_drag_begin(move |_gesture, x, y| {
            update_from_pos(&da_begin, &hue_begin, &sat_begin, x, y);
        });

        let hue_update = hue_c.clone();
        let sat_update = sat_c.clone();
        let da_update = da_ref.clone();
        drag.connect_drag_update(move |gesture, off_x, off_y| {
            if let Some((sx, sy)) = gesture.start_point() {
                update_from_pos(&da_update, &hue_update, &sat_update, sx + off_x, sy + off_y);
            }
        });

        drawing_area.add_controller(drag);

        ColorWheel { widget: drawing_area, hue: hue_c, saturation: sat_c }
    }

    pub fn get_rgb(&self) -> (u8, u8, u8) {
        let (r, g, b) = hsv_to_rgb(self.hue.get(), self.saturation.get(), 1.0);
        ((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
    }

    pub fn set_rgb(&self, r: u8, g: u8, b: u8) {
        let (h, s, _v) = rgb_to_hsv(r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
        self.hue.set(h);
        self.saturation.set(s);
        self.widget.queue_draw();
    }
}

fn update_from_pos(da: &gtk::DrawingArea, hue: &Rc<Cell<f64>>, sat: &Rc<Cell<f64>>, x: f64, y: f64) {
    let w = da.width() as f64;
    let h = da.height() as f64;
    let cx = w / 2.0;
    let cy = h / 2.0;
    let radius = cx.min(cy) - 6.0;
    let dx = x - cx;
    let dy = y - cy;
    let dist = (dx * dx + dy * dy).sqrt();
    if dist <= radius + 4.0 {
        let angle = dy.atan2(dx) + std::f64::consts::FRAC_PI_2;
        hue.set((angle.to_degrees() + 360.0) % 360.0);
        sat.set((dist / radius).clamp(0.0, 1.0));
        da.queue_draw();
    }
}

pub fn hsv_to_rgb(h: f64, s: f64, v: f64) -> (f64, f64, f64) {
    let c = v * s;
    let h2 = h / 60.0;
    let x = c * (1.0 - ((h2 % 2.0) - 1.0).abs());
    let (r1, g1, b1) = match h2 as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    (r1 + m, g1 + m, b1 + m)
}

fn rgb_to_hsv(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let v = max;
    let s = if max == 0.0 { 0.0 } else { d / max };
    let h = if d == 0.0 {
        0.0
    } else if max == r {
        60.0 * (((g - b) / d) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    ((h + 360.0) % 360.0, s, v)
}
