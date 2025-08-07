#!/bin/bash
#
# A comprehensive script to set up the environment for the wasm-compiler service.
# This script is idempotent and can be re-run safely for initial setup and updates.
#

# --- Configuration ---
APP_NAME="wasm-compiler"
SERVICE_USER="wasm-svc"
APP_DIR="/opt/${APP_NAME}"
BINARY_NAME="${APP_NAME}"
# The location of the pre-compiled binary.
# Update this path to point to your actual compiled binary.
BINARY_LOCATION="./target/release/service-compiler"
COMPILER_SRC_DIR="./compiler" # Source directory for the compiler build scripts
SERVICE_FILE="/etc/systemd/system/${APP_NAME}.service"
SUDOERS_FILE="/etc/sudoers.d/${SERVICE_USER}"
ENV_FILE="/etc/default/${APP_NAME}" # Secure environment file for secrets

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

source .env

# Exit immediately if a command exits with a non-zero status.
set -e

# --- Main Logic ---

# Ensure the script is run as root
if [ "$EUID" -ne 0 ]; then
  echo "Please run this script as root or with sudo."
  exit 1
fi

print_info "Starting setup/update for the ${APP_NAME} service..."

# Stop the service if it is running to allow for a clean update.
if systemctl is-active --quiet "${APP_NAME}.service"; then
    print_info "Stopping existing ${APP_NAME} service for update..."
    systemctl stop "${APP_NAME}.service"
    print_success "Service stopped."
fi

# 1. Install Dependencies (will skip if already installed)
print_info "Ensuring podman and podman-docker are installed..."
# Using pacman as per the original script. Adjust for other package managers if needed.
#pacman -Syu --noconfirm podman podman-compose

# 2. Create Application Directory and Copy Binary
if [ ! -d "${APP_DIR}" ]; then
    print_info "Creating application directory at ${APP_DIR}..."
    mkdir -p "${APP_DIR}"
    print_success "Directory ${APP_DIR} created."
fi

print_info "Copying binary from ${BINARY_LOCATION} to ${APP_DIR}/${APP_NAME}"
cp "${BINARY_LOCATION}" "${APP_DIR}/${APP_NAME}"
print_success "Binary copied."

# 3. Create the Service User
if id -u "${SERVICE_USER}" &>/dev/null; then
    print_warn "User ${SERVICE_USER} already exists. Skipping creation."
else
    print_info "Creating system user '${SERVICE_USER}'..."
    # Create a system user with no login shell for security.
    useradd --system --create-home --shell /usr/sbin/nologin "${SERVICE_USER}"
    print_success "User ${SERVICE_USER} created."
fi

# 4. Configure Rootless Podman for the Service User
print_info "Configuring subordinate IDs and lingering for ${SERVICE_USER}..."
# These steps are essential for enabling rootless container operation.
usermod --add-subuids 100000-165535 --add-subgids 100000-165535 "${SERVICE_USER}"
# Enable lingering to allow the user's services (like podman.socket) to run even when not logged in.
loginctl enable-linger "${SERVICE_USER}"
print_success "Rootless Podman configured for ${SERVICE_USER}."

# 5. Set Ownership on Application Directory
print_info "Setting ownership of ${APP_DIR} to ${SERVICE_USER}..."
chown -R "${SERVICE_USER}:${SERVICE_USER}" "${APP_DIR}"
chmod 755 "${APP_DIR}/${APP_NAME}" # Ensure the binary is executable

# 6. Build Compiler Images as Service User
print_info "Copying compiler build files..."
cp -r "${COMPILER_SRC_DIR}" "${APP_DIR}/compiler"
chown -R "${SERVICE_USER}:${SERVICE_USER}" "${APP_DIR}/compiler"
print_success "Compiler files copied."

print_info "Building container images as user '${SERVICE_USER}'..."
# This command runs the build script as the service user.
# The `sudo -u ... -H` ensures the environment is set up correctly for the user.
# The `bash -c '...'` allows us to run multiple commands in a subshell,
# so the main script's working directory does not change.
sudo -u "${SERVICE_USER}" -H bash -c "cd '${APP_DIR}/compiler' && CR_PAT=$CR_PAT bash fetch.sh"
print_success "Container images built successfully."


# 7. Set Sudo Permissions for Deployment
print_info "Configuring passwordless sudo for deployment commands..."
cat > "${SUDOERS_FILE}" << EOF
# Allow the ${SERVICE_USER} user to run specific commands for deployment
${SERVICE_USER} ALL=(ALL) NOPASSWD: /usr/bin/mv /home/${SERVICE_USER}/${BINARY_NAME} ${APP_DIR}/, /usr/bin/systemctl restart ${APP_NAME}.service
EOF
chmod 0440 "${SUDOERS_FILE}"
print_success "Sudoers file created at ${SUDOERS_FILE}."

# 8. Create Secure Environment File for Secrets
# This step will not overwrite an existing file, preserving your secrets.
if [ ! -f "${ENV_FILE}" ]; then
    print_info "Creating secure environment file at ${ENV_FILE}..."
    cat > "${ENV_FILE}" << EOF
# This file can be used to store secrets for the ${APP_NAME} service.
# For example:
# MY_API_KEY=YOUR_SECRET_KEY
EOF
    # Set secure permissions: root can read/write, service user's group can read.
    chown root:"${SERVICE_USER}" "${ENV_FILE}"
    chmod 640 "${ENV_FILE}"
    print_success "Environment file created. You can edit it to add secrets."
else
    print_warn "Environment file ${ENV_FILE} already exists. Skipping creation to preserve secrets."
fi


# 9. Create systemd Service File
print_info "Creating systemd service file at ${SERVICE_FILE}..."
# Get the UID of the service user to ensure the service starts after the user's runtime is ready.
SERVICE_UID=$(id -u "${SERVICE_USER}")
cat > "${SERVICE_FILE}" << EOF
[Unit]
Description=Wasm Compiler Service
# --- CRITICAL: Dependency on User Runtime ---
# This ensures that the user's runtime directory (/run/user/<UID>) exists before this service starts.
# This is the key to solving the "lstat /run/user/0" error for rootless podman in system services.
Requires=user@${SERVICE_UID}.service
After=network.target user@${SERVICE_UID}.service

[Service]
# Run as the dedicated service user
User=${SERVICE_USER}
Group=${SERVICE_USER}

# Set the working directory for the application
WorkingDirectory=${APP_DIR}

# Load secrets from the secure environment file (the '-' ignores errors if the file doesn't exist)
EnvironmentFile=-${ENV_FILE}

# --- CRITICAL: Environment for Rootless Podman ---
# The following variables are essential for allowing a systemd service to use rootless containers.
# %U is a systemd specifier that automatically resolves to the user's UID.
Environment="XDG_RUNTIME_DIR=/run/user/${SERVICE_UID}"
# DOCKER_HOST tells the 'docker' (podman-docker) client where to find the API socket.
Environment="DOCKER_HOST=unix:///run/user/${SERVICE_UID}/podman/podman.sock"
# --- End of Critical Environment ---

# Additional environment variables for the application itself
Environment="DOCKER_ARCH=linux/amd64"

# The main command to start the application
ExecStart=${APP_DIR}/${BINARY_NAME}

# Restart the service automatically if it fails
Restart=on-failure
RestartSec=5s

[Install]
WantedBy=multi-user.target
EOF
print_success "Systemd service file created."

# 10. Reload, Enable, and Restart Services
print_info "Reloading systemd daemon and restarting services..."
systemctl daemon-reload
# Enable the service to start on boot
systemctl enable "${APP_NAME}.service"
# Start/Restart the service now
systemctl restart "${APP_NAME}.service"
print_success "Service has been enabled and (re)started."

# 11. Final Instructions
echo ""
print_success "--------------------------------------------------------"
print_success "         SERVER SETUP/UPDATE IS COMPLETE!               "
print_success "--------------------------------------------------------"
echo ""
echo "The service is currently running. You can check its status with:"
echo "sudo systemctl status ${APP_NAME}"
echo ""
echo "You can view live logs with:"
echo "sudo journalctl -u ${APP_NAME} -f"
echo ""
