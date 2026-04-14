use ksni::menu::*;
use std::sync::mpsc;

#[derive(Debug, Clone)]
pub enum TrayAction {
    ShowWindow,
    Restart,
    Quit,
    SetPowerMode { ac: bool, profile: u8 },
    SetBrightness { ac: bool, percent: u8 },
    SetEffect { name: String, params: Vec<u8> },
}

pub struct RazerTray {
    pub action_sender: mpsc::Sender<TrayAction>,
}

impl RazerTray {
    fn send_action(&self, action: TrayAction) {
        let _ = self.action_sender.send(action);
    }
}

impl ksni::Tray for RazerTray {
    fn id(&self) -> String {
        "razer-blade-control".into()
    }

    fn icon_name(&self) -> String {
        "razer-blade-control".into()
    }

    fn title(&self) -> String {
        "Razer Blade Control".into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            icon_name: "razer-blade-control".into(),
            title: "Razer Blade Control".into(),
            description: "Left-click to open settings".into(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send_action(TrayAction::ShowWindow);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Open Settings".into(),
                icon_name: "preferences-desktop".into(),
                activate: Box::new(|this: &mut Self| {
                    this.send_action(TrayAction::ShowWindow);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            SubMenu {
                label: "Lighting Effect".into(),
                icon_name: "preferences-desktop-color-symbolic".into(),
                submenu: vec![
                    StandardItem {
                        label: "Static (Green)".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetEffect {
                                name: "static".into(),
                                params: vec![0, 255, 0],
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "Static (White)".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetEffect {
                                name: "static".into(),
                                params: vec![255, 255, 255],
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "Spectrum Cycle".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetEffect {
                                name: "spectrum_cycle".into(),
                                params: vec![3],
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "Rainbow Wave".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetEffect {
                                name: "rainbow_wave".into(),
                                params: vec![3, 0],
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "Wheel".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetEffect {
                                name: "wheel".into(),
                                params: vec![3, 0],
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "Breathing (Green)".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetEffect {
                                name: "breathing_single".into(),
                                params: vec![0, 255, 0, 10],
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "Ripple (Cyan)".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetEffect {
                                name: "ripple".into(),
                                params: vec![0, 255, 255, 3],
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "Starlight (White)".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetEffect {
                                name: "starlight".into(),
                                params: vec![255, 255, 255, 10],
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "Power Mode – AC".into(),
                icon_name: "battery-full-charging-symbolic".into(),
                submenu: vec![
                    StandardItem {
                        label: "Balanced".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetPowerMode {
                                ac: true,
                                profile: 0,
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "Gaming".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetPowerMode {
                                ac: true,
                                profile: 1,
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "Creator".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetPowerMode {
                                ac: true,
                                profile: 2,
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "Silent".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetPowerMode {
                                ac: true,
                                profile: 3,
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "Power Mode – Battery".into(),
                icon_name: "battery-good-symbolic".into(),
                submenu: vec![
                    StandardItem {
                        label: "Balanced".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetPowerMode {
                                ac: false,
                                profile: 0,
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "Gaming".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetPowerMode {
                                ac: false,
                                profile: 1,
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "Creator".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetPowerMode {
                                ac: false,
                                profile: 2,
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "Silent".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetPowerMode {
                                ac: false,
                                profile: 3,
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "Brightness – AC".into(),
                icon_name: "display-brightness-symbolic".into(),
                submenu: vec![
                    StandardItem {
                        label: "Off".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetBrightness {
                                ac: true,
                                percent: 0,
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "25%".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetBrightness {
                                ac: true,
                                percent: 25,
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "50%".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetBrightness {
                                ac: true,
                                percent: 50,
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "75%".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetBrightness {
                                ac: true,
                                percent: 75,
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "100%".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetBrightness {
                                ac: true,
                                percent: 100,
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "Brightness – Battery".into(),
                icon_name: "display-brightness-symbolic".into(),
                submenu: vec![
                    StandardItem {
                        label: "Off".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetBrightness {
                                ac: false,
                                percent: 0,
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "25%".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetBrightness {
                                ac: false,
                                percent: 25,
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "50%".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetBrightness {
                                ac: false,
                                percent: 50,
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "75%".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetBrightness {
                                ac: false,
                                percent: 75,
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "100%".into(),
                        activate: Box::new(|this: &mut Self| {
                            this.send_action(TrayAction::SetBrightness {
                                ac: false,
                                percent: 100,
                            })
                        }),
                        ..Default::default()
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Restart App".into(),
                icon_name: "view-refresh-symbolic".into(),
                activate: Box::new(|this: &mut Self| {
                    this.send_action(TrayAction::Restart);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|this: &mut Self| this.send_action(TrayAction::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}
