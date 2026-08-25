//! A QR code as a grid of squares, ready to be painted with terminal cells.
//!
//! Kept apart from the drawing so the awkward part — whether a code fits in the
//! room there is, and what is dark where — can be checked without a terminal.

use anyhow::{Context, Result};

/// The blank margin every QR code needs around it. Four modules is what the
/// specification asks for, and a scanner really does refuse a code that is
/// crowded up against the text beside it.
pub const QUIET: usize = 4;

/// One encoded code, quiet zone not included in `width`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Code {
    width: usize,
    dark: Vec<bool>,
}

/// Encode a link, at the lowest error correction the format allows.
///
/// Low correction on purpose: the code is on a screen a foot from the camera,
/// not printed on a box, and every level up makes it wider — which on a
/// terminal is the difference between fitting and not.
pub fn encode(text: &str) -> Result<Code> {
    let code = qrcode::QrCode::with_error_correction_level(text, qrcode::EcLevel::L)
        .context("that link will not fit in a QR code")?;
    Ok(Code {
        width: code.width(),
        dark: code
            .to_colors()
            .into_iter()
            .map(|color| color == qrcode::Color::Dark)
            .collect(),
    })
}

impl Code {
    /// Terminal columns this needs, quiet zone counted in. One module to a
    /// column: a cell is about twice as tall as it is wide, so a module drawn
    /// one column across and half a row down comes out square, which is what a
    /// scanner is looking for.
    pub fn columns(&self) -> usize {
        self.width + QUIET * 2
    }

    /// Terminal rows this needs — half as many as columns, two module rows to
    /// each row of half blocks.
    pub fn rows(&self) -> usize {
        self.columns().div_ceil(2)
    }

    /// Whether the module at these coordinates is dark, counting from the
    /// outside of the quiet zone. Anything past the edge is light, so the
    /// caller can walk a rectangle without minding where the code stops.
    pub fn dark(&self, x: usize, y: usize) -> bool {
        let (Some(x), Some(y)) = (x.checked_sub(QUIET), y.checked_sub(QUIET)) else {
            return false;
        };
        if x >= self.width || y >= self.width {
            return false;
        }
        self.dark[y * self.width + x]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_is_square_and_fenced_by_light() {
        let code = encode("https://example.com/muxloom").expect("encodable");
        assert!(code.columns() >= 21 + QUIET * 2);
        assert_eq!(code.rows(), code.columns().div_ceil(2));
        // The whole quiet zone, on every side.
        for step in 0..code.columns() {
            for edge in 0..QUIET {
                assert!(!code.dark(step, edge), "top row {edge} is not clear");
                assert!(!code.dark(edge, step), "left column {edge} is not clear");
                assert!(!code.dark(step, code.columns() - 1 - edge));
                assert!(!code.dark(code.columns() - 1 - edge, step));
            }
        }
        // Reading past the end is a light square rather than a panic: the
        // drawing walks whole terminal rows, and the last one runs over.
        assert!(!code.dark(code.columns() + 99, code.columns() + 99));
    }

    #[test]
    fn the_finder_square_is_where_a_scanner_looks_for_it() {
        let code = encode("muxloom").expect("encodable");
        // Every QR code opens with a 7x7 finder: a filled ring with a filled
        // core. If this is wrong, nothing else about the grid is right either.
        for step in 0..7 {
            assert!(code.dark(QUIET + step, QUIET), "top edge at {step}");
            assert!(code.dark(QUIET, QUIET + step), "left edge at {step}");
        }
        assert!(!code.dark(QUIET + 1, QUIET + 1));
        assert!(code.dark(QUIET + 3, QUIET + 3));
    }

    #[test]
    fn a_link_too_long_to_encode_is_refused_rather_than_drawn_wrong() {
        let far_too_much = "x".repeat(8000);
        assert!(encode(&far_too_much).is_err());
    }
}
