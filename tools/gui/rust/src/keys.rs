//! Key names to what a program receives: an X11 keysym (what the key means), a Linux evdev key
//! code (where it is, on a US layout; X11's keycode is this + 8) and the text it types.

/// One key press with the modifiers held around it.
#[derive(Debug, PartialEq)]
pub struct Stroke {
    pub mods: Vec<(u32, u32)>,
    pub keysym: u32,
    pub keycode: u32,
    pub text: String,
}

pub const SHIFT_L: (u32, u32) = (0xffe1, 42);
const CONTROL_L: (u32, u32) = (0xffe3, 29);
const ALT_L: (u32, u32) = (0xffe9, 56);
const SUPER_L: (u32, u32) = (0xffeb, 125);

const NAMED: &[(&str, u32, u32)] = &[
    ("Return", 0xff0d, 28),
    ("Enter", 0xff0d, 28),
    ("KP_Enter", 0xff8d, 96),
    ("Tab", 0xff09, 15),
    ("ISO_Left_Tab", 0xfe20, 15),
    ("Escape", 0xff1b, 1),
    ("Esc", 0xff1b, 1),
    ("BackSpace", 0xff08, 14),
    ("Delete", 0xffff, 111),
    ("Del", 0xffff, 111),
    ("Insert", 0xff63, 110),
    ("Home", 0xff50, 102),
    ("End", 0xff57, 107),
    ("Page_Up", 0xff55, 104),
    ("PageUp", 0xff55, 104),
    ("Prior", 0xff55, 104),
    ("Page_Down", 0xff56, 109),
    ("PageDown", 0xff56, 109),
    ("Next", 0xff56, 109),
    ("Left", 0xff51, 105),
    ("Up", 0xff52, 103),
    ("Right", 0xff53, 106),
    ("Down", 0xff54, 108),
    ("space", 0x20, 57),
    ("Space", 0x20, 57),
    // A modifier on its own (gui keydown TARGET shift).
    ("Shift", 0xffe1, 42),
    ("Ctrl", 0xffe3, 29),
    ("Control", 0xffe3, 29),
    ("Alt", 0xffe9, 56),
    ("Meta", 0xffe9, 56),
    ("Super", 0xffeb, 125),
    ("Win", 0xffeb, 125),
    ("Shift_L", 0xffe1, 42),
    ("Shift_R", 0xffe2, 54),
    ("Control_L", 0xffe3, 29),
    ("Control_R", 0xffe4, 97),
    ("Caps_Lock", 0xffe5, 58),
    ("Alt_L", 0xffe9, 56),
    ("Alt_R", 0xffea, 100),
    ("Super_L", 0xffeb, 125),
    ("Super_R", 0xffec, 126),
    ("Menu", 0xff67, 127),
    ("Print", 0xff61, 99),
    ("Pause", 0xff13, 119),
    ("Scroll_Lock", 0xff14, 70),
    ("Num_Lock", 0xff7f, 69),
];

const LETTER_CODES: [u32; 26] = [30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44];

/// Where a character is on a US keyboard: its key code and whether Shift is down.
fn us_key(c: char) -> Option<(u32, bool)> {
    Some(match c {
        'a'..='z' => (LETTER_CODES[c as usize - 'a' as usize], false),
        'A'..='Z' => (LETTER_CODES[c as usize - 'A' as usize], true),
        '1'..='9' => (c as u32 - '1' as u32 + 2, false),
        '0' => (11, false),
        ' ' => (57, false),
        '`' => (41, false),
        '-' => (12, false),
        '=' => (13, false),
        '[' => (26, false),
        ']' => (27, false),
        '\\' => (43, false),
        ';' => (39, false),
        '\'' => (40, false),
        ',' => (51, false),
        '.' => (52, false),
        '/' => (53, false),
        '~' => (41, true),
        '!' => (2, true),
        '@' => (3, true),
        '#' => (4, true),
        '$' => (5, true),
        '%' => (6, true),
        '^' => (7, true),
        '&' => (8, true),
        '*' => (9, true),
        '(' => (10, true),
        ')' => (11, true),
        '_' => (12, true),
        '+' => (13, true),
        '{' => (26, true),
        '}' => (27, true),
        '|' => (43, true),
        ':' => (39, true),
        '"' => (40, true),
        '<' => (51, true),
        '>' => (52, true),
        '?' => (53, true),
        _ => return None,
    })
}

/// The keysym of a character: Latin-1 as itself, the rest as X11's Unicode keysyms.
pub fn char_keysym(c: char) -> u32 {
    let cp = c as u32;
    if (0x20..0x7f).contains(&cp) || (0xa0..0x100).contains(&cp) {
        cp
    } else {
        0x0100_0000 | cp
    }
}

/// Typing one character: Shift where the US layout needs it; characters off the layout (Korean,
/// accents, emoji) come as their Unicode keysym with key code 0 and their text.
pub fn char_stroke(c: char) -> Stroke {
    match c {
        '\n' | '\r' => return Stroke { mods: vec![], keysym: 0xff0d, keycode: 28, text: String::new() },
        '\t' => return Stroke { mods: vec![], keysym: 0xff09, keycode: 15, text: String::new() },
        '\u{8}' => return Stroke { mods: vec![], keysym: 0xff08, keycode: 14, text: String::new() },
        _ => {}
    }
    match us_key(c) {
        Some((code, shift)) => Stroke { mods: if shift { vec![SHIFT_L] } else { vec![] }, keysym: char_keysym(c), keycode: code, text: c.to_string() },
        None => Stroke { mods: vec![], keysym: char_keysym(c), keycode: 0, text: c.to_string() },
    }
}

/// `ctrl+shift+t`, `Return`, `alt+F4`, `a`, `A`, `super+Left`, `0x1008ff13` (a raw keysym).
pub fn parse_combo(spec: &str) -> Result<Stroke, String> {
    let parts: Vec<&str> = if spec == "+" { vec!["+"] } else { spec.split('+').collect() };
    // "ctrl++" is ctrl and the plus key.
    let (mod_names, key) = if spec.ends_with("++") {
        (&parts[..parts.len() - 2], "+")
    } else {
        let (k, m) = parts.split_last().ok_or("empty key")?;
        (m, *k)
    };
    let mut mods = Vec::new();
    for m in mod_names {
        let m = match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "c" => CONTROL_L,
            "shift" | "s" => SHIFT_L,
            "alt" | "meta" | "option" | "opt" | "a" => ALT_L,
            "super" | "win" | "cmd" | "command" | "logo" => SUPER_L,
            "" => return Err(format!("bad key {spec:?}")),
            other => return Err(format!("unknown modifier {other:?} in {spec:?} (ctrl, shift, alt, super)")),
        };
        if !mods.contains(&m) {
            mods.push(m);
        }
    }
    if key.is_empty() {
        return Err(format!("bad key {spec:?}"));
    }
    let typing = mods.iter().all(|m| *m == SHIFT_L);
    let mut chars = key.chars();
    let (first, rest) = (chars.next().unwrap(), chars.next());
    if rest.is_none() {
        let mut s = char_stroke(first);
        // ctrl+A is ctrl+shift+a; the keysym stays what the key types with the modifiers held.
        if first.is_ascii_alphabetic() && mods.contains(&SHIFT_L) {
            let up = first.to_ascii_uppercase();
            s.keysym = up as u32;
            s.text = up.to_string();
        }
        for m in s.mods.drain(..) {
            if !mods.contains(&m) {
                mods.push(m);
            }
        }
        if !typing {
            s.text.clear();
        }
        s.mods = mods;
        return Ok(s);
    }
    let lower = key.to_ascii_lowercase();
    if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()).filter(|n| (1..=24).contains(n)) {
        let code = match n {
            1..=10 => 58 + n,
            11 => 87,
            12 => 88,
            _ => 183 + (n - 13),
        };
        return Ok(Stroke { mods, keysym: 0xffbe + n - 1, keycode: code, text: String::new() });
    }
    if let Some(hex) = lower.strip_prefix("0x") {
        let keysym = u32::from_str_radix(hex, 16).map_err(|_| format!("bad keysym {key}"))?;
        return Ok(Stroke { mods, keysym, keycode: 0, text: String::new() });
    }
    for (name, keysym, code) in NAMED {
        if name.eq_ignore_ascii_case(key) {
            let text = if *keysym == 0x20 && typing { " ".to_string() } else { String::new() };
            return Ok(Stroke { mods, keysym: *keysym, keycode: *code, text });
        }
    }
    Err(format!("unknown key {key:?} (a character, Return, Tab, Escape, BackSpace, Delete, Home, End, PageUp, PageDown, Left/Up/Right/Down, F1-F24, space, Insert, Menu, or 0xKEYSYM)"))
}

/// The modifier keys `ctrl+shift` (or `ctrl,shift`) names, in order: for holding them around
/// mouse input.
pub fn parse_mods(spec: &str) -> Result<Vec<(u32, u32)>, String> {
    let mut out = Vec::new();
    for m in spec.split(['+', ',']).filter(|m| !m.is_empty()) {
        let k = match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => CONTROL_L,
            "shift" => SHIFT_L,
            "alt" | "meta" | "option" | "opt" => ALT_L,
            "super" | "win" | "cmd" | "command" | "logo" => SUPER_L,
            other => return Err(format!("unknown modifier {other:?} (ctrl, shift, alt, super)")),
        };
        if !out.contains(&k) {
            out.push(k);
        }
    }
    Ok(out)
}

/// Keysyms of the modifier keys a state mask says are down, for releasing them.
pub fn mods_keys(mask: u32) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for (bit, key) in [(4, CONTROL_L), (1, SHIFT_L), (8, ALT_L), (0x40, SUPER_L)] {
        if mask & bit != 0 {
            out.push(key);
        }
    }
    out
}

// ── Hangul, as an input method composes it ──────────────────────────────────────────────────

/// Compatibility jamo of the initial consonants, in syllable order.
const CHO: [char; 19] = ['ㄱ', 'ㄲ', 'ㄴ', 'ㄷ', 'ㄸ', 'ㄹ', 'ㅁ', 'ㅂ', 'ㅃ', 'ㅅ', 'ㅆ', 'ㅇ', 'ㅈ', 'ㅉ', 'ㅊ', 'ㅋ', 'ㅌ', 'ㅍ', 'ㅎ'];

/// For a compound vowel, the vowel typed first (ㅘ is ㅗ then ㅏ).
fn vowel_first(v: u32) -> Option<u32> {
    match v {
        9..=11 => Some(8),   // ㅘ ㅙ ㅚ: ㅗ
        14..=16 => Some(13), // ㅝ ㅞ ㅟ: ㅜ
        19 => Some(18),      // ㅢ: ㅡ
        _ => None,
    }
}

/// For a compound final consonant, the one typed first (ㄺ is ㄹ then ㄱ).
fn final_first(t: u32) -> Option<u32> {
    match t {
        3 => Some(1),       // ㄳ: ㄱ
        5 | 6 => Some(4),   // ㄵ ㄶ: ㄴ
        9..=15 => Some(8),  // ㄺ ㄻ ㄼ ㄽ ㄾ ㄿ ㅀ: ㄹ
        18 => Some(17),     // ㅄ: ㅂ
        _ => None,
    }
}

/// What a Korean input method shows while the syllable is typed (its preedit after each jamo),
/// ending with the syllable itself: 한 is ㅎ, 하, 한; 괜 is ㄱ, 고, 괘, 괜. None for a
/// character that is not a precomposed Hangul syllable.
pub fn hangul_steps(c: char) -> Option<Vec<char>> {
    let cp = c as u32;
    if !(0xac00..=0xd7a3).contains(&cp) {
        return None;
    }
    let idx = cp - 0xac00;
    let (l, v, t) = (idx / 588, (idx % 588) / 28, idx % 28);
    let syl = |v: u32, t: u32| char::from_u32(0xac00 + l * 588 + v * 28 + t).unwrap();
    let mut steps = vec![CHO[l as usize]];
    if let Some(v1) = vowel_first(v) {
        steps.push(syl(v1, 0));
    }
    steps.push(syl(v, 0));
    if t > 0 {
        if let Some(t1) = final_first(t) {
            steps.push(syl(v, t1));
        }
        steps.push(syl(v, t));
    }
    Some(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combos() {
        let s = parse_combo("ctrl+c").unwrap();
        assert_eq!((s.mods, s.keysym, s.keycode, s.text.as_str()), (vec![CONTROL_L], 'c' as u32, 46, ""));
        let s = parse_combo("A").unwrap();
        assert_eq!((s.mods, s.keysym, s.keycode, s.text.as_str()), (vec![SHIFT_L], 'A' as u32, 30, "A"));
        let s = parse_combo("ctrl+shift+t").unwrap();
        assert_eq!((s.mods, s.keysym, s.text.as_str()), (vec![CONTROL_L, SHIFT_L], 'T' as u32, ""));
        let s = parse_combo("return").unwrap();
        assert_eq!((s.keysym, s.keycode), (0xff0d, 28));
        let s = parse_combo("alt+F4").unwrap();
        assert_eq!((s.mods, s.keysym, s.keycode), (vec![ALT_L], 0xffc1, 62));
        let s = parse_combo("ctrl++").unwrap();
        assert_eq!((s.mods, s.keysym), (vec![CONTROL_L, SHIFT_L], '+' as u32));
        let s = parse_combo("!").unwrap();
        assert_eq!((s.mods, s.keycode, s.text.as_str()), (vec![SHIFT_L], 2, "!"));
        assert_eq!(parse_combo("F12").unwrap().keycode, 88);
        assert!(parse_combo("hyper+x").is_err());
        assert!(parse_combo("Nope").is_err());
    }

    #[test]
    fn hangul() {
        assert_eq!(hangul_steps('한').unwrap(), vec!['ㅎ', '하', '한']);
        assert_eq!(hangul_steps('괜').unwrap(), vec!['ㄱ', '고', '괘', '괜']); // ㅙ is ㅗ then ㅐ
        assert_eq!(hangul_steps('왔').unwrap(), vec!['ㅇ', '오', '와', '왔']); // ㅆ is one key
        assert_eq!(hangul_steps('읽').unwrap(), vec!['ㅇ', '이', '일', '읽']);
        assert_eq!(hangul_steps('가').unwrap(), vec!['ㄱ', '가']);
        assert_eq!(hangul_steps('a'), None);
        assert_eq!(hangul_steps('ㅋ'), None);
    }

    #[test]
    fn modifiers() {
        assert_eq!(parse_mods("ctrl+shift").unwrap(), vec![CONTROL_L, SHIFT_L]);
        assert_eq!(parse_mods("alt,ctrl,alt").unwrap(), vec![ALT_L, CONTROL_L]);
        assert!(parse_mods("hyper").is_err());
        assert_eq!(mods_keys(4 | 1), vec![CONTROL_L, SHIFT_L]);
        let s = parse_combo("shift").unwrap();
        assert_eq!((s.mods.len(), s.keysym, s.keycode), (0, 0xffe1, 42));
    }

    #[test]
    fn typing() {
        let s = char_stroke('한');
        assert_eq!((s.keysym, s.keycode, s.text.as_str()), (0x0100_d55c, 0, "한"));
        assert_eq!(char_stroke('é').keysym, 0xe9);
        assert_eq!(char_stroke('\n').keysym, 0xff0d);
        assert_eq!(char_stroke('?').mods, vec![SHIFT_L]);
    }
}
