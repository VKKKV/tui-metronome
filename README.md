# tui-metronome

A terminal metronome with a clean TUI, built with Rust + ratatui + rodio.

## Features

- **Tap tempo** — press `t` repeatedly, BPM auto-detected from your taps
- **Swing** — 4 levels: straight / light / swing / triplet
- **5 sound types** — click / wood / cowbell / sidestick / beep
- **Adjustable volume** — 0% to 100%
- **Time signature** — numerator 1–9, denominator 4/8/16, plus Tab preset cycle: 4/4, 3/4, 2/4, 6/8, 5/4, 7/8, 9/8, 12/8
- **BPM range** — 10 to 400
- **Pulse effect** — full-screen center expansion rings on each beat (toggle with `p`)
- **Visual beat indicators** — flashing beat dots, accent on beat 1
- **Single-threaded** — no audio thread overhead, Instant-based precision

## Install

### From source

```bash
git clone https://github.com/VKKKV/tui-metronome.git
cd tui-metronome
cargo build --release
# Binary at target/release/tui-metronome
```

### Cargo

```bash
cargo install --path .
```

### Dependencies

Linux requires ALSA development headers for rodio:

```
# Debian/Ubuntu
sudo apt install libasound2-dev

# Arch
sudo pacman -S alsa-lib

# Fedora
sudo dnf install alsa-lib-devel
```

## Usage

```bash
tui-metronome
```

### Controls

| Key | Action |
|-----|--------|
| `Space` | Start / stop |
| `↑` `↓` or `+` `-` | BPM ±1 |
| `PgUp` `PgDn` | BPM ±10 |
| `Tab` / `Shift+Tab` | Cycle time signature presets forward / backward (4/4 → 3/4 → 2/4 → 6/8 → 5/4 → 7/8 → 9/8 → 12/8) |
| `1`–`9` | Set numerator (beats per measure) |
| `d` | Cycle denominator (4 → 8 → 16) |
| `t` | Tap tempo (press 2+ times) |
| `w` | Cycle swing (straight → light → swing → triplet) |
| `n` | Cycle sound (click → wood → cowbell → sidestick → beep) |
| `p` | Toggle pulse effect (full-screen center expansion rings on each beat) |
| `[` `]` | Volume −10% / +10% |
| `q` or `Esc` | Quit |

### Tap Tempo

Press `t` at least 2 times. The metronome averages your tap intervals and sets the BPM automatically. Taps older than 2 seconds are discarded. Up to 8 taps are kept; the detected BPM shows in the status bar.

### Swing

Swing shifts alternate beats — the off-beat is delayed, giving a triplet/shuffle feel:

| Setting | Swing factor | Feel |
|---------|-------------|------|
| straight | 0.0 | Even |
| light | 0.33 | Slight shuffle |
| swing | 0.5 | Classic swing |
| triplet | 0.66 | Heavy triplet |

### Pulse Effect

Press `p` to toggle a full-screen visual pulse: on each beat, a ring expands outward from the center of the terminal, fading as it grows. Accent beats (beat 1) glow warm yellow; other beats glow cool blue. Up to 4 rings can be active simultaneously. The effect is purely visual — it does not affect audio timing.

## Architecture

- **ratatui** — terminal UI framework
- **crossterm** — terminal control (raw mode, alternate screen)
- **rodio** — audio playback (ALSA sink via PulseAudio/PipeWire)
- Single-threaded event loop with 8ms poll interval
- `Instant`-based beat scheduling for drift-free timing
- Swing modifies the gap between beats: after on-beats the gap is longer, after off-beats shorter
- Pulse rings are drawn directly to the ratatui buffer as rectangular outlines, squashed vertically to approximate circles

## License

GPL-3.0 — see [LICENSE](LICENSE).