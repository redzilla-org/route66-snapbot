//! Key definitions for Input.dispatchKeyEvent, following puppeteer's US
//! keyboard layout for the keys a browse step can name or type.

pub struct KeyDef {
    pub key: String,
    pub code: String,
    pub key_code: i64,
    /// Text the key inserts; None for non-printing keys (rawKeyDown).
    pub text: Option<String>,
    pub location: i64,
}

fn named(key: &str, code: &str, key_code: i64, text: Option<&str>) -> KeyDef {
    KeyDef { key: key.to_string(), code: code.to_string(), key_code, text: text.map(str::to_string), location: 0 }
}

/// The definition for a key name ("Enter", "ArrowDown") or a single character.
/// None means the character has no key and is inserted as text instead.
pub fn lookup(key: &str) -> Option<KeyDef> {
    let d = match key {
        "Enter" | "\r" | "\n" => named("Enter", "Enter", 13, Some("\r")),
        "Tab" => named("Tab", "Tab", 9, None),
        "Backspace" => named("Backspace", "Backspace", 8, None),
        "Escape" => named("Escape", "Escape", 27, None),
        "Delete" => named("Delete", "Delete", 46, None),
        "Insert" => named("Insert", "Insert", 45, None),
        "ArrowLeft" => named("ArrowLeft", "ArrowLeft", 37, None),
        "ArrowUp" => named("ArrowUp", "ArrowUp", 38, None),
        "ArrowRight" => named("ArrowRight", "ArrowRight", 39, None),
        "ArrowDown" => named("ArrowDown", "ArrowDown", 40, None),
        "Home" => named("Home", "Home", 36, None),
        "End" => named("End", "End", 35, None),
        "PageUp" => named("PageUp", "PageUp", 33, None),
        "PageDown" => named("PageDown", "PageDown", 34, None),
        "Space" | " " => named(" ", "Space", 32, Some(" ")),
        "Shift" => KeyDef { location: 1, ..named("Shift", "ShiftLeft", 16, None) },
        "Control" => KeyDef { location: 1, ..named("Control", "ControlLeft", 17, None) },
        "Alt" => KeyDef { location: 1, ..named("Alt", "AltLeft", 18, None) },
        "Meta" => KeyDef { location: 1, ..named("Meta", "MetaLeft", 91, None) },
        _ => {
            let mut chars = key.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else {
                // F1..F12 and anything else unknown.
                if let Some(n) = key.strip_prefix('F').and_then(|n| n.parse::<i64>().ok()) {
                    if (1..=12).contains(&n) {
                        return Some(named(key, key, 111 + n, None));
                    }
                }
                return None;
            };
            return char_def(c);
        }
    };
    Some(d)
}

fn char_def(c: char) -> Option<KeyDef> {
    let t = c.to_string();
    let def = |code: String, key_code: i64| KeyDef { key: t.clone(), code, key_code, text: Some(t.clone()), location: 0 };
    if c.is_ascii_lowercase() || c.is_ascii_uppercase() {
        let up = c.to_ascii_uppercase();
        return Some(def(format!("Key{up}"), up as i64));
    }
    if c.is_ascii_digit() {
        return Some(def(format!("Digit{c}"), c as i64));
    }
    let (code, kc) = match c {
        '!' => ("Digit1", 49),
        '@' => ("Digit2", 50),
        '#' => ("Digit3", 51),
        '$' => ("Digit4", 52),
        '%' => ("Digit5", 53),
        '^' => ("Digit6", 54),
        '&' => ("Digit7", 55),
        '*' => ("Digit8", 56),
        '(' => ("Digit9", 57),
        ')' => ("Digit0", 48),
        '-' | '_' => ("Minus", 189),
        '=' | '+' => ("Equal", 187),
        '[' | '{' => ("BracketLeft", 219),
        ']' | '}' => ("BracketRight", 221),
        '\\' | '|' => ("Backslash", 220),
        ';' | ':' => ("Semicolon", 186),
        '\'' | '"' => ("Quote", 222),
        ',' | '<' => ("Comma", 188),
        '.' | '>' => ("Period", 190),
        '/' | '?' => ("Slash", 191),
        '`' | '~' => ("Backquote", 192),
        _ => return None,
    };
    Some(def(code.to_string(), kc))
}
