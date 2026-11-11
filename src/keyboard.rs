use std::os::fd::{AsRawFd, OwnedFd};

use kbvm::lookup::LookupTable;
use kbvm::xkb::Context;
use kbvm::xkb::diagnostic::Diagnostic;
use kbvm::{GroupIndex, Keycode, ModifierMask, syms};

#[derive(Debug, PartialEq)]
pub enum KeyAction {
    Insert(char),
    Erase,
    Clear,
    Submit,
}

/// Turns key presses into password edits using the keymap and layout state sent by the compositor.
#[derive(Default)]
pub struct Keyboard {
    lookup_table: Option<LookupTable>,
    modifiers: ModifierMask,
    group: GroupIndex,
}

impl Keyboard {
    pub fn load_keymap(&mut self, fd: OwnedFd, size: u32) {
        let Some(text) = read_keymap(&fd, size) else {
            eprintln!("glitchlock: cannot read the keymap");
            return;
        };
        self.set_keymap(&text);
    }

    fn set_keymap(&mut self, text: &[u8]) {
        match Context::default().keymap_from_bytes(Vec::<Diagnostic>::new(), None, text) {
            Ok(keymap) => self.lookup_table = Some(keymap.to_builder().build_lookup_table()),
            Err(error) => eprintln!("glitchlock: cannot parse the keymap: {error}"),
        }
    }

    pub fn set_modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        self.modifiers = ModifierMask(depressed | latched | locked);
        self.group = GroupIndex(group);
    }

    /// The action for a pressed evdev key code.
    pub fn press(&self, key: u32) -> Option<KeyAction> {
        let lookup_table = self.lookup_table.as_ref()?;
        let lookup = lookup_table.lookup(self.group, self.modifiers, Keycode::from_evdev(key));

        lookup
            .into_iter()
            .find_map(|production| match production.keysym() {
                syms::BackSpace => Some(KeyAction::Erase),
                syms::Escape => Some(KeyAction::Clear),
                syms::Return | syms::KP_Enter => Some(KeyAction::Submit),
                _ => production
                    .char()
                    .filter(|character| !character.is_control())
                    .map(KeyAction::Insert),
            })
    }
}

fn read_keymap(fd: &OwnedFd, size: u32) -> Option<Vec<u8>> {
    let size = usize::try_from(size).ok().filter(|size| *size > 0)?;

    // SAFETY: Mapping a file descriptor read-only and private has no side effects.
    let mapping = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            fd.as_raw_fd(),
            0,
        )
    };
    if mapping == libc::MAP_FAILED {
        return None;
    }

    // SAFETY: The mapping is `size` bytes long and stays valid until `munmap`.
    let mut text = unsafe { std::slice::from_raw_parts(mapping.cast::<u8>(), size) }.to_vec();
    // SAFETY: `mapping` was returned by `mmap` with this size.
    unsafe { libc::munmap(mapping, size) };

    let end = text
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |last| last + 1);
    text.truncate(end);
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: u32 = 30;
    const KEY_BACKSPACE: u32 = 14;
    const KEY_ENTER: u32 = 28;
    const KEY_ESC: u32 = 1;

    const KEYMAP: &str = r#"
        xkb_keymap {
            xkb_keycodes { <ESC> = 9; <BKSP> = 22; <RTRN> = 36; <AC01> = 38; };
            xkb_types {
                virtual_modifiers NumLock;
                type "ALPHABETIC" { modifiers = Shift + Lock; map[Shift] = Level2; map[Lock] = Level2; };
            };
            xkb_compat { };
            xkb_symbols {
                key <ESC> { [ Escape ] };
                key <BKSP> { [ BackSpace ] };
                key <RTRN> { [ Return ] };
                key <AC01> { type = "ALPHABETIC", [ a, A ], [ adiaeresis, Adiaeresis ] };
                modifier_map Shift { <LFSH> };
            };
        };
    "#;

    fn keyboard() -> Keyboard {
        let mut keyboard = Keyboard::default();
        keyboard.set_keymap(KEYMAP.as_bytes());
        keyboard
    }

    #[test]
    fn maps_editing_keys() {
        let keyboard = keyboard();

        assert_eq!(keyboard.press(KEY_A), Some(KeyAction::Insert('a')));
        assert_eq!(keyboard.press(KEY_BACKSPACE), Some(KeyAction::Erase));
        assert_eq!(keyboard.press(KEY_ENTER), Some(KeyAction::Submit));
        assert_eq!(keyboard.press(KEY_ESC), Some(KeyAction::Clear));
    }

    #[test]
    fn follows_modifiers_and_layout_group() {
        let mut keyboard = keyboard();

        keyboard.set_modifiers(1, 0, 0, 0);
        assert_eq!(keyboard.press(KEY_A), Some(KeyAction::Insert('A')));

        keyboard.set_modifiers(0, 0, 0, 1);
        assert_eq!(keyboard.press(KEY_A), Some(KeyAction::Insert('ä')));
    }

    #[test]
    fn ignores_keys_without_keymap() {
        assert_eq!(Keyboard::default().press(KEY_A), None);
    }
}
