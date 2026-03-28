use std::sync::mpsc;
use std::process::Command;
use ksni::menu::*;

fn cli(args: &[&str]) {
    let _ = Command::new("razer-cli").args(args).spawn();
}

pub struct RazerTray {
    pub show_window_sender: mpsc::Sender<()>,
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
        let _ = self.show_window_sender.send(());
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Open Settings".into(),
                icon_name: "preferences-desktop".into(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.show_window_sender.send(());
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
                        activate: Box::new(|_| cli(&["effect", "static", "0", "255", "0"])),
                        ..Default::default()
                    }.into(),
                    StandardItem {
                        label: "Static (White)".into(),
                        activate: Box::new(|_| cli(&["effect", "static", "255", "255", "255"])),
                        ..Default::default()
                    }.into(),
                    StandardItem {
                        label: "Spectrum Cycle".into(),
                        activate: Box::new(|_| cli(&["effect", "spectrum-cycle", "3"])),
                        ..Default::default()
                    }.into(),
                    StandardItem {
                        label: "Rainbow Wave".into(),
                        activate: Box::new(|_| cli(&["effect", "rainbow-wave", "3", "0"])),
                        ..Default::default()
                    }.into(),
                    StandardItem {
                        label: "Wheel".into(),
                        activate: Box::new(|_| cli(&["effect", "wheel", "3", "0"])),
                        ..Default::default()
                    }.into(),
                    StandardItem {
                        label: "Breathing (Green)".into(),
                        activate: Box::new(|_| cli(&["effect", "breathing-single", "0", "255", "0", "10"])),
                        ..Default::default()
                    }.into(),
                    StandardItem {
                        label: "Ripple (Cyan)".into(),
                        activate: Box::new(|_| cli(&["effect", "ripple", "0", "255", "255", "3"])),
                        ..Default::default()
                    }.into(),
                    StandardItem {
                        label: "Starlight (White)".into(),
                        activate: Box::new(|_| cli(&["effect", "starlight", "255", "255", "255", "10"])),
                        ..Default::default()
                    }.into(),
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
                        activate: Box::new(|_| cli(&["write", "power", "ac", "0"])),
                        ..Default::default()
                    }.into(),
                    StandardItem {
                        label: "Gaming".into(),
                        activate: Box::new(|_| cli(&["write", "power", "ac", "1"])),
                        ..Default::default()
                    }.into(),
                    StandardItem {
                        label: "Creator".into(),
                        activate: Box::new(|_| cli(&["write", "power", "ac", "2"])),
                        ..Default::default()
                    }.into(),
                    StandardItem {
                        label: "Silent".into(),
                        activate: Box::new(|_| cli(&["write", "power", "ac", "3"])),
                        ..Default::default()
                    }.into(),
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
                        activate: Box::new(|_| cli(&["write", "power", "bat", "0"])),
                        ..Default::default()
                    }.into(),
                    StandardItem {
                        label: "Gaming".into(),
                        activate: Box::new(|_| cli(&["write", "power", "bat", "1"])),
                        ..Default::default()
                    }.into(),
                    StandardItem {
                        label: "Creator".into(),
                        activate: Box::new(|_| cli(&["write", "power", "bat", "2"])),
                        ..Default::default()
                    }.into(),
                    StandardItem {
                        label: "Silent".into(),
                        activate: Box::new(|_| cli(&["write", "power", "bat", "3"])),
                        ..Default::default()
                    }.into(),
                ],
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|_| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        ]
    }
}
