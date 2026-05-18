//! Capability primitives + coarse profiles for the venv system.
//!
//! Locked in `project_kash_venv.md`. Capabilities are *advisory*
//! at the shell level — they only fire at the boundaries kash
//! itself sees (external command spawn, file open, network
//! builtins, env mutation). Real OS-level enforcement is v2+ work.

use alloc::collections::BTreeSet;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// One fine-grained capability primitive. The variants intentionally
/// stay small and orthogonal so coarse profiles can be expressed as
/// a set of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    /// Read existing files.
    FsRead,
    /// Modify existing files.
    FsWrite,
    /// Mark files executable / honour the `+x` bit on spawn.
    FsExec,
    /// Create new filesystem entries.
    FsCreate,
    /// Delete filesystem entries.
    FsDelete,
    /// Spawn an external command (`execvp` family).
    ExecSpawn,
    /// Open outbound TCP connections.
    NetTcpClient,
    /// Accept inbound TCP connections.
    NetTcpServer,
    /// Send / receive UDP datagrams.
    NetUdp,
    /// Perform DNS resolution.
    NetDns,
    /// Mutate the process environment.
    EnvMutate,
    /// Read environment entries flagged as secrets (a v.7 concept;
    /// kept here so the enum is closed-form).
    EnvReadSecret,
    /// Send signals to other processes.
    SignalSend,
    /// `fork()` / `clone()` — distinct from `ExecSpawn`.
    ProcFork,
    /// Read the realtime clock.
    ClockRealtime,
    /// Set the realtime clock.
    ClockSet,
}

impl Capability {
    /// Canonical kebab-case name (`fs-read`, `net-tcp-client`, …).
    #[inline]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FsRead => "fs-read",
            Self::FsWrite => "fs-write",
            Self::FsExec => "fs-exec",
            Self::FsCreate => "fs-create",
            Self::FsDelete => "fs-delete",
            Self::ExecSpawn => "exec-spawn",
            Self::NetTcpClient => "net-tcp-client",
            Self::NetTcpServer => "net-tcp-server",
            Self::NetUdp => "net-udp",
            Self::NetDns => "net-dns",
            Self::EnvMutate => "env-mutate",
            Self::EnvReadSecret => "env-read-secret",
            Self::SignalSend => "signal-send",
            Self::ProcFork => "proc-fork",
            Self::ClockRealtime => "clock-realtime",
            Self::ClockSet => "clock-set",
        }
    }

    /// Parse the canonical kebab-case name. Unknown names return `None`.
    #[must_use]
    pub fn parse_token(s: &str) -> Option<Self> {
        Some(match s {
            "fs-read" => Self::FsRead,
            "fs-write" => Self::FsWrite,
            "fs-exec" => Self::FsExec,
            "fs-create" => Self::FsCreate,
            "fs-delete" => Self::FsDelete,
            "exec-spawn" => Self::ExecSpawn,
            "net-tcp-client" => Self::NetTcpClient,
            "net-tcp-server" => Self::NetTcpServer,
            "net-udp" => Self::NetUdp,
            "net-dns" => Self::NetDns,
            "env-mutate" => Self::EnvMutate,
            "env-read-secret" => Self::EnvReadSecret,
            "signal-send" => Self::SignalSend,
            "proc-fork" => Self::ProcFork,
            "clock-realtime" => Self::ClockRealtime,
            "clock-set" => Self::ClockSet,
            _ => return None,
        })
    }

    /// Every variant, in canonical order. Used to build the `full`
    /// profile.
    #[must_use]
    pub fn all() -> &'static [Capability] {
        &[
            Self::FsRead,
            Self::FsWrite,
            Self::FsExec,
            Self::FsCreate,
            Self::FsDelete,
            Self::ExecSpawn,
            Self::NetTcpClient,
            Self::NetTcpServer,
            Self::NetUdp,
            Self::NetDns,
            Self::EnvMutate,
            Self::EnvReadSecret,
            Self::SignalSend,
            Self::ProcFork,
            Self::ClockRealtime,
            Self::ClockSet,
        ]
    }
}

/// A coarse named bundle of capabilities. Profiles compose
/// monotonically: `dev` is a superset of `basic`, `network` is a
/// superset of `dev`, etc. — except for `none` (empty) and `full`
/// (everything).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// Deny everything. The starting point for tight venvs.
    None,
    /// Read-only filesystem + restricted exec (via `allow-cmd`) +
    /// secret-env read.
    Basic,
    /// `basic` + write / create / fork / realtime clock.
    Dev,
    /// `dev` + outbound TCP + DNS.
    Network,
    /// `network` + inbound TCP / UDP.
    Server,
    /// Every capability primitive. Useful as a "no-op venv" base
    /// from which to subtract specific capabilities.
    Full,
}

impl Profile {
    /// Parse a profile name.
    #[must_use]
    pub fn parse_token(s: &str) -> Option<Self> {
        Some(match s {
            "none" => Self::None,
            "basic" => Self::Basic,
            "dev" => Self::Dev,
            "network" => Self::Network,
            "server" => Self::Server,
            "full" => Self::Full,
            _ => return None,
        })
    }

    /// The set of capabilities the profile carries. The relationship
    /// `basic ⊆ dev ⊆ network ⊆ server` is enforced here.
    #[must_use]
    pub fn capabilities(self) -> BTreeSet<Capability> {
        let mut s = BTreeSet::new();
        match self {
            Self::None => {}
            Self::Basic => {
                s.insert(Capability::FsRead);
                s.insert(Capability::ExecSpawn);
                s.insert(Capability::EnvReadSecret);
            }
            Self::Dev => {
                s.extend(Profile::Basic.capabilities());
                s.insert(Capability::FsWrite);
                s.insert(Capability::FsCreate);
                s.insert(Capability::ProcFork);
                s.insert(Capability::ClockRealtime);
            }
            Self::Network => {
                s.extend(Profile::Dev.capabilities());
                s.insert(Capability::NetTcpClient);
                s.insert(Capability::NetDns);
            }
            Self::Server => {
                s.extend(Profile::Network.capabilities());
                s.insert(Capability::NetTcpServer);
                s.insert(Capability::NetUdp);
            }
            Self::Full => {
                for c in Capability::all() {
                    s.insert(*c);
                }
            }
        }
        s
    }
}

/// Declarative spec produced by parsing a venv `capabilities { … }`
/// section. Each field is a raw textual fragment so the parser can
/// stay token-level; the evaluator resolves names against
/// [`Profile`] / [`Capability`] when materialising into a runtime
/// [`CapabilitySet`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapabilitySpec {
    /// `profile <NAME>` line, if any.
    pub profile: Option<String>,
    /// `+ <name>` lines — capabilities to grant on top of the
    /// profile.
    pub grants: Vec<String>,
    /// `- <name>` lines — capabilities to revoke from the profile.
    pub revokes: Vec<String>,
    /// `allow-cmd a b c …` — the allow-list for external command
    /// spawns. `None` means the venv didn't constrain spawns;
    /// `Some(empty)` means "no commands at all".
    pub allow_cmd: Option<Vec<String>>,
}

/// Materialised capability set the runtime checks against.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapabilitySet {
    /// Active capabilities.
    pub caps: BTreeSet<Capability>,
    /// Allow-list of external commands. `None` = unconstrained.
    pub allow_cmd: Option<BTreeSet<String>>,
}

impl CapabilitySet {
    /// True iff `cap` is in this set.
    #[inline]
    #[must_use]
    pub fn allows(&self, cap: Capability) -> bool {
        self.caps.contains(&cap)
    }

    /// True iff `cmd` is allowed to spawn (assuming `ExecSpawn` is
    /// granted in the first place). With no `allow-cmd` list, every
    /// name passes.
    #[must_use]
    pub fn cmd_allowed(&self, cmd: &str) -> bool {
        match &self.allow_cmd {
            None => true,
            Some(list) => list.contains(cmd),
        }
    }

    /// Resolve a [`CapabilitySpec`] into a [`CapabilitySet`].
    /// Returns an error if any name in the spec fails to parse.
    pub fn from_spec(spec: &CapabilitySpec) -> Result<Self, String> {
        let mut caps: BTreeSet<Capability> = match spec.profile.as_deref() {
            None => BTreeSet::new(),
            Some(name) => Profile::parse_token(name)
                .ok_or_else(|| alloc::format!("unknown capability profile `{name}`"))?
                .capabilities(),
        };
        for g in &spec.grants {
            let cap = Capability::parse_token(g)
                .ok_or_else(|| alloc::format!("unknown capability `{g}`"))?;
            caps.insert(cap);
        }
        for r in &spec.revokes {
            let cap = Capability::parse_token(r)
                .ok_or_else(|| alloc::format!("unknown capability `{r}`"))?;
            caps.remove(&cap);
        }
        let allow_cmd = spec
            .allow_cmd
            .as_ref()
            .map(|v| v.iter().cloned().collect::<BTreeSet<_>>());
        Ok(Self { caps, allow_cmd })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_basic_includes_fs_read_exec_secret() {
        let caps = Profile::Basic.capabilities();
        assert!(caps.contains(&Capability::FsRead));
        assert!(caps.contains(&Capability::ExecSpawn));
        assert!(caps.contains(&Capability::EnvReadSecret));
        assert!(!caps.contains(&Capability::FsWrite));
    }

    #[test]
    fn profile_dev_is_superset_of_basic() {
        let basic = Profile::Basic.capabilities();
        let dev = Profile::Dev.capabilities();
        for c in basic {
            assert!(dev.contains(&c));
        }
    }

    #[test]
    fn profile_server_is_superset_of_network() {
        let net = Profile::Network.capabilities();
        let srv = Profile::Server.capabilities();
        for c in net {
            assert!(srv.contains(&c));
        }
    }

    #[test]
    fn profile_full_has_every_primitive() {
        let full = Profile::Full.capabilities();
        for c in Capability::all() {
            assert!(full.contains(c), "{c:?} missing from full");
        }
    }

    #[test]
    fn capability_parse_round_trip() {
        for c in Capability::all() {
            assert_eq!(Capability::parse_token(c.as_str()), Some(*c));
        }
    }

    #[test]
    fn spec_resolves_with_grants_and_revokes() {
        let spec = CapabilitySpec {
            profile: Some("basic".into()),
            grants: alloc::vec!["fs-write".into()],
            revokes: alloc::vec!["exec-spawn".into()],
            allow_cmd: None,
        };
        let set = CapabilitySet::from_spec(&spec).unwrap();
        assert!(set.allows(Capability::FsRead));
        assert!(set.allows(Capability::FsWrite));
        assert!(!set.allows(Capability::ExecSpawn));
    }

    #[test]
    fn allow_cmd_filters_external_commands() {
        let set = CapabilitySet {
            allow_cmd: Some(alloc::vec!["ls".to_string()].into_iter().collect()),
            ..CapabilitySet::default()
        };
        assert!(set.cmd_allowed("ls"));
        assert!(!set.cmd_allowed("rm"));
    }

    #[test]
    fn allow_cmd_none_lets_everything_through() {
        let set = CapabilitySet::default();
        assert!(set.cmd_allowed("anything"));
    }
}
