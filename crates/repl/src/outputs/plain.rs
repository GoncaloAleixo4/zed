//! # Plain Text Output
//!
//! This module provides functionality for rendering plain text output in a terminal-like format.
//! It uses the Alacritty terminal emulator backend to process and display text, supporting
//! ANSI escape sequences for formatting, colors, and other terminal features.
//!
//! The main component of this module is the `TerminalOutput` struct, which handles the parsing
//! and rendering of text input, simulating a basic terminal environment within REPL output.
//!
//! This module is used for displaying:
//!
//! - Standard output (stdout)
//! - Standard error (stderr)
//! - Plain text content
//! - Error tracebacks
//!

use alacritty_terminal::{
    event::VoidListener,
    grid::Dimensions as _,
    index::{Column, Line, Point},
    term::Config,
    vte::ansi::Processor,
};
use gpui::{Bounds, ClipboardItem, Entity, FontStyle, Pixels, TextStyle, WhiteSpace, canvas, size};
use language::Buffer;
use settings::Settings as _;
use terminal::terminal_settings::TerminalSettings;
use terminal_view::terminal_element::TerminalElement;
use theme_settings::ThemeSettings;
use ui::{IntoElement, prelude::*};

use crate::outputs::OutputContent;
use crate::repl_settings::ReplSettings;

/// The `TerminalOutput` struct handles the parsing and rendering of text input,
/// simulating a basic terminal environment within REPL output.
///
/// `TerminalOutput` is designed to handle various types of text-based output, including:
///
/// * stdout (standard output)
/// * stderr (standard error)
/// * text/plain content
/// * error tracebacks
///
/// It uses the Alacritty terminal emulator backend to process and render text,
/// supporting ANSI escape sequences for text formatting and colors.
///
pub struct TerminalOutput {
    full_buffer: Option<Entity<Buffer>>,
    /// ANSI escape sequence processor for parsing input text.
    parser: Processor,
    /// Alacritty terminal instance that manages the terminal state and content.
    handler: alacritty_terminal::Term<VoidListener>,
}

/// Hard upper bound on the simulated terminal grid column count. Beyond this
/// the grid stops growing and alacritty's wrapping behavior takes over again.
/// Protects against pathological output (e.g., a kernel emitting one
/// multi-megabyte line) consuming unbounded memory.
const MAX_GRID_COLUMNS: usize = 1024;

/// Hard upper bound on the simulated terminal grid line count. Same rationale
/// as `MAX_GRID_COLUMNS` but for vertical growth.
const MAX_GRID_LINES: usize = 4096;

/// Returns the default text style for the terminal output.
pub fn text_style(window: &mut Window, cx: &App) -> TextStyle {
    let settings = ThemeSettings::get_global(cx).clone();

    let font_size = settings.buffer_font_size(cx).into();
    let font_family = settings.buffer_font.family;
    let font_features = settings.buffer_font.features;
    let font_weight = settings.buffer_font.weight;
    let font_fallbacks = settings.buffer_font.fallbacks;

    let theme = cx.theme();

    TextStyle {
        font_family,
        font_features,
        font_weight,
        font_fallbacks,
        font_size,
        font_style: FontStyle::Normal,
        line_height: window.line_height().into(),
        background_color: Some(theme.colors().terminal_ansi_background),
        white_space: WhiteSpace::Normal,
        // These are going to be overridden per-cell
        color: theme.colors().terminal_foreground,
        ..Default::default()
    }
}

/// Returns the default terminal size for the terminal output.
pub fn terminal_size(window: &mut Window, cx: &mut App) -> terminal::TerminalBounds {
    let text_style = text_style(window, cx);
    let text_system = window.text_system();

    let line_height = window.line_height();

    let font_pixels = text_style.font_size.to_pixels(window.rem_size());
    let font_id = text_system.resolve_font(&text_style.font());

    let cell_width = text_system
        .advance(font_id, font_pixels, 'w')
        .map(|advance| advance.width)
        .unwrap_or(Pixels::ZERO);

    let settings = ReplSettings::get_global(cx);
    let num_lines = settings.max_lines;
    // Size the simulated terminal wide enough that wide kernel output (e.g.,
    // polars/pandas DataFrames at `set_tbl_width_chars(2000)`) does not get
    // wrapped by alacritty before we can render it. Mid-stream column reflows
    // are visually corrupt for tables with box-drawing characters, so we pre-
    // allocate up to the user/safety cap instead of growing later.
    let user_cap = match settings.output_max_width_columns {
        0 => MAX_GRID_COLUMNS,
        n => n.min(MAX_GRID_COLUMNS),
    };
    let columns = settings.max_columns.max(user_cap);

    // Reversed math from terminal::TerminalSize to get pixel width according to terminal width
    let width = columns as f32 * cell_width;
    let height = num_lines as f32 * window.line_height();

    terminal::TerminalBounds {
        cell_width,
        line_height,
        bounds: Bounds {
            origin: gpui::Point::default(),
            size: size(width, height),
        },
    }
}

pub fn max_width_for_columns(
    columns: usize,
    window: &mut Window,
    cx: &App,
) -> Option<gpui::Pixels> {
    if columns == 0 {
        return None;
    }

    let text_style = text_style(window, cx);
    let text_system = window.text_system();
    let font_pixels = text_style.font_size.to_pixels(window.rem_size());
    let font_id = text_system.resolve_font(&text_style.font());
    let cell_width = text_system
        .advance(font_id, font_pixels, 'w')
        .map(|advance| advance.width)
        .unwrap_or(Pixels::ZERO);

    Some(cell_width * columns as f32)
}

impl TerminalOutput {
    /// Creates a new `TerminalOutput` instance.
    ///
    /// This method initializes a new terminal emulator with default configuration
    /// and sets up the necessary components for handling terminal events and rendering.
    ///
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        let term = alacritty_terminal::Term::new(
            Config::default(),
            &terminal_size(window, cx),
            VoidListener,
        );

        Self {
            parser: Processor::new(),
            handler: term,
            full_buffer: None,
        }
    }

    /// Creates a new `TerminalOutput` instance with initial content.
    ///
    /// Initializes a new terminal output and populates it with the provided text.
    ///
    /// # Arguments
    ///
    /// * `text` - A string slice containing the initial text for the terminal output.
    /// * `cx` - A mutable reference to the `WindowContext` for initialization.
    ///
    /// # Returns
    ///
    /// A new instance of `TerminalOutput` containing the provided text.
    pub fn from(text: &str, window: &mut Window, cx: &mut App) -> Self {
        let mut output = Self::new(window, cx);
        output.append_text(text, cx);
        output
    }

    /// Appends text to the terminal output.
    ///
    /// Processes each byte of the input text, handling newline characters specially
    /// to ensure proper cursor movement. Uses the ANSI parser to process the input
    /// and update the terminal state.
    ///
    /// As an example, if the user runs the following Python code in this REPL:
    ///
    /// ```python
    /// import time
    /// print("Hello,", end="")
    /// time.sleep(1)
    /// print(" world!")
    /// ```
    ///
    /// Then append_text will be called twice, with the following arguments:
    ///
    /// ```ignore
    /// terminal_output.append_text("Hello,");
    /// terminal_output.append_text(" world!");
    /// ```
    /// Resulting in a single output of "Hello, world!".
    ///
    /// # Arguments
    ///
    /// * `text` - A string slice containing the text to be appended.
    pub fn append_text(&mut self, text: &str, cx: &mut App) {
        for byte in text.as_bytes() {
            if *byte == b'\n' {
                // Dirty (?) hack to move the cursor down
                self.parser.advance(&mut self.handler, &[b'\r']);
                self.parser.advance(&mut self.handler, &[b'\n']);
            } else {
                self.parser.advance(&mut self.handler, &[*byte]);
            }
        }

        // This will keep the buffer up to date, though with some terminal codes it won't be perfect
        if let Some(buffer) = self.full_buffer.as_ref() {
            buffer.update(cx, |buffer, cx| {
                buffer.edit([(buffer.len()..buffer.len(), text)], None, cx);
            });
        }
    }

    /// Scan the alacritty grid (visible + scrollback) and return
    /// `(longest_non_empty_line_in_cells, number_of_non_empty_lines)`.
    ///
    /// Used to size the canvas to the actual painted content and to decide
    /// when the simulated grid needs to grow.
    fn measure_content(&self) -> (usize, usize) {
        let total_lines = self.handler.grid().total_lines();
        let visible_lines = self.handler.screen_lines();
        let history_lines = total_lines.saturating_sub(visible_lines);
        let cols = self.handler.columns();
        if cols == 0 {
            return (0, 0);
        }

        let mut max_cols = 0usize;
        let mut line_count = 0usize;

        let mut measure = |line_index: Line| {
            let start = Point::new(line_index, Column(0));
            let end = Point::new(line_index, Column(cols - 1));
            let text = self.handler.bounds_to_string(start, end);
            let trimmed = text.trim_end_matches(|ch: char| ch == ' ' || ch == '\t' || ch == '\0');
            if !trimmed.is_empty() {
                let len = trimmed.chars().count();
                if len > max_cols {
                    max_cols = len;
                }
                line_count += 1;
            }
        };

        for line in (0..history_lines).rev() {
            measure(Line(-(line as i32) - 1));
        }
        for line in 0..visible_lines {
            measure(Line(line as i32));
        }

        (max_cols, line_count)
    }

    /// Grow the simulated terminal grid vertically so scrollback rows become
    /// part of the visible grid (and thus get rendered).
    ///
    /// We deliberately only grow the line count, not the column count: a
    /// mid-stream column resize triggers alacritty's reflow path which
    /// visually corrupts content that was wrapped at the old width (notably
    /// tables drawn with box-drawing characters). The column dimension is
    /// instead pre-allocated up to the user / safety cap in `terminal_size`.
    ///
    /// Resize is bounded by `MAX_GRID_LINES` to keep runaway output from
    /// consuming unbounded memory.
    fn ensure_grid_fits(&mut self, cell_width: Pixels, line_height: Pixels, cx: &mut App) {
        let (_content_cols, content_lines) = self.measure_content();
        let settings = ReplSettings::get_global(cx);

        let target_lines = content_lines
            .max(settings.max_lines)
            .clamp(1, MAX_GRID_LINES);

        let current_cols = self.handler.columns();
        let current_lines = self.handler.screen_lines();
        if target_lines == current_lines {
            return;
        }
        if cell_width <= Pixels::ZERO || line_height <= Pixels::ZERO {
            return;
        }

        let new_bounds = terminal::TerminalBounds {
            cell_width,
            line_height,
            bounds: Bounds {
                origin: gpui::Point::default(),
                size: size(
                    cell_width * current_cols as f32,
                    line_height * target_lines as f32,
                ),
            },
        };
        self.handler.resize(new_bounds);
    }

    pub fn full_text(&self) -> String {
        fn sanitize(mut line: String) -> Option<String> {
            line.retain(|ch| ch != '\u{0}' && ch != '\r');
            if line.trim().is_empty() {
                return None;
            }
            let trimmed = line.trim_end_matches([' ', '\t']);
            Some(trimmed.to_owned())
        }

        let mut lines = Vec::new();

        // Get the total number of lines, including history
        let total_lines = self.handler.grid().total_lines();
        let visible_lines = self.handler.screen_lines();
        let history_lines = total_lines - visible_lines;

        // Capture history lines in correct order (oldest to newest)
        for line in (0..history_lines).rev() {
            let line_index = Line(-(line as i32) - 1);
            let start = Point::new(line_index, Column(0));
            let end = Point::new(line_index, Column(self.handler.columns() - 1));
            if let Some(cleaned) = sanitize(self.handler.bounds_to_string(start, end)) {
                lines.push(cleaned);
            }
        }

        // Capture visible lines
        for line in 0..visible_lines {
            let line_index = Line(line as i32);
            let start = Point::new(line_index, Column(0));
            let end = Point::new(line_index, Column(self.handler.columns() - 1));
            if let Some(cleaned) = sanitize(self.handler.bounds_to_string(start, end)) {
                lines.push(cleaned);
            }
        }

        if lines.is_empty() {
            String::new()
        } else {
            let mut full_text = lines.join("\n");
            full_text.push('\n');
            full_text
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use settings::SettingsStore;

    fn init_test(cx: &mut TestAppContext) -> &mut VisualTestContext {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        cx.add_empty_window()
    }

    #[gpui::test]
    fn test_max_width_for_columns_zero(cx: &mut TestAppContext) {
        let cx = init_test(cx);
        let result = cx.update(|window, cx| max_width_for_columns(0, window, cx));
        assert!(result.is_none());
    }

    #[gpui::test]
    fn test_max_width_for_columns_matches_cell_width(cx: &mut TestAppContext) {
        let cx = init_test(cx);
        let columns = 5;
        let (result, expected) = cx.update(|window, cx| {
            let text_style = text_style(window, cx);
            let text_system = window.text_system();
            let font_pixels = text_style.font_size.to_pixels(window.rem_size());
            let font_id = text_system.resolve_font(&text_style.font());
            let cell_width = text_system
                .advance(font_id, font_pixels, 'w')
                .map(|advance| advance.width)
                .unwrap_or(gpui::Pixels::ZERO);
            let result = max_width_for_columns(columns, window, cx);
            (result, cell_width * columns as f32)
        });

        let Some(result) = result else {
            panic!("expected max width for columns {columns}");
        };
        let result_f32: f32 = result.into();
        let expected_f32: f32 = expected.into();
        assert!((result_f32 - expected_f32).abs() < 0.01);
    }

    #[gpui::test]
    fn test_measure_content_empty_output_reports_zero(cx: &mut TestAppContext) {
        let cx = init_test(cx);
        let (max_cols, line_count) = cx.update(|window, cx| {
            let output = TerminalOutput::new(window, cx);
            output.measure_content()
        });
        assert_eq!(max_cols, 0);
        assert_eq!(line_count, 0);
    }

    #[gpui::test]
    fn test_measure_content_single_line_reports_visible_length(cx: &mut TestAppContext) {
        let cx = init_test(cx);
        let (max_cols, line_count) = cx.update(|window, cx| {
            let mut output = TerminalOutput::new(window, cx);
            output.append_text("hello", cx);
            output.measure_content()
        });
        assert_eq!(max_cols, "hello".len());
        assert_eq!(line_count, 1);
    }

    #[gpui::test]
    fn test_measure_content_multi_line_takes_max_width(cx: &mut TestAppContext) {
        let cx = init_test(cx);
        let (max_cols, line_count) = cx.update(|window, cx| {
            let mut output = TerminalOutput::new(window, cx);
            // Three lines of lengths 3, 5, 2.
            output.append_text("aaa\nbbbbb\ncc", cx);
            output.measure_content()
        });
        assert_eq!(max_cols, 5);
        assert_eq!(line_count, 3);
    }

    #[gpui::test]
    fn test_ensure_grid_fits_grows_for_taller_content(cx: &mut TestAppContext) {
        let cx = init_test(cx);
        let starting_lines = cx.update(|window, cx| {
            use alacritty_terminal::grid::Dimensions as _;
            let output = TerminalOutput::new(window, cx);
            output.handler.screen_lines()
        });
        let after_lines = cx.update(|window, cx| {
            use alacritty_terminal::grid::Dimensions as _;
            let mut output = TerminalOutput::new(window, cx);
            // Append more lines than the default `max_lines` (32) so the grid
            // has to grow vertically to avoid scrollback truncation.
            let many_lines = "x\n".repeat(60);
            output.append_text(&many_lines, cx);
            let text_style_inner = text_style(window, cx);
            let line_height = text_style_inner.line_height_in_pixels(window.rem_size());
            let text_system = window.text_system();
            let font_pixels = text_style_inner.font_size.to_pixels(window.rem_size());
            let font_id = text_system.resolve_font(&text_style_inner.font());
            let cell_width = text_system
                .advance(font_id, font_pixels, 'w')
                .map(|advance| advance.width)
                .unwrap_or(gpui::Pixels::ZERO);
            output.ensure_grid_fits(cell_width, line_height, cx);
            output.handler.screen_lines()
        });
        assert!(
            after_lines > starting_lines,
            "expected grid to grow vertically (was {starting_lines}, now {after_lines})"
        );
    }

    #[gpui::test]
    fn test_ensure_grid_fits_respects_safety_cap(cx: &mut TestAppContext) {
        let cx = init_test(cx);
        let columns_after = cx.update(|window, cx| {
            use alacritty_terminal::grid::Dimensions as _;
            let mut output = TerminalOutput::new(window, cx);
            let text_style_inner = text_style(window, cx);
            let line_height = text_style_inner.line_height_in_pixels(window.rem_size());
            let text_system = window.text_system();
            let font_pixels = text_style_inner.font_size.to_pixels(window.rem_size());
            let font_id = text_system.resolve_font(&text_style_inner.font());
            let cell_width = text_system
                .advance(font_id, font_pixels, 'w')
                .map(|advance| advance.width)
                .unwrap_or(gpui::Pixels::ZERO);
            // Try to push the grid well past the safety cap; the resize must
            // refuse to allocate more than MAX_GRID_COLUMNS columns.
            for _ in 0..5 {
                output.append_text(&"a".repeat(MAX_GRID_COLUMNS * 4), cx);
                output.ensure_grid_fits(cell_width, line_height, cx);
            }
            output.handler.columns()
        });
        assert!(
            columns_after <= MAX_GRID_COLUMNS,
            "grid grew past safety cap: {columns_after} > {MAX_GRID_COLUMNS}"
        );
    }
}

impl Render for TerminalOutput {
    /// Renders the terminal output as a GPUI element.
    ///
    /// Converts the current terminal state into a renderable GPUI element. It handles
    /// the layout of the terminal grid, calculates the dimensions of the output, and
    /// creates a canvas element that paints the terminal cells and background rectangles.
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text_style = text_style(window, cx);
        let text_system = window.text_system();
        let text_line_height = text_style.line_height_in_pixels(window.rem_size());
        let font_pixels = text_style.font_size.to_pixels(window.rem_size());
        let font_id = text_system.resolve_font(&text_style.font());
        let cell_width = text_system
            .advance(font_id, font_pixels, 'w')
            .map(|advance| advance.width)
            .unwrap_or(Pixels::ZERO);

        // Grow the simulated terminal so wrapped lines reflow back into single
        // lines and scrollback rows become part of the visible grid (and thus
        // get rendered). Bounded by `MAX_GRID_*` to prevent runaway growth.
        self.ensure_grid_fits(cell_width, text_line_height, cx);

        let grid = self
            .handler
            .renderable_content()
            .display_iter
            .map(|ic| terminal::IndexedCell {
                point: ic.point,
                cell: ic.cell.clone(),
            });
        let minimum_contrast = TerminalSettings::get_global(cx).minimum_contrast;
        let (rects, batched_text_runs) =
            TerminalElement::layout_grid(grid, 0, &text_style, None, minimum_contrast, cx);

        // Size the canvas to the actual painted content. Setting width
        // explicitly is what lets the parent `overflow_x_scroll` engage when
        // the content is wider than the visible output box.
        let num_lines = batched_text_runs
            .iter()
            .map(|b| b.start_point.line)
            .max()
            .unwrap_or(0)
            + 1;
        let height = num_lines as f32 * text_line_height;
        let num_cols = batched_text_runs
            .iter()
            .map(|b| b.start_point.column as usize + b.cell_count)
            .max()
            .unwrap_or(0);
        let width = num_cols as f32 * cell_width;

        canvas(
            // prepaint
            move |_bounds, _, _| {},
            // paint
            move |bounds, _, window, cx| {
                for rect in rects {
                    rect.paint(
                        bounds.origin,
                        &terminal::TerminalBounds {
                            cell_width,
                            line_height: text_line_height,
                            bounds,
                        },
                        window,
                    );
                }

                for batch in batched_text_runs {
                    batch.paint(
                        bounds.origin,
                        &terminal::TerminalBounds {
                            cell_width,
                            line_height: text_line_height,
                            bounds,
                        },
                        window,
                        cx,
                    );
                }
            },
        )
        // We must set both dimensions explicitly so the editor block sizes
        // itself correctly and horizontal overflow engages the parent scroll.
        .w(width)
        .h(height)
    }
}

impl OutputContent for TerminalOutput {
    fn clipboard_content(&self, _window: &Window, _cx: &App) -> Option<ClipboardItem> {
        Some(ClipboardItem::new_string(self.full_text()))
    }

    fn has_clipboard_content(&self, _window: &Window, _cx: &App) -> bool {
        true
    }

    fn has_buffer_content(&self, _window: &Window, _cx: &App) -> bool {
        true
    }

    fn buffer_content(&mut self, _: &mut Window, cx: &mut App) -> Option<Entity<Buffer>> {
        if self.full_buffer.as_ref().is_some() {
            return self.full_buffer.clone();
        }

        let buffer = cx.new(|cx| {
            let mut buffer =
                Buffer::local(self.full_text(), cx).with_language(language::PLAIN_TEXT.clone(), cx);
            buffer.set_capability(language::Capability::ReadOnly, cx);
            buffer
        });

        self.full_buffer = Some(buffer.clone());
        Some(buffer)
    }
}
