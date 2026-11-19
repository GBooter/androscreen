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

use androscreen_core::control::Request;

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
pub(crate) fn button_labels(cable: Cable) -> (&'static str, &'static str) {
    match cable {
        Cable::WithUs => ("关闭副屏", "改用文件传输"),
        Cable::WithPhone => ("启用副屏", "关闭文件传输"),
    }
}

/// What the secondary-screen button does from here.
#[must_use]
pub(crate) fn screen_action(cable: Cable) -> (Request, &'static str) {
    match cable {
        Cable::WithUs => (
            Request::Release,
            "副屏正在关闭：数据线交还手机，手机回到它自己的 USB 用途。",
        ),
        Cable::WithPhone => (Request::Enable, "已请求接管手机，副屏会在几秒内出现。"),
    }
}

/// What the file-transfer button does from here.
#[must_use]
pub(crate) fn transfer_action(cable: Cable) -> (Request, &'static str) {
    match cable {
        Cable::WithUs => (
            Request::Release,
            "已释放数据线。在手机的下拉通知里把 USB 用途选成“文件传输”，电脑上才会出现手机存储。",
        ),
        Cable::WithPhone => (
            Request::Enable,
            "一根数据线只能有一种用途：已收回它去当副屏，手机存储会断开。",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{button_labels, screen_action, transfer_action, Cable};
    use androscreen_core::control::Request;

    #[test]
    fn each_button_names_the_change_it_makes() {
        assert_eq!(
            button_labels(Cable::WithPhone),
            ("启用副屏", "关闭文件传输")
        );
        assert_eq!(button_labels(Cable::WithUs), ("关闭副屏", "改用文件传输"));
    }

    #[test]
    fn neither_button_is_ever_a_no_op() {
        // Every label promises a move, so no state may leave a button holding
        // the request that would keep the port where it already is.
        assert_eq!(screen_action(Cable::WithPhone).0, Request::Enable);
        assert_eq!(screen_action(Cable::WithUs).0, Request::Release);
        assert_eq!(transfer_action(Cable::WithUs).0, Request::Release);
        assert_eq!(transfer_action(Cable::WithPhone).0, Request::Enable);
    }

    #[test]
    fn the_two_buttons_never_disagree_about_the_port() {
        // One cable, one switch: whichever verb the user reached for, the port
        // ends up in the same place, and each button still explains the use it
        // handed the port to.
        for cable in [Cable::WithPhone, Cable::WithUs] {
            assert_eq!(screen_action(cable).0, transfer_action(cable).0);
            assert_ne!(screen_action(cable).1, transfer_action(cable).1);
        }
    }
}
