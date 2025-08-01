#!/bin/bash
set -e

# The build marker is placed in the mounted volume to persist across restarts.
BUILD_MARKER="/home/podman/.local/share/containers/build_complete_marker"

# Only run the build if the marker file does not exist.
if [ ! -f "$BUILD_MARKER" ]; then
  echo "--- Performing one-time compiler image setup ---"
  
  cd /home/podman/compiler
  /bin/bash build.sh
  
  # Create the marker file to prevent this from running again.
  touch "$BUILD_MARKER"
  echo "--- Compiler setup complete ---"
else
  echo "--- Compiler setup already complete, skipping. ---"
fi

echo "--- Starting application ---"
# The 'exec' command replaces the script process with your application.
exec "$@"