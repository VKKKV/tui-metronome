use std::io::stdout;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::layout::{Constraint, Layout};
use ratatui::prelude::*;
use ratatui::widgets::*;
use rodio::Source;

// ─── Constants ──────────────────────────────────────────────────────────────

const BPM_MIN: i32 = 10;
const BPM_MAX: i32 = 400;
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

/// Max simultaneously active pulse rings.
const PULSE_MAX_RINGS: usize = 4;
/// How many render frames each pulse ring lives for.
const PULSE_LIFE_FRAMES: u8 = 18;

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
    fn next(self) -> Self {
        match self {
            SoundType::Click => SoundType::Wood,
            SoundType::Wood => SoundType::Cowbell,
            SoundType::Cowbell => SoundType::Sidestick,
            SoundType::Sidestick => SoundType::Beep,
            SoundType::Beep => SoundType::Click,
        }
    }

    /// (accent_freq, weak_freq) in Hz
    fn freqs(self) -> (f32, f32) {
        match self {
            SoundType::Click => (1760.0, 1320.0),
            SoundType::Wood => (1500.0, 900.0),
            SoundType::Cowbell => (2000.0, 1200.0),
            SoundType::Sidestick => (1200.0, 800.0),
            SoundType::Beep => (880.0, 660.0),
        }
    }

    fn label(self) -> &'static str {
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

// ─── Audio ──────────────────────────────────────────────────────────────────

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
    Duration::from_secs_f64(60.0 / bpm as f64)
}

/// Interval to the next beat, applying swing.
fn next_beat_duration(bpm: u32, swing: f32, current_beat: u8) -> Duration {
    let base = beat_interval(bpm);
    if swing <= 0.0 {
        return base;
    }
    let factor = if current_beat % 2 == 0 {
        1.0 + swing as f64
    } else {
        1.0 - swing as f64
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
        Some((60.0 / avg).round() as u32)
    }

    fn count(&self) -> usize {
        self.taps.len()
    }
}

// ─── Pulse rings (full-screen center expansion effect) ──────────────────────

/// One expanding ring from the center of the screen, triggered on each beat.
struct PulseRing {
    /// 0 = just spawned, PULSE_LIFE_FRAMES = expired.
    age: u8,
    /// true for accent beat (beat 1), false otherwise.
    accent: bool,
}

struct PulseFx {
    rings: Vec<PulseRing>,
    enabled: bool,
}

impl PulseFx {
    fn new() -> Self {
        Self {
            rings: Vec::with_capacity(PULSE_MAX_RINGS),
            enabled: false,
        }
    }

    fn trigger(&mut self, accent: bool) {
        if !self.enabled {
            return;
        }
        // Evict oldest ring if at capacity.
        if self.rings.len() >= PULSE_MAX_RINGS {
            self.rings.remove(0);
        }
        self.rings.push(PulseRing { age: 0, accent });
    }

    fn tick(&mut self) {
        // Age all rings, evict expired ones.
        self.rings.iter_mut().for_each(|r| r.age += 1);
        self.rings.retain(|r| r.age < PULSE_LIFE_FRAMES);
    }

    fn toggle(&mut self) {
        self.enabled = !self.enabled;
        if !self.enabled {
            self.rings.clear();
        }
    }
}

/// Draw the pulse rings centered on the full screen area.
fn render_pulse_rings(frame: &mut Frame, area: Rect, fx: &PulseFx) {
    if !fx.enabled || fx.rings.is_empty() {
        return;
    }
    let cx = area.x + area.width / 2;
    let cy = area.y + area.height / 2;
    // Scale: expand rate in columns per frame step. We use half-rows
    // (terminal cells are taller than wide) so horizontal expansion is
    // 2× vertical to look like a circle.
    let max_rings = fx.rings.len();
    for ring in &fx.rings {
        let progress = ring.age as f64 / PULSE_LIFE_FRAMES as f64;
        let fade = ((1.0 - progress) * 255.0) as u8;
        let radius = (ring.age as u16) * 2 + 1; // grows ~2 cells per frame
        let color = if ring.accent { Color::Rgb(255, 200, fade) } else { Color::Rgb(100, 200, fade) };

        // Draw expanded rectangle at each corner
        // We only draw the rectangular outline on a terminal so we
        // iterate from the center outward.
        let buf = frame.buffer_mut();
        let half_w = radius;
        let half_h = (radius as f32 * 0.5) as u16;
        let x1 = cx.saturating_sub(half_w);
        let x2 = (cx + half_w).min(area.right().saturating_sub(1));
        let y1 = cy.saturating_sub(half_h);
        let y2 = (cy + half_h).min(area.bottom().saturating_sub(1));

        for x in x1..=x2 {
            for &y in &[y1, y2] {
                if y < area.top() || y >= area.bottom() || x >= area.right() {
                    continue;
                }
                let cell = &mut buf[(x, y)];
                cell.set_symbol("·");
                cell.set_style(Style::default().fg(color));
            }
        }
        for y in y1..=y2 {
            for &x in &[x1, x2] {
                if x < area.left() || x >= area.right() || y >= area.bottom() {
                    continue;
                }
                let cell = &mut buf[(x, y)];
                cell.set_symbol("·");
                cell.set_style(Style::default().fg(color));
            }
        }
    }
    let _ = max_rings; // suppress unused warning
}

// ─── Beat indicators ────────────────────────────────────────────────────────

fn beat_span(beat_idx: u8, current_beat: u8, flash_active: bool) -> Span<'static> {
    let accent = beat_idx == 0;
    let is_current = beat_idx == current_beat;

    let symbol = if is_current && flash_active { "◉" } else { "○" };

    let style = match (is_current, flash_active, accent) {
        (true, true, true) => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        (true, true, false) => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        (true, false, _) => Style::default().fg(Color::DarkGray),
        (false, _, true) => Style::default().fg(Color::Gray),
        (false, _, false) => Style::default().fg(Color::DarkGray),
    };

    Span::styled(format!(" {} ", symbol), style)
}

// ─── TUI rendering ──────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn render(
    frame: &mut Frame,
    bpm: u32,
    running: bool,
    beat: u8,
    ts_num: u8,
    ts_den: u8,
    flash: u8,
    volume: f32,
    swing: f32,
    sound: SoundType,
    tap: &TapTempo,
    pulse: &PulseFx,
) {
    let area = frame.area();
    if area.width < 40 || area.height < 12 {
        let p = Paragraph::new("Terminal too small (min 40x12)").alignment(Alignment::Center);
        frame.render_widget(p, area);
        return;
    }

    // Render pulse rings behind everything (full screen area).
    render_pulse_rings(frame, area, pulse);

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

    // Controls hint
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
    frame.render_widget(Paragraph::new(hint).alignment(Alignment::Left), chunks[0]);

    // BPM display + beats (centered vertically)
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

    let flash_beat = flash > 0 && running;
    let bpm_style = if flash_beat {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else if running {
        Style::default().fg(Color::White)
    } else {
        Style::default().fg(Color::Gray)
    };

    // BPM line with time signature
    let bpm_line = Line::from(vec![
        Span::styled(format!("▐  {}  ▌", bpm), bpm_style),
        Span::raw("   "),
        Span::styled(
            format!("{}/{}", ts_num, ts_den),
            Style::default().fg(Color::Magenta),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(bpm_line).alignment(Alignment::Center),
        bpm_chunks[0],
    );

    let flash_active = flash > 0;
    let mut beat_spans: Vec<Span> = Vec::with_capacity(ts_num as usize * 3);
    for i in 0..ts_num {
        if i > 0 {
            beat_spans.push(Span::raw(" "));
        }
        beat_spans.push(beat_span(i, beat, flash_active));
    }
    frame.render_widget(
        Paragraph::new(Line::from(beat_spans)).alignment(Alignment::Center),
        bpm_chunks[1],
    );

    // Status bar
    let icon = if running { "▶" } else { "■" };
    let state_label = if running { "RUNNING" } else { "STOPPED" };
    let state_color = if running {
        Color::Green
    } else {
        Color::DarkGray
    };

    let mut status_spans = vec![
        Span::styled(
            format!("{} {}", icon, state_label),
            Style::default().fg(state_color),
        ),
        Span::raw("  |  "),
        Span::raw(format!("beat {}/{}", beat + 1, ts_num)),
        Span::raw("  |  "),
        Span::raw(format!("{} BPM", bpm)),
        Span::raw("  |  "),
        Span::raw(format!("vol: {:.0}%", volume * 100.0)),
        Span::raw("  |  "),
        Span::styled(
            format!("swing: {}", swing_label(swing)),
            if swing > 0.0 {
                Style::default().fg(Color::Magenta)
            } else {
                Style::default()
            },
        ),
        Span::raw("  |  "),
        Span::styled(
            format!("sound: {}", sound.label()),
            Style::default().fg(Color::Blue),
        ),
        Span::raw("  |  "),
        Span::styled(
            format!("pulse: {}", if pulse.enabled { "on" } else { "off" }),
            if pulse.enabled {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
    ];

    // Tap tempo indicator
    if tap.count() >= 2 {
        if let Some(tap_bpm) = tap.estimate_bpm() {
            status_spans.push(Span::raw("  |  "));
            status_spans.push(Span::styled(
                format!("tap: {} → {} BPM", tap.count(), tap_bpm),
                Style::default().fg(Color::Yellow),
            ));
        }
    }

    frame.render_widget(
        Paragraph::new(Line::from(status_spans)).alignment(Alignment::Center),
        chunks[2],
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

    let audio = AudioOutput::try_new();

    // State
    let mut bpm: u32 = 120;
    let mut running = false;
    let mut beat: u8 = 0;
    let mut ts_num: u8 = 4;
    let mut ts_den: u8 = 4;
    let mut flash: u8 = 0;
    let mut volume: f32 = 0.5;
    let mut swing: f32 = 0.0;
    let mut sound = SoundType::Click;
    let mut tap = TapTempo::new();
    let mut pulse = PulseFx::new();
    let mut next_beat = Instant::now();

    /// Cycle through common time signature presets (forward or backward).
    fn cycle_ts_preset(num: u8, den: u8, forward: bool) -> (u8, u8) {
        let idx = TS_PRESETS
            .iter()
            .position(|&(n, d)| n == num && d == den)
            .unwrap_or(0);
        let len = TS_PRESETS.len() as i32;
        let next_idx = if forward {
            (idx + 1) % len as usize
        } else {
            ((idx as i32 - 1 + len) % len) as usize
        };
        let next = TS_PRESETS[next_idx];
        (next.0, next.1)
    }

    loop {
        if event::poll(Duration::from_millis(8))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != event::KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    // Quit
                    KeyCode::Char('q') | KeyCode::Esc => break,

                    // Play/pause
                    KeyCode::Char(' ') => {
                        running = !running;
                        if running {
                            beat = 0;
                            next_beat = Instant::now();
                            audio.play_click(true, volume, sound);
                            flash = 6;
                            pulse.trigger(true);
                            next_beat += beat_interval(bpm);
                        }
                    }

                    // BPM adjust — no pause/resume, just change and reschedule
                    KeyCode::Up | KeyCode::Char('+') => {
                        let new = (bpm as i32 + 1).clamp(BPM_MIN, BPM_MAX) as u32;
                        if new != bpm {
                            bpm = new;
                            if running {
                                next_beat = Instant::now() + next_beat_duration(bpm, swing, beat);
                            }
                        }
                    }
                    KeyCode::Down | KeyCode::Char('-') => {
                        let new = (bpm as i32 - 1).clamp(BPM_MIN, BPM_MAX) as u32;
                        if new != bpm {
                            bpm = new;
                            if running {
                                next_beat = Instant::now() + next_beat_duration(bpm, swing, beat);
                            }
                        }
                    }
                    KeyCode::PageUp => {
                        let new = (bpm as i32 + 10).clamp(BPM_MIN, BPM_MAX) as u32;
                        if new != bpm {
                            bpm = new;
                            if running {
                                next_beat = Instant::now() + next_beat_duration(bpm, swing, beat);
                            }
                        }
                    }
                    KeyCode::PageDown => {
                        let new = (bpm as i32 - 10).clamp(BPM_MIN, BPM_MAX) as u32;
                        if new != bpm {
                            bpm = new;
                            if running {
                                next_beat = Instant::now() + next_beat_duration(bpm, swing, beat);
                            }
                        }
                    }

                    // Volume
                    KeyCode::Char('[') => volume = (volume - 0.1).max(0.0),
                    KeyCode::Char(']') => volume = (volume + 0.1).min(1.0),

                    // Time signature — Tab cycles presets forward, Shift+Tab (BackTab) backward
                    KeyCode::Tab => {
                        let (n, d) = cycle_ts_preset(ts_num, ts_den, true);
                        ts_num = n;
                        ts_den = d;
                        if beat >= ts_num {
                            beat = 0;
                        }
                    }
                    KeyCode::BackTab => {
                        let (n, d) = cycle_ts_preset(ts_num, ts_den, false);
                        ts_num = n;
                        ts_den = d;
                        if beat >= ts_num {
                            beat = 0;
                        }
                    }

                    // Time signature — numerator (1-9 beats per measure)
                    KeyCode::Char(c) if c >= '1' && c <= '9' => {
                        ts_num = cell_to_digit(9, c);
                        if beat >= ts_num {
                            beat = 0;
                        }
                    }

                    // Time signature — denominator (d cycles 4→8→16)
                    KeyCode::Char('d') => {
                        ts_den = match ts_den {
                            4 => 8,
                            8 => 16,
                            _ => 4,
                        };
                    }

                    // Tap tempo
                    KeyCode::Char('t') => {
                        if let Some(tap_bpm) = tap.tap() {
                            if tap_bpm >= BPM_MIN as u32 && tap_bpm <= BPM_MAX as u32 {
                                bpm = tap_bpm;
                                if running {
                                    next_beat = Instant::now()
                                        + next_beat_duration(bpm, swing, beat);
                                }
                            }
                        }
                    }

                    // Swing cycle
                    KeyCode::Char('w') => swing = next_swing(swing),

                    // Sound cycle
                    KeyCode::Char('n') => sound = sound.next(),

                    // Pulse effect toggle
                    KeyCode::Char('p') => pulse.toggle(),

                    _ => {}
                }
            }
        }

        // Beat scheduling
        if running {
            let now = Instant::now();
            while now >= next_beat {
                beat = (beat + 1) % ts_num;
                audio.play_click(beat == 0, volume, sound);
                flash = 6;
                pulse.trigger(beat == 0);
                next_beat += next_beat_duration(bpm, swing, beat);

                if now + beat_interval(bpm) * 2 < next_beat {
                    next_beat = now + next_beat_duration(bpm, swing, beat);
                }
            }
        }

        // Flash decay
        if flash > 0 {
            flash -= 1;
        }

        // Pulse ring aging
        pulse.tick();

        // Render
        terminal.draw(|f| {
            render(
                f, bpm, running, beat, ts_num, ts_den, flash, volume, swing, sound, &tap, &pulse,
            );
        })?;
    }

    // Cleanup
    disable_raw_mode()?;
    terminal.show_cursor()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    println!("metronome stopped");
    Ok(())
}

/// Convert a digit char to a u8, clamped to a maximum.
fn cell_to_digit(max: u8, c: char) -> u8 {
    let d = c.to_digit(10).unwrap_or(0) as u8;
    d.min(max)
}
