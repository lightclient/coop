# Container Cleanup Policy Update

## Changes Made

### 1. Extended Default Cleanup Window
- **Previous**: 24 hours cleanup window 
- **New**: 30 days (1 month) cleanup window
- **Rationale**: Users need more time to customize and develop their environments

### 2. Trust-Based Protection  
- **New Feature**: Full trust users' containers are protected from auto-cleanup by default
- **Configurable**: Can be disabled via `protect_full_trust = false`
- **Security**: Trusted users get persistent environments while maintaining cleanup for others

### 3. Configuration Options Added

```toml
[sandbox]
cleanup_after_days = 30       # Days before cleanup (default: 30)
protect_full_trust = true     # Protect full trust users (default: true)
```

### 4. Enhanced Container Tracking
- **User Information**: Container registry now tracks user name and trust level
- **Cleanup Logic**: Respects user trust levels when determining cleanup eligibility
- **API Updates**: Sandbox exec functions accept optional user context

## Implementation Details

### Container Registry Updates
```rust
#[derive(Debug, Clone)]
struct ContainerInfo {
    id: String,
    workspace: String,
    last_used: std::time::Instant,
    user_name: Option<String>,      // New: Track container owner
    user_trust: Option<TrustLevel>, // New: Track user trust level
}
```

### Cleanup Policy Structure
```rust
#[derive(Debug, Clone)]
pub struct ContainerCleanupPolicy {
    pub cleanup_after_days: u64,     // Configurable cleanup window
    pub protect_full_trust: bool,    // Trust-based protection
}
```

### Trust-Based Cleanup Logic
```rust
let to_remove: Vec<_> = registry
    .iter()
    .filter(|(_, info)| {
        // Never clean up containers for full trust users if protection is enabled
        if policy.protect_full_trust {
            if let Some(trust) = info.user_trust {
                if trust >= TrustLevel::Full {
                    return false; // Skip cleanup
                }
            }
        }
        info.last_used < cutoff
    })
    .map(|(name, _)| name.clone())
    .collect();
```

## User Experience

### For Full Trust Users
- **Persistent Environment**: Containers never auto-cleaned by default
- **Long-term Customization**: Can build sophisticated development environments
- **Manual Control**: Can still manually remove containers when desired

### For Other Users  
- **Reasonable Window**: 30 days to use and customize containers
- **Automatic Cleanup**: Prevents indefinite resource accumulation
- **Override Options**: Admins can adjust cleanup policies per deployment

### For Administrators
- **Configurable Policy**: Set cleanup windows based on use case
- **Trust-based Control**: Different policies for different user trust levels
- **Resource Management**: Prevent runaway container accumulation while allowing customization

## Configuration Examples

### Default (Recommended)
```toml
[sandbox]
enabled = true
long_lived = true
cleanup_after_days = 30      # 1 month
protect_full_trust = true    # Protect trusted users
```

### Aggressive Cleanup
```toml  
[sandbox]
cleanup_after_days = 7       # 1 week
protect_full_trust = false   # Clean up all users
```

### Development Environment
```toml
[sandbox]
cleanup_after_days = 90      # 3 months  
protect_full_trust = true    # Long-term development
```

## Files Modified

1. **crates/coop-sandbox/src/apple.rs**:
   - Added `ContainerCleanupPolicy` struct
   - Enhanced `ContainerInfo` with user tracking  
   - Implemented trust-based cleanup logic
   - Updated function signatures to accept user context

2. **crates/coop-sandbox/src/lib.rs**:
   - Added `exec_with_user_context()` function
   - Exposed policy-based cleanup functions
   - Updated platform-specific function calls

3. **crates/coop-sandbox/src/linux.rs**:
   - Updated function signature for compatibility
   - Added parameter suppression for unused user context

4. **crates/coop-gateway/src/config.rs**:
   - Added `cleanup_after_days` and `protect_full_trust` fields
   - Added default value functions  
   - Updated configuration tests

5. **crates/coop-gateway/src/sandbox_executor.rs**:
   - Updated to use `exec_with_user_context()` 
   - Passes user information from tool context

6. **crates/coop-sandbox/Cargo.toml**:
   - Added `coop-core` dependency for `TrustLevel`

## Backward Compatibility

- **Configuration**: All existing configurations work unchanged
- **API**: Original `exec()` function maintained for compatibility  
- **Behavior**: More generous defaults improve user experience
- **Migration**: Seamless upgrade without breaking changes

## Testing

- ✅ All existing tests pass
- ✅ New configuration options parse correctly
- ✅ Default values work as expected
- ✅ Trust-based logic functions properly
- ✅ API compatibility maintained

## Benefits

1. **Better User Experience**: Full trust users get persistent environments
2. **Resource Management**: Automatic cleanup prevents resource leaks
3. **Flexibility**: Configurable policies for different deployment needs
4. **Security**: Trust levels determine cleanup behavior appropriately  
5. **Scalability**: System can handle long-term user customization