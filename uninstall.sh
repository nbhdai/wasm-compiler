#!/bin/bash
#
# A comprehensive script to completely uninstall the wasm-compiler service.
#

# --- Configuration (Must match your setup script) ---
APP_NAME="wasm-compiler"
SERVICE_USER="wasm-svc"
APP_DIR="/opt/${APP_NAME}"
SERVICE_FILE="/etc/systemd/system/${APP_NAME}.service"
SUDOERS_FILE="/etc/sudoers.d/${SERVICE_USER}"
ENV_FILE="/etc/default/${APP_NAME}"

# --- Helper Functions ---
print_success() {
    echo -e "\e[32m[SUCCESS]\e[0m $1"
}

print_info() {
    echo -e "\e[34m[INFO]\e[0m $1"
}

print_warn() {
    echo -e "\e[33m[WARN]\e[0m $1"
}

# Exit immediately if a command exits with a non-zero status.
set -e

# --- Main Logic ---

# Ensure the script is run as root
if [ "$EUID" -ne 0 ]; then
  echo "Please run this script as root or with sudo."
  exit 1
fi

print_info "Starting uninstallation of the ${APP_NAME} service..."

# 1. Stop and Disable the systemd Service
if systemctl list-unit-files | grep -q "${APP_NAME}.service"; then
    print_info "Stopping and disabling ${APP_NAME} service..."
    systemctl stop "${APP_NAME}.service" || true # Ignore error if not running
    systemctl disable "${APP_NAME}.service"
    print_success "Service stopped and disabled."
else
    print_warn "Service ${APP_NAME}.service not found. Skipping."
fi

# 2. Remove systemd and Sudoers Files
if [ -f "${SERVICE_FILE}" ]; then
    print_info "Removing systemd service file..."
    rm -f "${SERVICE_FILE}"
    print_success "Removed ${SERVICE_FILE}."
fi

if [ -f "${SUDOERS_FILE}" ]; then
    print_info "Removing sudoers configuration..."
    rm -f "${SUDOERS_FILE}"
    print_success "Removed ${SUDOERS_FILE}."
fi

print_info "Reloading systemd daemon..."
systemctl daemon-reload
systemctl reset-failed

# 3. Remove Secure Environment File
if [ -f "${ENV_FILE}" ]; then
    print_info "Removing environment file..."
    rm -f "${ENV_FILE}"
    print_success "Removed ${ENV_FILE}."
fi

# 4. Remove Application Directory
if [ -d "${APP_DIR}" ]; then
    print_info "Removing application directory ${APP_DIR}..."
    rm -rf "${APP_DIR}"
    print_success "Removed ${APP_DIR}."
fi

# 5. Clean Up the Service User
if id -u "${SERVICE_USER}" &>/dev/null; then
    print_info "Cleaning up user '${SERVICE_USER}'..."
    
    # Disable lingering for the user
    print_info "Disabling lingering for ${SERVICE_USER}..."
    loginctl disable-linger "${SERVICE_USER}"

    # Attempt to remove all Podman images owned by the user
    # This requires the user's runtime to be available, but we'll try our best.
    print_info "Attempting to remove Podman images for user ${SERVICE_USER}..."
    # The `-H` flag is crucial for finding the user's home-based configuration
    sudo -u "${SERVICE_USER}" -H bash -c 'podman rmi --all --force' || print_warn "Could not remove all Podman images. This may be okay if none existed."

    # Finally, remove the user and their home directory
    print_info "Deleting user '${SERVICE_USER}' and their home directory..."
    userdel --remove "${SERVICE_USER}"
    print_success "User ${SERVICE_USER} has been removed."
else
    print_warn "User ${SERVICE_USER} does not exist. Skipping user cleanup."
fi

# 6. Note about Dependencies
echo ""
print_info "The script does not uninstall system dependencies like 'podman' or 'podman-docker'."
print_info "If you no longer need them, you can remove them manually. For example:"
print_info "sudo pacman -Rns podman podman-docker podman-compose"
echo ""

# 7. Final Instructions
print_success "--------------------------------------------------------"
print_success "         UNINSTALLATION IS COMPLETE!                    "
print_success "--------------------------------------------------------"