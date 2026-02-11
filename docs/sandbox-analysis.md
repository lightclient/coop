# Sandbox Analysis

Analysis of sandboxing options for Coop, based on "A field guide to sandboxes for AI" (luiscardoso.dev, Jan 2026).

## Threat model

Coop's `BashTool` runs `sh -c <command>` directly on the host. The primary threats are:

- **Prompt injection** tricks the agent into reading `~/.ssh/id_rsa`, `~/.aws/credentials`, or other secrets
- **Data exfiltration** via `curl`, `nc`, or DNS — sending private code or credentials to an attacker
- **Lateral movement** into internal networks or services reachable from the host
- **Accidental damage** — `rm -rf /`, fork bombs, runaway builds consuming disk/memory
- **Supply chain attacks** via malicious packages the agent installs

The primary threat is **not** kernel exploits. It's the AI model executing arbitrary commands with full host access.

## The three-question model (from the blog)

| Question | Coop today | What we need |
|---|---|---|
| What is shared with the host? | Everything — same kernel, FS, network, user | Only the workspace directory |
| What can the code touch? | Anything the process can | Workspace files, pre-installed tools, nothing else |
| What survives between runs? | Everything on host filesystem | Workspace files + installed tooling (committed image) |

## Options evaluated

### MicroVMs (Firecracker)

- **Strongest isolation** (hardware virtualization, guest kernel)
- Requires KVM — doesn't work inside cloud VMs (no nested virt), doesn't work on macOS
- Best for multi-tenant SaaS, not for self-hosted single-user tool

### gVisor

- **Strong isolation** (userspace kernel, ~53-68 host syscalls)
- No KVM needed — works inside VMs, works on any Linux
- Drops in as a Docker runtime (`--runtime=runsc`)
- Has a compatibility matrix — not everything works identically

### Containers (plain Docker/runc)

- **Namespace isolation** — weaker boundary but prevents most practical threats
- Ubiquitous — runs everywhere Docker runs
- On macOS, Docker Desktop already runs a Linux VM, so you get VM isolation automatically

### apple/container

- **VM per container** on macOS via Virtualization.framework
- Apple Silicon only, macOS 26+ for full support
- CLI is Docker-compatible in shape (`container run`, `exec`, `rm`)
- Exactly Firecracker-grade isolation, natively on Mac

### Landlock + seccomp

- **Policy-only** — restricts filesystem paths and syscalls, no boundary change
- Zero overhead, zero dependencies, works on any Linux 5.13+
- Good seatbelt but not sufficient alone for hostile code

## Decision

**Use OCI containers as the universal abstraction.** The isolation strength varies by platform but is strong everywhere:

| Platform | Backend | Isolation |
|---|---|---|
| macOS (Apple Silicon) | apple/container | VM per container |
| Linux with gVisor | Docker + runsc | Userspace kernel |
| Linux without gVisor | Docker + runc | Namespace isolation |
| No container runtime | Direct execution (fallback) | Warning — unsandboxed |

This gives VM-grade isolation on macOS, near-VM isolation with gVisor on Linux, and namespace isolation as baseline — all through the same `run`/`exec`/`rm` CLI interface.

## References

- Blog: "A field guide to sandboxes for AI" — luiscardoso.dev, Jan 2026
- apple/container: https://github.com/apple/container
- apple/containerization: https://github.com/apple/containerization
- gVisor: https://gvisor.dev/docs/
- Firecracker: https://github.com/firecracker-microvm/firecracker
