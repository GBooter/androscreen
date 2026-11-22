//! What the two buttons mean, and where they point.
//!
//! File transfer and the secondary screen are the two answers to one question
//! — who holds the cable — so a button cannot be a fixed verb: "启用副屏" is a
//! lie the moment the phone is already the screen. Everything the window says
//! about the pair comes from here, which the off-screen preview shares so it
//! cannot drift from what the real window draws.
//!
//! There is one port, so both buttons always move it the same way; they are the
//! two verbs the user might be looking for, not two commands. What tells them
//! apart is the feedback line, which names the use the cable is being handed to.
//!
//! The words themselves are in [`crate::i18n`]; this module only decides which
//! of them fit the cable as it is now.

use androscreen_core::control::Request;

use crate::i18n::{Lang, Words};

/// Which end holds the cable.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cable {
    /// The phone holds it: storage, charging, whatever it offers by itself.
    WithPhone,
    /// The daemon holds it as an accessory, streaming or not.
    WithUs,
}

/// What the two buttons say, in order: the screen one, then the transfer one.
/// Each names the change it makes, so the pair always reads as the way the
/// cable currently points.
#[must_use]
pub(crate) fn button_labels(lang: Lang, cable: Cable) -> (&'static str, &'static str) {
    let words = Words::of(lang);
    match cable {
        Cable::WithUs => (words.screen_on_ours, words.transfer_on_ours),
        Cable::WithPhone => (words.screen_on_theirs, words.transfer_on_theirs),
    }
}

/// What the secondary-screen button does from here.
#[must_use]
pub(crate) fn screen_action(lang: Lang, cable: Cable) -> (Request, &'static str) {
    let words = Words::of(lang);
    match cable {
        Cable::WithUs => (Request::Release, words.screen_done_ours),
        Cable::WithPhone => (Request::Enable, words.screen_done_theirs),
    }
}

/// What the file-transfer button does from here.
#[must_use]
pub(crate) fn transfer_action(lang: Lang, cable: Cable) -> (Request, &'static str) {
    let words = Words::of(lang);
    match cable {
        Cable::WithUs => (Request::Release, words.transfer_done_ours),
        Cable::WithPhone => (Request::Enable, words.transfer_done_theirs),
    }
}

#[cfg(test)]
mod tests {
    use super::{button_labels, screen_action, transfer_action, Cable};
    use crate::i18n::Lang;
    use androscreen_core::control::Request;

    /// Every language the window can draw in.
    const LANGUAGES: [Lang; 2] = [Lang::Zh, Lang::En];

    #[test]
    fn each_button_names_the_change_it_makes() {
        for lang in LANGUAGES {
            let (screen, transfer) = button_labels(lang, Cable::WithPhone);
            let (screen_ours, transfer_ours) = button_labels(lang, Cable::WithUs);
            // Both states have to be told apart, or one of the two labels is a
            // word the buttons would never say.
            assert_ne!(screen, screen_ours, "screen label, {lang:?}");
            assert_ne!(transfer, transfer_ours, "transfer label, {lang:?}");
            assert!(!screen.is_empty() && !transfer.is_empty(), "{lang:?}");
        }
    }

    #[test]
    fn neither_button_is_ever_a_no_op() {
        // Every label promises a move, so no state may leave a button holding
        // the request that would keep the port where it already is.
        for lang in LANGUAGES {
            assert_eq!(screen_action(lang, Cable::WithPhone).0, Request::Enable);
            assert_eq!(screen_action(lang, Cable::WithUs).0, Request::Release);
            assert_eq!(transfer_action(lang, Cable::WithUs).0, Request::Release);
            assert_eq!(transfer_action(lang, Cable::WithPhone).0, Request::Enable);
        }
    }

    #[test]
    fn the_two_buttons_never_disagree_about_the_port() {
        // One cable, one switch: whichever verb the user reached for, the port
        // ends up in the same place, and each button still explains the use it
        // handed the port to.
        for lang in LANGUAGES {
            for cable in [Cable::WithPhone, Cable::WithUs] {
                assert_eq!(screen_action(lang, cable).0, transfer_action(lang, cable).0);
                assert_ne!(screen_action(lang, cable).1, transfer_action(lang, cable).1);
            }
        }
    }
}
