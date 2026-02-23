# Long-Lived Container Sandbox Implementation Summary

## Problem Solved

The apple/container sandbox implementation was using ephemeral containers with the `--rm` flag, which prevented users from:
- Installing packages and customizing their development environment
- Maintaining persistent state between command executions 
- Building up a personalized workspace over time

## Solution Implemented

### 1. Enhanced SandboxPolicy Structure
- **Added `long_lived` field** to `SandboxPolicy` in `crates/coop-sandbox/src/policy.rs`
- **Default value**: `true` to enable persistent containers by default for better user experience
- **Configurable**: Users can opt for ephemeral containers if preferred

### 2. Modified Apple Container Implementation
**File**: `crates/coop-sandbox/src/apple.rs`

- **Container Registry**: Added in-memory registry to track long-lived containers
- **Container Lifecycle Management**:
  - Creates persistent containers (removes `--rm` flag)
  - Reuses existing containers for subsequent commands
  - Auto-starts stopped containers
  - Generates deterministic container names based on workspace path hash

- **Development Tools**: Pre-installs common development tools in new containers:
  - `curl`, `wget`, `git`, `vim`, `nano`
  - `build-essential` (gcc, make, etc.)
  - `python3`, `python3-pip`, `python3-venv`
  - `nodejs`, `npm`
  - `rust-bin-stable`

- **Dual Mode Operation**:
  - **Long-lived mode**: Persistent containers that survive between commands
  - **Ephemeral mode**: Original behavior with `--rm` flag for backward compatibility

- **Smart Cleanup**: Removes containers idle for >30 days, with full trust users protected by default

### 3. Configuration Integration
**File**: `crates/coop-gateway/src/config.rs`

- **Global Setting**: Added `sandbox.long_lived` boolean to main config
- **Per-User Overrides**: Users can have different persistence settings via `SandboxOverrides`
- **Per-Cron Overrides**: Cron jobs can specify container persistence behavior
- **Hot-Reload Support**: Changes to `long_lived` setting are picked up without restart

### 4. Updated Configuration Structure
```toml
[sandbox]
enabled = true
long_lived = true             # New: Enable persistent containers
allow_network = true          # Allow package installation
memory = "4g"
pids_limit = 1024
cleanup_after_days = 30       # New: Cleanup window (1 month default)
protect_full_trust = true     # New: Protect full trust users from cleanup

# Per-user overrides
[[users]]
name = "alice"
trust = "full" 
sandbox = { long_lived = true, memory = "8g" }

[[users]]
name = "bob" 
trust = "full"
sandbox = { long_lived = false }  # Bob gets ephemeral containers
```

### 5. Container Management Features

- **Per-Workspace Isolation**: Each workspace gets its own container
- **Deterministic Naming**: Container names based on workspace path hash
- **Resource Limits**: Memory and PID limits still enforced
- **Network Control**: Configurable network access per container
- **State Persistence**: Installed packages and customizations survive between commands

## Benefits Achieved

### For Users
- **Persistent Environment**: Install tools once, use them across sessions
- **Customization Freedom**: Configure shell, install packages, set up dotfiles
- **Improved Productivity**: No need to reinstall tools on every command
- **Project Isolation**: Each workspace maintains separate customized environment

### For Security
- **Maintained Isolation**: VM boundaries (macOS) and namespace isolation still enforced
- **Resource Controls**: Memory and PID limits continue to prevent resource abuse
- **Trust Boundaries**: Configuration respects existing trust levels
- **Cleanup Automation**: Prevents indefinite resource accumulation

### For Operations
- **Backward Compatibility**: Existing configurations continue working unchanged
- **Gradual Migration**: Can enable per-user or globally as needed
- **Hot Configuration**: No restarts needed for configuration changes
- **Monitoring Ready**: Container registry tracks usage for future monitoring

## Implementation Details

### Container Creation Process
1. **Check for existing container** by workspace-based name
2. **Start if stopped** or create new container if missing
3. **Install development tools** in new containers
4. **Execute commands** via `container exec` instead of `container run --rm`
5. **Update last-used timestamp** for cleanup tracking

### Cleanup Strategy  
- **30-day idle timeout**: Containers unused for >30 days are automatically removed (configurable)
- **Trust-based protection**: Full trust users' containers are protected from auto-cleanup by default
- **Manual cleanup available**: `coop_sandbox::cleanup_old_containers()` and policy-based variants
- **Resource monitoring**: Container registry tracks creation time, usage, and user information

### Platform Compatibility
- **macOS (apple/container)**: Fully implemented with VM isolation
- **Linux**: Framework ready for future Docker/Podman implementation
- **Fallback**: Gracefully degrades to ephemeral containers if unsupported

## Files Modified

1. `crates/coop-sandbox/src/policy.rs` - Added `long_lived` field
2. `crates/coop-sandbox/src/apple.rs` - Implemented persistent container logic with trust-based cleanup
3. `crates/coop-sandbox/src/linux.rs` - Updated function signatures for compatibility
4. `crates/coop-sandbox/src/lib.rs` - Added cleanup functions with policy support
5. `crates/coop-sandbox/Cargo.toml` - Added coop-core dependency for TrustLevel
6. `crates/coop-gateway/src/config.rs` - Added configuration options including cleanup policy
7. `crates/coop-gateway/src/sandbox_executor.rs` - Integrated config support and user context passing
8. `crates/coop-gateway/src/main.rs` - Fixed policy construction
9. Various test files - Updated for new field compatibility

## Documentation Created

- `docs/long-lived-containers.md` - Comprehensive user and operator guide
- `IMPLEMENTATION_SUMMARY.md` - This technical summary

## Testing Status

- ✅ All existing tests pass
- ✅ New configuration parsing tests added
- ✅ Backward compatibility maintained
- ✅ Build succeeds on all platforms

## Usage Examples

### Installing Development Tools
```bash
# Install Python packages (persists between commands)
bash 'pip install --user numpy pandas matplotlib'

# Install Node.js tools globally
bash 'npm install -g typescript eslint'

# Customize shell environment  
bash 'echo "alias ll=\"ls -la\"" >> ~/.bashrc'
```

### Environment Persistence
```bash
# First command - installs and configures
bash 'git clone https://github.com/user/repo.git && cd repo && npm install'

# Later command - reuses previous setup
bash 'cd repo && npm test'  # Dependencies still available
```

The implementation successfully transforms the apple container sandbox from an ephemeral isolation mechanism into a long-lived, customizable development environment while maintaining all security boundaries and adding comprehensive management features.