#!/usr/bin/env bash

require_sudo() {
    if ! sudo -v; then
        echo "Sudo authentication is required to install system files"
        exit 1
    fi
}

detect_init_system() {
    if pidof systemd 1>/dev/null 2>/dev/null; then
        INIT_SYSTEM="systemd"
    elif [ -f "/sbin/rc-update" ]; then
        INIT_SYSTEM="openrc"
    else
        INIT_SYSTEM="other"
    fi
}

install() {
    require_sudo

    echo "Building the project..."
    cargo build --release # TODO: The GUI should be optional. At least for now. Before releasing this, it sould be turned into a feature with an explicit cli switch to install it

    if [ $? -ne 0 ]; then
        echo "An error occurred while building the project"
        exit 1
    fi

    # Stop the service if it's running
    echo "Stopping the service..."
    case $INIT_SYSTEM in
    systemd)
        systemctl --user stop razercontrol
        ;;
    openrc)
        sudo rc-service razercontrol stop
        ;;
    esac

    # Install the files
    echo "Installing the files..."
    mkdir -p ~/.local/share/razercontrol
    sudo bash <<EOF
        set -e
        mkdir -p /usr/share/razercontrol
        install -Dm755 target/release/razer-cli /usr/bin/razer-cli.new
        mv -f /usr/bin/razer-cli.new /usr/bin/razer-cli
        install -Dm755 target/release/razer-settings /usr/bin/razer-settings.new
        mv -f /usr/bin/razer-settings.new /usr/bin/razer-settings
        if ls /usr/share/applications/*.desktop 1> /dev/null 2>&1; then
            # We only install the desktop file if there are already desktop
            # files on the system
            cp data/gui/razer-settings.desktop /usr/share/applications/
        fi
        install -Dm755 target/release/daemon /usr/share/razercontrol/daemon.new
        mv -f /usr/share/razercontrol/daemon.new /usr/share/razercontrol/daemon
        cp data/devices/laptops.json /usr/share/razercontrol/
        cp data/udev/99-hidraw-permissions.rules /etc/udev/rules.d/
        mkdir -p /usr/share/icons/hicolor/scalable/apps
        cp data/gui/razer-blade-control.svg /usr/share/icons/hicolor/scalable/apps/
        gtk-update-icon-cache -f /usr/share/icons/hicolor/ 2>/dev/null || true
        udevadm control --reload-rules
EOF

    if [ $? -ne 0 ]; then
        echo "An error occurred while installing the files"
        exit 1
    fi

    # Start the service
    echo "Starting the service..."
    case $INIT_SYSTEM in
    systemd)
        sudo cp data/services/systemd/razercontrol.service /etc/systemd/user/
        systemctl --user daemon-reload
        systemctl --user enable --now razercontrol
        ;;
    openrc)
        sudo bash <<EOF
            cp data/services/openrc/razercontrol /etc/init.d/
            # HACK: Change the username in the script
            sed -i 's/USERNAME_CHANGEME/$USER/' /etc/init.d/razercontrol

            chmod +x /etc/init.d/razercontrol
            rc-update add razercontrol default
            rc-service razercontrol start
EOF
        ;;
    esac

    echo "Installation complete"
    echo "Tip: enable tray autostart from razer-settings -> System -> Desktop Integration"
}

uninstall() {
    require_sudo

    # Remove the files
    echo "Uninstalling the files..."
    sudo bash <<EOF
        set -e
        rm -f /usr/bin/razer-cli
        rm -f /usr/bin/razer-settings
        rm -f /usr/share/applications/razer-settings.desktop
        rm -f /usr/share/razercontrol/daemon
        rm -f /usr/share/razercontrol/laptops.json
        rm -f /etc/udev/rules.d/99-hidraw-permissions.rules
        rm -f /usr/share/icons/hicolor/scalable/apps/razer-blade-control.svg
        gtk-update-icon-cache -f /usr/share/icons/hicolor/ 2>/dev/null || true
        udevadm control --reload-rules
EOF

    if [ $? -ne 0 ]; then
        echo "An error occurred while uninstalling the files"
        exit 1
    fi

    # Stop the service
    echo "Stopping the service..."
    case $INIT_SYSTEM in
    systemd)
        systemctl --user disable --now razercontrol
    sudo bash <<EOF
        set -e
        rm -f /etc/systemd/user/razercontrol.service
EOF
        systemctl --user daemon-reload
        ;;
    openrc)
        sudo bash <<EOF
            set -e
            rc-service razercontrol stop
            rc-update del razercontrol default
            rm -f /etc/init.d/razercontrol
EOF
        ;;
    esac

    # Remove XDG autostart entry if it was enabled from the GUI
    rm -f "$HOME/.config/autostart/razer-settings.desktop"

    echo "Uninstalled"
}

main() {
    if [ "$EUID" -eq 0 ]; then
        echo "Please do not run as root"
        exit 1
    fi

    detect_init_system

    if [ "$INIT_SYSTEM" = "other" ]; then
        echo "Unsupported init system"
        exit 1
    fi

    case $1 in
    install)
        install
        ;;
    uninstall)
        uninstall
        ;;
    *)
        echo "Usage: $0 {install|uninstall}"
        exit 1
        ;;
    esac
}

main $@
