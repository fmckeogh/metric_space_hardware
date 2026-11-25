//! Minimal terminal UI for talking to the FPGA over the iCEstick's UART.
//!
//! Usage: cli <PORT> [BAUD]
//! Run without arguments to list serial ports.

use {
    ratatui::{
        DefaultTerminal, Frame,
        crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
        layout::{Constraint, Layout},
        text::Line,
        widgets::{Block, Paragraph},
    },
    serialport::{SerialPort, SerialPortType},
    std::{
        error::Error,
        io::{self, Read, Write},
        sync::mpsc::{self, Receiver, TryRecvError},
        thread,
        time::Duration,
    },
};

/// Must match CLKS_PER_BIT in src/top.veryl (12 MHz / 12 = 1 Mbaud)
const DEFAULT_BAUD: u32 = 1_000_000;
const BYTES_PER_LINE: usize = 16;
const MAX_LINES: usize = 1000;

#[derive(Clone, Copy, PartialEq)]
enum Dir {
    Tx,
    Rx,
}

struct App {
    port: Box<dyn SerialPort>,
    port_name: String,
    baud: u32,
    /// Hex log, at most BYTES_PER_LINE bytes per line
    lines: Vec<(Dir, Vec<u8>)>,
    input: String,
    tx_count: usize,
    rx_count: usize,
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let Some(port_name) = args.next() else {
        println!("usage: cli <PORT> [BAUD]\n\navailable ports:");
        for p in serialport::available_ports()? {
            match p.port_type {
                SerialPortType::UsbPort(usb) => {
                    println!("  {}  (USB {:04x}:{:04x})", p.port_name, usb.vid, usb.pid)
                }
                _ => println!("  {}", p.port_name),
            }
        }
        return Ok(());
    };
    let baud = match args.next() {
        Some(b) => b.parse()?,
        None => DEFAULT_BAUD,
    };

    let port = serialport::new(&port_name, baud)
        .timeout(Duration::from_millis(100))
        .open()?;
    let rx = spawn_reader(port.try_clone()?);

    let mut app = App {
        port,
        port_name,
        baud,
        lines: Vec::new(),
        input: String::new(),
        tx_count: 0,
        rx_count: 0,
    };

    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal, &rx);
    ratatui::restore();
    result
}

/// Read from the port on its own thread so the UI never blocks on it.
/// The channel closes when the port does.
fn spawn_reader(mut port: Box<dyn SerialPort>) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = [0u8; 256];
        loop {
            match port.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        return; // UI has exited
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {}
                Err(_) => return,
            }
        }
    });
    rx
}

impl App {
    fn run(
        &mut self,
        terminal: &mut DefaultTerminal,
        rx: &Receiver<Vec<u8>>,
    ) -> Result<(), Box<dyn Error>> {
        loop {
            loop {
                match rx.try_recv() {
                    Ok(bytes) => self.record(Dir::Rx, &bytes),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Err("serial port closed".into()),
                }
            }

            terminal.draw(|frame| self.draw(frame))?;

            if !event::poll(Duration::from_millis(20))? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(());
                }
                KeyCode::Char(c) => self.input.push(c),
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Enter if !self.input.is_empty() => {
                    let bytes = std::mem::take(&mut self.input).into_bytes();
                    self.port.write_all(&bytes)?;
                    self.record(Dir::Tx, &bytes);
                }
                _ => {}
            }
        }
    }

    /// Append bytes to the log, starting a new line when the direction changes
    /// or the current line is full.
    fn record(&mut self, dir: Dir, bytes: &[u8]) {
        match dir {
            Dir::Tx => self.tx_count += bytes.len(),
            Dir::Rx => self.rx_count += bytes.len(),
        }
        for &b in bytes {
            match self.lines.last_mut() {
                Some((d, line)) if *d == dir && line.len() < BYTES_PER_LINE => line.push(b),
                _ => self.lines.push((dir, vec![b])),
            }
        }
        if self.lines.len() > MAX_LINES {
            self.lines.drain(..self.lines.len() - MAX_LINES);
        }
    }

    fn draw(&self, frame: &mut Frame) {
        let [log_area, input_area] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(3)]).areas(frame.area());

        // Most recent lines that fit inside the border
        let visible = log_area.height.saturating_sub(2) as usize;
        let start = self.lines.len().saturating_sub(visible);
        let lines: Vec<Line> = self.lines[start..]
            .iter()
            .map(|(dir, bytes)| format_line(*dir, bytes))
            .collect();

        let title = format!(
            " {} @ {} baud · TX {} B · RX {} B ",
            self.port_name, self.baud, self.tx_count, self.rx_count
        );
        frame.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(title)),
            log_area,
        );

        frame.render_widget(
            Paragraph::new(self.input.as_str())
                .block(Block::bordered().title(" Send text: Enter · Quit: Esc ")),
            input_area,
        );
        frame.set_cursor_position((
            input_area.x + 1 + self.input.chars().count() as u16,
            input_area.y + 1,
        ));
    }
}

/// e.g. `RX 68 65 6c 6c 6f                                    hello`
fn format_line(dir: Dir, bytes: &[u8]) -> Line<'static> {
    let label = match dir {
        Dir::Tx => "TX",
        Dir::Rx => "RX",
    };
    let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let ascii: String = bytes
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            }
        })
        .collect();
    Line::from(format!(
        "{label} {:<width$}  {ascii}",
        hex.join(" "),
        width = BYTES_PER_LINE * 3 - 1
    ))
}
