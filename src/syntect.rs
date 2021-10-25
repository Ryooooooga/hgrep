use crate::chunk::File;
use crate::chunk::Line;
use crate::printer::{Printer, PrinterOptions, TermColorSupport};
use anyhow::Result;
use memchr::{memchr_iter, Memchr};
use rgb2ansi256::rgb_to_ansi256;
use std::cmp;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt;
use std::io::Write;
use std::io::{self, Stdout, StdoutLock};
use std::ops::{Deref, DerefMut};
use std::path::Path;
use syntect::highlighting::{
    Color, FontStyle, HighlightIterator, HighlightState, Highlighter, Style, Theme, ThemeSet,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

// Note for lifetimes:
// - 'file is a lifetime for File instance which is passed to print() method
// - 'main is a lifetime for the scope of main function (the caller of printer)

const SYNTAX_SET_BIN: &[u8] = include_bytes!("../assets/syntaxes.bin");
const THEME_SET_BIN: &[u8] = include_bytes!("../assets/themes.bin");

pub trait LockableWrite<'a> {
    type Locked: Write;
    fn lock(&'a self) -> Self::Locked;
}

impl<'a> LockableWrite<'a> for Stdout {
    type Locked = StdoutLock<'a>;
    fn lock(&'a self) -> Self::Locked {
        self.lock()
    }
}

pub fn list_themes<W: Write>(mut out: W) -> Result<()> {
    let mut seen = HashSet::new();
    let bat_defaults = bincode::deserialize_from(flate2::read::ZlibDecoder::new(THEME_SET_BIN))?;
    let defaults = ThemeSet::load_defaults();
    for themes in &[bat_defaults, defaults] {
        for name in themes.themes.keys() {
            if !seen.contains(name) {
                writeln!(out, "{}", name)?;
                seen.insert(name);
            }
        }
    }
    Ok(())
}

// Use u64::log10 once it is stabilized: https://github.com/rust-lang/rust/issues/70887
#[inline]
fn num_digits(n: u64) -> u16 {
    (n as f64).log10() as u16 + 1
}

#[derive(Debug)]
pub struct PrintError {
    message: String,
}

impl PrintError {
    fn new<S: Into<String>>(msg: S) -> Self {
        Self {
            message: msg.into(),
        }
    }
}

impl std::error::Error for PrintError {}

impl fmt::Display for PrintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Error while printing output with syntect: {}",
            &self.message
        )
    }
}

struct Canvas<'file, W: Write> {
    out: W,
    tab_width: u16,
    theme: &'file Theme,
    true_color: bool,
    background: bool,
    match_color: Option<Color>,
}

impl<'file, W: Write> Deref for Canvas<'file, W> {
    type Target = W;
    fn deref(&self) -> &Self::Target {
        &self.out
    }
}
impl<'file, W: Write> DerefMut for Canvas<'file, W> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.out
    }
}

enum LineDrawState<'line> {
    Continue(usize),
    Break(&'line str),
}
enum LineDrawn<'line> {
    Done,
    Wrap(&'line str, usize),
}

impl<'file, W: Write> Canvas<'file, W> {
    fn set_bg(&mut self, c: Color) -> Result<()> {
        // In case of c.a == 0 and c.a == 1 are handling for special colorscheme by bat for non true
        // color terminals. Color value is encoded in R. See `to_ansi_color()` in bat/src/terminal.rs
        match c.a {
            0 if c.r <= 7 => write!(self.out, "\x1b[{}m", c.r + 40)?, // 16 colors; e.g. 0 => 40 (Black), 7 => 47 (White)
            0 => write!(self.out, "\x1b[48;5;{}m", c.r)?,             // 256 colors
            1 => { /* Pass through. Do nothing */ }
            _ if self.true_color => write!(self.out, "\x1b[48;2;{};{};{}m", c.r, c.g, c.b)?,
            _ => write!(self.out, "\x1b[48;5;{}m", rgb_to_ansi256(c.r, c.g, c.b))?,
        }
        Ok(())
    }

    fn set_fg(&mut self, c: Color) -> Result<()> {
        // In case of c.a == 0 and c.a == 1 are handling for special colorscheme by bat for non true
        // color terminals. Color value is encoded in R. See `to_ansi_color()` in bat/src/terminal.rs
        match c.a {
            0 if c.r <= 7 => write!(self.out, "\x1b[{}m", c.r + 30)?, // 16 colors; e.g. 3 => 33 (Yellow), 6 => 36 (Cyan)
            0 => write!(self.out, "\x1b[38;5;{}m", c.r)?,             // 256 colors
            1 => { /* Pass through. Do nothing */ }
            _ if self.true_color => write!(self.out, "\x1b[38;2;{};{};{}m", c.r, c.g, c.b)?,
            _ => write!(self.out, "\x1b[38;5;{}m", rgb_to_ansi256(c.r, c.g, c.b))?,
        }
        Ok(())
    }

    fn set_default_bg(&mut self) -> Result<()> {
        if self.background {
            if let Some(bg) = self.theme.settings.background {
                self.set_bg(bg)?;
            }
        }
        Ok(())
    }

    fn set_bold(&mut self) -> Result<()> {
        self.out.write_all(b"\x1b[1m")?;
        Ok(())
    }

    fn set_underline(&mut self) -> Result<()> {
        self.out.write_all(b"\x1b[4m")?;
        Ok(())
    }

    fn set_font_style(&mut self, style: FontStyle) -> Result<()> {
        if style.contains(FontStyle::BOLD) {
            self.set_bold()?;
        }
        if style.contains(FontStyle::UNDERLINE) {
            self.set_underline()?;
        }
        Ok(())
    }

    fn unset_font_style(&mut self, style: FontStyle) -> Result<()> {
        if style.contains(FontStyle::BOLD) {
            self.out.write_all(b"\x1b[22m")?;
        }
        if style.contains(FontStyle::UNDERLINE) {
            self.out.write_all(b"\x1b[24m")?;
        }
        Ok(())
    }

    fn reset_color(&mut self) -> Result<()> {
        self.out.write_all(b"\x1b[0m")?;
        Ok(())
    }

    fn draw_spaces(&mut self, num: usize) -> Result<()> {
        for _ in 0..num {
            self.out.write_all(b" ")?;
        }
        Ok(())
    }

    // Returns number of tab characters in the text
    fn draw_text<'line>(&mut self, text: &'line str, limit: usize) -> Result<LineDrawState<'line>> {
        let mut width = 0;
        for (i, c) in text.char_indices() {
            width += if c == '\t' && self.tab_width > 0 {
                let w = self.tab_width as usize;
                if width + w > limit {
                    self.draw_spaces(limit - width)?;
                    // `+ 1` for skipping rest of \t
                    return Ok(LineDrawState::Break(&text[i + 1..]));
                }
                self.draw_spaces(self.tab_width as usize)?;
                w
            } else {
                let w = c.width_cjk().unwrap_or(0);
                if width + w > limit {
                    return Ok(LineDrawState::Break(&text[i..]));
                }
                write!(self.out, "{}", c)?;
                w
            };
        }
        Ok(LineDrawState::Continue(width))
    }

    fn fill_spaces(&mut self, written_width: usize, max_width: usize) -> Result<()> {
        if written_width < max_width {
            self.draw_spaces(max_width - written_width)?;
        }
        self.reset_color()
    }

    fn draw_texts<'line>(
        &mut self,
        parts: &[(Style, &'line str)],
        matched: bool,
        max_width: usize,
    ) -> Result<LineDrawn<'line>> {
        if matched {
            if let Some(bg) = self.match_color {
                self.set_bg(bg)?;
            }
        }

        let mut width = 0;
        for (idx, (style, text)) in parts.iter().enumerate() {
            if !matched && self.background {
                self.set_bg(style.background)?;
            }
            self.set_fg(style.foreground)?;
            self.set_font_style(style.font_style)?;
            match self.draw_text(text, max_width - width)? {
                LineDrawState::Continue(w) => width += w,
                LineDrawState::Break(rest) => {
                    self.reset_color()?;
                    return Ok(LineDrawn::Wrap(rest, idx));
                }
            }
            self.unset_font_style(style.font_style)?;
        }

        if width == 0 && !matched {
            self.set_default_bg()?; // For empty line
        }
        if matched || self.background {
            self.fill_spaces(width, max_width)?;
        } else {
            self.reset_color()?;
        }

        Ok(LineDrawn::Done)
    }
}

// Note: More flexible version of syntect::easy::HighlightLines for our use case
struct LineHighlighter<'a> {
    hl: Highlighter<'a>,
    parse_state: ParseState,
    hl_state: HighlightState,
    syntaxes: &'a SyntaxSet,
}

impl<'a> LineHighlighter<'a> {
    fn new(syntax: &SyntaxReference, theme: &'a Theme, syntaxes: &'a SyntaxSet) -> Self {
        let hl = Highlighter::new(theme);
        let parse_state = ParseState::new(syntax);
        let hl_state = HighlightState::new(&hl, ScopeStack::new());
        Self {
            hl,
            parse_state,
            hl_state,
            syntaxes,
        }
    }

    fn skip_line(&mut self, line: &str) {
        let ops = self.parse_state.parse_line(line, self.syntaxes);
        for _ in HighlightIterator::new(&mut self.hl_state, &ops, line, &self.hl) {}
    }

    fn highlight<'line>(&mut self, line: &'line str) -> Vec<(Style, &'line str)> {
        let ops = self.parse_state.parse_line(line, self.syntaxes);
        HighlightIterator::new(&mut self.hl_state, &ops, line, &self.hl).collect()
    }
}
// Like chunk::Lines, but includes newlines
struct LinesInclusive<'a> {
    lnum: usize,
    prev: usize,
    buf: &'a [u8],
    iter: Memchr<'a>,
}
impl<'a> LinesInclusive<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self {
            lnum: 1,
            prev: 0,
            buf,
            iter: memchr_iter(b'\n', buf),
        }
    }
}
impl<'a> Iterator for LinesInclusive<'a> {
    type Item = Line<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if let Some(idx) = self.iter.next() {
            let lnum = self.lnum;
            let end = idx + 1;
            let line = &self.buf[self.prev..end];
            self.prev = end;
            self.lnum += 1;
            Some(Line(line, lnum as u64))
        } else if self.prev == self.buf.len() {
            None
        } else {
            let line = &self.buf[self.prev..];
            self.prev = self.buf.len();
            Some(Line(line, self.lnum as u64))
        }
    }
}

// Drawer is responsible for one-time screen drawing
struct Drawer<'file, W: Write> {
    theme: &'file Theme,
    grid: bool,
    term_width: u16,
    lnum_width: u16,
    background: bool,
    gutter_color: Color,
    canvas: Canvas<'file, W>,
}

impl<'file, W: Write> Drawer<'file, W> {
    fn new(out: W, opts: &PrinterOptions, theme: &'file Theme, chunks: &[(u64, u64)]) -> Self {
        let last_lnum = chunks.last().map(|(_, e)| *e).unwrap_or(0);
        let mut lnum_width = num_digits(last_lnum);
        if chunks.len() > 1 {
            lnum_width = cmp::max(lnum_width, 3); // Consider '...' in gutter
        }

        let gutter_color = theme.settings.gutter_foreground.unwrap_or(Color {
            r: 128,
            g: 128,
            b: 128,
            a: 255,
        });

        let canvas = Canvas {
            theme,
            true_color: opts.color_support == TermColorSupport::True,
            tab_width: opts.tab_width as u16,
            background: opts.background_color,
            match_color: theme.settings.line_highlight.or(theme.settings.background),
            out,
        };

        Drawer {
            theme,
            grid: opts.grid,
            term_width: opts.term_width,
            lnum_width,
            background: opts.background_color,
            gutter_color,
            canvas,
        }
    }

    #[inline]
    fn gutter_width(&self) -> u16 {
        if self.grid {
            self.lnum_width + 4
        } else {
            self.lnum_width + 2
        }
    }

    fn draw_horizontal_line(&mut self, sep: &str) -> Result<()> {
        self.canvas.set_fg(self.gutter_color)?;
        self.canvas.set_default_bg()?;
        let gutter_width = self.gutter_width();
        for _ in 0..gutter_width - 2 {
            self.canvas.write_all("─".as_bytes())?;
        }
        self.canvas.write_all(sep.as_bytes())?;
        for _ in 0..self.term_width - gutter_width + 1 {
            self.canvas.write_all("─".as_bytes())?;
        }
        self.canvas.reset_color()?;
        writeln!(self.canvas)?;
        Ok(())
    }

    fn draw_line_number(&mut self, lnum: u64, matched: bool) -> Result<()> {
        let fg = if matched {
            self.theme.settings.foreground.unwrap()
        } else {
            self.gutter_color
        };
        self.canvas.set_fg(fg)?;
        self.canvas.set_default_bg()?;
        let width = num_digits(lnum);
        self.canvas
            .draw_spaces((self.lnum_width - width) as usize)?;
        write!(self.canvas, " {}", lnum)?;
        if self.grid {
            if matched {
                self.canvas.set_fg(self.gutter_color)?;
            }
            self.canvas.write_all(" │".as_bytes())?;
        }
        self.canvas.set_default_bg()?;
        write!(self.canvas, " ")?;
        Ok(()) // Do not reset color because another color text will follow
    }

    fn draw_wrapping_gutter(&mut self) -> Result<()> {
        self.canvas.set_fg(self.gutter_color)?;
        self.canvas.set_default_bg()?;
        self.canvas.draw_spaces(self.lnum_width as usize + 2)?;
        if self.grid {
            self.canvas.write_all("│ ".as_bytes())?;
        }
        Ok(())
    }

    fn draw_separator_line(&mut self) -> Result<()> {
        self.canvas.set_fg(self.gutter_color)?;
        self.canvas.set_default_bg()?;
        // + 1 for left margin and - 3 for length of "..."
        let left_margin = self.lnum_width + 1 - 3;
        self.canvas.draw_spaces(left_margin as usize)?;
        let w = if self.grid {
            write!(self.canvas, "... ├")?;
            5
        } else {
            write!(self.canvas, "...")?;
            3
        };
        self.canvas.set_default_bg()?;
        let body_width = self.term_width - left_margin - w; // This crashes when terminal width is smaller than gutter
        for _ in 0..body_width {
            self.canvas.write_all("─".as_bytes())?;
        }
        writeln!(self.canvas)?;
        Ok(()) // We don't need to reset color for next line
    }

    fn draw_line(
        &mut self,
        mut parts: Vec<(Style, &'_ str)>,
        lnum: u64,
        matched: bool,
    ) -> Result<()> {
        // The highlighter requires newline at the end. But we don't want it since we sometimes need to fill the rest
        // of line with spaces. Chomp it.
        if let Some((_, s)) = parts.last_mut() {
            if s.ends_with('\n') {
                *s = &s[..s.len() - 1];
                if s.ends_with('\r') {
                    *s = &s[..s.len() - 1];
                }
            }
        }

        let body_width = (self.term_width - self.gutter_width()) as usize;
        self.draw_line_number(lnum, matched)?;
        let mut parts = parts.as_mut_slice();

        while let LineDrawn::Wrap(rest, idx) = self.canvas.draw_texts(parts, matched, body_width)? {
            writeln!(self.canvas.out)?;
            self.draw_wrapping_gutter()?;
            if rest.is_empty() {
                parts = &mut parts[idx + 1..];
            } else {
                parts = &mut parts[idx..];
                parts[0].1 = rest; // Set rest of the text broken by text wrapping
            }
        }

        writeln!(self.canvas.out)?;
        Ok(())
    }

    fn draw_body(&mut self, file: &File, mut hl: LineHighlighter<'_>) -> Result<()> {
        assert!(!file.chunks.is_empty());

        let mut matched = file.line_numbers.as_ref();
        let mut chunks = file.chunks.iter();
        let mut chunk = chunks.next().unwrap(); // OK since chunks is not empty

        // Note: `bytes` contains newline at the end since SyntaxSet requires it. The newline will be trimmed when
        // `HighlightedLine` instance is created.
        for Line(bytes, lnum) in LinesInclusive::new(&file.contents) {
            let (start, end) = *chunk;
            if lnum < start {
                hl.skip_line(String::from_utf8_lossy(bytes).as_ref()); // Discard parsed result
                continue;
            }
            if start <= lnum && lnum <= end {
                let matched = match matched.first().copied() {
                    Some(n) if n == lnum => {
                        matched = &matched[1..];
                        true
                    }
                    _ => false,
                };
                let line = String::from_utf8_lossy(bytes);
                // Collect to `Vec` rather than handing HighlightIterator as-is. HighlightIterator takes ownership of Highlighter
                // while the iteration. When the highlighter is stored in `self`, it means the iterator takes ownership of `self`.
                self.draw_line(hl.highlight(line.as_ref()), lnum, matched)?;

                if lnum == end {
                    if let Some(c) = chunks.next() {
                        self.draw_separator_line()?;
                        chunk = c;
                    } else {
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    fn draw_header(&mut self, path: &Path) -> Result<()> {
        self.draw_horizontal_line("─")?;
        self.canvas.set_default_bg()?;
        let path = path.as_os_str().to_string_lossy();
        self.canvas.set_bold()?;
        write!(self.canvas, " {}", path)?;
        if self.background {
            self.canvas
                .fill_spaces(path.width_cjk() + 1, self.term_width as usize)?;
        } else {
            self.canvas.reset_color()?;
        }
        writeln!(self.canvas)?;
        if self.grid {
            self.draw_horizontal_line("┬")?;
        }
        Ok(())
    }

    fn draw_footer(&mut self) -> Result<()> {
        if self.grid {
            self.draw_horizontal_line("┴")?;
        }
        Ok(())
    }
}

fn load_themes(name: Option<&str>) -> Result<ThemeSet> {
    let bat_defaults: ThemeSet =
        bincode::deserialize_from(flate2::read::ZlibDecoder::new(THEME_SET_BIN))?;
    match name {
        None => Ok(bat_defaults),
        Some(name) if bat_defaults.themes.contains_key(name) => Ok(bat_defaults),
        Some(name) => {
            let defaults = ThemeSet::load_defaults();
            if defaults.themes.contains_key(name) {
                Ok(defaults)
            } else {
                let msg = format!("Unknown theme '{}'. See --list-themes output", name);
                Err(PrintError::new(msg).into())
            }
        }
    }
}

pub struct SyntectPrinter<'main, W>
where
    for<'a> W: LockableWrite<'a>,
{
    writer: W, // Protected with mutex because it should print file by file
    syntaxes: SyntaxSet,
    themes: ThemeSet,
    opts: PrinterOptions<'main>,
}

impl<'main> SyntectPrinter<'main, Stdout> {
    pub fn with_stdout(opts: PrinterOptions<'main>) -> Result<Self> {
        Self::new(io::stdout(), opts)
    }
}

impl<'main, W> SyntectPrinter<'main, W>
where
    for<'a> W: LockableWrite<'a>,
{
    pub fn new(out: W, opts: PrinterOptions<'main>) -> Result<Self> {
        Ok(Self {
            writer: out,
            syntaxes: bincode::deserialize_from(flate2::read::ZlibDecoder::new(SYNTAX_SET_BIN))?,
            themes: load_themes(opts.theme)?,
            opts,
        })
    }

    pub fn writer_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    fn theme(&self) -> &Theme {
        let name = self.opts.theme.unwrap_or_else(|| {
            if self.opts.color_support == TermColorSupport::Ansi16 {
                "ansi"
            } else {
                "Monokai Extended" // Our 25bit -> 8bit color conversion works really well with this colorscheme
            }
        });
        &self.themes.themes[name]
    }

    fn find_syntax(&self, path: &Path) -> Result<&SyntaxReference> {
        let name = match path.extension().and_then(OsStr::to_str) {
            Some("fs") => Some("F#"),
            Some("h") => Some("C++"),
            Some("pac") => Some("JavaScript (Babel)"),
            _ => None,
        };
        if let Some(syntax) = name.and_then(|n| self.syntaxes.find_syntax_by_name(n)) {
            return Ok(syntax);
        }

        Ok(self
            .syntaxes
            .find_syntax_for_file(path)?
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text()))
    }
}

impl<'main, W> Printer for SyntectPrinter<'main, W>
where
    for<'a> W: LockableWrite<'a>,
{
    fn print(&self, file: File) -> Result<()> {
        if file.chunks.is_empty() || file.line_numbers.is_empty() {
            return Ok(());
        }

        let mut buf = vec![];
        let theme = self.theme();
        let syntax = self.find_syntax(&file.path)?;

        let mut drawer = Drawer::new(&mut buf, &self.opts, theme, &file.chunks);
        drawer.draw_header(&file.path)?;
        let hl = LineHighlighter::new(syntax, theme, &self.syntaxes);
        drawer.draw_body(&file, hl)?;
        drawer.draw_footer()?;

        // Take lock here to print files in serial from multiple threads
        let mut output = self.writer.lock();
        output.write_all(&buf)?;
        Ok(output.flush()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::File;
    use std::cell::{RefCell, RefMut};
    use std::fmt;
    use std::fs;
    use std::mem;
    use std::path::PathBuf;

    struct DummyStdoutLock<'a>(RefMut<'a, Vec<u8>>);
    impl<'a> Write for DummyStdoutLock<'a> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }

    #[derive(Default)]
    struct DummyStdout(RefCell<Vec<u8>>);
    impl<'a> LockableWrite<'a> for DummyStdout {
        type Locked = DummyStdoutLock<'a>;
        fn lock(&'a self) -> Self::Locked {
            DummyStdoutLock(self.0.borrow_mut())
        }
    }

    #[cfg(not(windows))]
    mod uitests {
        use super::*;
        use std::cmp;
        use std::path::Path;

        fn read_chunks(path: PathBuf) -> File {
            let contents = fs::read(&path).unwrap();
            let lines = contents.split_inclusive(|b| *b == b'\n').count() as u64;
            let mut lnums = vec![];
            let mut chunks = vec![];
            for (idx, line) in contents.split_inclusive(|b| *b == b'\n').enumerate() {
                let lnum = (idx + 1) as u64;
                let pat = "*match to this line*".as_bytes();
                if line.windows(pat.len()).any(|s| s == pat) {
                    lnums.push(lnum);
                    chunks.push((lnum.saturating_sub(6), cmp::min(lnum + 6, lines)));
                }
            }
            File::new(path, lnums, chunks, contents)
        }

        fn run_uitest(file: File, expected_file: PathBuf, f: fn(&mut PrinterOptions<'_>) -> ()) {
            let stdout = DummyStdout(RefCell::new(vec![]));
            let mut opts = PrinterOptions::default();
            opts.term_width = 80;
            opts.color_support = TermColorSupport::True;
            f(&mut opts);
            let mut printer = SyntectPrinter::new(stdout, opts).unwrap();
            printer.print(file).unwrap();
            let printed = mem::take(printer.writer_mut()).0.into_inner();
            let expected = fs::read(expected_file).unwrap();
            assert_eq!(
                printed,
                expected,
                "got:\n{}\nwant:\n{}",
                String::from_utf8_lossy(&printed),
                String::from_utf8_lossy(&expected),
            );
        }

        fn run_parametrized_uitest_single_chunk(
            mut input: &str,
            f: fn(&mut PrinterOptions<'_>) -> (),
        ) {
            let dir = Path::new(".").join("testdata").join("syntect");
            if input.starts_with("test_") {
                input = &input["test_".len()..];
            }
            let infile = dir.join(format!("{}.rs", input));
            let outfile = dir.join(format!("{}.out", input));
            let file = read_chunks(infile);
            run_uitest(file, outfile, f);
        }

        macro_rules! uitest {
            ($($input:ident($f:expr),)+) => {
                $(
                    #[cfg(not(windows))]
                    #[test]
                    fn $input() {
                        run_parametrized_uitest_single_chunk(stringify!($input), $f);
                    }
                )+
            }
        }

        uitest!(
            test_default(|_| {}),
            test_background(|o| {
                o.background_color = true;
            }),
            test_no_grid(|o| {
                o.grid = false;
            }),
            test_theme(|o| {
                o.theme = Some("Nord");
            }),
            test_tab_width_2(|o| {
                o.tab_width = 2;
            }),
            test_hard_tab(|o| {
                o.tab_width = 0;
            }),
            test_ansi256_colors(|o| {
                o.color_support = TermColorSupport::Ansi256;
            }),
            test_ansi16_colors(|o| {
                o.color_support = TermColorSupport::Ansi16;
            }),
            test_long_line(|_| {}),
            test_long_line_bg(|o| {
                o.background_color = true;
            }),
            test_empty_lines(|_| {}),
            test_wrap_between_text(|_| {}),
            test_wrap_middle_of_text(|_| {}),
            test_wrap_middle_of_spaces(|_| {}),
            test_wrap_middle_of_tab(|_| {}),
            test_wrap_twice(|_| {}),
            test_wrap_no_grid(|o| {
                o.grid = false;
            }),
            test_wrap_theme(|o| {
                o.theme = Some("Nord");
            }),
            test_wrap_ansi256(|o| {
                o.color_support = TermColorSupport::Ansi256;
            }),
            test_wrap_middle_text_bg(|o| {
                o.background_color = true;
            }),
            test_wrap_between_bg(|o| {
                o.background_color = true;
            }),
        );
    }

    #[derive(Debug)]
    struct DummyError;
    impl std::error::Error for DummyError {}
    impl fmt::Display for DummyError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "dummy error!")
        }
    }

    struct ErrorStdoutLock;
    impl Write for ErrorStdoutLock {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::Other, DummyError))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct ErrorStdout;
    impl<'a> LockableWrite<'a> for ErrorStdout {
        type Locked = ErrorStdoutLock;
        fn lock(&'a self) -> Self::Locked {
            ErrorStdoutLock
        }
    }

    fn sample_chunk(file: &str) -> File {
        let readme = PathBuf::from(file);
        let lnums = vec![3];
        let chunks = vec![(1, 6)];
        let contents = fs::read(&readme).unwrap();
        File::new(readme, lnums, chunks, contents)
    }

    #[test]
    fn test_error_write() {
        let file = sample_chunk("README.md");
        let opts = PrinterOptions::default();
        let printer = SyntectPrinter::new(ErrorStdout, opts).unwrap();
        let err = printer.print(file).unwrap_err();
        assert_eq!(&format!("{}", err), "dummy error!", "message={}", err);
    }

    #[test]
    fn test_unknown_theme() {
        let mut opts = PrinterOptions::default();
        opts.theme = Some("this theme does not exist");
        let err = match SyntectPrinter::with_stdout(opts) {
            Err(e) => e,
            Ok(_) => panic!("error did not occur"),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("Unknown theme"), "message={:?}", msg);
    }

    #[test]
    fn test_list_themes() {
        let mut buf = vec![];
        list_themes(&mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();

        // From bat's assets
        assert!(out.contains("Monokai Extended\n"), "output={:?}", out);

        // From default assets
        assert!(out.contains("base16-ocean.dark\n"), "output={:?}", out);
    }

    #[test]
    fn test_print_nothing() {
        let file = File::new(PathBuf::from("x.txt"), vec![], vec![], vec![]);
        let opts = PrinterOptions::default();
        let stdout = DummyStdout(RefCell::new(vec![]));
        let mut printer = SyntectPrinter::new(stdout, opts).unwrap();
        printer.print(file).unwrap();
        let printed = mem::take(printer.writer_mut()).0.into_inner();
        assert!(
            printed.is_empty(),
            "pritned:\n{}",
            String::from_utf8_lossy(&printed)
        );
    }

    #[test]
    fn test_no_syntax_found() {
        let file = sample_chunk("LICENSE.txt");
        let opts = PrinterOptions::default();
        let stdout = DummyStdout(RefCell::new(vec![]));
        let mut printer = SyntectPrinter::new(stdout, opts).unwrap();
        printer.print(file).unwrap();
        let printed = mem::take(printer.writer_mut()).0.into_inner();
        assert!(!printed.is_empty());
    }
}
