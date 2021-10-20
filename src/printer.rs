use crate::chunk::File;
use anyhow::Result;

pub struct PrinterOptions<'main> {
    pub tab_width: usize,
    pub theme: Option<&'main str>,
    pub grid: bool,
    pub background_color: bool,
}

impl<'main> Default for PrinterOptions<'main> {
    fn default() -> Self {
        Self {
            tab_width: 4,
            theme: None,
            grid: true,
            background_color: false,
        }
    }
}

// Trait to replace printer implementation for unit tests
pub trait Printer {
    fn print(&self, file: File) -> Result<()>;
}
