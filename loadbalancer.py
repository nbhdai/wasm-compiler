#!/usr/bin/env python3
#
# A script to deploy an NGINX load balancer with a Tailscale sidecar
# using podman-compose. This creates a single entry point for a cluster
# of backend services.
#
import os
import sys
import subprocess
import shutil
import pwd

# --- Configuration ---
APP_NAME = "nginx-balancer"
SERVICE_USER = "nginx-svc"
APP_DIR = f"/opt/{APP_NAME}"
SERVICE_FILE = f"/etc/systemd/system/{APP_NAME}.service"
ENV_FILE = f"{APP_DIR}/.env"

# --- List your backend compiler hostnames here ---
COMPILER_HOSTNAMES = ["devserver0", "devserver1"]
COMPILER_PORT = "8080"
BALANCER_PORT = "80"
BALANCER_HOSTNAME = "compiler-lb"

# --- Helper Functions for Colored Output ---
def print_success(message):
    print(f"\033[32m[SUCCESS]\033[0m {message}")

def print_info(message):
    print(f"\033[34m[INFO]\033[0m {message}")

def print_warn(message):
    print(f"\033[33m[WARN]\033[0m {message}")

def print_error(message):
    print(f"\033[91m[ERROR]\033[0m {message}", file=sys.stderr)


def run_command(command, check=True):
    """Runs a command, captures output, and prints it on error."""
    try:
        result = subprocess.run(
            command,
            check=check,
            shell=True,
            capture_output=True,
            text=True
        )
        return result
    except subprocess.CalledProcessError as e:
        print_error(f"Command failed: {command}")
        if e.stdout:
            print(f"--- STDOUT ---\n{e.stdout.strip()}", file=sys.stderr)
        if e.stderr:
            print(f"--- STDERR ---\n{e.stderr.strip()}", file=sys.stderr)
        # Re-raise the exception to stop the script
        raise

def user_exists(username):
    """Checks if a user exists."""
    try:
        pwd.getpwnam(username)
        return True
    except KeyError:
        return False

def main():
    """Main function to run the setup process."""
    # 1. Ensure the script is run as root
    if os.geteuid() != 0:
        print_error("Please run this script as root or with sudo.")
        sys.exit(1)

    print_info(f"Setting up the {APP_NAME} service...")

    # Stop the service if it is running to allow for a clean update.
    print_info(f"Stopping existing {APP_NAME} service if it is running...")
    run_command(f"systemctl stop {APP_NAME}.service || true", check=False)

    # 2. Install Dependencies
    print_info("Ensuring podman and podman-compose are installed...")
    run_command("pacman -Syu --noconfirm podman podman-compose")

    # 3. Create and Configure the Service User for Rootless Podman
    if user_exists(SERVICE_USER):
        print_warn(f"User {SERVICE_USER} already exists. Skipping creation.")
    else:
        print_info(f"Creating system user '{SERVICE_USER}'...")
        run_command(f"useradd --system --create-home --shell /usr/sbin/nologin {SERVICE_USER}")
        print_success(f"User {SERVICE_USER} created.")

    print_info(f"Configuring subordinate IDs and lingering for {SERVICE_USER}...")
    run_command(f"usermod --add-subuids 100000-165535 --add-subgids 100000-165535 {SERVICE_USER}")
    run_command(f"loginctl enable-linger {SERVICE_USER}")
    print_success(f"Rootless Podman configured for {SERVICE_USER}.")

    # 4. Create Application Directory Structure
    print_info(f"Creating application directory at {APP_DIR}...")
    os.makedirs(f"{APP_DIR}/nginx", exist_ok=True)
    os.makedirs(f"{APP_DIR}/tailscale_state", exist_ok=True)
    print_success("Directory structure created.")

    # 5. Set Ownership on App Directory so service user can access it
    print_info(f"Setting initial ownership of {APP_DIR} to {SERVICE_USER}...")
    shutil.chown(APP_DIR, user=SERVICE_USER, group=SERVICE_USER)
    shutil.chown(f"{APP_DIR}/nginx", user=SERVICE_USER, group=SERVICE_USER)
    shutil.chown(f"{APP_DIR}/tailscale_state", user=SERVICE_USER, group=SERVICE_USER)
    print_success(f"Initial ownership of {APP_DIR} set.")

    # 6. Initialize Podman Storage for the User
    print_info(f"Initializing podman storage for user {SERVICE_USER}...")
    # This command ensures that podman's internal storage is initialized
    # correctly. Running from the app directory provides the correct context.
    init_command = f"sudo -u {SERVICE_USER} -H bash -c 'cd {APP_DIR} && podman info'"
    run_command(init_command)
    print_success("Podman storage initialized.")


    # 7. Create NGINX Configuration
    print_info("Generating nginx.conf...")
    upstream_servers = "\n".join([f"        server {host}:{COMPILER_PORT};" for host in COMPILER_HOSTNAMES])
    nginx_conf = f"""
events {{}}

http {{
    # Define the pool of backend compiler services
    upstream backend_services {{
        least_conn; # Distribute requests to the server with the fewest connections
{upstream_servers}
    }}

    server {{
        listen {BALANCER_PORT};

        # Use Tailscale's DNS for resolving backend hostnames
        resolver 100.100.100.100 valid=10s;

        location / {{
            proxy_pass http://backend_services;
            proxy_set_header Host $host;
            proxy_set_header X-Real-IP $remote_addr;
            proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
            proxy_set_header X-Forwarded-Proto $scheme;
        }}
    }}
}}
"""
    with open(f"{APP_DIR}/nginx/nginx.conf", "w") as f:
        f.write(nginx_conf)
    print_success("nginx.conf created successfully.")

    # 8. Create Docker Compose File
    print_info("Generating docker-compose.yml...")
    compose_yml = f"""
version: '3.8'

services:
  tailscale:
    image: tailscale/tailscale:latest
    hostname: {BALANCER_HOSTNAME}
    environment:
      - TS_AUTHKEY=${{TS_AUTHKEY}}
      - TS_STATE_DIR=/var/lib/tailscale
      - TS_EXTRA_ARGS=--advertise-tags=tag:load-balancer
    volumes:
      - ./tailscale_state:/var/lib/tailscale
      - /dev/net/tun:/dev/net/tun
    cap_add:
      - net_admin
      - sys_module
    restart: unless-stopped

  nginx:
    image: nginx:latest
    network_mode: service:tailscale
    volumes:
      - ./nginx/nginx.conf:/etc/nginx/nginx.conf:ro
    depends_on:
      - tailscale
    restart: unless-stopped

volumes:
  tailscale_state:
"""
    with open(f"{APP_DIR}/docker-compose.yml", "w") as f:
        f.write(compose_yml)
    print_success("docker-compose.yml created successfully.")

    # 9. Create Environment File for Tailscale Auth Key
    if os.path.exists(ENV_FILE):
        print_warn(f"Environment file {ENV_FILE} already exists. Skipping.")
    else:
        print_info("Creating environment file for Tailscale key...")
        ts_authkey = input("Please enter your Tailscale Auth Key: ")
        with open(ENV_FILE, "w") as f:
            f.write(f"TS_AUTHKEY={ts_authkey}\n")
        print_success(f"Tailscale key saved to {ENV_FILE}.")

    # 10. Set Ownership on Configuration Files
    print_info(f"Setting ownership of configuration files to {SERVICE_USER}...")
    shutil.chown(f"{APP_DIR}/nginx/nginx.conf", user=SERVICE_USER, group=SERVICE_USER)
    shutil.chown(f"{APP_DIR}/docker-compose.yml", user=SERVICE_USER, group=SERVICE_USER)
    if os.path.exists(ENV_FILE):
        shutil.chown(ENV_FILE, user=SERVICE_USER, group=SERVICE_USER)
    print_success("File ownership set.")

    # 11. Create systemd Service File
    print_info(f"Creating systemd service file at {SERVICE_FILE}...")
    service_uid = pwd.getpwnam(SERVICE_USER).pw_uid
    systemd_service = f"""
[Unit]
Description=NGINX Load Balancer with Tailscale Sidecar
# This ensures that the user's runtime directory (/run/user/<UID>) exists before this service starts.
Requires=user@{service_uid}.service
After=network-online.target user@{service_uid}.service

[Service]
User={SERVICE_USER}
Group={SERVICE_USER}
WorkingDirectory={APP_DIR}

# Load the Tailscale Auth Key from the .env file
EnvironmentFile=-{ENV_FILE}

# --- CRITICAL: Environment for Rootless Podman ---
# Set the user's runtime directory for the podman socket.
Environment="XDG_RUNTIME_DIR=/run/user/{service_uid}"

# Use podman-compose to manage the services
# Ensure a clean start by running 'down' first
ExecStartPre=/usr/bin/podman-compose -f {APP_DIR}/docker-compose.yml down --remove-orphans
ExecStart=/usr/bin/podman-compose -f {APP_DIR}/docker-compose.yml up
ExecStop=/usr/bin/podman-compose -f {APP_DIR}/docker-compose.yml down

Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
"""
    with open(SERVICE_FILE, "w") as f:
        f.write(systemd_service)
    print_success("Systemd service file created.")

    # 12. Reload, Enable, and Start Service
    print_info(f"Reloading systemd and starting the {APP_NAME} service...")
    run_command("systemctl daemon-reload")
    run_command(f"systemctl enable {APP_NAME}.service")
    run_command(f"systemctl restart {APP_NAME}.service")
    print_success("Service has been enabled and started.")

    # 13. Final Instructions
    print("\n" + "-"*56)
    print_success("     NGINX LOAD BALANCER SETUP IS COMPLETE!             ")
    print("-" * 56 + "\n")
    print(f"The load balancer is running and connected to Tailscale as '{BALANCER_HOSTNAME}'.")
    print(f"It is listening on port {BALANCER_PORT} and forwarding traffic to your compilers.\n")
    print("You can check its status with:")
    print(f"sudo systemctl status {APP_NAME}\n")
    print("To see the container logs, navigate to the app directory and run:")
    print(f"cd {APP_DIR} && sudo -u {SERVICE_USER} podman-compose logs -f\n")


if __name__ == "__main__":
    main()
