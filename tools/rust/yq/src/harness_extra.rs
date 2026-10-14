//! Format scenario set-ups for the formats implemented in their own modules.

use crate::formats::{Decoder as Dec, Encoder as Enc};

pub fn format_setup(_var: &str, _typ: &str, _indent: i64, _p: &mut crate::prefs::Prefs) -> Option<(Box<dyn Dec>, Box<dyn Enc>, bool)> {
    None
}
