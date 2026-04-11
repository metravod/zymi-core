use std::collections::HashSet;

use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct PolicyConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Commands always allowed (glob patterns). Matched first.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Commands always denied (glob patterns). Matched second.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Commands that require human approval even if allow matches (glob patterns).
    #[serde(default)]
    pub require_approval: Vec<String>,
}


#[derive(Debug, Clone, PartialEq)]
pub enum PolicyDecision {
    /// Command is allowed to execute
    Allow,
    /// Command requires human approval before execution
    RequireApproval,
    /// Command is denied
    Deny(String),
}

pub struct PolicyEngine {
    config: PolicyConfig,
    allow_patterns: Vec<Regex>,
    deny_patterns: Vec<Regex>,
    approval_patterns: Vec<Regex>,
}

impl PolicyEngine {
    pub fn new(config: PolicyConfig) -> Self {
        let allow_patterns = config
            .allow
            .iter()
            .filter_map(|p| glob_to_regex(p))
            .collect();
        let deny_patterns = config
            .deny
            .iter()
            .filter_map(|p| glob_to_regex(p))
            .collect();
        let approval_patterns = config
            .require_approval
            .iter()
            .filter_map(|p| glob_to_regex(p))
            .collect();

        Self {
            config,
            allow_patterns,
            deny_patterns,
            approval_patterns,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Evaluate a command against the policy rules.
    ///
    /// The command is decomposed into fragments (sub-commands, subshell bodies,
    /// `sh -c` arguments) and each fragment is checked independently. The most
    /// restrictive decision wins: Deny > RequireApproval > Allow.
    pub fn evaluate(&self, command: &str) -> PolicyDecision {
        if !self.config.enabled {
            return PolicyDecision::RequireApproval;
        }

        let normalized = command.trim();

        // Decompose into all checkable fragments
        let fragments = collect_all_fragments(normalized, 0);

        let mut worst = PolicyDecision::Allow;
        let mut any_evaluated = false;

        for fragment in &fragments {
            let trimmed = fragment.trim();
            if trimmed.is_empty() {
                continue;
            }

            let decision = self.evaluate_single(trimmed);
            worst = merge_decisions(worst, decision);
            any_evaluated = true;

            // Early exit on Deny
            if matches!(worst, PolicyDecision::Deny(_)) {
                return worst;
            }
        }

        // Variable expansion check on the whole original command
        if has_variable_expansion(normalized) {
            worst = merge_decisions(worst, PolicyDecision::RequireApproval);
        }

        // Suspicious base commands (eval, source) in any fragment
        for fragment in &fragments {
            let norm = normalize_command(fragment.trim());
            if SUSPICIOUS_COMMANDS.contains(&norm.base.as_str()) {
                worst = merge_decisions(worst, PolicyDecision::RequireApproval);
                break;
            }
        }

        if !any_evaluated {
            return PolicyDecision::RequireApproval;
        }

        worst
    }

    /// Evaluate a single simple command (no compound operators).
    fn evaluate_single(&self, command: &str) -> PolicyDecision {
        // Check deny list first (highest priority)
        for (i, re) in self.deny_patterns.iter().enumerate() {
            if re.is_match(command) {
                return PolicyDecision::Deny(format!(
                    "Command matches deny rule: {}",
                    self.config.deny[i]
                ));
            }
        }

        // Check dangerous patterns (always denied regardless of config)
        if let Some(reason) = check_dangerous(command) {
            return PolicyDecision::Deny(reason);
        }

        // Check require_approval list
        for re in &self.approval_patterns {
            if re.is_match(command) {
                return PolicyDecision::RequireApproval;
            }
        }

        // Check allow list
        for re in &self.allow_patterns {
            if re.is_match(command) {
                return PolicyDecision::Allow;
            }
        }

        // Default: require approval (safe default)
        PolicyDecision::RequireApproval
    }
}

// ======================================================================
// Command decomposition — split compound commands, extract subshells
// ======================================================================

/// Parser state for quote/escape-aware command splitting.
#[derive(Clone, Copy, PartialEq)]
enum ParseState {
    Normal,
    SingleQuote,
    DoubleQuote,
    Escape,
    DoubleQuoteEscape,
    /// Inside `(...)` group — track depth to avoid splitting inside subshells.
    Paren(u32),
}

/// Split a compound shell command on `;`, `&&`, `||`, `|`, `&`
/// while respecting quotes, escapes, and parenthesized groups.
fn split_compound_commands(command: &str) -> Vec<String> {
    let chars: Vec<char> = command.chars().collect();
    let mut fragments: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut i = 0;
    let mut state = ParseState::Normal;

    while i < chars.len() {
        match state {
            ParseState::Normal => match chars[i] {
                '\\' => {
                    current.push(chars[i]);
                    state = ParseState::Escape;
                }
                '\'' => {
                    current.push(chars[i]);
                    state = ParseState::SingleQuote;
                }
                '"' => {
                    current.push(chars[i]);
                    state = ParseState::DoubleQuote;
                }
                '(' => {
                    current.push(chars[i]);
                    state = ParseState::Paren(1);
                }
                ';' => {
                    push_fragment(&mut fragments, &current);
                    current.clear();
                }
                '&' => {
                    if i + 1 < chars.len() && chars[i + 1] == '&' {
                        push_fragment(&mut fragments, &current);
                        current.clear();
                        i += 1;
                    } else {
                        // Background `&` — still a separator for safety
                        push_fragment(&mut fragments, &current);
                        current.clear();
                    }
                }
                '|' => {
                    if i + 1 < chars.len() && chars[i + 1] == '|' {
                        push_fragment(&mut fragments, &current);
                        current.clear();
                        i += 1;
                    } else {
                        // Pipe — separator
                        push_fragment(&mut fragments, &current);
                        current.clear();
                    }
                }
                _ => current.push(chars[i]),
            },
            ParseState::SingleQuote => {
                current.push(chars[i]);
                if chars[i] == '\'' {
                    state = ParseState::Normal;
                }
            }
            ParseState::DoubleQuote => {
                current.push(chars[i]);
                if chars[i] == '\\' {
                    state = ParseState::DoubleQuoteEscape;
                } else if chars[i] == '"' {
                    state = ParseState::Normal;
                }
            }
            ParseState::DoubleQuoteEscape => {
                current.push(chars[i]);
                state = ParseState::DoubleQuote;
            }
            ParseState::Escape => {
                current.push(chars[i]);
                state = ParseState::Normal;
            }
            ParseState::Paren(depth) => {
                current.push(chars[i]);
                if chars[i] == '(' {
                    state = ParseState::Paren(depth + 1);
                } else if chars[i] == ')' {
                    if depth == 1 {
                        state = ParseState::Normal;
                    } else {
                        state = ParseState::Paren(depth - 1);
                    }
                }
            }
        }
        i += 1;
    }
    push_fragment(&mut fragments, &current);
    fragments
}

fn push_fragment(fragments: &mut Vec<String>, s: &str) {
    let trimmed = s.trim();
    if !trimmed.is_empty() {
        fragments.push(trimmed.to_string());
    }
}

/// Extract contents of `$(...)` and `` `...` `` subshells (non-recursive).
/// The caller handles recursion via `collect_all_fragments`.
fn extract_subshells(command: &str) -> Vec<String> {
    let mut results = Vec::new();
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;
    let mut in_single_quote = false;

    while i < chars.len() {
        if chars[i] == '\'' && !in_single_quote {
            in_single_quote = true;
            i += 1;
            continue;
        }
        if chars[i] == '\'' && in_single_quote {
            in_single_quote = false;
            i += 1;
            continue;
        }
        if in_single_quote {
            i += 1;
            continue;
        }

        // $(...) — track parenthesis depth
        if chars[i] == '$' && i + 1 < chars.len() && chars[i + 1] == '(' {
            let start = i + 2;
            let mut depth = 1u32;
            let mut j = start;
            while j < chars.len() && depth > 0 {
                if chars[j] == '(' {
                    depth += 1;
                } else if chars[j] == ')' {
                    depth -= 1;
                }
                if depth > 0 {
                    j += 1;
                }
            }
            if depth == 0 {
                let body: String = chars[start..j].iter().collect();
                results.push(body);
                i = j + 1;
                continue;
            }
        }

        // Backticks
        if chars[i] == '`' {
            let start = i + 1;
            if let Some(end_offset) = chars[start..].iter().position(|&c| c == '`') {
                let body: String = chars[start..start + end_offset].iter().collect();
                results.push(body);
                i = start + end_offset + 1;
                continue;
            }
        }

        i += 1;
    }

    results
}

/// Cached regex for shell -c wrapper detection.
static SHELL_C_REGEX: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:bash|sh|zsh|dash)\s+-c\s+(.+)").unwrap()
});

/// Extract the argument of `sh -c "..."` / `bash -c '...'` wrappers.
fn extract_shell_c_arg(command: &str) -> Option<String> {
    let caps = SHELL_C_REGEX.captures(command)?;
    let arg = caps.get(1)?.as_str().trim();
    // Strip one layer of outer quotes
    let stripped = if (arg.starts_with('"') && arg.ends_with('"'))
        || (arg.starts_with('\'') && arg.ends_with('\''))
    {
        &arg[1..arg.len() - 1]
    } else {
        arg
    };
    if stripped.is_empty() {
        None
    } else {
        Some(stripped.to_string())
    }
}

/// Maximum recursion depth for fragment collection.
const MAX_FRAGMENT_DEPTH: u8 = 10;

/// Decompose a command into all checkable fragments:
/// sub-commands, subshell bodies, and `sh -c` arguments, recursively.
/// Uses a work queue instead of recursion to avoid stack overflow and deduplicates
/// fragments to prevent exponential blowup on nested commands.
fn collect_all_fragments(command: &str, _depth: u8) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    let mut queue: Vec<(String, u8)> = vec![(command.to_string(), 0)];

    while let Some((cmd, depth)) = queue.pop() {
        if depth >= MAX_FRAGMENT_DEPTH {
            if seen.insert(cmd.clone()) {
                result.push(cmd);
            }
            continue;
        }

        let sub_commands = split_compound_commands(&cmd);

        for sub_cmd in sub_commands {
            if seen.insert(sub_cmd.clone()) {
                result.push(sub_cmd.clone());

                for body in extract_subshells(&sub_cmd) {
                    if !seen.contains(&body) {
                        queue.push((body, depth + 1));
                    }
                }

                if let Some(inner_cmd) = extract_shell_c_arg(&sub_cmd) {
                    if !seen.contains(&inner_cmd) {
                        queue.push((inner_cmd, depth + 1));
                    }
                }
            }
        }
    }

    result
}

// ======================================================================
// Dangerous command detection (enhanced with flag normalization)
// ======================================================================

/// A parsed simple command with normalized flags.
struct NormalizedCommand {
    base: String,
    flags: HashSet<char>,
    args: Vec<String>,
}

/// Known long-flag → short-flag mappings for dangerous commands.
fn long_flag_map(base: &str) -> &'static [(&'static str, char)] {
    match base {
        "rm" => &[("--recursive", 'r'), ("--force", 'f')],
        "chmod" | "chown" => &[("--recursive", 'R')],
        _ => &[],
    }
}

/// Parse a simple command (no pipes/semicolons) into base + flags + args.
/// Uses case-insensitive comparison where possible to avoid allocations.
fn normalize_command(command: &str) -> NormalizedCommand {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    if tokens.is_empty() {
        return NormalizedCommand {
            base: String::new(),
            flags: HashSet::new(),
            args: Vec::new(),
        };
    }

    let base = tokens[0].to_ascii_lowercase();
    let flag_map = long_flag_map(&base);
    let mut flags = HashSet::new();
    let mut args = Vec::new();

    for &token in &tokens[1..] {
        if token.starts_with("--") {
            // Try long flag mapping with case-insensitive compare (no allocation)
            if let Some(&(_, short)) = flag_map.iter().find(|&&(long, _)| long.eq_ignore_ascii_case(token)) {
                flags.insert(short);
            } else {
                args.push(token.to_ascii_lowercase());
            }
        } else if token.starts_with('-') && token.len() > 1 {
            // Short flags: -rf → {'r', 'f'} — lowercase each char directly
            for ch in token[1..].chars() {
                flags.insert(ch.to_ascii_lowercase());
            }
        } else {
            args.push(token.to_ascii_lowercase());
        }
    }

    NormalizedCommand { base, flags, args }
}

/// Commands that are suspicious by nature and should always require approval.
const SUSPICIOUS_COMMANDS: &[&str] = &["eval", "source"];

/// Check for inherently dangerous commands that should always be denied
/// regardless of policy configuration.
fn check_dangerous(command: &str) -> Option<String> {
    let lower = command.to_lowercase();
    let norm = normalize_command(command);

    // Fork bomb (raw pattern — not parseable as a normal command)
    if lower.contains(":(){:|:&};:") {
        return Some("Blocked: Fork bomb".to_string());
    }

    // mkfs — filesystem formatting
    if lower.contains("mkfs.") {
        return Some("Blocked: Filesystem formatting".to_string());
    }

    // > /dev/sda — disk overwrite via redirect
    if lower.contains("> /dev/sda") {
        return Some("Blocked: Disk overwrite".to_string());
    }

    // rm -rf / (normalized: base=rm, flags⊇{r,f}, args contains "/" or "/*")
    if norm.base == "rm"
        && norm.flags.contains(&'r')
        && norm.flags.contains(&'f')
        && norm.args.iter().any(|a| a == "/" || a == "/*")
    {
        return Some("Blocked: Recursive deletion of root filesystem".to_string());
    }

    // dd if=/dev/zero of=/dev/sd* or if=/dev/random of=/dev/sd*
    if norm.base == "dd" {
        let has_dangerous_if = norm.args.iter().any(|a| {
            a.starts_with("if=/dev/zero") || a.starts_with("if=/dev/random")
        });
        let has_dangerous_of = norm.args.iter().any(|a| a.starts_with("of=/dev/sd"));
        if has_dangerous_if && has_dangerous_of {
            return Some("Blocked: Disk overwrite".to_string());
        }
    }

    // chmod -R 777 / (normalized: base=chmod, flags⊇{R}, args contains "777" and "/")
    if norm.base == "chmod"
        && (norm.flags.contains(&'r') || norm.flags.contains(&'R'))
        && norm.args.iter().any(|a| a == "777")
        && norm.args.iter().any(|a| a == "/")
    {
        return Some("Blocked: Recursive permission change on root".to_string());
    }

    // chown -R (any recursive chown on /)
    if norm.base == "chown"
        && (norm.flags.contains(&'r') || norm.flags.contains(&'R'))
        && norm.args.iter().any(|a| a == "/")
    {
        return Some("Blocked: Recursive ownership change on root".to_string());
    }

    // Docker privilege escalation — container escape to host root
    if check_docker_escape(&lower) {
        return Some(
            "Blocked: Docker privilege escalation (container escape to host root)".into(),
        );
    }

    // nsenter into PID 1 — direct host namespace escape (not Docker-specific)
    if lower.contains("nsenter") && lower.contains("-t 1") {
        return Some("Blocked: nsenter into PID 1 (host namespace escape)".into());
    }

    None
}

/// Check if a command contains `$VAR` or `${VAR}` outside single quotes.
/// Returns true if variable expansion is detected (should trigger RequireApproval).
/// Zero-allocation: iterates over bytes directly (shell syntax is ASCII).
fn has_variable_expansion(command: &str) -> bool {
    let bytes = command.as_bytes();
    let mut in_single_quote = false;
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'\'' {
            in_single_quote = !in_single_quote;
            i += 1;
            continue;
        }
        if !in_single_quote && bytes[i] == b'$' {
            if let Some(&next) = bytes.get(i + 1) {
                // $(...) is a subshell — handled separately
                if next == b'(' {
                    i += 2;
                    continue;
                }
                // $VAR or ${VAR} — opaque at static analysis time
                if next.is_ascii_alphanumeric() || next == b'{' || next == b'_' {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

/// Detect Docker commands that grant root-equivalent access to the host.
///
/// The docker group gives broad API access (needed for container management).
/// These checks block specific escalation vectors that turn Docker access
/// into unrestricted host root access.
///
/// **Limitation:** this operates on the raw command string before `sh -c`
/// interprets it. Shell variable expansion, subshells, and complex quoting
/// can theoretically bypass these checks. Defense-in-depth: the default
/// policy is `require_approval`, so a human sees the command in Telegram
/// before execution. These hardcoded checks catch what LLMs typically generate.
fn check_docker_escape(lower: &str) -> bool {
    if !lower.contains("docker") {
        return false;
    }

    // --privileged: full host device access + all capabilities
    if lower.contains("--privileged") {
        return true;
    }

    // --device: raw host device access (e.g., /dev/sda1)
    if lower.contains("--device") {
        return true;
    }

    // --pid=host: host PID namespace (can ptrace any process)
    if lower.contains("--pid=host") || lower.contains("--pid host") {
        return true;
    }

    // --cap-add=all or --cap-add=sys_admin: near-root capabilities
    if lower.contains("--cap-add=all") || lower.contains("--cap-add all") {
        return true;
    }
    if lower.contains("--cap-add=sys_admin") || lower.contains("--cap-add sys_admin") {
        return true;
    }

    // --security-opt apparmor:unconfined — disables AppArmor
    if lower.contains("apparmor:unconfined") || lower.contains("apparmor=unconfined") {
        return true;
    }

    // Bind mounts from sensitive host paths
    if has_sensitive_mount(lower) {
        return true;
    }

    false
}

/// Host paths that must never be bind-mounted into a container.
/// Mounting these gives the container write access to critical host config,
/// credentials, or the Docker socket itself (DinD escape).
const SENSITIVE_MOUNT_SOURCES: &[&str] = &[
    "/etc",                    // shadow, sudoers, passwd, fstab, ssh configs
    "/root",                   // root SSH keys, shell history
    "/var/run/docker.sock",    // Docker-in-Docker escape
    "/run/docker.sock",        // alternative socket path
    "/proc",                   // host process info
    "/sys",                    // kernel interfaces
    "/dev",                    // host devices
    "/boot",                   // bootloader, initramfs
];

/// Extract bind-mount source paths from -v/--volume/--mount flags,
/// normalize them (strip quotes), and check against the sensitive blocklist.
fn has_sensitive_mount(lower: &str) -> bool {
    let sources = extract_mount_sources(lower);
    for raw in &sources {
        let path = normalize_path(raw);
        if is_sensitive_path(&path) {
            return true;
        }
    }
    false
}

/// Strip surrounding quotes and whitespace from a path extracted from a command.
fn normalize_path(raw: &str) -> String {
    raw.trim()
        .trim_matches(|c: char| c == '"' || c == '\'')
        .trim()
        .to_string()
}

/// Check if a path is exactly root `/` or falls under a sensitive prefix.
fn is_sensitive_path(path: &str) -> bool {
    // Exact root mount
    if path == "/" {
        return true;
    }

    for sensitive in SENSITIVE_MOUNT_SOURCES {
        // Exact match: -v /etc:/...
        if path == *sensitive {
            return true;
        }
        // Subpath: -v /etc/shadow:/...
        if path.starts_with(sensitive) && path.as_bytes().get(sensitive.len()) == Some(&b'/') {
            return true;
        }
    }

    false
}

/// Extract host source paths from docker volume/mount flags.
///
/// Handles formats:
/// - `-v HOST:CONTAINER`, `-v=HOST:CONTAINER`
/// - `--volume HOST:CONTAINER`, `--volume=HOST:CONTAINER`
/// - `--mount type=bind,source=HOST,target=CONTAINER`
/// - `--mount type=bind,src=HOST,target=CONTAINER`
fn extract_mount_sources(cmd: &str) -> Vec<String> {
    let mut sources = Vec::new();

    // -v / --volume: extract HOST from HOST:CONTAINER
    for flag in ["-v ", "-v=", "--volume ", "--volume="] {
        let mut search_from = 0;
        while let Some(offset) = cmd[search_from..].find(flag) {
            let arg_start = search_from + offset + flag.len();
            if arg_start >= cmd.len() {
                break;
            }
            // Take the next whitespace-delimited token
            let rest = cmd[arg_start..].trim_start();
            let token = rest.split_whitespace().next().unwrap_or("");
            // HOST:CONTAINER — split on first colon not preceded by windows drive letter
            if let Some(colon_pos) = token.find(':') {
                let host_part = &token[..colon_pos];
                if !host_part.is_empty() {
                    sources.push(host_part.to_string());
                }
            }
            search_from = arg_start + 1;
        }
    }

    // --mount: extract source= or src= value
    for flag in ["--mount ", "--mount="] {
        let mut search_from = 0;
        while let Some(offset) = cmd[search_from..].find(flag) {
            let arg_start = search_from + offset + flag.len();
            if arg_start >= cmd.len() {
                break;
            }
            let rest = cmd[arg_start..].trim_start();
            let token = rest.split_whitespace().next().unwrap_or("");
            // Parse comma-separated key=value pairs
            for part in token.split(',') {
                for key in ["source=", "src="] {
                    if let Some(stripped) = part.strip_prefix(key) {
                        if !stripped.is_empty() {
                            sources.push(stripped.to_string());
                        }
                    }
                }
            }
            search_from = arg_start + 1;
        }
    }

    sources
}

/// Convert a simple glob pattern to an anchored regex.
/// Supports `*` (any characters) and `?` (single character).
fn glob_to_regex(pattern: &str) -> Option<Regex> {
    let mut regex_str = String::from("(?i)^");
    for ch in pattern.chars() {
        match ch {
            '*' => regex_str.push_str(".*"),
            '?' => regex_str.push('.'),
            '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\' => {
                regex_str.push('\\');
                regex_str.push(ch);
            }
            _ => regex_str.push(ch),
        }
    }
    regex_str.push('$');

    match Regex::new(&regex_str) {
        Ok(re) => Some(re),
        Err(e) => {
            log::warn!("Invalid policy pattern '{}': {e}", pattern);
            None
        }
    }
}

/// Merge two policy decisions: Deny > RequireApproval > Allow.
fn merge_decisions(current: PolicyDecision, new: PolicyDecision) -> PolicyDecision {
    match (&current, &new) {
        (PolicyDecision::Deny(_), _) => current,
        (_, PolicyDecision::Deny(_)) => new,
        (PolicyDecision::RequireApproval, _) => current,
        (_, PolicyDecision::RequireApproval) => new,
        _ => current,
    }
}

pub fn load_policy(path: &std::path::Path) -> PolicyConfig {
    match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
            log::warn!("Failed to parse policy config: {e}, policy disabled");
            PolicyConfig::default()
        }),
        Err(_) => PolicyConfig::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(allow: &[&str], deny: &[&str], approval: &[&str]) -> PolicyEngine {
        PolicyEngine::new(PolicyConfig {
            enabled: true,
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            require_approval: approval.iter().map(|s| s.to_string()).collect(),
        })
    }

    #[test]
    fn allow_matches() {
        let e = engine(&["echo *", "ls *", "cat *"], &[], &[]);
        assert_eq!(e.evaluate("echo hello"), PolicyDecision::Allow);
        assert_eq!(e.evaluate("ls -la /tmp"), PolicyDecision::Allow);
    }

    #[test]
    fn deny_overrides_allow() {
        let e = engine(&["rm *"], &["rm -rf *"], &[]);
        assert_eq!(
            e.evaluate("rm file.txt"),
            PolicyDecision::Allow
        );
        match e.evaluate("rm -rf /tmp/foo") {
            PolicyDecision::Deny(_) => {}
            other => panic!("Expected Deny, got {:?}", other),
        }
    }

    #[test]
    fn require_approval_overrides_allow() {
        let e = engine(
            &["systemctl *"],
            &[],
            &["systemctl restart *", "systemctl stop *"],
        );
        assert_eq!(e.evaluate("systemctl status nginx"), PolicyDecision::Allow);
        assert_eq!(
            e.evaluate("systemctl restart nginx"),
            PolicyDecision::RequireApproval
        );
    }

    #[test]
    fn default_is_require_approval() {
        let e = engine(&["echo *"], &[], &[]);
        assert_eq!(e.evaluate("unknown-command"), PolicyDecision::RequireApproval);
    }

    #[test]
    fn disabled_engine_always_requires_approval() {
        let e = PolicyEngine::new(PolicyConfig::default());
        assert_eq!(e.evaluate("echo hello"), PolicyDecision::RequireApproval);
    }

    #[test]
    fn dangerous_commands_always_denied() {
        let e = engine(&["*"], &[], &[]); // Allow everything
        match e.evaluate("rm -rf /") {
            PolicyDecision::Deny(_) => {}
            other => panic!("Expected Deny for rm -rf /, got {:?}", other),
        }
        match e.evaluate("dd if=/dev/zero of=/dev/sda") {
            PolicyDecision::Deny(_) => {}
            other => panic!("Expected Deny for dd, got {:?}", other),
        }
    }

    #[test]
    fn glob_patterns() {
        let e = engine(&["docker ps *", "docker logs *"], &["docker rm *"], &[]);
        assert_eq!(e.evaluate("docker ps -a"), PolicyDecision::Allow);
        match e.evaluate("docker rm my-container") {
            PolicyDecision::Deny(_) => {}
            other => panic!("Expected Deny, got {:?}", other),
        }
    }

    // ── Docker: privilege escalation flags ─────────────────────────

    #[test]
    fn docker_privileged_denied() {
        let e = engine(&["*"], &[], &[]);
        for cmd in [
            "docker run --privileged ubuntu",
            "docker run -it --privileged nginx sh",
            "docker create --privileged alpine",
        ] {
            match e.evaluate(cmd) {
                PolicyDecision::Deny(msg) => assert!(msg.contains("Docker"), "cmd: {cmd}"),
                other => panic!("Expected Deny for '{cmd}', got {:?}", other),
            }
        }
    }

    #[test]
    fn docker_device_denied() {
        let e = engine(&["*"], &[], &[]);
        for cmd in [
            "docker run --device /dev/sda1 ubuntu",
            "docker run --device=/dev/mem ubuntu",
            "docker run --device /dev/kvm ubuntu",
        ] {
            match e.evaluate(cmd) {
                PolicyDecision::Deny(_) => {}
                other => panic!("Expected Deny for '{cmd}', got {:?}", other),
            }
        }
    }

    #[test]
    fn docker_namespace_escape_denied() {
        let e = engine(&["*"], &[], &[]);
        for cmd in [
            "docker run --pid=host ubuntu",
            "docker run --cap-add=ALL ubuntu",
            "docker run --cap-add=SYS_ADMIN ubuntu",
            "docker run --security-opt apparmor:unconfined ubuntu",
            "nsenter -t 1 -m -u -i -n -p sh",
        ] {
            match e.evaluate(cmd) {
                PolicyDecision::Deny(_) => {}
                other => panic!("Expected Deny for '{cmd}', got {:?}", other),
            }
        }
    }

    // ── Docker: sensitive bind mounts ───────────────────────────────

    #[test]
    fn docker_root_mount_denied() {
        let e = engine(&["*"], &[], &[]);
        for cmd in [
            "docker run -v /:/host ubuntu",
            "docker run --volume=/:/mnt alpine sh",
            "docker run --mount type=bind,source=/,target=/host ubuntu",
            "docker run --mount src=/,target=/mnt alpine",
        ] {
            match e.evaluate(cmd) {
                PolicyDecision::Deny(_) => {}
                other => panic!("Expected Deny for '{cmd}', got {:?}", other),
            }
        }
    }

    #[test]
    fn docker_etc_mount_denied() {
        let e = engine(&["*"], &[], &[]);
        for cmd in [
            "docker run -v /etc:/host_etc ubuntu",
            "docker run -v /etc/shadow:/shadow ubuntu",
            "docker run --volume /etc/sudoers.d:/sudoers ubuntu",
            "docker run --mount type=bind,source=/etc,target=/cfg ubuntu",
        ] {
            match e.evaluate(cmd) {
                PolicyDecision::Deny(_) => {}
                other => panic!("Expected Deny for '{cmd}', got {:?}", other),
            }
        }
    }

    #[test]
    fn docker_root_home_mount_denied() {
        let e = engine(&["*"], &[], &[]);
        for cmd in [
            "docker run -v /root:/host_root ubuntu",
            "docker run -v /root/.ssh:/ssh ubuntu",
        ] {
            match e.evaluate(cmd) {
                PolicyDecision::Deny(_) => {}
                other => panic!("Expected Deny for '{cmd}', got {:?}", other),
            }
        }
    }

    #[test]
    fn docker_socket_mount_denied() {
        let e = engine(&["*"], &[], &[]);
        for cmd in [
            "docker run -v /var/run/docker.sock:/var/run/docker.sock ubuntu",
            "docker run -v /run/docker.sock:/var/run/docker.sock ubuntu",
            "docker run --volume=/var/run/docker.sock:/sock ubuntu",
        ] {
            match e.evaluate(cmd) {
                PolicyDecision::Deny(_) => {}
                other => panic!("Expected Deny for '{cmd}', got {:?}", other),
            }
        }
    }

    #[test]
    fn docker_proc_sys_dev_mount_denied() {
        let e = engine(&["*"], &[], &[]);
        for cmd in [
            "docker run -v /proc:/host_proc ubuntu",
            "docker run -v /sys:/host_sys ubuntu",
            "docker run -v /dev:/dev ubuntu",
            "docker run -v /boot:/boot ubuntu",
        ] {
            match e.evaluate(cmd) {
                PolicyDecision::Deny(_) => {}
                other => panic!("Expected Deny for '{cmd}', got {:?}", other),
            }
        }
    }

    #[test]
    fn docker_quoted_mount_denied() {
        let e = engine(&["*"], &[], &[]);
        // LLM may wrap paths in quotes — normalization should strip them
        for cmd in [
            r#"docker run -v "/etc":/host ubuntu"#,
            r#"docker run -v '/root':/host ubuntu"#,
            r#"docker run --mount type=bind,source="/",target=/host ubuntu"#,
        ] {
            match e.evaluate(cmd) {
                PolicyDecision::Deny(_) => {}
                other => panic!("Expected Deny for '{cmd}', got {:?}", other),
            }
        }
    }

    // ── Docker: safe operations ─────────────────────────────────────

    #[test]
    fn docker_safe_mounts_allowed() {
        let e = engine(&["*"], &[], &[]);
        for cmd in [
            "docker run -v /var/log:/logs ubuntu",
            "docker run -v /home/user/data:/data alpine",
            "docker run -v /opt/zymi/memory:/data alpine",
            "docker run -v /tmp/test:/tmp/test ubuntu",
            "docker run --mount type=bind,source=/opt/app,target=/app ubuntu",
        ] {
            assert_eq!(e.evaluate(cmd), PolicyDecision::Allow, "cmd: {cmd}");
        }
    }

    #[test]
    fn docker_normal_commands_allowed() {
        let e = engine(&["*"], &[], &[]);
        for cmd in [
            "docker ps -a",
            "docker run -d nginx",
            "docker run -p 8080:80 nginx",
            "docker stop my-container",
            "docker logs -f my-container",
            "docker exec my-container ls",
            "docker build -t myapp .",
            "docker compose up -d",
            "docker inspect my-container",
            "docker stats --no-stream",
        ] {
            assert_eq!(e.evaluate(cmd), PolicyDecision::Allow, "cmd: {cmd}");
        }
    }

    // ── Docker: mount path extraction unit tests ────────────────────

    #[test]
    fn extract_mount_sources_volume_flags() {
        let sources = extract_mount_sources("docker run -v /host:/container -v /tmp:/tmp ubuntu");
        assert_eq!(sources, vec!["/host", "/tmp"]);
    }

    #[test]
    fn extract_mount_sources_volume_equals() {
        let sources = extract_mount_sources("docker run --volume=/data:/data ubuntu");
        assert_eq!(sources, vec!["/data"]);
    }

    #[test]
    fn extract_mount_sources_mount_flag() {
        let sources = extract_mount_sources(
            "docker run --mount type=bind,source=/opt/app,target=/app ubuntu",
        );
        assert_eq!(sources, vec!["/opt/app"]);
    }

    #[test]
    fn extract_mount_sources_mount_src() {
        let sources =
            extract_mount_sources("docker run --mount type=bind,src=/data,target=/data ubuntu");
        assert_eq!(sources, vec!["/data"]);
    }

    #[test]
    fn sensitive_path_checks() {
        // Exact matches
        assert!(is_sensitive_path("/"));
        assert!(is_sensitive_path("/etc"));
        assert!(is_sensitive_path("/root"));
        assert!(is_sensitive_path("/var/run/docker.sock"));

        // Subpaths
        assert!(is_sensitive_path("/etc/shadow"));
        assert!(is_sensitive_path("/root/.ssh/authorized_keys"));
        assert!(is_sensitive_path("/proc/1/status"));

        // Safe paths
        assert!(!is_sensitive_path("/var/log"));
        assert!(!is_sensitive_path("/home/user"));
        assert!(!is_sensitive_path("/opt/zymi"));
        assert!(!is_sensitive_path("/tmp"));
        // Must not match prefix-substring without path separator
        assert!(!is_sensitive_path("/etcetera"));
        assert!(!is_sensitive_path("/rooted"));
    }

    // ── Command decomposition ──────────────────────────────────────

    #[test]
    fn split_on_semicolon() {
        let frags = split_compound_commands("echo hello; echo world");
        assert_eq!(frags, vec!["echo hello", "echo world"]);
    }

    #[test]
    fn split_on_and_or_pipe() {
        let frags = split_compound_commands("a && b || c | d");
        assert_eq!(frags, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn split_respects_single_quotes() {
        let frags = split_compound_commands("echo 'hello; world'");
        assert_eq!(frags, vec!["echo 'hello; world'"]);
    }

    #[test]
    fn split_respects_double_quotes() {
        let frags = split_compound_commands(r#"echo "hello && world""#);
        assert_eq!(frags, vec![r#"echo "hello && world""#]);
    }

    #[test]
    fn split_respects_parens() {
        let frags = split_compound_commands("(a; b) && c");
        assert_eq!(frags, vec!["(a; b)", "c"]);
    }

    #[test]
    fn extract_dollar_paren_subshell() {
        let subs = extract_subshells("echo $(rm -rf /)");
        assert_eq!(subs, vec!["rm -rf /"]);
    }

    #[test]
    fn extract_backtick_subshell() {
        let subs = extract_subshells("echo `rm -rf /`");
        assert_eq!(subs, vec!["rm -rf /"]);
    }

    #[test]
    fn extract_nested_subshell() {
        let subs = extract_subshells("echo $(echo $(inner))");
        assert_eq!(subs, vec!["echo $(inner)"]);
    }

    #[test]
    fn extract_subshell_in_single_quotes_ignored() {
        let subs = extract_subshells("echo '$(rm -rf /)'");
        assert!(subs.is_empty());
    }

    #[test]
    fn extract_shell_c_double_quotes() {
        let arg = extract_shell_c_arg(r#"bash -c "rm -rf /""#);
        assert_eq!(arg, Some("rm -rf /".to_string()));
    }

    #[test]
    fn extract_shell_c_single_quotes() {
        let arg = extract_shell_c_arg("sh -c 'echo hello'");
        assert_eq!(arg, Some("echo hello".to_string()));
    }

    #[test]
    fn extract_shell_c_no_quotes() {
        let arg = extract_shell_c_arg("bash -c ls");
        assert_eq!(arg, Some("ls".to_string()));
    }

    // ── Flag normalization ─────────────────────────────────────────

    #[test]
    fn normalize_rm_combined_flags() {
        let n = normalize_command("rm -rf /");
        assert_eq!(n.base, "rm");
        assert!(n.flags.contains(&'r'));
        assert!(n.flags.contains(&'f'));
        assert!(n.args.contains(&"/".to_string()));
    }

    #[test]
    fn normalize_rm_split_flags() {
        let n = normalize_command("rm -r -f /");
        assert!(n.flags.contains(&'r'));
        assert!(n.flags.contains(&'f'));
    }

    #[test]
    fn normalize_rm_long_flags() {
        let n = normalize_command("rm --recursive --force /");
        assert!(n.flags.contains(&'r'));
        assert!(n.flags.contains(&'f'));
    }

    // ── Compound command bypass scenarios ───────────────────────────

    #[test]
    fn semicolon_split_catches_dangerous() {
        let e = engine(&["echo *"], &[], &[]);
        match e.evaluate("echo hello; rm -rf /") {
            PolicyDecision::Deny(_) => {}
            other => panic!("Expected Deny, got {:?}", other),
        }
    }

    #[test]
    fn and_operator_catches_dangerous() {
        let e = engine(&["echo *"], &[], &[]);
        match e.evaluate("echo hello && rm -rf /") {
            PolicyDecision::Deny(_) => {}
            other => panic!("Expected Deny, got {:?}", other),
        }
    }

    #[test]
    fn or_operator_catches_dangerous() {
        let e = engine(&["*"], &[], &[]);
        match e.evaluate("false || rm -rf /") {
            PolicyDecision::Deny(_) => {}
            other => panic!("Expected Deny, got {:?}", other),
        }
    }

    #[test]
    fn pipe_catches_dangerous() {
        let e = engine(&["echo *", "cat *"], &[], &[]);
        match e.evaluate("echo hello | rm -rf /") {
            PolicyDecision::Deny(_) => {}
            other => panic!("Expected Deny, got {:?}", other),
        }
    }

    // ── Subshell bypass scenarios ──────────────────────────────────

    #[test]
    fn dollar_paren_subshell_denied() {
        let e = engine(&["echo *"], &[], &[]);
        match e.evaluate("echo $(rm -rf /)") {
            PolicyDecision::Deny(_) => {}
            other => panic!("Expected Deny, got {:?}", other),
        }
    }

    #[test]
    fn backtick_subshell_denied() {
        let e = engine(&["echo *"], &[], &[]);
        match e.evaluate("echo `rm -rf /`") {
            PolicyDecision::Deny(_) => {}
            other => panic!("Expected Deny, got {:?}", other),
        }
    }

    #[test]
    fn nested_subshell_denied() {
        let e = engine(&["echo *"], &[], &[]);
        match e.evaluate("echo $(echo $(rm -rf /))") {
            PolicyDecision::Deny(_) => {}
            other => panic!("Expected Deny, got {:?}", other),
        }
    }

    // ── Flag normalization bypass scenarios ─────────────────────────

    #[test]
    fn rm_split_flags_denied() {
        let e = engine(&["*"], &[], &[]);
        for cmd in [
            "rm -rf /",
            "rm -r -f /",
            "rm --recursive --force /",
            "rm --force --recursive /",
            "rm -f -r /",
        ] {
            match e.evaluate(cmd) {
                PolicyDecision::Deny(_) => {}
                other => panic!("Expected Deny for '{cmd}', got {:?}", other),
            }
        }
    }

    #[test]
    fn rm_safe_cases_allowed() {
        let e = engine(&["*"], &[], &[]);
        assert_eq!(e.evaluate("rm file.txt"), PolicyDecision::Allow);
        assert_eq!(e.evaluate("rm -f file.txt"), PolicyDecision::Allow);
        assert_eq!(e.evaluate("rm -rf /tmp/mydir"), PolicyDecision::Allow);
    }

    // ── Shell wrapper bypass ───────────────────────────────────────

    #[test]
    fn bash_c_wrapper_denied() {
        let e = engine(&["*"], &[], &[]);
        match e.evaluate(r#"bash -c "rm -rf /""#) {
            PolicyDecision::Deny(_) => {}
            other => panic!("Expected Deny, got {:?}", other),
        }
    }

    #[test]
    fn sh_c_wrapper_denied() {
        let e = engine(&["*"], &[], &[]);
        match e.evaluate("sh -c 'rm -rf /'") {
            PolicyDecision::Deny(_) => {}
            other => panic!("Expected Deny, got {:?}", other),
        }
    }

    // ── Variable expansion ─────────────────────────────────────────

    #[test]
    fn variable_expansion_requires_approval() {
        let e = engine(&["echo *"], &[], &[]);
        assert_eq!(e.evaluate("echo $HOME"), PolicyDecision::RequireApproval);
        assert_eq!(e.evaluate("echo ${PATH}"), PolicyDecision::RequireApproval);
    }

    #[test]
    fn variable_in_single_quotes_is_safe() {
        let e = engine(&["echo *"], &[], &[]);
        assert_eq!(e.evaluate("echo '$HOME'"), PolicyDecision::Allow);
    }

    // ── Suspicious commands ────────────────────────────────────────

    #[test]
    fn eval_requires_approval() {
        let e = engine(&["*"], &[], &[]);
        assert_eq!(e.evaluate("eval something"), PolicyDecision::RequireApproval);
    }

    // ── Operators inside quotes not split ──────────────────────────

    #[test]
    fn operators_inside_quotes_not_split() {
        let e = engine(&["echo *"], &[], &[]);
        assert_eq!(e.evaluate("echo 'hello; world'"), PolicyDecision::Allow);
        assert_eq!(
            e.evaluate(r#"echo "hello && world""#),
            PolicyDecision::Allow
        );
    }

    // ── Deny pattern matches sub-command ───────────────────────────

    #[test]
    fn deny_pattern_matches_subcommand() {
        let e = engine(&["echo *"], &["curl *"], &[]);
        match e.evaluate("echo hello; curl http://evil.com") {
            PolicyDecision::Deny(_) => {}
            other => panic!("Expected Deny, got {:?}", other),
        }
    }
}
