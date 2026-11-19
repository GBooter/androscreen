//! The window's markup, kept in its own module so the preview tool can render
//! exactly what the user sees.
//!
//! The window carries no title of its own: the compositor already draws one
//! above this surface, and a second large "安卓副屏" inside the content is what
//! used to read as two title bars. What sits at the top is the state of the
//! service, which is the thing a user opens this window to find out.

slint::slint! {
    // A card: the one surface repeated through the page.
    component Card inherits Rectangle {
        border-radius: 16px;
        background: #FFFFFF;
        border-width: 1px;
        border-color: #E7EBF3;
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
        }
        Text {
            text: root.value;
            font-size: 19px;
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
        in property <bool> primary: false;
        callback clicked();
        height: 44px;
        border-radius: 12px;
        background: primary
            ? (touch.has-hover ? #08695D : #0A7D6F)
            : (touch.has-hover ? #F2F5FA : #FFFFFF);
        border-width: primary ? 0px : 1px;
        border-color: touch.has-hover ? #C9D4E4 : #DDE3EE;
        drop-shadow-blur: primary ? 10px : 0px;
        drop-shadow-color: #330A7D6F;
        drop-shadow-offset-y: primary ? 3px : 0px;
        Text {
            text: root.label;
            color: root.primary ? #FFFFFF : #26334A;
            font-size: 13px;
            font-weight: 600;
            horizontal-alignment: center;
            vertical-alignment: center;
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

    export component SettingsWindow inherits Window {
        title: "安卓副屏";
        width: 460px;
        height: 554px;
        background: #F4F6FA;

        in property <string> state-title: "正在读取服务状态…";
        in property <string> state-detail: "";
        in property <string> feedback: "";
        in property <bool> running: false;
        in property <bool> attached: false;
        in property <bool> streaming: false;
        in-out property <bool> auto-takeover: true;
        in property <string> version: "";
        in property <color> accent: #B4393A;
        in property <string> cpu-text: "—";
        in property <string> mem-text: "—";
        in property <string> temp-text: "—";
        in property <string> temp-label: "温度";
        in property <float> cpu-ratio: 0;
        in property <float> mem-ratio: 0;
        in property <float> temp-ratio: 0;
        in property <color> temp-tint: #98A3B5;
        in property <string> apk-version: "";
        // Secure Boot refusing the virtual-display module is not the same
        // problem as "no cable", so it gets its own card and its own consent
        // rather than a firmware prompt appearing unannounced at the next boot.
        in property <bool> evdi-blocked: false;
        in-out property <bool> mok-requested: false;
        // The question that has to be answered before the firmware is touched.
        in-out property <bool> mok-confirm-open: false;
        in property <string> mok-title: "允许登记签名密钥";
        in property <string> mok-detail: "";
        // The two buttons are the two ends of one cable, so each says what it
        // will do from where the phone stands right now.
        in property <string> enable-label: "启用副屏";
        in property <string> transfer-label: "改用文件传输";

        callback auto-toggled(bool);
        callback release-clicked();
        callback enable-clicked();
        callback restart-clicked();
        callback download-clicked();
        callback mok-toggled(bool);

        VerticalLayout {
            padding: 18px;
            spacing: 12px;

            // What the service is doing, in one glance.
            Rectangle {
                height: 104px;
                border-radius: 16px;
                background: @linear-gradient(100deg, #E9FAF7 0%, #F2EFFE 100%);
                border-width: 1px;
                border-color: #E3EDF4;
                HorizontalLayout {
                    padding-left: 16px;
                    padding-right: 16px;
                    spacing: 12px;
                    VerticalLayout {
                        alignment: center;
                        width: 18px;
                        Rectangle {
                            width: 18px;
                            height: 18px;
                            border-radius: 9px;
                            background: root.accent.transparentize(0.82);
                            Rectangle {
                                width: 9px;
                                height: 9px;
                                border-radius: 5px;
                                background: root.accent;
                                x: 4.5px;
                                y: 4.5px;
                            }
                        }
                    }
                    VerticalLayout {
                        alignment: center;
                        spacing: 6px;
                        horizontal-stretch: 1;
                        Text {
                            text: root.state-title;
                            font-size: 17px;
                            font-weight: 700;
                            color: #0F1A2B;
                        }
                        Text {
                            text: root.state-detail;
                            font-size: 12px;
                            color: #5B6880;
                            wrap: word-wrap;
                        }
                    }
                }
            }

            // The computer's own numbers, or — while Secure Boot is refusing
            // the virtual-display module — the switch that can fix it. Both
            // rows share this card's rectangle, so the rest of the window
            // keeps its shape whichever one is showing.
            Card {
                height: 92px;
                HorizontalLayout {
                    visible: !root.evdi-blocked;
                    x: 0;
                    y: 0;
                    width: parent.width;
                    height: parent.height;
                    padding: 14px;
                    padding-left: 16px;
                    padding-right: 16px;
                    spacing: 14px;
                    Metric {
                        label: "处理器";
                        value: root.cpu-text;
                        ratio: root.cpu-ratio;
                        tint: #0A7D6F;
                        horizontal-stretch: 1;
                    }
                    Metric {
                        label: "内存";
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
                // Off until the user turns it on. It only records a request
                // that the blue MOK screen answers at the next boot, so the
                // words around it say what will happen before it can.
                HorizontalLayout {
                    visible: root.evdi-blocked;
                    x: 0;
                    y: 0;
                    width: parent.width;
                    height: parent.height;
                    padding: 14px;
                    padding-left: 16px;
                    padding-right: 16px;
                    spacing: 12px;
                    VerticalLayout {
                        alignment: center;
                        spacing: 4px;
                        horizontal-stretch: 1;
                        Text {
                            text: root.mok-title;
                            font-size: 14px;
                            font-weight: 600;
                            color: #0F1A2B;
                        }
                        Text {
                            text: root.mok-detail;
                            font-size: 11px;
                            color: #7A879E;
                            wrap: word-wrap;
                        }
                    }
                    VerticalLayout {
                        alignment: center;
                        Toggle { checked: root.mok-requested; }
                    }
                }
                mok-touch := TouchArea {
                    visible: root.evdi-blocked;
                    clicked => {
                        if root.mok-requested {
                            root.mok-requested = false;
                            root.mok-toggled(false);
                        } else {
                            // Turning the switch on writes a request into the
                            // firmware, so the question comes first.
                            root.mok-confirm-open = true;
                        }
                    }
                }
            }

            // The one setting this window owns.
            Card {
                height: 68px;
                HorizontalLayout {
                    padding: 14px;
                    spacing: 12px;
                    VerticalLayout {
                        alignment: center;
                        spacing: 4px;
                        horizontal-stretch: 1;
                        Text {
                            text: "插入手机时自动接管为副屏";
                            font-size: 14px;
                            font-weight: 600;
                            color: #0F1A2B;
                        }
                        Text {
                            text: "关闭后插线不会抢占手机，手机上仍可选“文件传输”。";
                            font-size: 11px;
                            color: #7A879E;
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

            HorizontalLayout {
                spacing: 10px;
                FlatButton {
                    label: root.transfer-label;
                    horizontal-stretch: 1;
                    clicked => { root.release-clicked(); }
                }
                FlatButton {
                    label: root.enable-label;
                    primary: true;
                    horizontal-stretch: 1;
                    clicked => { root.enable-clicked(); }
                }
            }

            // The phone app rides inside this package; this hands it over as a
            // file the user can move to the phone themselves.
            Card {
                height: 62px;
                HorizontalLayout {
                    padding: 12px;
                    padding-left: 14px;
                    padding-right: 14px;
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
                            text: "下载手机安装包";
                            font-size: 13px;
                            font-weight: 600;
                            color: #0F1A2B;
                        }
                        Text {
                            text: "安卓端 " + root.apk-version + " · 用数据线或聊天软件装到手机";
                            font-size: 11px;
                            color: #7A879E;
                            overflow: elide;
                        }
                    }
                    VerticalLayout {
                        alignment: center;
                        Rectangle {
                            width: 68px;
                            height: 32px;
                            border-radius: 10px;
                            background: apk-touch.has-hover ? #E2EFEB : #EFF3F8;
                            Text {
                                text: "下载";
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

            // Always present, so a click never moves the buttons under the
            // pointer that just pressed them. Two lines, because the answer to
            // "改用文件传输" is a sentence and not a word.
            Rectangle {
                height: 42px;
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

            HorizontalLayout {
                height: 24px;
                alignment: space-between;
                // The restart link is a target of its own: the whole 24px row
                // is thumb-sized even though the word is small.
                Rectangle {
                    width: 96px;
                    height: 24px;
                    border-radius: 8px;
                    background: restart-touch.has-hover ? #E4EFEC : transparent;
                    Text {
                        text: "重启副屏服务";
                        font-size: 12px;
                        color: #0A7D6F;
                        x: 6px;
                        width: parent.width - 12px;
                        vertical-alignment: center;
                    }
                    restart-touch := TouchArea {
                        clicked => { root.restart-clicked(); }
                    }
                }
                Text {
                    text: root.version;
                    font-size: 11px;
                    color: #98A3B5;
                    vertical-alignment: center;
                }
            }

            Rectangle { vertical-stretch: 1; }
        }

        // Confirmation before the firmware is touched. It covers the whole
        // window so no other control can be pressed while the question stands.
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
                width: 396px;
                height: 316px;
                x: (parent.width - self.width) / 2;
                y: (parent.height - self.height) / 2;
                border-radius: 16px;
                background: #FFFFFF;
                border-width: 1px;
                border-color: #E7EBF3;

                VerticalLayout {
                    padding: 20px;
                    spacing: 12px;

                    Text {
                        text: "登记签名密钥并不适用于每台电脑";
                        font-size: 16px;
                        font-weight: 700;
                        color: #0F1A2B;
                    }
                    Text {
                        text: "登记后重启时会先出现蓝色 MOK 界面：依次选择 Enroll MOK → Continue → Yes，再输入窗口里显示的密码。\n\n有些电脑的固件会拒绝这个登记，重启后出现 Security Policy Violation 或 Verification failed，导致无法进入系统。\n\n如果发生这种情况：进入 BIOS，执行“恢复默认安全启动密钥”（Restore Factory Keys），保存后即可正常开机；也可以关闭安全启动，进入系统后运行 sudo mokutil --revoke-import 清理登记。";
                        font-size: 11px;
                        color: #5B6880;
                        wrap: word-wrap;
                        vertical-stretch: 1;
                    }
                    HorizontalLayout {
                        height: 44px;
                        spacing: 10px;
                        FlatButton {
                            label: "取消";
                            horizontal-stretch: 1;
                            clicked => { root.mok-confirm-open = false; }
                        }
                        FlatButton {
                            label: "仍然登记";
                            primary: true;
                            horizontal-stretch: 1;
                            clicked => {
                                root.mok-confirm-open = false;
                                root.mok-requested = true;
                                root.mok-toggled(true);
                            }
                        }
                    }
                }
            }
        }
    }
}
