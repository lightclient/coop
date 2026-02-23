# Long-Lived Container Sandboxes

Coop now supports long-lived container sandboxes that persist between command executions, allowing users to install packages, customize their environment, and maintain state across sessions.

## Overview

The traditional sandbox approach creates ephemeral containers that are destroyed after each command execution. While this provides strong isolation, it prevents users from:
- Installing packages and tools
- Customizing their development environment 
- Maintaining persistent state between commands
- Building up a personalized workspace over time

Long-lived containers solve this by creating persistent containers that survive between command executions while still maintaining sandbox security boundaries.

## Configuration

### Global Configuration

Enable long-lived containers in your `coop.toml`:

```toml
[sandbox]
enabled = true
long_lived = true             # Default: true
allow_network = true          # Allow package installation
memory = "4g"
pids_limit = 1024
cleanup_after_days = 30       # Default: 30 days (1 month)
protect_full_trust = true     # Default: true (never cleanup full trust users)
```

#### Cleanup Policy Options

- **`cleanup_after_days`**: Number of days of inactivity before containers are removed
- **`protect_full_trust`**: When `true`, containers owned by full trust users are never automatically cleaned up

### Per-User Overrides

Users can have different container persistence settings:

```toml
[[users]]
name = "alice"
trust = "full"
match = ["terminal:default"]
sandbox = { long_lived = true, memory = "8g" }

[[users]]  
name = "bob"
trust = "full"
match = ["signal:bob-uuid"]
sandbox = { long_lived = false }  # Bob gets ephemeral containers
```

## How It Works

### Container Lifecycle

1. **First Command**: When a user runs their first sandbox command, a new container is created with:
   - Persistent name based on workspace path (e.g., `coop-sandbox-a1b2c3d4`)
   - Pre-installed development tools (curl, git, python3, nodejs, rust, etc.)
   - Workspace directory mounted at `/work`

2. **Subsequent Commands**: Following commands reuse the existing container:
   - Container is started if stopped
   - Commands execute in the same persistent environment
   - All installed packages and customizations remain

3. **Cleanup**: Containers are automatically cleaned up after 30 days of inactivity (configurable), with full trust users protected from cleanup by default

### Container Management

Each workspace gets its own container, identified by a hash of the workspace path. This means:
- Different projects get separate environments
- Users can customize each project's container independently
- No cross-contamination between projects

## Platform Support

### macOS (apple/container)

Long-lived containers use the apple/container CLI with:
- **VM-grade isolation**: Each container runs in its own lightweight VM
- **Persistent storage**: Container filesystem persists between commands
- **Resource limits**: Memory and PID limits still enforced
- **Network isolation**: Configurable network access

### Linux (Docker/Podman)

*(Future implementation)*
- Standard Docker containers with persistent volumes
- Namespace isolation with optional gVisor
- Automatic cleanup via container lifecycle hooks

## Usage Examples

### Installing Development Tools

```bash
# Install Python packages (persists for future commands)
bash 'pip install --user numpy pandas matplotlib'

# Install Node.js packages globally  
bash 'npm install -g typescript @types/node'

# Install Rust tools
bash 'cargo install ripgrep fd-find'
```

### Environment Customization

```bash
# Customize shell environment
bash 'echo "alias ll=\"ls -la\"" >> ~/.bashrc'

# Set up development configurations
bash 'git config --global user.name "Alice"'
bash 'git config --global user.email "alice@example.com"'

# Create project templates
bash 'mkdir -p ~/templates && echo "console.log(\"Hello World\")" > ~/templates/hello.js'
```

### Persistent Data

```bash
# Download and cache large datasets
bash 'mkdir -p ~/cache && curl -o ~/cache/dataset.json https://example.com/data.json'

# Build and cache dependencies
bash 'cd /work && npm install'  # node_modules persists
bash 'cd /work && pip install -r requirements.txt'  # packages persist
```

## Security Considerations

Long-lived containers maintain sandbox security while providing persistence:

### Maintained Isolation
- **VM boundaries**: (macOS) Each container runs in isolated VM
- **Namespace isolation**: (Linux) Standard container namespace separation  
- **Resource limits**: Memory, CPU, and PID limits still enforced
- **Network controls**: Network access still configurable per-user

### Added Attack Surface
- **Persistent state**: Malicious code can persist between commands
- **Container escape**: Long-running containers provide more opportunity for escape attempts
- **Resource consumption**: Containers consume resources even when idle

### Mitigations
- **Automatic cleanup**: Containers are removed after 30 days of inactivity (configurable)
- **Trust-based protection**: Full trust users' containers are protected from auto-cleanup by default
- **Per-workspace isolation**: Each workspace gets separate container
- **Trust levels**: Owner trust can bypass sandbox entirely if needed
- **Configuration flexibility**: Users can opt for ephemeral containers if preferred

## Management Commands

### Manual Container Management

```bash
# List sandbox containers
container ps -a --filter "name=coop-sandbox-*"

# Connect to a container for debugging  
container exec -it coop-sandbox-a1b2c3d4 /bin/bash

# Remove a specific container
container stop coop-sandbox-a1b2c3d4
container rm coop-sandbox-a1b2c3d4

# Remove all coop sandbox containers
container ps -a --filter "name=coop-sandbox-*" -q | xargs container rm -f
```

### Programmatic Cleanup

```rust
use coop_sandbox;

// Clean up old containers (called automatically)
coop_sandbox::cleanup_old_containers().await?;
```

## Migration from Ephemeral Containers

Existing installations can migrate seamlessly:

1. **Update configuration**: Set `sandbox.long_lived = true` in `coop.toml`
2. **Restart coop**: New commands will use long-lived containers
3. **No data loss**: Workspace files remain unaffected
4. **Gradual rollout**: Enable per-user with sandbox overrides

To revert to ephemeral containers:
```toml
[sandbox]
long_lived = false
```

## Troubleshooting

### Container Won't Start
```bash
# Check container status
container ps -a --filter "name=coop-sandbox-*"

# View container logs
container logs coop-sandbox-a1b2c3d4

# Remove corrupted container (will be recreated)
container rm -f coop-sandbox-a1b2c3d4
```

### Disk Space Issues
```bash
# Check container disk usage
container system df

# Clean up old containers manually (respects full trust protection)
# Note: Full trust users' containers are protected by default

# Force cleanup all coop containers (ignores trust protection)
container ps -a --filter "name=coop-sandbox-*" -q | xargs container rm -f

# Or clean up only old containers
container container prune -f
```

### Performance Issues
```bash
# Monitor container resource usage
container stats coop-sandbox-a1b2c3d4

# Increase memory limit in config
[sandbox]
memory = "8g"  # Up from default 2g
```

## Best Practices

1. **Regular Updates**: Occasionally recreate containers to get security updates
2. **Resource Monitoring**: Monitor container resource usage in long-running setups  
3. **Backup Important Data**: Keep important data in the workspace, not container filesystem
4. **Trust Boundaries**: Use ephemeral containers for untrusted code execution
5. **Cleanup Scheduling**: Consider running cleanup more frequently in high-usage environments

## Future Enhancements

- **Container Templates**: Pre-built containers for different development stacks
- **Snapshot/Restore**: Save and restore container states
- **Multi-Container Support**: Link containers for complex development environments
- **Cloud Synchronization**: Sync container customizations across machines
- **Resource Monitoring**: Built-in container resource monitoring and alerting