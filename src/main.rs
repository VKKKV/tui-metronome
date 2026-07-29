// Narrowing casts in this module are intentional and value-bounded.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap
)]

use std::io::stdout;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::layout::{Constraint, Layout};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};
use rodio::Source;

// ─── Constants ──────────────────────────────────────────────────────────────

const BPM_MIN: u32 = 10;
const BPM_MAX: u32 = 400;
const SWING_OPTIONS: [f32; 4] = [0.0, 0.33, 0.5, 0.66];
const TAP_TIMEOUT: Duration = Duration::from_secs(2);

/// Preset time signatures cycled with Tab.
const TS_PRESETS: &[(u8, u8)] = &[
    (4, 4),
    (3, 4),
    (2, 4),
    (6, 8),
    (5, 4),
    (7, 8),
    (9, 8),
    (12, 8),
];

/// Max simultaneously active pulse waves.
const PULSE_MAX_WAVES: usize = 4;
/// Wave lifetime in seconds (how long a wave keeps expanding + fading).
const PULSE_LIFETIME: f64 = 2.0;
/// Expansion speed in terminal-cells per second.
const PULSE_SPEED: f64 = 35.0;
/// Crest half-width in cells (how thick the bright band is).
const PULSE_CREST_WIDTH: f64 = 3.8;
/// Tail length in cells (trailing fade behind the crest).
const PULSE_TAIL: f64 = 9.5;
/// Crest amplitude (peak brightness of the wave front).
const PULSE_AMP: f64 = 0.55;
/// Tail amplitude (brightness of the trailing fade).
const PULSE_TAIL_AMP: f64 = 0.16;
/// Breathing amplitude (subtle constant pulsing of the whole screen).
const PULSE_BREATH_AMP: f64 = 0.05;
/// Breathing speed.
const PULSE_BREATH_SPEED: f64 = 0.0008;
/// Final blend multiplier (max strength of the wave color blend).
const PULSE_BLEND_MAX: f64 = 0.7;
const POLL_INTERVAL: Duration = Duration::from_millis(8);
const FLASH_FRAMES: u8 = 6;
const MIN_WIDTH: u16 = 40;
const MIN_HEIGHT: u16 = 12;

// ─── Sound types ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum SoundType {
    Click,
    Wood,
    Cowbell,
    Sidestick,
    Beep,
}

impl SoundType {
    const fn next(self) -> Self {
        match self {
            SoundType::Click => SoundType::Wood,
            SoundType::Wood => SoundType::Cowbell,
            SoundType::Cowbell => SoundType::Sidestick,
            SoundType::Sidestick => SoundType::Beep,
            SoundType::Beep => SoundType::Click,
        }
    }

    /// `(accent_freq, weak_freq)` in Hz
    const fn freqs(self) -> (f32, f32) {
        match self {
            SoundType::Click => (1760.0, 1320.0),
            SoundType::Wood => (1500.0, 900.0),
            SoundType::Cowbell => (2000.0, 1200.0),
            SoundType::Sidestick => (1200.0, 800.0),
            SoundType::Beep => (880.0, 660.0),
        }
    }

    const fn label(self) -> &'static str {
        match self {
            SoundType::Click => "click",
            SoundType::Wood => "wood",
            SoundType::Cowbell => "cowbell",
            SoundType::Sidestick => "sidestick",
            SoundType::Beep => "beep",
        }
    }
}

// ─── Swing ──────────────────────────────────────────────────────────────────

fn next_swing(current: f32) -> f32 {
    let idx = SWING_OPTIONS
        .iter()
        .position(|&s| (s - current).abs() < 0.01)
        .unwrap_or(0);
    SWING_OPTIONS[(idx + 1) % SWING_OPTIONS.len()]
}

fn swing_label(swing: f32) -> &'static str {
    match swing {
        0.0 => "straight",
        0.33 => "light",
        0.5 => "swing",
        0.66 => "triplet",
        _ => "?",
    }
}

// ─── Audio ────────────────────────────────────────────────────────────────

struct AudioOutput {
    _stream: Option<rodio::OutputStream>,
    handle: Option<rodio::OutputStreamHandle>,
}

impl AudioOutput {
    fn try_new() -> Self {
        match rodio::OutputStream::try_default() {
            Ok((stream, handle)) => Self {
                _stream: Some(stream),
                handle: Some(handle),
            },
            Err(_) => Self {
                _stream: None,
                handle: None,
            },
        }
    }

    fn play_click(&self, accent: bool, volume: f32, sound_type: SoundType) {
        let Some(ref handle) = self.handle else {
            return;
        };
        let (accent_freq, weak_freq) = sound_type.freqs();
        let freq = if accent { accent_freq } else { weak_freq };
        let dur = Duration::from_millis(35);
        let base_vol = if accent { 0.7 } else { 0.5 };
        let vol = base_vol * volume;
        let source = rodio::source::SineWave::new(freq)
            .take_duration(dur)
            .amplify(vol);
        let _ = handle.play_raw(source.convert_samples());
    }
}

// ─── Beat timing ────────────────────────────────────────────────────────────

/// Interval per top-level beat (a single click).
fn beat_interval(bpm: u32) -> Duration {
    Duration::from_secs_f64(60.0 / f64::from(bpm))
}

/// Interval to the next beat, applying swing.
fn next_beat_duration(bpm: u32, swing: f32, current_beat: u8) -> Duration {
    let base = beat_interval(bpm);
    if swing <= 0.0 {
        return base;
    }
    let factor = if current_beat.is_multiple_of(2) {
        f64::from(1.0 + swing)
    } else {
        f64::from(1.0 - swing)
    };
    Duration::from_secs_f64(base.as_secs_f64() * factor)
}

// ─── Tap tempo ──────────────────────────────────────────────────────────────

struct TapTempo {
    taps: Vec<Instant>,
}

impl TapTempo {
    fn new() -> Self {
        Self { taps: Vec::new() }
    }

    fn tap(&mut self) -> Option<u32> {
        let now = Instant::now();
        if let Some(&last) = self.taps.last() {
            if now.duration_since(last) > TAP_TIMEOUT {
                self.taps.clear();
            }
        }
        self.taps.push(now);
        if self.taps.len() > 8 {
            self.taps.remove(0);
        }
        self.estimate_bpm()
    }

    fn estimate_bpm(&self) -> Option<u32> {
        if self.taps.len() < 2 {
            return None;
        }
        let intervals: Vec<f64> = self
            .taps
            .windows(2)
            .map(|w| w[1].duration_since(w[0]).as_secs_f64())
            .collect();
        let avg = intervals.iter().sum::<f64>() / intervals.len() as f64;
        if avg <= 0.0 {
            return None;
        }
        Some(u32::try_from((60.0 / avg).round() as i64).unwrap_or(BPM_MAX))
    }

    fn count(&self) -> usize {
        self.taps.len()
    }
}

// ─── Pulse wave field (radial scalar field, opencode-style) ─────────────────

/// One expanding wave from the center of the screen.
struct PulseWave {
    /// Spawn time.
    born: Instant,
    /// true for accent beat (warm color), false (cool color).
    accent: bool,
}

/// The pulse system: active waves + geometry caches.
struct PulseField {
    waves: Vec<PulseWave>,
    enabled: bool,
    /// Cached per-cell distance from screen center (y doubled for aspect).
    distances: Vec<f64>,
    /// Cached per-cell edge falloff.
    edge_falloff: Vec<f64>,
    geo_w: u16,
    geo_h: u16,
    cx: f64,
    cy: f64,
    reach: f64,
    /// Animation clock in seconds.
    elapsed: f64,
    last_frame: Instant,
}

impl PulseField {
    fn new() -> Self {
        Self {
            waves: Vec::with_capacity(PULSE_MAX_WAVES),
            enabled: false,
            distances: Vec::new(),
            edge_falloff: Vec::new(),
            geo_w: 0,
            geo_h: 0,
            cx: 0.0,
            cy: 0.0,
            reach: 1.0,
            elapsed: 0.0,
            last_frame: Instant::now(),
        }
    }

    fn trigger(&mut self, accent: bool) {
        if !self.enabled {
            return;
        }
        if self.waves.len() >= PULSE_MAX_WAVES {
            self.waves.remove(0);
        }
        self.waves.push(PulseWave {
            born: Instant::now(),
            accent,
        });
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f64();
        self.last_frame = now;
        self.elapsed += dt;
        self.waves
            .retain(|w| now.duration_since(w.born).as_secs_f64() < PULSE_LIFETIME);
    }

    fn toggle(&mut self) {
        self.enabled = !self.enabled;
        if !self.enabled {
            self.waves.clear();
        }
    }

    fn ensure_geometry(&mut self, w: u16, h: u16) {
        if w == self.geo_w && h == self.geo_h {
            return;
        }
        self.geo_w = w;
        self.geo_h = h;
        let wf = f64::from(w);
        let hf = f64::from(h);
        self.cx = wf / 2.0;
        self.cy = hf / 2.0;
        let max_dx = self.cx.max(wf - self.cx);
        let max_dy = self.cy.max(hf - self.cy) * 2.0;
        self.reach = max_dx.hypot(max_dy) + PULSE_TAIL;
        let area = usize::from(w) * usize::from(h);
        self.distances = Vec::with_capacity(area);
        self.edge_falloff = Vec::with_capacity(area);
        for y in 0..h {
            for x in 0..w {
                let dx = f64::from(x) + 0.5 - self.cx;
                let dy = (f64::from(y) + 0.5 - self.cy) * 2.0;
                self.distances.push(dx.hypot(dy));
                let f = (1.0 - (self.distances.last().unwrap() / (self.reach * 0.85)).powi(2)).max(0.0);
                self.edge_falloff.push(f);
            }
        }
    }

    /// Wave intensity at a given distance at the current time.
    fn wave_strength(&self, dist: f64, wave: &PulseWave, now: Instant) -> f64 {
        let age = now.duration_since(wave.born).as_secs_f64();
        let progress = age / PULSE_LIFETIME;
        let envelope = (progress * std::f64::consts::PI).sin();
        let eased = envelope * envelope * (3.0 - 2.0 * envelope);
        let head = age * PULSE_SPEED;
        let delta = dist - head;
        let abs_delta = delta.abs();
        let crest = if abs_delta < PULSE_CREST_WIDTH {
            0.5 + 0.5 * (delta / PULSE_CREST_WIDTH * std::f64::consts::PI).cos()
        } else {
            0.0
        };
        let tail = if delta < 0.0 && delta > -PULSE_TAIL {
            (1.0 + delta / PULSE_TAIL).powf(2.3)
        } else {
            0.0
        };
        (crest * PULSE_AMP + tail * PULSE_TAIL_AMP) * eased
    }

    /// Post-process: blend wave colors into every cell's background.
    fn post_process(&mut self, buf: &mut ratatui::buffer::Buffer, area: Rect) {
        if !self.enabled {
            return;
        }
        let w = area.width;
        let h = area.height;
        self.ensure_geometry(w, h);

        let now = Instant::now();
        let breath =
            (0.5 + 0.5 * (self.elapsed * PULSE_BREATH_SPEED).sin()) * PULSE_BREATH_AMP;

        let base = Color::Rgb(20, 20, 30);
        let accent_primary = Color::Rgb(255, 200, 100);
        let weak_primary = Color::Rgb(100, 180, 255);

        for y in 0..h {
            for x in 0..w {
                let idx = usize::from(y) * usize::from(w) + usize::from(x);
                let dist = self.distances[idx];
                let falloff = self.edge_falloff[idx];

                let mut level = 0.0_f64;
                let mut is_accent = false;
                for wave in &self.waves {
                    let s = self.wave_strength(dist, wave, now);
                    if s > 0.0 {
                        level += s;
                        if wave.accent {
                            is_accent = true;
                        }
                    }
                }
                let level = level / PULSE_MAX_WAVES as f64;
                let strength = ((level + breath) * falloff).min(1.0) * PULSE_BLEND_MAX;

                if strength < 0.01 {
                    continue;
                }

                let primary = if is_accent { accent_primary } else { weak_primary };
                blend_cell_color(buf, area.x + x, area.y + y, base, primary, strength as f32);
            }
        }
    }
}

/// Linear blend from `base` to `primary` by `t` (0..1), set as cell bg.
fn blend_cell_color(
    buf: &mut ratatui::buffer::Buffer,
    x: u16,
    y: u16,
    base: Color,
    primary: Color,
    t: f32,
) {
    let blended = match (base, primary) {
        (Color::Rgb(br, bg, bb), Color::Rgb(pr, pg, pb)) => {
            let r = (f32::from(br) + (f32::from(pr) - f32::from(br)) * t).round() as u8;
            let g = (f32::from(bg) + (f32::from(pg) - f32::from(bg)) * t).round() as u8;
            let b = (f32::from(bb) + (f32::from(pb) - f32::from(bb)) * t).round() as u8;
            Color::Rgb(r, g, b)
        }
        _ => primary,
    };
    buf[(x, y)].set_bg(blended);
}

// ─── TUI rendering ──────────────────────────────────────────────────────────

/// All mutable state for the metronome UI.
struct App {
    bpm: u32,
    running: bool,
    beat: u8,
    ts_num: u8,
    ts_den: u8,
    flash: u8,
    volume: f32,
    swing: f32,
    sound: SoundType,
    tap: TapTempo,
    pulse: PulseField,
    next_beat: Instant,
}

impl App {
    fn new() -> Self {
        Self {
            bpm: 120,
            running: false,
            beat: 0,
            ts_num: 4,
            ts_den: 4,
            flash: 0,
            volume: 0.5,
            swing: 0.0,
            sound: SoundType::Click,
            tap: TapTempo::new(),
            pulse: PulseField::new(),
            next_beat: Instant::now(),
        }
    }

    // ── Beat scheduling ──

    fn start(&mut self, audio: &AudioOutput) {
        self.running = true;
        self.beat = 0;
        self.next_beat = Instant::now();
        audio.play_click(true, self.volume, self.sound);
        self.flash = FLASH_FRAMES;
        self.pulse.trigger(true);
        self.next_beat += beat_interval(self.bpm);
    }

    /// Reschedule next beat relative to now (after BPM/swing change while running).
    fn reschedule(&mut self) {
        self.next_beat = Instant::now() + next_beat_duration(self.bpm, self.swing, self.beat);
    }

    /// Process a single key event. Returns true if the app should quit.
    fn handle_key(&mut self, key: KeyCode, audio: &AudioOutput) -> bool {
        match key {
            // Quit
            KeyCode::Char('q') | KeyCode::Esc => return true,

            // Play/pause
            KeyCode::Char(' ') => {
                self.running = !self.running;
                if self.running {
                    self.start(audio);
                }
            }

            // BPM adjust — no pause/resume, just change and reschedule
            KeyCode::Up | KeyCode::Char('+') => self.adjust_bpm(1),
            KeyCode::Down | KeyCode::Char('-') => self.adjust_bpm(-1),
            KeyCode::PageUp => self.adjust_bpm(10),
            KeyCode::PageDown => self.adjust_bpm(-10),

            // Volume
            KeyCode::Char('[') => self.volume = (self.volume - 0.1).max(0.0),
            KeyCode::Char(']') => self.volume = (self.volume + 0.1).min(1.0),

            // Time signature — Tab cycles presets
            KeyCode::Tab => self.cycle_ts_preset(true),
            KeyCode::BackTab => self.cycle_ts_preset(false),

            // Time signature — numerator (1-9 beats per measure)
            KeyCode::Char(c) if ('1'..='9').contains(&c) => {
                self.ts_num = char_to_digit(c, 9);
                if self.beat >= self.ts_num {
                    self.beat = 0;
                }
            }

            // Time signature — denominator (d cycles 4→8→16)
            KeyCode::Char('d') => self.cycle_ts_den(),

            // Tap tempo
            KeyCode::Char('t') => {
                if let Some(tap_bpm) = self.tap.tap() {
                    if (BPM_MIN..=BPM_MAX).contains(&tap_bpm) {
                        self.bpm = tap_bpm;
                        if self.running {
                            self.reschedule();
                        }
                    }
                }
            }

            // Swing cycle
            KeyCode::Char('w') => self.swing = next_swing(self.swing),

            // Sound cycle
            KeyCode::Char('n') => self.sound = self.sound.next(),

            // Pulse effect toggle
            KeyCode::Char('p') => self.pulse.toggle(),

            _ => {}
        }
        false
    }

    fn adjust_bpm(&mut self, delta: i32) {
        let new =
            (i64::from(self.bpm) + i64::from(delta)).clamp(i64::from(BPM_MIN), i64::from(BPM_MAX));
        let new = u32::try_from(new).unwrap_or(BPM_MAX);
        if new != self.bpm {
            self.bpm = new;
            if self.running {
                self.reschedule();
            }
        }
    }

    /// Cycle through common time signature presets (forward or backward).
    fn cycle_ts_preset(&mut self, forward: bool) {
        let idx = TS_PRESETS
            .iter()
            .position(|&(n, d)| n == self.ts_num && d == self.ts_den)
            .unwrap_or(0);

        let len = TS_PRESETS.len();
        let next_idx = if forward {
            (idx + 1) % len
        } else {
            (idx + len - 1) % len
        };

        let (n, d) = TS_PRESETS[next_idx];
        self.ts_num = n;
        self.ts_den = d;
        if self.beat >= self.ts_num {
            self.beat = 0;
        }
    }

    fn cycle_ts_den(&mut self) {
        self.ts_den = match self.ts_den {
            4 => 8,
            8 => 16,
            _ => 4,
        };
    }

    /// Process beat scheduling for the current frame.
    /// Only the most recent missed beat plays audio (no machine-gun burst).
    fn tick_beats(&mut self, audio: &AudioOutput) {
        if !self.running {
            return;
        }
        let now = Instant::now();

        // Count how many beats have passed since next_beat.
        let mut missed = 0u32;
        while now >= self.next_beat {
            missed += 1;
            // Advance beat counter without audio for intermediate catch-ups.
            self.beat = (self.beat + 1) % self.ts_num;
            self.next_beat += next_beat_duration(self.bpm, self.swing, self.beat);
        }

        if missed > 0 {
            // Play audio + trigger visual only for the final (current) beat.
            let accent = self.beat == 0;
            audio.play_click(accent, self.volume, self.sound);
            self.flash = FLASH_FRAMES;
            self.pulse.trigger(accent);

            // If we fell behind by more than 2 beats (e.g. system suspend),
            // snap to now to avoid runaway catch-up.
            if missed > 2 {
                self.next_beat = now + next_beat_duration(self.bpm, self.swing, self.beat);
            }
        }
    }

    fn tick_flash(&mut self) {
        self.flash = self.flash.saturating_sub(1);
    }
}

// ─── Beat indicators ────────────────────────────────────────────────────

fn beat_span(beat_idx: u8, current_beat: u8, flash_active: bool) -> Span<'static> {
    let accent = beat_idx == 0;
    let is_current = beat_idx == current_beat;

    let symbol = if is_current && flash_active {
        "◉"
    } else {
        "○"
    };

    let style = match (is_current, flash_active, accent) {
        (true, true, true) => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        (true, true, false) => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        (true, false, _) | (false, _, false) => Style::default().fg(Color::DarkGray),
        (false, _, true) => Style::default().fg(Color::Gray),
    };

    Span::styled(format!(" {symbol} "), style)
}

// ─── Render functions ────────────────────────────────────────────────────

fn render_hint(frame: &mut Frame, area: Rect) {
    let hint = Line::from(vec![
        Span::raw("  "),
        Span::styled("SPACE", Style::default().fg(Color::Cyan)),
        Span::raw(" start/stop  "),
        Span::styled("↑↓+-", Style::default().fg(Color::Cyan)),
        Span::raw(" BPM  "),
        Span::styled("Tab", Style::default().fg(Color::Cyan)),
        Span::raw(" time sig  "),
        Span::styled("1-9", Style::default().fg(Color::Cyan)),
        Span::raw(" num  "),
        Span::styled("d", Style::default().fg(Color::Cyan)),
        Span::raw(" den  "),
        Span::styled("t", Style::default().fg(Color::Cyan)),
        Span::raw(" tap  "),
        Span::styled("w", Style::default().fg(Color::Cyan)),
        Span::raw(" swing  "),
        Span::styled("n", Style::default().fg(Color::Cyan)),
        Span::raw(" sound  "),
        Span::styled("p", Style::default().fg(Color::Cyan)),
        Span::raw(" pulse  "),
        Span::styled("Q", Style::default().fg(Color::Cyan)),
        Span::raw(" quit"),
    ]);
    frame.render_widget(Paragraph::new(hint).alignment(Alignment::Left), area);
}

fn render_bpm_display(frame: &mut Frame, area: Rect, app: &App) {
    let flash_beat = app.flash > 0 && app.running;

    let bpm_style = if flash_beat {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else if app.running {
        Style::default().fg(Color::White)
    } else {
        Style::default().fg(Color::Gray)
    };

    // BPM line with time signature
    let bpm_line = Line::from(vec![
        Span::styled(format!("▐  {}  ▌", app.bpm), bpm_style),
        Span::raw("   "),
        Span::styled(
            format!("{}/{}", app.ts_num, app.ts_den),
            Style::default().fg(Color::Magenta),
        ),
    ]);
    frame.render_widget(Paragraph::new(bpm_line).alignment(Alignment::Center), area);
}

fn render_beat_circles(frame: &mut Frame, area: Rect, app: &App) {
    let flash_active = app.flash > 0;
    let mut beat_spans: Vec<Span> = Vec::with_capacity(usize::from(app.ts_num) * 3);
    for i in 0..app.ts_num {
        if i > 0 {
            beat_spans.push(Span::raw(" "));
        }
        beat_spans.push(beat_span(i, app.beat, flash_active));
    }
    frame.render_widget(
        Paragraph::new(Line::from(beat_spans)).alignment(Alignment::Center),
        area,
    );
}

fn render_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    let icon = if app.running { "▶" } else { "■" };
    let state_label = if app.running { "RUNNING" } else { "STOPPED" };
    let state_color = if app.running {
        Color::Green
    } else {
        Color::DarkGray
    };

    let mut status_spans = vec![
        Span::styled(
            format!("{icon} {state_label}"),
            Style::default().fg(state_color),
        ),
        Span::raw("  |  "),
        Span::raw(format!("beat {}/{}", app.beat + 1, app.ts_num)),
        Span::raw("  |  "),
        Span::raw(format!("{} BPM", app.bpm)),
        Span::raw("  |  "),
        Span::raw(format!("vol: {:.0}%", app.volume * 100.0)),
        Span::raw("  |  "),
        Span::styled(
            format!("swing: {}", swing_label(app.swing)),
            if app.swing > 0.0 {
                Style::default().fg(Color::Magenta)
            } else {
                Style::default()
            },
        ),
        Span::raw("  |  "),
        Span::styled(
            format!("sound: {}", app.sound.label()),
            Style::default().fg(Color::Blue),
        ),
        Span::raw("  |  "),
        Span::styled(
            format!("pulse: {}", if app.pulse.enabled { "on" } else { "off" }),
            if app.pulse.enabled {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
    ];

    // Tap tempo indicator
    if app.tap.count() >= 2 {
        if let Some(tap_bpm) = app.tap.estimate_bpm() {
            status_spans.push(Span::raw("  |  "));
            status_spans.push(Span::styled(
                format!("tap: {} → {tap_bpm} BPM", app.tap.count()),
                Style::default().fg(Color::Yellow),
            ));
        }
    }

    frame.render_widget(
        Paragraph::new(Line::from(status_spans)).alignment(Alignment::Center),
        area,
    );
}

// ─── Main ──────────────────────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdo = stdout();
    stdo.execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdo);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    // Guard: if the main loop panics or returns Err, restore the terminal.
    let result = run_main_loop(&mut terminal);

    // Cleanup — always runs, even on error/panic via unwind.
    disable_raw_mode()?;
    terminal.show_cursor()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    if result.is_ok() {
        println!("metronome stopped");
    }
    result
}

fn run_main_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let audio = AudioOutput::try_new();
    let mut app = App::new();

    loop {
        if event::poll(POLL_INTERVAL)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != event::KeyEventKind::Press {
                    continue;
                }
                if app.handle_key(key.code, &audio) {
                    break;
                }
            }
        }

        // Beat scheduling
        app.tick_beats(&audio);

        // Flash decay
        app.tick_flash();

        // Pulse ring aging
        app.pulse.tick();

        // Render
        terminal.draw(|f| render(f, &mut app))?;
    }

    Ok(())
}

fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let p = Paragraph::new("Terminal too small (min 40x12)").alignment(Alignment::Center);
        frame.render_widget(p, area);
        return;
    }

    let block = Block::default()
        .title(" TUI Metronome ")
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .split(inner);

    render_hint(frame, chunks[0]);

    let mid = chunks[1];
    let bpm_area_height = if mid.height >= 8 {
        8
    } else {
        mid.height.saturating_sub(2)
    };
    let bpm_top = mid.top() + (mid.height.saturating_sub(bpm_area_height)) / 2;
    let bpm_area = Rect::new(mid.x, bpm_top, mid.width, bpm_area_height);

    let bpm_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Length(2)])
        .split(bpm_area);

    render_bpm_display(frame, bpm_chunks[0], app);
    render_beat_circles(frame, bpm_chunks[1], app);
    render_status_bar(frame, chunks[2], app);

    // Post-process: blend pulse wave colors into the frame buffer.
    app.pulse.post_process(frame.buffer_mut(), area);
}

/// Convert a digit char '0'-'9' to a u8, clamped to a maximum.
fn char_to_digit(c: char, max: u8) -> u8 {
    let d = u8::try_from(c.to_digit(10).unwrap_or(0)).unwrap_or(0);
    d.min(max)
}
