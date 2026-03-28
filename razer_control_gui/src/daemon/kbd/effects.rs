use super::*;

///
/// STATIC KEYBOARD EFFECT
/// 1 colour, simple
///

#[derive(Copy, Clone)]
pub struct Static {
    kbd: board::KeyboardData,
    args: [u8; 3],
}

impl Effect for Static {
    fn new(args: Vec<u8>) -> Box<dyn Effect>
    where
        Self: Sized,
    {
        let mut kbd = board::KeyboardData::new();
        kbd.set_kbd_colour(args[0], args[1], args[2]);
        let s = Static {
            kbd,
            args: [args[0], args[1], args[2]],
        };
        return Box::new(s);
    }

    fn update(&mut self) -> board::KeyboardData {
        return self.kbd;
    }

    fn get_name() -> &'static str
    where
        Self: Sized,
    {
        return "Static";
    }

    fn get_varargs(&mut self) -> &[u8] {
        &self.args
    }

    fn clone_box(&self) -> Box<dyn Effect> {
        return Box::new(*self);
    }

    fn save(&mut self) -> EffectSave {
        EffectSave {
            args: self.args.to_vec(),
            name: String::from("Static"),
        }
    }

    fn get_state(&mut self) -> Vec<u8> {
        self.kbd.get_curr_state()
    }
}

///
/// STATIC_BLEND KEYBOARD EFFECT
/// 2 colours forming a gradient
///

#[derive(Copy, Clone)]
pub struct StaticGradient {
    kbd: board::KeyboardData,
    args: [u8; 6],
}

impl Effect for StaticGradient {
    fn new(args: Vec<u8>) -> Box<dyn Effect>
    where
        Self: Sized,
    {
        let mut kbd = board::KeyboardData::new();
        let args: [u8; 6] = [
            args[0], args[1], args[2], args[3], args[4], args[5]
        ];
        let mut c1 = board::AnimatorKeyColour::new_u(args[0], args[1], args[2]);
        let c2 = board::AnimatorKeyColour::new_u(args[3], args[4], args[5]);
        let delta = (c2 - c1).divide(14.0);
        for i in 0..15 {
            let clamped = c1.get_clamped_colour();
            kbd.set_col_colour(i, clamped.red, clamped.green, clamped.blue);
            c1 += delta;
        }

        Box::new(StaticGradient { kbd, args })
    }

    fn update(&mut self) -> board::KeyboardData {
        self.kbd // Nothing to update
    }

    fn get_name() -> &'static str
    where
        Self: Sized,
    {
        "Static Gradient"
    }

    fn get_varargs(&mut self) -> &[u8] {
        return &self.args;
    }

    fn clone_box(&self) -> Box<dyn Effect> {
        return Box::new(*self);
    }

    fn save(&mut self) -> EffectSave {
        EffectSave {
            args: self.args.to_vec(),
            name: String::from("Static Gradient"),
        }
    }

    fn get_state(&mut self) -> Vec<u8> {
        self.kbd.get_curr_state()
    }
}

///
/// WAVE GRADIENT KEYBOARD EFFECT
/// 2 colours forming a gradient, animated across the keyboard
///

pub struct WaveGradient {
    kbd: board::KeyboardData,
    args: [u8; 6],
    colour_band: Vec<board::AnimatorKeyColour>,
}

impl Effect for WaveGradient {
    fn new(args: Vec<u8>) -> Box<dyn Effect>
    where
        Self: Sized,
    {
        let args: [u8; 6] = [
            args[0], args[1], args[2], args[3], args[4], args[5],
        ];
        let mut wave = WaveGradient {
            kbd: board::KeyboardData::new(),
            args,
            colour_band: vec![],
        };
        let mut c1 = board::AnimatorKeyColour::new_u(args[0], args[1], args[2]);
        let mut c2 = board::AnimatorKeyColour::new_u(args[3], args[4], args[5]);
        let c_delta = (c2 - c1).divide(15.0);
        for _ in 0..15 {
            wave.colour_band.push(c1);
            c1 += c_delta;
        }
        for _ in 0..15 {
            wave.colour_band.push(c2);
            c2 -= c_delta;
        }
        Box::new(wave)
    }

    fn update(&mut self) -> board::KeyboardData {
        for i in 0..15 {
            let c = self.colour_band[i].get_clamped_colour();
            self.kbd.set_col_colour(i, c.red, c.green, c.blue);
        }
        self.colour_band.rotate_right(1);
        self.kbd
    }

    fn get_name() -> &'static str
    where
        Self: Sized,
    {
        "Wave Gradient"
    }

    fn get_varargs(&mut self) -> &[u8] {
        return &self.args;
    }

    fn clone_box(&self) -> Box<dyn Effect> {
        return Box::new(self.clone());
    }

    fn save(&mut self) -> EffectSave {
        EffectSave {
            args: self.args.to_vec(),
            name: String::from("Wave Gradient"),
        }
    }

    fn get_state(&mut self) -> Vec<u8> {
        self.kbd.get_curr_state()
    }
}

impl Clone for WaveGradient {
    fn clone(&self) -> Self {
        WaveGradient {
            kbd: self.kbd,
            args: self.args,
            colour_band: self.colour_band.to_vec(),
        }
    }
}

///
/// BREATHING (1 Colour) KEYBOARD EFFECT
/// 1 colour, fading in and out
///
#[derive(Copy, Clone)]
pub struct BreathSingle {
    args: [u8; 4],
    kbd: board::KeyboardData,
    step_duration_ms: u128,
    static_start_ms: u128,
    curr_step: u8, // Step 0 = Off, 1 = increasing, 2 = On, 3 = decreasing
    target_colour: board::AnimatorKeyColour,
    current_colour: board::AnimatorKeyColour,
    animator_step_colour: board::AnimatorKeyColour,
}

impl Effect for BreathSingle {
    fn new(args: Vec<u8>) -> Box<dyn Effect> {
        let mut k = board::KeyboardData::new();
        let cycle_duration_ms = args[3] as f32 * 100.0;
        k.set_kbd_colour(0, 0, 0); // Sets all keyboard lights off initially
        Box::new(BreathSingle {
            args: [args[0], args[1], args[2], args[3]],
            kbd: k,
            step_duration_ms: cycle_duration_ms as u128,
            static_start_ms: get_millis(),
            curr_step: 0,
            target_colour: board::AnimatorKeyColour::new_u(args[0], args[1], args[2]),
            current_colour: board::AnimatorKeyColour::new_u(0, 0, 0),
            animator_step_colour: board::AnimatorKeyColour::new_f(
                args[0] as f32 / (cycle_duration_ms as f32 / ANIMATION_SLEEP_MS as f32) as f32,
                args[1] as f32 / (cycle_duration_ms as f32 / ANIMATION_SLEEP_MS as f32) as f32,
                args[2] as f32 / (cycle_duration_ms as f32 / ANIMATION_SLEEP_MS as f32) as f32,
            ),
        })
    }

    fn update(&mut self) -> board::KeyboardData {
        match self.curr_step {
            0 => {
                self.current_colour = board::AnimatorKeyColour::new_u(0, 0, 0);
                if get_millis() - self.static_start_ms >= self.step_duration_ms {
                    self.curr_step += 1;
                }
            }
            1 => {
                // Increasing
                self.current_colour += self.animator_step_colour;
                if self.current_colour >= self.target_colour {
                    self.curr_step += 1;
                    self.static_start_ms = get_millis();
                }
            }
            2 => {
                self.current_colour = self.target_colour;
                if get_millis() - self.static_start_ms >= self.step_duration_ms {
                    self.curr_step += 1;
                }
            }
            3 => {
                // Decreasing
                self.current_colour -= self.animator_step_colour;
                let target = board::AnimatorKeyColour::new_u(0, 0, 0);
                if self.current_colour <= target {
                    self.curr_step = 0;
                    self.static_start_ms = get_millis();
                }
            }
            _ => {} // Unknown state? Ignore
        }
        let col = self.current_colour.get_clamped_colour();
        self.kbd.set_kbd_colour(col.red, col.green, col.blue); // Cast back to u8
        return self.kbd;
    }

    fn get_name() -> &'static str
    where
        Self: Sized,
    {
        "Breathing Single"
    }

    fn get_varargs(&mut self) -> &[u8] {
        return &self.args;
    }

    fn clone_box(&self) -> Box<dyn Effect> {
        return Box::new(*self);
    }

    fn save(&mut self) -> EffectSave {
        EffectSave {
            args: self.args.to_vec(),
            name: String::from("Breathing Single"),
        }
    }

    fn get_state(&mut self) -> Vec<u8> {
        self.kbd.get_curr_state()
    }
}

///
/// BREATHING DUAL KEYBOARD EFFECT
/// 2 colours, alternating fade in/out
/// params: [R1, G1, B1, R2, G2, B2, duration_x100ms]
///
#[derive(Copy, Clone)]
pub struct BreathDual {
    args: [u8; 7],
    kbd: board::KeyboardData,
    step_duration_ms: u128,
    static_start_ms: u128,
    curr_step: u8,
    colour1: board::AnimatorKeyColour,
    colour2: board::AnimatorKeyColour,
    current_colour: board::AnimatorKeyColour,
    step_colour1: board::AnimatorKeyColour,
    step_colour2: board::AnimatorKeyColour,
    using_colour1: bool,
}

impl Effect for BreathDual {
    fn new(args: Vec<u8>) -> Box<dyn Effect> {
        let mut k = board::KeyboardData::new();
        let cycle_duration_ms = args[6] as f32 * 100.0;
        let steps = cycle_duration_ms / ANIMATION_SLEEP_MS as f32;
        k.set_kbd_colour(0, 0, 0);
        Box::new(BreathDual {
            args: [args[0], args[1], args[2], args[3], args[4], args[5], args[6]],
            kbd: k,
            step_duration_ms: cycle_duration_ms as u128,
            static_start_ms: get_millis(),
            curr_step: 0,
            colour1: board::AnimatorKeyColour::new_u(args[0], args[1], args[2]),
            colour2: board::AnimatorKeyColour::new_u(args[3], args[4], args[5]),
            current_colour: board::AnimatorKeyColour::new_u(0, 0, 0),
            step_colour1: board::AnimatorKeyColour::new_f(
                args[0] as f32 / steps,
                args[1] as f32 / steps,
                args[2] as f32 / steps,
            ),
            step_colour2: board::AnimatorKeyColour::new_f(
                args[3] as f32 / steps,
                args[4] as f32 / steps,
                args[5] as f32 / steps,
            ),
            using_colour1: true,
        })
    }

    fn update(&mut self) -> board::KeyboardData {
        let step_colour = if self.using_colour1 { self.step_colour1 } else { self.step_colour2 };
        let target = if self.using_colour1 { self.colour1 } else { self.colour2 };
        match self.curr_step {
            0 => {
                self.current_colour = board::AnimatorKeyColour::new_u(0, 0, 0);
                if get_millis() - self.static_start_ms >= self.step_duration_ms / 2 {
                    self.curr_step = 1;
                }
            }
            1 => {
                self.current_colour += step_colour;
                if self.current_colour >= target {
                    self.curr_step = 2;
                    self.static_start_ms = get_millis();
                }
            }
            2 => {
                self.current_colour = target;
                if get_millis() - self.static_start_ms >= self.step_duration_ms / 2 {
                    self.curr_step = 3;
                }
            }
            3 => {
                self.current_colour -= step_colour;
                let zero = board::AnimatorKeyColour::new_u(0, 0, 0);
                if self.current_colour <= zero {
                    self.curr_step = 0;
                    self.static_start_ms = get_millis();
                    self.using_colour1 = !self.using_colour1;
                }
            }
            _ => {}
        }
        let col = self.current_colour.get_clamped_colour();
        self.kbd.set_kbd_colour(col.red, col.green, col.blue);
        self.kbd
    }

    fn get_name() -> &'static str where Self: Sized { "Breathing Dual" }
    fn get_varargs(&mut self) -> &[u8] { &self.args }
    fn clone_box(&self) -> Box<dyn Effect> { Box::new(*self) }
    fn save(&mut self) -> EffectSave {
        EffectSave { args: self.args.to_vec(), name: String::from("Breathing Dual") }
    }
    fn get_state(&mut self) -> Vec<u8> { self.kbd.get_curr_state() }
}

///
/// SPECTRUM CYCLE KEYBOARD EFFECT
/// Cycles through full HSV hue spectrum
/// params: [speed] (1=slow, 5=fast, default 3)
///
pub struct SpectrumCycle {
    kbd: board::KeyboardData,
    args: [u8; 1],
    hue: f32,
    speed: f32,
}

impl Effect for SpectrumCycle {
    fn new(args: Vec<u8>) -> Box<dyn Effect> where Self: Sized {
        let speed = if args.is_empty() { 3 } else { args[0].max(1).min(10) };
        Box::new(SpectrumCycle {
            kbd: board::KeyboardData::new(),
            args: [speed],
            hue: 0.0,
            speed: speed as f32 * 0.5,
        })
    }

    fn update(&mut self) -> board::KeyboardData {
        self.hue = (self.hue + self.speed) % 360.0;
        let (r, g, b) = hsv_to_rgb(self.hue, 1.0, 1.0);
        self.kbd.set_kbd_colour(r, g, b);
        self.kbd
    }

    fn get_name() -> &'static str where Self: Sized { "Spectrum Cycle" }
    fn get_varargs(&mut self) -> &[u8] { &self.args }
    fn clone_box(&self) -> Box<dyn Effect> { Box::new(self.clone()) }
    fn save(&mut self) -> EffectSave {
        EffectSave { args: self.args.to_vec(), name: String::from("Spectrum Cycle") }
    }
    fn get_state(&mut self) -> Vec<u8> { self.kbd.get_curr_state() }
}

impl Clone for SpectrumCycle {
    fn clone(&self) -> Self {
        SpectrumCycle { kbd: self.kbd, args: self.args, hue: self.hue, speed: self.speed }
    }
}

///
/// RAINBOW WAVE KEYBOARD EFFECT
/// Full rainbow scrolling across the keyboard columns
/// params: [speed, direction] (speed 1-10, direction 0=left 1=right)
///
pub struct RainbowWave {
    kbd: board::KeyboardData,
    args: [u8; 2],
    offset: f32,
    speed: f32,
    direction: f32,
}

impl Effect for RainbowWave {
    fn new(args: Vec<u8>) -> Box<dyn Effect> where Self: Sized {
        let speed = if args.is_empty() { 3 } else { args[0].max(1).min(10) };
        let dir = if args.len() < 2 { 1 } else { args[1] };
        Box::new(RainbowWave {
            kbd: board::KeyboardData::new(),
            args: [speed, dir],
            offset: 0.0,
            speed: speed as f32 * 1.5,
            direction: if dir == 0 { -1.0 } else { 1.0 },
        })
    }

    fn update(&mut self) -> board::KeyboardData {
        self.offset = (self.offset + self.speed * self.direction) % 360.0;
        if self.offset < 0.0 { self.offset += 360.0; }
        for col in 0..15 {
            let hue = (self.offset + col as f32 * 24.0) % 360.0; // 360/15 = 24 deg per column
            let (r, g, b) = hsv_to_rgb(hue, 1.0, 1.0);
            self.kbd.set_col_colour(col, r, g, b);
        }
        self.kbd
    }

    fn get_name() -> &'static str where Self: Sized { "Rainbow Wave" }
    fn get_varargs(&mut self) -> &[u8] { &self.args }
    fn clone_box(&self) -> Box<dyn Effect> { Box::new(self.clone()) }
    fn save(&mut self) -> EffectSave {
        EffectSave { args: self.args.to_vec(), name: String::from("Rainbow Wave") }
    }
    fn get_state(&mut self) -> Vec<u8> { self.kbd.get_curr_state() }
}

impl Clone for RainbowWave {
    fn clone(&self) -> Self {
        RainbowWave {
            kbd: self.kbd, args: self.args,
            offset: self.offset, speed: self.speed, direction: self.direction,
        }
    }
}

///
/// STARLIGHT KEYBOARD EFFECT (software)
/// Random keys twinkle on/off like stars
/// params: [R, G, B, density] (density 1-20, number of stars per frame)
///
pub struct Starlight {
    kbd: board::KeyboardData,
    args: [u8; 4],
    stars: Vec<(usize, f32)>, // (key_index, brightness 0.0-1.0)
    density: usize,
    r: u8,
    g: u8,
    b: u8,
}

impl Effect for Starlight {
    fn new(args: Vec<u8>) -> Box<dyn Effect> where Self: Sized {
        let r = if args.is_empty() { 255 } else { args[0] };
        let g = if args.len() < 2 { 255 } else { args[1] };
        let b = if args.len() < 3 { 255 } else { args[2] };
        let density = if args.len() < 4 { 5 } else { args[3].max(1).min(20) };
        Box::new(Starlight {
            kbd: board::KeyboardData::new(),
            args: [r, g, b, density],
            stars: Vec::new(),
            density: density as usize,
            r, g, b,
        })
    }

    fn update(&mut self) -> board::KeyboardData {
        // Fade existing stars
        for star in self.stars.iter_mut() {
            star.1 -= 0.05; // Fade speed
        }
        self.stars.retain(|s| s.1 > 0.0);

        // Spawn new stars
        let now = get_millis();
        for i in 0..self.density {
            // Simple pseudo-random using time
            let key = ((now as usize).wrapping_mul(31).wrapping_add(i * 97)) % 90;
            if !self.stars.iter().any(|s| s.0 == key) {
                self.stars.push((key, 1.0));
            }
        }

        // Render
        self.kbd.set_kbd_colour(0, 0, 0);
        for &(key, brightness) in &self.stars {
            let r = (self.r as f32 * brightness) as u8;
            let g = (self.g as f32 * brightness) as u8;
            let b = (self.b as f32 * brightness) as u8;
            let row = key / board::KEYS_PER_ROW;
            let col = key % board::KEYS_PER_ROW;
            self.kbd.set_key_colour(row, col, r, g, b);
        }
        self.kbd
    }

    fn get_name() -> &'static str where Self: Sized { "Starlight" }
    fn get_varargs(&mut self) -> &[u8] { &self.args }
    fn clone_box(&self) -> Box<dyn Effect> { Box::new(self.clone()) }
    fn save(&mut self) -> EffectSave {
        EffectSave { args: self.args.to_vec(), name: String::from("Starlight") }
    }
    fn get_state(&mut self) -> Vec<u8> { self.kbd.get_curr_state() }
}

impl Clone for Starlight {
    fn clone(&self) -> Self {
        Starlight {
            kbd: self.kbd, args: self.args,
            stars: self.stars.clone(), density: self.density,
            r: self.r, g: self.g, b: self.b,
        }
    }
}

///
/// RIPPLE KEYBOARD EFFECT
/// Waves expand from center outward in concentric rings
/// params: [R, G, B, speed] (speed 1-10)
///
pub struct Ripple {
    kbd: board::KeyboardData,
    args: [u8; 4],
    radius: f32,
    speed: f32,
    max_radius: f32,
    r: u8,
    g: u8,
    b: u8,
}

impl Effect for Ripple {
    fn new(args: Vec<u8>) -> Box<dyn Effect> where Self: Sized {
        let r = if args.is_empty() { 0 } else { args[0] };
        let g = if args.len() < 2 { 255 } else { args[1] };
        let b = if args.len() < 3 { 255 } else { args[2] };
        let speed = if args.len() < 4 { 3 } else { args[3].max(1).min(10) };
        Box::new(Ripple {
            kbd: board::KeyboardData::new(),
            args: [r, g, b, speed],
            radius: 0.0,
            speed: speed as f32 * 0.3,
            max_radius: 10.0,
            r, g, b,
        })
    }

    fn update(&mut self) -> board::KeyboardData {
        self.radius += self.speed;
        if self.radius > self.max_radius * 2.0 {
            self.radius = 0.0;
        }
        let center_row = 3.0_f32; // center of 6 rows
        let center_col = 7.0_f32; // center of 15 cols

        for row in 0..board::ROWS {
            for col in 0..board::KEYS_PER_ROW {
                let dr = row as f32 - center_row;
                let dc = (col as f32 - center_col) * 0.6; // Scale cols to roughly match rows
                let dist = (dr * dr + dc * dc).sqrt();
                let ring_dist = (dist - self.radius).abs();
                let brightness = if ring_dist < 1.0 { 1.0 - ring_dist } else { 0.0 };
                let r = (self.r as f32 * brightness) as u8;
                let g = (self.g as f32 * brightness) as u8;
                let b = (self.b as f32 * brightness) as u8;
                self.kbd.set_key_colour(row, col, r, g, b);
            }
        }
        self.kbd
    }

    fn get_name() -> &'static str where Self: Sized { "Ripple" }
    fn get_varargs(&mut self) -> &[u8] { &self.args }
    fn clone_box(&self) -> Box<dyn Effect> { Box::new(self.clone()) }
    fn save(&mut self) -> EffectSave {
        EffectSave { args: self.args.to_vec(), name: String::from("Ripple") }
    }
    fn get_state(&mut self) -> Vec<u8> { self.kbd.get_curr_state() }
}

impl Clone for Ripple {
    fn clone(&self) -> Self {
        Ripple {
            kbd: self.kbd, args: self.args,
            radius: self.radius, speed: self.speed,
            max_radius: self.max_radius, r: self.r, g: self.g, b: self.b,
        }
    }
}

/// Convert HSV to RGB (h: 0-360, s: 0-1, v: 0-1) -> (r, g, b) as u8
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h as u32 {
        0..=59 => (c, x, 0.0),
        60..=119 => (x, c, 0.0),
        120..=179 => (0.0, c, x),
        180..=239 => (0.0, x, c),
        240..=299 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (((r + m) * 255.0) as u8, ((g + m) * 255.0) as u8, ((b + m) * 255.0) as u8)
}

///
/// WHEEL EFFECT
/// Rotational color sweep around a center point (like Razer Synapse Wheel)
/// Args: [speed, direction] — speed 1-10, direction 0=CW, 1=CCW
///

#[derive(Clone)]
pub struct Wheel {
    kbd: board::KeyboardData,
    args: [u8; 2],
    offset: f32,
    speed: f32,
    direction: f32,
}

impl Effect for Wheel {
    fn new(args: Vec<u8>) -> Box<dyn Effect> where Self: Sized {
        let speed = if args.is_empty() { 3 } else { args[0].max(1).min(10) };
        let dir = if args.len() < 2 { 0 } else { args[1] };
        Box::new(Wheel {
            kbd: board::KeyboardData::new(),
            args: [speed, dir],
            offset: 0.0,
            speed: speed as f32 * 2.0,
            direction: if dir == 0 { 1.0 } else { -1.0 },
        })
    }

    fn update(&mut self) -> board::KeyboardData {
        self.offset = (self.offset + self.speed * self.direction) % 360.0;
        if self.offset < 0.0 { self.offset += 360.0; }

        let center_row = 4.0_f32; // N key row
        let center_col = 6.0_f32; // N key column

        for row in 0..board::ROWS {
            for col in 0..board::KEYS_PER_ROW {
                let dr = row as f32 - center_row;
                let dc = (col as f32 - center_col) * 0.6; // aspect ratio correction
                let angle = dr.atan2(dc).to_degrees() + 180.0; // 0-360
                let dist = (dr * dr + dc * dc).sqrt();

                // Pure rotational hue: angle determines color, offset spins it
                let hue = (angle + self.offset) % 360.0;
                // Full saturation, slight brightness fade at center for depth
                let val = (0.6 + 0.4 * (dist / 6.0).min(1.0)).clamp(0.6, 1.0);
                let (r, g, b) = hsv_to_rgb(hue, 1.0, val);
                self.kbd.set_key_colour(row, col, r, g, b);
            }
        }
        self.kbd
    }

    fn get_name() -> &'static str where Self: Sized { "Wheel" }
    fn get_varargs(&mut self) -> &[u8] { &self.args }
    fn clone_box(&self) -> Box<dyn Effect> { Box::new(self.clone()) }
    fn save(&mut self) -> EffectSave {
        EffectSave { args: self.args.to_vec(), name: String::from("Wheel") }
    }
    fn get_state(&mut self) -> Vec<u8> { self.kbd.get_curr_state() }
}
