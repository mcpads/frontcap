//! Key definitions for keyboard input: a US layout for printable ASCII and the named keys an agent
//! presses. Characters outside it are inserted as text.
use crate::input::InputStep;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyDef {
    pub key: String,
    pub code: &'static str,
    pub key_code: i64,
    pub text: Option<String>,
}

const NAMED: &[(&str, &str, i64, Option<&str>)] = &[
    ("Enter", "Enter", 13, Some("\r")),
    ("Tab", "Tab", 9, None),
    ("Backspace", "Backspace", 8, None),
    ("Delete", "Delete", 46, None),
    ("Escape", "Escape", 27, None),
    ("Insert", "Insert", 45, None),
    ("Space", "Space", 32, Some(" ")),
    ("ArrowUp", "ArrowUp", 38, None),
    ("ArrowDown", "ArrowDown", 40, None),
    ("ArrowLeft", "ArrowLeft", 37, None),
    ("ArrowRight", "ArrowRight", 39, None),
    ("Home", "Home", 36, None),
    ("End", "End", 35, None),
    ("PageUp", "PageUp", 33, None),
    ("PageDown", "PageDown", 34, None),
    ("Shift", "ShiftLeft", 16, None),
    ("Control", "ControlLeft", 17, None),
    ("Alt", "AltLeft", 18, None),
    ("Meta", "MetaLeft", 91, None),
];

/// Punctuation of the US layout: (unshifted, shifted, code, key code).
const PUNCTUATION: &[(char, char, &str, i64)] = &[
    ('-', '_', "Minus", 189),
    ('=', '+', "Equal", 187),
    ('[', '{', "BracketLeft", 219),
    (']', '}', "BracketRight", 221),
    ('\\', '|', "Backslash", 220),
    (';', ':', "Semicolon", 186),
    ('\'', '"', "Quote", 222),
    (',', '<', "Comma", 188),
    ('.', '>', "Period", 190),
    ('/', '?', "Slash", 191),
    ('`', '~', "Backquote", 192),
];

const DIGIT_SHIFTED: [char; 10] = [')', '!', '@', '#', '$', '%', '^', '&', '*', '('];
const DIGIT_CODES: [&str; 10] = ["Digit0", "Digit1", "Digit2", "Digit3", "Digit4", "Digit5", "Digit6", "Digit7", "Digit8", "Digit9"];
const LETTER_CODES: [&str; 26] = [
    "KeyA", "KeyB", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG", "KeyH", "KeyI", "KeyJ", "KeyK", "KeyL", "KeyM", "KeyN", "KeyO", "KeyP", "KeyQ",
    "KeyR", "KeyS", "KeyT", "KeyU", "KeyV", "KeyW", "KeyX", "KeyY", "KeyZ",
];
const FUNCTION_CODES: [&str; 12] = ["F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12"];

/// Whether `c` needs Shift on the US layout.
fn shifted(c: char) -> bool {
    c.is_ascii_uppercase() || DIGIT_SHIFTED.contains(&c) || PUNCTUATION.iter().any(|(_, s, _, _)| *s == c)
}

/// The character Shift turns `c` into on the US layout (`a` → `A`, `1` → `!`), or `c` itself.
fn with_shift(c: char) -> char {
    if c.is_ascii_lowercase() {
        return c.to_ascii_uppercase();
    }
    if let Some(d) = c.to_digit(10) {
        return DIGIT_SHIFTED[d as usize];
    }
    PUNCTUATION.iter().find(|(u, _, _, _)| *u == c).map_or(c, |(_, s, _, _)| *s)
}

fn of_char(c: char) -> Option<KeyDef> {
    let def = |code, key_code| Some(KeyDef { key: c.to_string(), code, key_code, text: Some(c.to_string()) });
    match c {
        'a'..='z' | 'A'..='Z' => {
            let i = (c.to_ascii_lowercase() as u8 - b'a') as usize;
            def(LETTER_CODES[i], 65 + i as i64)
        }
        '0'..='9' => {
            let i = (c as u8 - b'0') as usize;
            def(DIGIT_CODES[i], 48 + i as i64)
        }
        ' ' => def("Space", 32),
        _ => {
            if let Some(i) = DIGIT_SHIFTED.iter().position(|d| *d == c) {
                return def(DIGIT_CODES[i], 48 + i as i64);
            }
            PUNCTUATION.iter().find(|(u, s, _, _)| *u == c || *s == c).and_then(|(_, _, code, key_code)| def(code, *key_code))
        }
    }
}

/// A key by its `KeyboardEvent.key` name, or a single printable ASCII character.
pub fn named(key: &str) -> Option<KeyDef> {
    if let Some((name, code, key_code, text)) = NAMED.iter().find(|(n, ..)| *n == key) {
        let key = if *name == "Space" { " " } else { name };
        return Some(KeyDef { key: key.to_string(), code, key_code: *key_code, text: text.map(str::to_string) });
    }
    if let Some(i) = FUNCTION_CODES.iter().position(|f| *f == key) {
        return Some(KeyDef { key: key.to_string(), code: FUNCTION_CODES[i], key_code: 112 + i as i64, text: None });
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => of_char(c),
        _ => None,
    }
}

/// Resolve a physical US-layout code, preserving left/right modifier identity.
fn physical(code: &str) -> Option<KeyDef> {
    if let Some(i) = LETTER_CODES.iter().position(|c| *c == code) {
        return of_char((b'a' + i as u8) as char);
    }
    if let Some(i) = DIGIT_CODES.iter().position(|c| *c == code) {
        return of_char((b'0' + i as u8) as char);
    }
    if let Some((unshifted, ..)) = PUNCTUATION.iter().find(|(_, _, c, _)| *c == code) {
        return of_char(*unshifted);
    }
    for (physical, logical) in [
        ("ShiftLeft", "Shift"), ("ShiftRight", "Shift"),
        ("ControlLeft", "Control"), ("ControlRight", "Control"),
        ("AltLeft", "Alt"), ("AltRight", "Alt"),
        ("MetaLeft", "Meta"), ("MetaRight", "Meta"),
    ] {
        if physical == code {
            let mut def = named(logical)?;
            def.code = physical;
            if physical == "MetaRight" { def.key_code = 92; }
            return Some(def);
        }
    }
    None
}

/// Held input accepts logical names or physical codes. Codes use the explicit US layout;
/// modifier changes affect subsequent repeat/release events. Logical characters stay explicit.
pub fn held(key: &str, modifiers: u8) -> Option<KeyDef> {
    let physical = physical(key);
    let mut def = physical.clone().or_else(|| named(key))?;
    if physical.is_some() && modifiers & 8 != 0
        && let Some(c) = def.text.as_deref().and_then(|t| t.chars().next())
        && let Some(shifted) = of_char(with_shift(c))
    {
        def = shifted;
    }
    if modifiers & !8 != 0 { def.text = None; }
    Some(def)
}

/// CDP modifier bits.
pub fn modifier_bit(key: &str) -> Option<u8> {
    match key {
        "Alt" | "AltLeft" | "AltRight" => Some(1),
        "Control" | "ControlLeft" | "ControlRight" => Some(2),
        "Meta" | "MetaLeft" | "MetaRight" => Some(4),
        "Shift" | "ShiftLeft" | "ShiftRight" => Some(8),
        _ => None,
    }
}

fn step(down: bool, def: &KeyDef, modifiers: u8, text: Option<String>) -> InputStep {
    InputStep::Key { repeat: false, down, key: def.key.clone(), code: def.code.to_string(), key_code: def.key_code, text, modifiers }
}

/// `"Control+Shift+K"`: modifiers pressed in order, the key pressed and released, the modifiers released
/// in reverse. A key pressed with Control, Alt, or Meta inserts no text. Platform editing commands
/// bound to chords (Meta+A on macOS) are not emulated: the page receives the key events only.
pub fn chord(spec: &str) -> Result<Vec<InputStep>, String> {
    let (mods, last) = match spec.strip_suffix("++") {
        _ if spec == "+" => ("", "+"),
        Some(rest) => (rest, "+"),
        None => match spec.rsplit_once('+') {
            Some((rest, last)) => (rest, last),
            None => ("", spec),
        },
    };
    let mut held: Vec<KeyDef> = Vec::new();
    let mut modifiers = 0u8;
    let mut steps = Vec::new();
    for name in mods.split('+').filter(|m| !m.is_empty()) {
        let bit = modifier_bit(name).ok_or_else(|| format!("{name:?} is not a modifier (Shift, Control, Alt, Meta)"))?;
        let def = self::held(name, 0).expect("modifiers have key definitions");
        if held.iter().any(|key| key.code == def.code) { return Err(format!("physical modifier {} is repeated in the chord", def.code)); }
        modifiers |= bit;
        steps.push(step(true, &def, modifiers, None));
        held.push(def);
    }
    let mut def = named(last).ok_or_else(|| format!("{last:?} is not a key name"))?;
    // Shift with a character key gives the shifted character (`Shift+a` is `A`).
    if modifiers & 8 != 0 {
        let mut chars = last.chars();
        if let (Some(c), None) = (chars.next(), chars.next())
            && let Some(shifted) = of_char(with_shift(c))
        {
            def = shifted;
        }
    }
    let text = if modifiers & !8 == 0 { def.text.clone() } else { None };
    steps.push(step(true, &def, modifiers, text));
    steps.push(step(false, &def, modifiers, None));
    // Each modifier is reported released by its own key-up, as a browser does.
    while let Some(def) = held.pop() {
        let remaining = held.iter().filter_map(|key| modifier_bit(&key.key)).fold(0, |a, b| a | b);
        steps.push(step(false, &def, remaining, None));
    }
    Ok(steps)
}

/// One character of typed text: its key steps when the layout has it (`'\n'` and `'\r'` are Enter),
/// otherwise one inserted-text step. A tab is inserted as text: as a key it would move focus, which is
/// `press Tab`'s to do.
pub fn typed(c: char) -> Vec<InputStep> {
    let def = match c {
        '\n' | '\r' => named("Enter"),
        _ => of_char(c),
    };
    match def {
        // A shifted character is typed with Shift held, as on a keyboard, so `shiftKey` is true.
        Some(def) if shifted(c) => {
            let shift = named("Shift").expect("a named key");
            vec![step(true, &shift, 8, None), step(true, &def, 8, def.text.clone()), step(false, &def, 8, None), step(false, &shift, 0, None)]
        }
        Some(def) => vec![step(true, &def, 0, def.text.clone()), step(false, &def, 0, None)],
        None => vec![InputStep::InsertText { text: c.to_string() }],
    }
}

/// The steps of typing `text`; a CR LF pair is one line break.
pub fn typed_text(text: &str) -> Vec<InputStep> {
    text.replace("\r\n", "\n").chars().flat_map(typed).collect()
}

#[cfg(test)]
#[path = "keys_tests.rs"]
mod tests;
