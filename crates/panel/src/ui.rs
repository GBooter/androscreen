//! The window's markup, kept in its own module so the preview tool can render
//! exactly what the user sees.
//!
//! The window carries no title of its own: the compositor already draws one
//! above this surface, and a second large "安卓副屏" inside the content is what
//! used to read as two title bars. What sits at the top is the state of the
//! service, which is the thing a user opens this window to find out.
//!
//! Every word is a property rather than a literal: the window speaks two
//! languages, and [`crate::i18n`] decides which one before the first frame is
//! drawn. The defaults here are only what a renderer that never got a language
//! would show, so they are blank instead of one of the two.
//!
//! # Why nothing has a fixed height
//!
//! The window used to give every card a number — 112px for the state, 104px
//! for the numbers — because a fixed window with fixed rows is easy to lay out
//! and never moves under the pointer. It stopped being true when the sentences
//! grew: an English sentence is routinely half again as long as its Chinese
//! counterpart, so the same card was comfortable in one language and clipped
//! in the other, and it always clipped the sentence that mattered (the recovery
//! step). So each surface is now as tall as what it holds, and the page scrolls
//! if the sum outgrows the window. The cost is a page that can move; the
//! benefit is that no sentence is ever cut.
//!
//! # Why the page reads top to bottom
//!
//! The order is: what is happening, what works, what the numbers are, what can
//! be done. The three capability dots are always in the same place, so "the
//! driver is not loaded" is visible without reading a paragraph, and the
//! telemetry card is *only* telemetry — it used to be borrowed for the two
//! firmware notices, which put the most urgent message of the window in the
//! spot the eye had learned to skip.

slint::slint! {
    // A card: one surface, sized by what it holds.
    component Card inherits Rectangle {
        in property <length> pad: 14px;
        in property <length> gap: 5px;
        border-radius: 16px;
        background: #FFFFFF;
        border-width: 1px;
        border-color: #E7EBF3;
        height: body.preferred-height;
        body := VerticalLayout {
            padding: root.pad;
            spacing: root.gap;
            @children
        }
    }

    // One of the three numbers in the metrics strip.
    component Metric inherits VerticalLayout {
        in property <string> label;
        in property <string> value;
        in property <float> ratio: 0;
        in property <color> tint: #0A7D6F;
        spacing: 6px;
        alignment: space-between;
        Text {
            text: root.label;
            font-size: 11px;
            color: #98A3B5;
            wrap: word-wrap;
        }
        Text {
            text: root.value;
            font-size: 17px;
            font-weight: 700;
            color: #0F1A2B;
        }
        Rectangle {
            height: 3px;
            border-radius: 2px;
            background: #EDF0F6;
            Rectangle {
                width: parent.width * root.ratio;
                height: parent.height;
                border-radius: 2px;
                background: root.tint;
            }
        }
    }

    // A button that does not come from the widget set, so it can follow the
    // page instead of the platform's idea of a button.
    component FlatButton inherits Rectangle {
        in property <string> label;
        // Filled: the action the window is recommending.
        in property <bool> primary: false;
        // Outlined in red: the action that takes the screen away. It is the
        // same button in the same place whether or not the screen is on, so
        // the *look* has to say which of the two it is doing; a filled green
        // button that sometimes starts and sometimes stops was the one control
        // in this window that could be pressed by mistake.
        in property <bool> stop: false;
        callback clicked();
        height: 44px;
        border-radius: 12px;
        background: primary
            ? (touch.has-hover ? #08695D : #0A7D6F)
            : (stop
                ? (touch.has-hover ? #FBEEEE : #FFFFFF)
                : (touch.has-hover ? #F2F5FA : #FFFFFF));
        border-width: primary ? 0px : 1px;
        border-color: stop ? #EFD2D2 : (touch.has-hover ? #C9D4E4 : #DDE3EE);
        drop-shadow-blur: primary ? 10px : 0px;
        drop-shadow-color: #330A7D6F;
        drop-shadow-offset-y: primary ? 3px : 0px;
        Text {
            text: root.label;
            color: primary ? #FFFFFF : (stop ? #B4393A : #26334A);
            font-size: 13px;
            font-weight: 600;
            horizontal-alignment: center;
            vertical-alignment: center;
            wrap: word-wrap;
        }
        touch := TouchArea {
            // The area is what the pointer lands on, so it has to hand the
            // press to the button's own callback: without this the label
            // changes on hover and nothing else ever happens.
            clicked => {
                root.clicked();
            }
        }
    }

    // A two-position switch, drawn rather than borrowed.
    component Toggle inherits Rectangle {
        in-out property <bool> checked: true;
        width: 44px;
        height: 26px;
        border-radius: 13px;
        background: checked ? #0A7D6F : #D3DBE8;
        animate background { duration: 150ms; }
        Rectangle {
            width: 20px;
            height: 20px;
            border-radius: 10px;
            background: #FFFFFF;
            y: 3px;
            x: root.checked ? parent.width - self.width - 3px : 3px;
            drop-shadow-blur: 3px;
            drop-shadow-color: #22000000;
            animate x { duration: 150ms; easing: ease-in-out; }
        }
    }

    // One thing the screen needs, and whether it is there. Always drawn, so
    // the same three sit in the same three places in every state.
    component Capability inherits Rectangle {
        in property <string> name;
        in property <string> state;
        in property <color> tint: #0A7D6F;
        // The row of three is the answer to "why is there no picture", read
        // before any of the numbers below it: at 26px with 11px type it was
        // the smallest text in the window, so it now carries the same weight
        // as the labels it is meant to be the headline for.
        height: 32px;
        border-radius: 9px;
        background: #FFFFFF;
        border-width: 1px;
        border-color: #E7EBF3;
        HorizontalLayout {
            padding-left: 12px;
            padding-right: 12px;
            spacing: 7px;
            alignment: start;
            // A dot in a row of text has to be told to sit on the text's
            // centre line; left to itself the layout hangs it from the top.
            VerticalLayout {
                alignment: center;
                width: 7px;
                Rectangle {
                    width: 7px;
                    height: 7px;
                    border-radius: 3.5px;
                    background: root.tint;
                }
            }
            Text {
                text: root.name;
                font-size: 12px;
                color: #7A879E;
                vertical-alignment: center;
                overflow: elide;
            }
            Text {
                text: root.state;
                font-size: 12px;
                font-weight: 600;
                color: root.tint;
                vertical-alignment: center;
                overflow: elide;
            }
        }
    }

    export component SettingsWindow inherits Window {
        title: root.window-title;
        width: 520px;
        // The window is as tall as what it has to say. A state with one short
        // sentence does not hold a window the size of the one that explains a
        // firmware prompt, and no state carries a band of empty cards: a fixed
        // height left the leftover space inside the page, which the layout then
        // spread between the cards as a gap nobody asked for.
        //
        // 660 is the ceiling, and above it the page scrolls instead of growing
        // past the bottom of a laptop screen; 480 is the floor, so the window
        // never opens as a sliver before the labels have been filled in.
        height: min(660px, max(480px, page.preferred-height));
        background: #F4F6FA;

        in property <string> window-title: "AndroScreen";
        // The conclusion: one sentence, one explanation, one action.
        in property <string> state-title: "";
        in property <string> state-detail: "";
        in property <color> accent: #B4393A;
        // 0 = nothing to offer here; 1 = enroll; 2 = withdraw; 3 = reboot;
        // 4 = restart the service. Rust decides which, because only it can see
        // the firmware and the daemon.
        in property <int> hero-action: 0;
        in property <string> hero-action-label: "";

        in property <string> feedback: "";
        in property <bool> running: false;
        in property <bool> attached: false;
        in property <bool> streaming: false;
        in-out property <bool> auto-takeover: true;
        in property <string> version: "";
        in property <string> cpu-text: "—";
        in property <string> mem-text: "—";
        in property <string> temp-text: "—";
        in property <string> temp-label: "";
        in property <float> cpu-ratio: 0;
        in property <float> mem-ratio: 0;
        in property <float> temp-ratio: 0;
        in property <color> temp-tint: #98A3B5;
        in property <string> apk-version: "";

        // The three preconditions, each with its own verdict.
        in property <string> driver-label: "";
        in property <string> driver-state: "";
        in property <color> driver-tint: #98A3B5;
        in property <string> secure-label: "";
        in property <string> secure-state: "";
        in property <color> secure-tint: #98A3B5;
        in property <string> link-label: "";
        in property <string> link-state: "";
        in property <color> link-tint: #98A3B5;

        // Secure Boot refusing the virtual-display module gets a card of its
        // own rather than a firmware prompt appearing unannounced at boot.
        in property <bool> evdi-blocked: false;
        // The module is installed but deliberately not loaded yet: deepin 25.3
        // freezes when a display driver is loaded into a live X11 session, so
        // the package leaves it for the next boot and the window says so.
        in property <bool> reboot-required: false;
        // The question that has to be answered before the firmware is touched.
        in-out property <bool> mok-confirm-open: false;
        in property <string> mok-title: "";
        in property <string> mok-detail: "";
        // The two buttons are the two ends of one cable, so each says what it
        // will do from where the phone stands right now.
        in property <string> enable-label: "";
        in property <string> transfer-label: "";

        // The labels of everything that does not change with the service's
        // state, filled once per language by `i18n`.
        in property <string> processor-label: "";
        in property <string> memory-label: "";
        in property <string> auto-title: "";
        in property <string> auto-detail: "";
        in property <string> apk-title: "";
        in property <string> apk-detail: "";
        in property <string> apk-button: "";
        in property <string> restart-label: "";
        in property <string> mok-confirm-title: "";
        in property <string> mok-confirm-body: "";
        in property <string> cancel-label: "";
        in property <string> enroll-label: "";
        // The language the switch would move to, written in that language.
        in property <string> lang-label: "";

        callback auto-toggled(bool);
        callback release-clicked();
        callback enable-clicked();
        callback restart-clicked();
        callback download-clicked();
        callback reboot-clicked();
        callback mok-toggled(bool);
        callback lang-clicked();
        callback hero-action-clicked();

        // The page can outgrow the window in one language and not the other;
        // a cord the user can drag is cheaper than a card that clips.
        Flickable {
            content-width: self.width;
            content-height: page.preferred-height;
            interactive: page.preferred-height > self.height;

            page := VerticalLayout {
                width: parent.content-width;
                padding: 16px;
                spacing: 8px;

                // 1. What is happening, and what can be done about it.
                Rectangle {
                    border-radius: 18px;
                    background: @linear-gradient(100deg, #E9FAF7 0%, #F2EFFE 100%);
                    border-width: 1px;
                    border-color: #E3EDF4;
                    height: hero.preferred-height;
                    hero := VerticalLayout {
                        padding: 14px;
                        spacing: 7px;
                        HorizontalLayout {
                            spacing: 9px;
                            VerticalLayout {
                                alignment: center;
                                width: 10px;
                                Rectangle {
                                    width: 10px;
                                    height: 10px;
                                    border-radius: 5px;
                                    background: root.accent;
                                }
                            }
                            Text {
                                text: root.state-title;
                                font-size: 17px;
                                font-weight: 700;
                                color: #0F1A2B;
                                wrap: word-wrap;
                                vertical-alignment: center;
                                horizontal-stretch: 1;
                            }
                        }
                        Text {
                            text: root.state-detail;
                            font-size: 12px;
                            color: #5B6880;
                            wrap: word-wrap;
                        }
                        // The next step belongs next to the reason for it.
                        // Written as an `if` rather than `visible:`: a hidden
                        // element still holds its slot in a layout, and the
                        // sentence with no next step was carrying 37 pixels of
                        // Nothing underneath it.
                        if root.hero-action != 0 : Rectangle {
                            height: 30px;
                            width: hero-label.preferred-width + 26px;
                            border-radius: 9px;
                            background: hero-touch.has-hover ? #08695D : #0A7D6F;
                            hero-label := Text {
                                x: 13px;
                                height: parent.height;
                                text: root.hero-action-label;
                                font-size: 12px;
                                font-weight: 600;
                                color: #FFFFFF;
                                vertical-alignment: center;
                            }
                            hero-touch := TouchArea {
                                clicked => { root.hero-action-clicked(); }
                            }
                        }
                    }
                }

                // 2. Whether the three things a picture needs are there.
                HorizontalLayout {
                    spacing: 8px;
                    Capability {
                        name: root.driver-label;
                        state: root.driver-state;
                        tint: root.driver-tint;
                        horizontal-stretch: 1;
                    }
                    Capability {
                        name: root.secure-label;
                        state: root.secure-state;
                        tint: root.secure-tint;
                        horizontal-stretch: 1;
                    }
                    Capability {
                        name: root.link-label;
                        state: root.link-state;
                        tint: root.link-tint;
                        horizontal-stretch: 1;
                    }
                }

                // 3. The one situation that needs instructions rather than a
                //    number: the firmware is holding the module back. Omitted
                //    from the layout entirely while it does not apply — the
                //    blank band it used to leave was the whole reason the cards
                //    below it sat a hundred pixels away.
                if root.evdi-blocked : Card {
                    pad: 14px;
                    gap: 5px;
                    Text {
                        text: root.mok-title;
                        font-size: 13px;
                        font-weight: 600;
                        color: #0F1A2B;
                        wrap: word-wrap;
                    }
                    Text {
                        text: root.mok-detail;
                        font-size: 11px;
                        color: #7A879E;
                        wrap: word-wrap;
                    }
                }

                // 4. The computer's own numbers. Only ever numbers.
                Card {
                    pad: 12px;
                    HorizontalLayout {
                        spacing: 14px;
                        Metric {
                            label: root.processor-label;
                            value: root.cpu-text;
                            ratio: root.cpu-ratio;
                            tint: #0A7D6F;
                            horizontal-stretch: 1;
                        }
                        Metric {
                            label: root.memory-label;
                            value: root.mem-text;
                            ratio: root.mem-ratio;
                            tint: #6D5BD0;
                            horizontal-stretch: 1;
                        }
                        Metric {
                            label: root.temp-label;
                            value: root.temp-text;
                            ratio: root.temp-ratio;
                            tint: root.temp-tint;
                            horizontal-stretch: 1;
                        }
                    }
                }

                // 5. The one setting this window owns.
                Card {
                    pad: 13px;
                    HorizontalLayout {
                        spacing: 12px;
                        VerticalLayout {
                            alignment: center;
                            spacing: 4px;
                            horizontal-stretch: 1;
                            Text {
                                text: root.auto-title;
                                font-size: 14px;
                                font-weight: 600;
                                color: #0F1A2B;
                                wrap: word-wrap;
                            }
                            Text {
                                text: root.auto-detail;
                                font-size: 11px;
                                color: #7A879E;
                                wrap: word-wrap;
                            }
                        }
                        VerticalLayout {
                            alignment: center;
                            Toggle { checked: root.auto-takeover; }
                        }
                    }
                    touch := TouchArea {
                        clicked => {
                            root.auto-takeover = !root.auto-takeover;
                            root.auto-toggled(root.auto-takeover);
                        }
                    }
                }

                // 6. The two things the cable can do.
                HorizontalLayout {
                    spacing: 10px;
                    FlatButton {
                        label: root.transfer-label;
                        horizontal-stretch: 1;
                        clicked => { root.release-clicked(); }
                    }
                    FlatButton {
                        label: root.enable-label;
                        primary: !root.attached;
                        stop: root.attached;
                        horizontal-stretch: 1;
                        clicked => { root.enable-clicked(); }
                    }
                }

                // 7. The phone app rides inside this package; this hands it over
                //    as a file the user can move to the phone themselves.
                Card {
                    pad: 11px;
                    HorizontalLayout {
                        spacing: 12px;
                        VerticalLayout {
                            alignment: center;
                            width: 34px;
                            Rectangle {
                                width: 34px;
                                height: 34px;
                                border-radius: 11px;
                                background: @linear-gradient(135deg, #0A7D6F 0%, #6D5BD0 100%);
                                Text {
                                    text: "A";
                                    color: #FFFFFF;
                                    font-size: 16px;
                                    font-weight: 800;
                                    horizontal-alignment: center;
                                    vertical-alignment: center;
                                }
                            }
                        }
                        VerticalLayout {
                            alignment: center;
                            spacing: 3px;
                            horizontal-stretch: 1;
                            Text {
                                text: root.apk-title;
                                font-size: 13px;
                                font-weight: 600;
                                color: #0F1A2B;
                                wrap: word-wrap;
                            }
                            Text {
                                text: root.apk-detail;
                                font-size: 11px;
                                color: #7A879E;
                                wrap: word-wrap;
                            }
                        }
                        VerticalLayout {
                            alignment: center;
                            Rectangle {
                                width: 76px;
                                height: 32px;
                                border-radius: 10px;
                                background: apk-touch.has-hover ? #E2EFEB : #EFF3F8;
                                Text {
                                    text: root.apk-button;
                                    font-size: 12px;
                                    font-weight: 600;
                                    color: #0A7D6F;
                                    horizontal-alignment: center;
                                    vertical-alignment: center;
                                }
                                apk-touch := TouchArea {
                                    clicked => { root.download-clicked(); }
                                }
                            }
                        }
                    }
                }

                // 8. What the last click did. The space is reserved whether or
                //    not there is a sentence in it, so a click never moves the
                //    buttons under the pointer that just pressed them.
                Rectangle {
                    height: 34px;
                    border-radius: 10px;
                    background: root.feedback == "" ? transparent : #E9F4F1;
                    Text {
                        x: 12px;
                        width: parent.width - 24px;
                        height: parent.height;
                        text: root.feedback;
                        font-size: 12px;
                        color: #0A6B5E;
                        wrap: word-wrap;
                        vertical-alignment: center;
                    }
                }

                // Slack from a window that ended up taller than the page — a
                // long feedback sentence, or a compositor that refused the
                // height below — belongs at the bottom edge, where it reads as
                // a margin. Between two cards it reads as a mistake.
                Rectangle { vertical-stretch: 1; min-height: 0px; }

                // 9. The standing things: the service, the version, the language.
                HorizontalLayout {
                    height: 24px;
                    spacing: 8px;
                    // The restart link is a target of its own: the whole row is
                    // thumb-sized even though the word is small.
                    Rectangle {
                        width: restart-label.preferred-width + 12px;
                        height: 24px;
                        border-radius: 8px;
                        background: restart-touch.has-hover ? #E4EFEC : transparent;
                        restart-label := Text {
                            x: 6px;
                            height: parent.height;
                            text: root.restart-label;
                            font-size: 12px;
                            color: #0A7D6F;
                            vertical-alignment: center;
                        }
                        restart-touch := TouchArea {
                            clicked => { root.restart-clicked(); }
                        }
                    }
                    Rectangle { horizontal-stretch: 1; }
                    Text {
                        text: root.version;
                        font-size: 11px;
                        color: #98A3B5;
                        vertical-alignment: center;
                    }
                    // The way out of one language and into the other. It names
                    // the language it moves to, in that language, so a reader
                    // who cannot read the window can still find it.
                    Rectangle {
                        width: 64px;
                        height: 24px;
                        border-radius: 8px;
                        background: lang-touch.has-hover ? #E4EFEC : transparent;
                        border-width: 1px;
                        border-color: #DCE5EF;
                        Text {
                            text: root.lang-label;
                            font-size: 11px;
                            font-weight: 600;
                            color: #0A7D6F;
                            horizontal-alignment: center;
                            vertical-alignment: center;
                        }
                        lang-touch := TouchArea {
                            clicked => { root.lang-clicked(); }
                        }
                    }
                }
            }
        }

        // Confirmation before the firmware is touched. It covers the whole
        // window so no other control can be pressed while the question stands,
        // and it is as tall as its text: the paragraph about a machine that
        // will not boot is the longest sentence in the application and the
        // last one that should be cut off.
        Rectangle {
            visible: root.mok-confirm-open;
            x: 0;
            y: 0;
            width: 100%;
            height: 100%;
            background: #70000000;

            // Swallows presses that land outside the card.
            TouchArea {
                clicked => { }
            }

            Rectangle {
                width: 400px;
                height: dialog.preferred-height;
                x: (parent.width - self.width) / 2;
                y: (parent.height - self.height) / 2;
                border-radius: 16px;
                background: #FFFFFF;
                border-width: 1px;
                border-color: #E7EBF3;

                dialog := VerticalLayout {
                    padding: 20px;
                    spacing: 12px;

                    Text {
                        text: root.mok-confirm-title;
                        font-size: 16px;
                        font-weight: 700;
                        color: #0F1A2B;
                        wrap: word-wrap;
                    }
                    Text {
                        text: root.mok-confirm-body;
                        font-size: 11px;
                        color: #5B6880;
                        wrap: word-wrap;
                    }
                    HorizontalLayout {
                        height: 44px;
                        spacing: 10px;
                        FlatButton {
                            label: root.cancel-label;
                            horizontal-stretch: 1;
                            clicked => { root.mok-confirm-open = false; }
                        }
                        FlatButton {
                            label: root.enroll-label;
                            primary: true;
                            horizontal-stretch: 1;
                            clicked => {
                                root.mok-confirm-open = false;
                                root.mok-toggled(true);
                            }
                        }
                    }
                }
            }
        }
    }
}
