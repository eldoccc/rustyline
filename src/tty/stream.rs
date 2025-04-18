use crate::config::{BellStyle, Config};
use crate::error::ReadlineError;
use crate::tty::{Event, RawMode, RawReader, Renderer, Term};
use crate::{Behavior, Cmd, ColorMode, ExternalPrinter, GraphemeClusterMode, KeyEvent, Result};
use std::io::{self, stdin, stdout, BufWriter, Read, Stdin, Stdout, Write};
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use nix::errno::Errno;
use nix::unistd::write;
use unicode_segmentation::UnicodeSegmentation;
use crate::highlight::Highlighter;
use crate::layout::{Layout, Position, Unit};
use crate::line_buffer::LineBuffer;

pub type Terminal = StreamTerminal;
pub type KeyMap = ();
pub type Mode = StreamMode;
pub type Buffer = ();

#[derive(Debug)]
pub struct StreamTerminal {
    pub(crate) color_mode: ColorMode,
    grapheme_cluster_mode: GraphemeClusterMode,
    behavior: Behavior,
    tab_stop: u8,
    bell_style: BellStyle,
    enable_bracketed_paste: bool,
    enable_signals: bool,
}

impl Term for StreamTerminal {
    type Buffer = ();
    type KeyMap = ();
    type Reader = StreamReader;
    type Writer = StreamWriter;
    type Mode = StreamMode;

    type ExternalPrinter = StreamExternalPrinter<io::Stdout>;

    type CursorGuard = ();

    fn new(
        color_mode: ColorMode,
        grapheme_cluster_mode: GraphemeClusterMode,
        behavior: Behavior,
        tab_stop: u8,
        bell_style: BellStyle,
        enable_bracketed_paste: bool,
        enable_signals: bool,
    ) -> Result<Self> {

        Ok(Self{
            color_mode,
            grapheme_cluster_mode,
            behavior,
            tab_stop,
            bell_style,
            enable_bracketed_paste,
            enable_signals,
        })
    }

    fn is_unsupported(&self) -> bool {
        false
    }

    fn is_input_tty(&self) -> bool {
        true
    }

    fn is_output_tty(&self) -> bool {
        false
    }

    fn enable_raw_mode(&mut self) -> Result<(Self::Mode, ())> {
        Ok((StreamMode, ()))
    }

    fn create_reader(
        &self,
        buffer: Option<()>,
        config: &Config,
        key_map: (),
    ) -> Self::Reader {
        StreamReader::new()
    }
    fn create_writer(&self) -> Self::Writer {
        StreamWriter::new(
            Box::new(BufWriter::new(stdout())),
            stdout().as_raw_fd(),
            self.grapheme_cluster_mode,
            self.bell_style,
        )
    }

    fn writeln(&self) -> Result<()> {
        Ok(())
    }

    fn create_external_printer(&mut self) -> Result<Self::ExternalPrinter> {
        Ok(StreamExternalPrinter {
            writer: io::stdout(),
        })
    }

    fn set_cursor_visibility(&mut self, visible: bool) -> Result<Option<Self::CursorGuard>> {
        Ok(None)
    }
}

pub struct StreamReader {
    input: Stdin,
}

impl StreamReader {
    pub fn new() -> Self {
        Self { input : stdin() }
    }
}

impl RawReader for StreamReader {
    type Buffer = ();

    fn wait_for_input(&mut self, _single_esc_abort: bool) -> Result<Event> {
        let mut buf = [0u8; 128];
        if self.input.read(&mut buf)? == 0 {
            return Err(ReadlineError::Eof);
        }
        Ok(Event::ExternalPrint(String::from_utf8_lossy(&buf).to_string()))
    }

    fn next_key(&mut self, _single_esc_abort: bool) -> Result<KeyEvent> {
        // Implement any necessary key parsing if wanted
        unimplemented!()
    }

    #[cfg(unix)]
    fn next_char(&mut self) -> Result<char> {
        let mut single = [0_u8];
        self.input.read_exact(&mut single)?;
        Ok(single[0] as char)
    }

    fn read_pasted_text(&mut self) -> Result<String> {
        // Handle multi-line paste scenarios if desired
        let mut buf = String::new();
        self.input.read_to_string(&mut buf)?;
        Ok(buf)
    }

    fn find_binding(&self, _key: &KeyEvent) -> Option<Cmd> {
        None
    }

    fn unbuffer(self) -> Option<()> {
        None
    }
}

pub struct StreamWriter {
    stream: Box<dyn Write + Send>,
    cols: Unit,
    rows: Unit,
    buffer: String,
    grapheme_cluster_mode: GraphemeClusterMode,
    bell_style: BellStyle,
}

impl StreamWriter {
    pub fn new(
        stream: Box<dyn Write + Send>,
        out: RawFd,
        grapheme_cluster_mode: GraphemeClusterMode,
        bell_style: BellStyle,
    ) -> Self {
        #[cfg(unix)]
        let (cols, rows) = crate::tty::unix::get_win_size(out);
        #[cfg(windows)]
        let (cols, rows) = crate::tty::windows::get_win_size(out);
        Self {
            stream,
            cols,
            rows,
            buffer: String::with_capacity(1024),
            grapheme_cluster_mode,
            bell_style,
        }
    }

    fn clear(&mut self, length: u32, pos: Position) -> Result<()> {
        let mut clear_cmd = String::new();
        for _ in 0..length {
            clear_cmd.push(' ');
        }
        self.write_and_flush(&clear_cmd)?;
        Ok(())
    }
}

impl Renderer for StreamWriter {
    type Reader = StreamReader;

    fn move_cursor(&mut self, old: Position, new: Position) -> Result<()> {
        let mut cursor_cmd = String::new();
        if new.row > old.row {
            cursor_cmd.push_str(&format!("\x1b[{}B", new.row - old.row));
        } else {
            cursor_cmd.push_str(&format!("\x1b[{}A", old.row - new.row));
        }
        if new.col > old.col {
            cursor_cmd.push_str(&format!("\x1b[{}C", new.col - old.col));
        } else {
            cursor_cmd.push_str(&format!("\x1b[{}D", old.col - new.col));
        }
        self.write_and_flush(&cursor_cmd)?;
        Ok(())
    }

    fn refresh_line(
        &mut self,
        prompt: &str,
        line: &LineBuffer,
        hint: Option<&str>,
        old_layout: &Layout,
        new_layout: &Layout,
        highlighter: Option<&dyn Highlighter>,
    ) -> Result<()> {
        self.buffer.clear();
        self.buffer.push_str(prompt);
        if let Some(hint) = hint {
            self.buffer.push_str(hint);
        }
        write_all(&mut self.stream, self.buffer.as_str())?;
        
        Ok(())
    }

    fn calculate_position(&self, s: &str, orig: Position) -> Position {
        let mut pos = orig;
        for c in s.graphemes(true) {
            if c == "\n" {
                pos.col = 0;
                pos.row += 1;
            } else {
                let cw = self.grapheme_cluster_mode.width(c);
                pos.col += cw;
                pos.row += 1;
                pos.col = cw;
            }
        }
    if pos.col == self.cols {
    pos.col = 0;
    pos.row += 1;
}
pos
}

    fn write_and_flush(&mut self, buf: &str) -> Result<()> {
        self.stream.write_all(buf.as_bytes())?;
        self.stream.flush()?;
        Ok(())
    }

    fn beep(&mut self) -> Result<()> {
    if self.bell_style == BellStyle::Audible {
        self.write_and_flush("\x07")?;
    }
    Ok(())
}

fn clear_screen(&mut self) -> Result<()> {
    self.write_and_flush("\x1b[H\x1b[J")
}

fn clear_rows(&mut self, layout: &Layout) -> Result<()> {
    let mut clear_cmd = String::new();
    for _ in 0..=layout.end.row {
        clear_cmd.push_str("\x1b[K\n");
    }
    self.write_and_flush(&clear_cmd)?;
    Ok(())
}

fn update_size(&mut self) {
    // Assuming fixed size for simplicity
    self.cols = 80;
    self.rows = 24;
}

fn get_columns(&self) -> Unit {
    self.cols
}

fn get_rows(&self) -> Unit {
    self.rows
}

fn colors_enabled(&self) -> bool {
    true
}

fn grapheme_cluster_mode(&self) -> GraphemeClusterMode {
    self.grapheme_cluster_mode
}

fn move_cursor_at_leftmost(&mut self, _: &mut Self::Reader) -> Result<()> {
    self.write_and_flush("\x1b[H")?;
    Ok(())
}
}

fn write_all(writer: &mut Box<dyn Write + Send>, buf: &str) -> nix::Result<()> {
    let mut bytes = buf.as_bytes();
    while !bytes.is_empty() {
        match writer.write(bytes) {
            Ok(0) => return Err(Errno::EIO),
            Ok(n) => bytes = &bytes[n..],
            Err(e) => return Err(Errno::from_i32(e.raw_os_error().unwrap())),
        }
    }
    Ok(())
}

pub struct StreamMode;

impl RawMode for StreamMode {
    fn disable_raw_mode(&self) -> Result<()> {
        Ok(())
    }
}

pub struct StreamExternalPrinter<W: Write> {
    writer: W,
}

impl<W: Write> ExternalPrinter for StreamExternalPrinter<W> {
    fn print(&mut self, msg: String) -> crate::Result<()> {
        self.writer.write_all(msg.as_bytes())?;
        self.writer.flush()?;
        Ok(())
    }
}